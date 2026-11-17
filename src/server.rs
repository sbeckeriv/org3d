use anyhow::Result;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Form, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use minijinja::{Value, value::Serde};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::db::{self, ModelRow};
use crate::preview::PreviewCache;

// ── Application bootstrap ────────────────────────────────────────────────────

/// How the server is being run. Controls defaults that differ between the
/// desktop app (single local user) and a headless LAN server (e.g. Docker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum Mode {
    /// Tauri desktop app: bound to loopback, falls back to a free port,
    /// settings are editable in the UI, "Open in slicer" launches local apps.
    Desktop,
    /// Headless server: configured by CLI/env, fails fast on a busy port,
    /// settings page is read-only, model files are offered as downloads.
    #[default]
    Server,
}

pub struct AppConfig {
    pub db_path: String,
    pub thumb_dir: String,
    /// Converted-STL preview cache. Should be on fast, always-on storage.
    pub cache_dir: String,
    pub files_root: String,
    pub bind: IpAddr,
    pub port: u16,
    pub data_dir: String,
    pub mode: Mode,
    /// Daily rescan time (local time, honours `TZ`). None = manual only.
    pub rescan_at: Option<jiff::civil::Time>,
    /// Preview cache cap in bytes; 0 disables the cache.
    pub preview_cache_bytes: u64,
    /// Days a vanished file stays hidden-but-restorable before its row (and
    /// project assignment) is deleted. 0 = keep forever.
    pub purge_missing_days: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            db_path: "org3d.db".into(),
            thumb_dir: "thumbs".into(),
            cache_dir: "cache".into(),
            files_root: String::new(),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 3000,
            data_dir: ".".into(),
            mode: Mode::Server,
            rescan_at: None,
            preview_cache_bytes: 1024 * 1024 * 1024,
            purge_missing_days: 30,
        }
    }
}

pub struct Application {
    port: u16,
    listener: tokio::net::TcpListener,
    state: SharedState,
}

impl Application {
    pub async fn build(cfg: AppConfig) -> Result<Self> {
        let conn = crate::db::open(&cfg.db_path)?;
        std::fs::create_dir_all(&cfg.thumb_dir)?;
        let preferred_slicer = match cfg.mode {
            Mode::Desktop => crate::settings::Settings::load().preferred_slicer,
            Mode::Server => None,
        };

        let addr = SocketAddr::new(cfg.bind, cfg.port);
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            // Desktop: never fail to launch over a busy port. Server: a port
            // that silently changes is unreachable behind a Docker port map,
            // so fail loudly instead.
            Err(e) if cfg.port != 0 && cfg.mode == Mode::Desktop => {
                tracing::warn!(port = cfg.port, error = %e, "port in use, binding to OS-assigned port");
                tokio::net::TcpListener::bind(SocketAddr::new(cfg.bind, 0)).await?
            }
            Err(e) => return Err(anyhow::anyhow!("cannot bind {addr}: {e}")),
        };
        let port = listener.local_addr()?.port();

        let state = Arc::new(AppState {
            conn: Mutex::new(conn),
            preview: PreviewCache::new(&cfg.cache_dir, cfg.preview_cache_bytes),
            db_path: cfg.db_path,
            thumb_dir: cfg.thumb_dir,
            cache_dir: cfg.cache_dir,
            files_root: RwLock::new(cfg.files_root),
            env: make_env(),
            preferred_slicer: RwLock::new(preferred_slicer),
            data_dir: cfg.data_dir,
            mode: cfg.mode,
            rescan_at: cfg.rescan_at,
            preview_cache_bytes: cfg.preview_cache_bytes,
            purge_missing_days: cfg.purge_missing_days,
            scanning: AtomicBool::new(false),
            update_banner: Arc::new(Mutex::new(None)),
        });
        Ok(Application {
            port,
            listener,
            state,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn update_banner(&self) -> Arc<Mutex<Option<String>>> {
        self.state.update_banner.clone()
    }

    /// Start a background scan of the library. Returns false if one is running
    /// or no library folder is configured. Must be called inside a Tokio runtime.
    pub fn start_scan(&self) -> bool {
        start_scan(&self.state, false).is_ok()
    }

    pub async fn run(self) -> Result<()> {
        if let Some(at) = self.state.rescan_at {
            tokio::spawn(rescan_loop(self.state.clone(), at));
        }
        tracing::info!(
            "listening on http://{}:{}",
            self.listener.local_addr()?.ip(),
            self.port
        );
        axum::serve(self.listener, make_router(self.state))
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        Ok(())
    }
}

/// Resolve on Ctrl-C or SIGTERM (`docker stop`) so in-flight requests finish
/// and SQLite connections are dropped cleanly.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}

async fn rescan_loop(state: SharedState, at: jiff::civil::Time) {
    loop {
        let (next, wait) = match crate::schedule::until_next(at) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("rescan schedule disabled: {e}");
                return;
            }
        };
        tracing::info!("next scheduled rescan at {next}");
        tokio::time::sleep(wait).await;
        if let Err(msg) = start_scan(&state, false) {
            tracing::info!("scheduled rescan skipped: {msg}");
        }
    }
}

// ── State ────────────────────────────────────────────────────────────────────

pub struct AppState {
    conn: Mutex<Connection>,
    preview: PreviewCache,
    pub db_path: String,
    pub thumb_dir: String,
    pub cache_dir: String,
    pub files_root: RwLock<String>,
    pub env: minijinja::Environment<'static>,
    pub preferred_slicer: RwLock<Option<String>>,
    pub data_dir: String,
    pub mode: Mode,
    pub rescan_at: Option<jiff::civil::Time>,
    pub preview_cache_bytes: u64,
    pub purge_missing_days: u32,
    pub scanning: AtomicBool,
    pub update_banner: Arc<Mutex<Option<String>>>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    /// Lock the shared connection. A panic in another handler must not take
    /// the whole UI down, so recover from poisoning instead of unwrapping.
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn files_root(&self) -> String {
        self.files_root
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn preferred_slicer(&self) -> Option<String> {
        self.preferred_slicer
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// "Open in slicer" shells out to `open -a` on the machine running the
    /// server, which is only meaningful for the macOS desktop app.
    fn can_open_local(&self) -> bool {
        self.mode == Mode::Desktop && cfg!(target_os = "macos")
    }

    fn render(&self, name: &str, ctx: Value) -> Response {
        match self.env.get_template(name).and_then(|t| t.render(ctx)) {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                tracing::error!("template {name}: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
            }
        }
    }
}

/// Resets the scanning flag even if the scan panics.
struct ScanGuard(SharedState);
impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.0.scanning.store(false, Ordering::Release);
    }
}

fn start_scan(state: &SharedState, extract_first: bool) -> Result<(), &'static str> {
    let root = state.files_root();
    if root.is_empty() {
        return Err("No base folder configured.");
    }
    if state
        .scanning
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("A scan is already in progress.");
    }
    let guard = ScanGuard(state.clone());
    tokio::task::spawn_blocking(move || {
        let st = &guard.0;
        let root = std::path::Path::new(&root);
        if extract_first {
            match crate::extract::extract_all(root) {
                Ok(results) => {
                    let n = results.iter().filter(|r| !r.skipped).count();
                    tracing::info!("extracted {n} new ZIPs");
                    if n == 0 {
                        return;
                    }
                }
                Err(e) => {
                    tracing::error!("extract failed: {e:#}");
                    return;
                }
            }
        }
        let res = crate::db::open(&st.db_path).and_then(|conn| {
            let opts = crate::scanner::ScanOptions {
                purge_missing_after_days: st.purge_missing_days,
                ..Default::default()
            };
            crate::scanner::scan_with(&conn, root, std::path::Path::new(&st.thumb_dir), &opts)
        });
        if let Err(e) = res {
            tracing::error!("scan failed: {e:#}");
        }
    });
    Ok(())
}

// ── Router ───────────────────────────────────────────────────────────────────

pub fn make_env() -> minijinja::Environment<'static> {
    let mut env = minijinja::Environment::new();
    env.set_auto_escape_callback(|name: &str| {
        if name.ends_with(".html") {
            minijinja::AutoEscape::Html
        } else {
            minijinja::AutoEscape::None
        }
    });
    for (name, src) in [
        ("gallery.html", include_str!("../templates/gallery.html")),
        ("cards.html", include_str!("../templates/cards.html")),
        (
            "pagination.html",
            include_str!("../templates/pagination.html"),
        ),
        ("detail.html", include_str!("../templates/detail.html")),
        ("projects.html", include_str!("../templates/projects.html")),
        (
            "project_widget.html",
            include_str!("../templates/project_widget.html"),
        ),
        ("settings.html", include_str!("../templates/settings.html")),
    ] {
        env.add_template(name, src)
            .expect("built-in template parses");
    }
    env
}

pub fn make_router(state: SharedState) -> Router {
    let thumb_dir = state.thumb_dir.clone();
    Router::new()
        .route("/health_check", get(health_check))
        .route("/", get(gallery))
        .route("/projects", get(projects_page))
        .route("/settings", get(settings_page).post(save_settings))
        .route("/static/{name}", get(static_asset))
        .route("/api/scan/status", get(scan_status))
        .route("/api/update/status", get(update_status_handler))
        .route("/search", get(search_results))
        .route("/model/{id}", get(model_detail))
        .route("/file/{id}", get(serve_model_file))
        .route("/stl/{id}", get(serve_as_stl))
        .route("/api/autogroup", post(autogroup_handler))
        .route("/api/rescan", post(rescan_handler))
        .route("/api/extract", post(extract_handler))
        .route("/api/model/{id}/thumbnail", post(upload_thumbnail))
        .route("/api/model/{id}/open/{app}", post(open_in_slicer))
        .route("/api/model/{id}/project", post(set_project))
        .route("/api/model/{id}/project", delete(clear_project))
        .nest_service("/thumbs", ServeDir::new(&thumb_dir))
        .layer(middleware::from_fn(reject_cross_site_writes))
        .with_state(state)
}

/// CSRF guard. The app has no login, so without this any web page a LAN user
/// visits could POST to it (e.g. trigger extraction into the library).
/// Browsers send `Sec-Fetch-Site`; older ones send `Origin`. Non-browser
/// clients (curl, scripts) send neither and are allowed.
async fn reject_cross_site_writes(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let h = req.headers();
    let site = h.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    let cross = match site {
        Some(s) => !matches!(s, "same-origin" | "none"),
        None => match (
            h.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
            h.get(header::HOST).and_then(|v| v.to_str().ok()),
        ) {
            (Some(origin), Some(host)) => origin
                .split_once("://")
                .map(|(_, rest)| rest != host)
                .unwrap_or(true),
            _ => false,
        },
    };
    if cross {
        return (StatusCode::FORBIDDEN, "cross-site request rejected").into_response();
    }
    next.run(req).await
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn health_check() -> StatusCode {
    StatusCode::OK
}

async fn static_asset(Path(name): Path<String>) -> Response {
    let (body, ct): (&'static [u8], &str) = match name.as_str() {
        "htmx-2.0.3.min.js" => (
            include_bytes!("../static/vendor/htmx.min.js"),
            "text/javascript",
        ),
        "three-r128.min.js" => (
            include_bytes!("../static/vendor/three.min.js"),
            "text/javascript",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, ct),
            // File names are versioned, so they can be cached forever.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub struct GalleryQuery {
    pub q: Option<String>,
    pub format: Option<String>,
    pub project: Option<String>, // String so empty "project=" doesn't 400
    pub page: Option<i64>,
}

const PAGE_SIZE: i64 = 48;

fn parse_project(s: Option<&str>) -> Option<i64> {
    s.and_then(|v| v.parse().ok())
}

fn truncate_description(s: &str) -> String {
    let s = s.replace('\n', " ");
    let mut chars = s.chars();
    let out: String = chars.by_ref().take(300).collect();
    if chars.next().is_some() {
        format!("{out}…")
    } else {
        out
    }
}

/// Treat empty strings from query params as absent (no filter).
fn nonempty(s: Option<&str>) -> Option<&str> {
    s.filter(|v| !v.is_empty())
}

/// Clamp the page so a huge `?page=` can't overflow `page * PAGE_SIZE`.
fn page_of(p: Option<i64>) -> i64 {
    p.unwrap_or(0).clamp(0, i64::MAX / PAGE_SIZE - 1)
}

fn search_params(params: &GalleryQuery, page: i64) -> db::SearchParams<'_> {
    db::SearchParams {
        query: nonempty(params.q.as_deref()),
        designer: None,
        folder: None,
        format: nonempty(params.format.as_deref()),
        project: parse_project(params.project.as_deref()),
        limit: PAGE_SIZE,
        offset: page * PAGE_SIZE,
    }
}

fn log_err<T: Default>(what: &str, r: Result<T>) -> T {
    r.unwrap_or_else(|e| {
        tracing::warn!("{what}: {e:#}");
        T::default()
    })
}

async fn gallery(State(state): State<SharedState>, Query(params): Query<GalleryQuery>) -> Response {
    let page = page_of(params.page);
    let sp = search_params(&params, page);
    let (models, projects, total) = {
        let conn = state.db();
        let models: Vec<ModelCtx> = log_err("search", db::search(&conn, &sp))
            .iter()
            .map(ModelCtx::from)
            .collect();
        (
            models,
            log_err("list_projects", db::list_projects(&conn)),
            log_err("search_count", db::search_count(&conn, &sp)),
        )
    };

    state.render(
        "gallery.html",
        minijinja::context! {
            total,
            models => Value::from(Serde(&models)),
            projects => Value::from(Serde(&projects)),
            q       => params.q.as_deref().unwrap_or(""),
            format  => params.format.as_deref().unwrap_or(""),
            project => parse_project(params.project.as_deref()),
            page,
            prev_page => if page > 0 { Some(page - 1) } else { None::<i64> },
            next_page => page + 1,
        },
    )
}

async fn search_results(
    State(state): State<SharedState>,
    Query(params): Query<GalleryQuery>,
) -> Response {
    let page = page_of(params.page);
    let q = params.q.as_deref().unwrap_or("").to_string();
    let format = params.format.as_deref().unwrap_or("").to_string();
    let project = parse_project(params.project.as_deref());
    let sp = search_params(&params, page);
    let (models, total) = {
        let conn = state.db();
        let models: Vec<ModelCtx> = log_err("search", db::search(&conn, &sp))
            .iter()
            .map(ModelCtx::from)
            .collect();
        (
            models,
            log_err("search_count", db::search_count(&conn, &sp)),
        )
    };

    let render = |name: &str, ctx: Value| {
        state
            .env
            .get_template(name)
            .and_then(|t| t.render(ctx))
            .map_err(|e| tracing::error!("template {name}: {e:#}"))
    };
    let prev_page: Option<i64> = if page > 0 { Some(page - 1) } else { None };
    let next_page = page + 1;
    match (
        render(
            "cards.html",
            minijinja::context! { models => Value::from(Serde(&models)) },
        ),
        render(
            "pagination.html",
            minijinja::context! { total, page, prev_page, next_page, q, format, project },
        ),
    ) {
        (Ok(cards), Ok(pagination)) => Html(format!("{cards}\n{pagination}")).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response(),
    }
}

async fn model_detail(State(state): State<SharedState>, Path(id): Path<i64>) -> Response {
    let loaded = {
        let conn = state.db();
        match db::get_by_id(&conn, id) {
            Ok(Some(row)) => {
                let project_name = row
                    .project_id
                    .and_then(|pid| db::get_project_name(&conn, pid).ok().flatten());
                let projects = log_err("list_projects", db::list_projects(&conn));
                Some((row, project_name, projects))
            }
            _ => None,
        }
    };
    let Some((row, project_name, projects)) = loaded else {
        return (StatusCode::NOT_FOUND, "model not found").into_response();
    };
    state.render(
        "detail.html",
        minijinja::context! {
            m => Value::from(Serde(ModelCtx::from(&row))),
            project_name,
            projects => Value::from(Serde(&projects)),
            preferred_slicer => state.preferred_slicer(),
            can_open_local => state.can_open_local(),
        },
    )
}

async fn projects_page(State(state): State<SharedState>) -> Response {
    let projects = log_err("list_projects", db::list_projects(&state.db()));
    state.render(
        "projects.html",
        minijinja::context! { projects => Value::from(Serde(&projects)) },
    )
}

/// Look up the file for a model, holding the DB lock only for the query.
fn lookup_file(state: &AppState, id: i64) -> Option<db::FileRef> {
    db::file_ref(&state.db(), id).ok().flatten()
}

/// STL bytes for the 3D viewer. Served from the SSD preview cache when
/// possible; misses read/convert the original on a blocking thread so a disk
/// spinning up (several seconds) doesn't stall the async runtime or the DB.
async fn serve_as_stl(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let Some(f) = lookup_file(&state, id) else {
        return (StatusCode::NOT_FOUND, "model not found").into_response();
    };
    let etag = format!("\"{}\"", PreviewCache::key(id, &f));
    // The ETag comes from the DB, so a revalidation never touches the library.
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == etag.as_bytes())
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    let st = state.clone();
    match tokio::task::spawn_blocking(move || st.preview.get_or_build(id, &f)).await {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, "model/stl".to_string()),
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!("preview {id}: {e:#}");
            (StatusCode::NOT_FOUND, "could not read model").into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Stream the original file (supports Range / conditional requests) without
/// buffering it in memory. `?download=1` sets Content-Disposition: attachment.
async fn serve_model_file(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Query(q): Query<DownloadQuery>,
    req: Request,
) -> Response {
    let Some(f) = lookup_file(&state, id) else {
        return (StatusCode::NOT_FOUND, "model not found").into_response();
    };
    let mut res = match ServeFile::new(&f.path).oneshot(req).await {
        Ok(r) => r.map(Body::new),
        Err(e) => match e {},
    };
    if res.status() == StatusCode::NOT_FOUND {
        return (StatusCode::NOT_FOUND, "file not on disk").into_response();
    }
    if f.format == "3MF" {
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("model/3mf"));
    }
    if q.download.is_some()
        && let Ok(v) = HeaderValue::from_str(&content_disposition(&f.filename))
    {
        res.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

#[derive(Deserialize)]
struct DownloadQuery {
    download: Option<String>,
}

/// RFC 6266 attachment header with an ASCII fallback and a UTF-8 filename*.
fn content_disposition(filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in filename.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

async fn upload_thumbnail(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    body: Bytes,
) -> StatusCode {
    // Only accept real PNGs: anything stored here is served back from /thumbs.
    if !body.starts_with(PNG_MAGIC) {
        return StatusCode::BAD_REQUEST;
    }
    if lookup_file(&state, id).is_none() {
        return StatusCode::NOT_FOUND;
    }
    let thumb_name = format!("cap_{id}.png");
    let dir = std::path::PathBuf::from(&state.thumb_dir);
    let write = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let tmp = dir.join(format!("{thumb_name}.tmp"));
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, dir.join(&thumb_name))
    })
    .await;
    if !matches!(write, Ok(Ok(()))) {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    match db::update_thumbnail(&state.db(), id, &format!("/thumbs/cap_{id}.png")) {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::warn!("update_thumbnail {id}: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[derive(Deserialize)]
struct SetProjectBody {
    name: String,
}

fn project_widget(state: &AppState, conn: &Connection, id: i64, name: Option<String>) -> Response {
    let projects = log_err("list_projects", db::list_projects(conn));
    state.render(
        "project_widget.html",
        minijinja::context! {
            m_id => id,
            project_name => name,
            projects => Value::from(Serde(&projects)),
        },
    )
}

async fn set_project(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    axum::Json(body): axum::Json<SetProjectBody>,
) -> Response {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let conn = state.db();
    match db::find_or_create_project(&conn, &name)
        .and_then(|pid| db::assign_project(&conn, id, pid))
    {
        Ok(_) => project_widget(&state, &conn, id, Some(name)),
        Err(e) => {
            tracing::warn!("set_project error: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn clear_project(State(state): State<SharedState>, Path(id): Path<i64>) -> Response {
    let conn = state.db();
    if let Err(e) = db::remove_from_project(&conn, id) {
        tracing::warn!("clear_project error: {e:#}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    project_widget(&state, &conn, id, None)
}

fn settings_ctx(
    state: &AppState,
    settings: &crate::settings::Settings,
    saved: bool,
    scan_started: bool,
) -> Value {
    minijinja::context! {
        settings => Value::from(Serde(settings)),
        saved,
        scan_started,
        data_dir => &state.data_dir,
        managed => state.mode == Mode::Server,
        runtime => minijinja::context! {
            files_root => state.files_root(),
            db_path => &state.db_path,
            thumb_dir => &state.thumb_dir,
            cache_dir => &state.cache_dir,
            preview_cache_mb => state.preview_cache_bytes / (1024 * 1024),
            purge_missing_days => state.purge_missing_days,
            rescan_at => state.rescan_at.map(|t| t.strftime("%H:%M").to_string()),
        },
    }
}

async fn settings_page(State(state): State<SharedState>) -> Response {
    let settings = crate::settings::Settings::load();
    state.render(
        "settings.html",
        settings_ctx(&state, &settings, false, false),
    )
}

#[derive(Deserialize)]
struct SettingsForm {
    base_folder: String,
    port: String,
    db_path: String,
    thumb_dir: String,
    preferred_slicer: String,
    rescan_on_startup: Option<String>,
}

async fn save_settings(
    State(state): State<SharedState>,
    Form(form): Form<SettingsForm>,
) -> Response {
    if state.mode == Mode::Server {
        return (
            StatusCode::FORBIDDEN,
            "Settings are managed by command-line flags / environment variables.",
        )
            .into_response();
    }
    let previous = crate::settings::Settings::load();
    let preferred_slicer = match form.preferred_slicer.as_str() {
        "none" | "" => None,
        s if KNOWN_SLICERS.iter().any(|(k, _)| *k == s) => Some(s.to_string()),
        _ => previous.preferred_slicer.clone(),
    };
    let base_folder = form.base_folder.trim().to_string();
    let settings = crate::settings::Settings {
        base_folder: base_folder.clone(),
        // Keep the old port on a typo rather than silently switching to "any".
        port: form.port.trim().parse::<u16>().unwrap_or(previous.port),
        db_path: form.db_path.trim().to_string(),
        thumb_dir: form.thumb_dir.trim().to_string(),
        preferred_slicer: preferred_slicer.clone(),
        rescan_on_startup: form.rescan_on_startup.as_deref() == Some("on"),
    };
    if let Ok(mut guard) = state.preferred_slicer.write() {
        *guard = preferred_slicer;
    }
    let folder_changed = state.files_root() != base_folder;
    if let Ok(mut guard) = state.files_root.write() {
        *guard = base_folder.clone();
    }
    let saved = match settings.save() {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("saving settings: {e:#}");
            false
        }
    };
    // Only rescan when the library actually moved; saving e.g. the slicer
    // preference shouldn't walk the whole library.
    let scan_started = folder_changed && start_scan(&state, false).is_ok();
    state.render(
        "settings.html",
        settings_ctx(&state, &settings, saved, scan_started),
    )
}

const KNOWN_SLICERS: &[(&str, &str)] = &[
    ("bambu", "BambuStudio"),
    ("orca", "OrcaSlicer"),
    ("prusa", "PrusaSlicer"),
];

async fn open_in_slicer(
    State(state): State<SharedState>,
    Path((id, app)): Path<(i64, String)>,
) -> StatusCode {
    if !state.can_open_local() {
        return StatusCode::NOT_FOUND;
    }
    let app_name = match KNOWN_SLICERS.iter().find(|(key, _)| *key == app.as_str()) {
        Some((_, name)) => *name,
        None => return StatusCode::BAD_REQUEST,
    };
    let Some(f) = lookup_file(&state, id) else {
        return StatusCode::NOT_FOUND;
    };
    match tokio::process::Command::new("open")
        .args(["-a", app_name])
        .arg(&f.path)
        .status()
        .await
    {
        Ok(s) if s.success() => StatusCode::OK,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn status_span(ok: bool, msg: &str) -> Html<String> {
    let color = if ok { "#80c080" } else { "var(--muted)" };
    Html(format!(r#"<span style="color:{color}">{msg}</span>"#))
}

async fn rescan_handler(State(state): State<SharedState>) -> Html<String> {
    match start_scan(&state, false) {
        Ok(()) => status_span(
            true,
            r#"Scan started — check the <a href="/" style="color:inherit">Gallery</a> for progress."#,
        ),
        Err(msg) => status_span(false, msg),
    }
}

async fn extract_handler(State(state): State<SharedState>) -> Html<String> {
    match start_scan(&state, true) {
        Ok(()) => status_span(
            true,
            r#"Extracting ZIPs and scanning — check the <a href="/" style="color:inherit">Gallery</a> for progress."#,
        ),
        Err(msg) => status_span(false, msg),
    }
}

async fn autogroup_handler(State(state): State<SharedState>) -> Html<String> {
    let result = crate::autogroup::run(&state.db());
    match result {
        Ok(s) if s.models_assigned == 0 => status_span(
            false,
            "Nothing to group — run a scan first or all models are already assigned.",
        ),
        Ok(s) => status_span(
            true,
            &format!(
                "Created {} project{}, assigned {} model{}.",
                s.projects_created,
                if s.projects_created == 1 { "" } else { "s" },
                s.models_assigned,
                if s.models_assigned == 1 { "" } else { "s" },
            ),
        ),
        Err(e) => {
            tracing::warn!("autogroup: {e:#}");
            Html(r#"<span style="color:#c08080">Autogroup failed — see server log.</span>"#.into())
        }
    }
}

async fn scan_status(State(state): State<SharedState>) -> Html<String> {
    let is_scanning = state.scanning.load(Ordering::Acquire);
    let count = log_err("count", db::count(&state.db()));
    if is_scanning {
        Html(format!(
            r#"<span id="scan-status" class="count" hx-get="/api/scan/status" hx-trigger="every 1s" hx-swap="outerHTML"><span class="spinner"></span> {count} models</span>"#
        ))
    } else {
        Html(format!(
            r#"<span id="scan-status" class="count">{count} models</span>"#
        ))
    }
}

async fn update_status_handler(State(state): State<SharedState>) -> Html<String> {
    // The self-updater only exists in the desktop app; don't make every open
    // tab on a headless server poll forever.
    if state.mode == Mode::Server {
        return Html(r#"<div id="update-banner"></div>"#.into());
    }
    let banner = state.update_banner.lock().ok().and_then(|b| b.clone());
    match banner {
        Some(msg) => Html(format!(
            r#"<div id="update-banner" class="update-banner" hx-get="/api/update/status" hx-trigger="every 2s" hx-swap="outerHTML">{msg}</div>"#
        )),
        None => Html(
            r#"<div id="update-banner" hx-get="/api/update/status" hx-trigger="every 15s" hx-swap="outerHTML"></div>"#.to_string(),
        ),
    }
}

// ── View model ───────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct ModelCtx {
    pub id: i64,
    pub title: String,
    pub designer: Option<String>,
    pub description: Option<String>,
    pub format: String,
    pub format_lower: String,
    pub thumbnail_path: Option<String>,
    pub dims: Option<String>,
    pub triangle_count: Option<i64>,
    pub application: Option<String>,
    pub license: Option<String>,
    pub created_at: Option<String>,
    pub path: String,
    pub filename: String,
    pub folder: String,
}

impl From<&ModelRow> for ModelCtx {
    fn from(m: &ModelRow) -> Self {
        ModelCtx {
            id: m.id,
            title: m.display_name().to_string(),
            designer: m.designer.clone(),
            description: m.description.as_deref().map(truncate_description),
            format: m.format.clone(),
            format_lower: m.format.to_lowercase(),
            thumbnail_path: m.thumbnail_path.clone(),
            dims: m.dims_str(),
            triangle_count: m.triangle_count,
            application: m.application.clone(),
            license: m.license.clone(),
            created_at: m.created_at.clone(),
            path: m.path.clone(),
            filename: m.filename.clone(),
            folder: m.folder.clone(),
        }
    }
}
