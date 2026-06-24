use anyhow::Result;
use axum::{
    Router,
    body::Bytes,
    extract::{Form, Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::{delete, get, post},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tower_http::services::ServeDir;

use crate::db::{self, ModelRow};

// ── Application bootstrap ────────────────────────────────────────────────────

pub struct AppConfig {
    pub db_path: String,
    pub thumb_dir: String,
    pub files_root: String,
    pub port: u16,
    pub data_dir: String,
}

pub struct Application {
    port: u16,
    listener: tokio::net::TcpListener,
    state: SharedState,
}

impl Application {
    pub async fn build(cfg: AppConfig) -> Result<Self> {
        let conn = crate::db::open(&cfg.db_path)?;
        let preferred_slicer = crate::settings::Settings::load().preferred_slicer;
        let state = Arc::new(AppState {
            conn: Mutex::new(conn),
            db_path: cfg.db_path,
            thumb_dir: cfg.thumb_dir,
            files_root: std::sync::RwLock::new(cfg.files_root),
            env: make_env(),
            preferred_slicer: std::sync::RwLock::new(preferred_slicer),
            data_dir: cfg.data_dir,
            scanning: Arc::new(AtomicBool::new(false)),
            update_banner: Arc::new(Mutex::new(None)),
        });
        // Port 0 lets the OS assign a free port. If a specific port is configured
        // but already taken, fall back to OS-assigned rather than crashing.
        let listener = match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", cfg.port)).await {
            Ok(l) => l,
            Err(e) if cfg.port != 0 => {
                tracing::warn!(port = cfg.port, error = %e, "port in use, binding to OS-assigned port");
                tokio::net::TcpListener::bind("0.0.0.0:0").await?
            }
            Err(e) => return Err(e.into()),
        };
        let port = listener.local_addr()?.port();
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

    pub async fn run(self) -> Result<()> {
        tracing::info!("listening on http://localhost:{}", self.port);
        axum::serve(self.listener, make_router(self.state)).await?;
        Ok(())
    }
}

// ── Router ───────────────────────────────────────────────────────────────────

pub struct AppState {
    pub conn: Mutex<Connection>,
    pub db_path: String,
    pub thumb_dir: String,
    pub files_root: std::sync::RwLock<String>,
    pub env: minijinja::Environment<'static>,
    pub preferred_slicer: std::sync::RwLock<Option<String>>,
    pub data_dir: String,
    pub scanning: Arc<AtomicBool>,
    pub update_banner: Arc<Mutex<Option<String>>>,
}

pub type SharedState = Arc<AppState>;

pub fn make_env() -> minijinja::Environment<'static> {
    let mut env = minijinja::Environment::new();
    env.set_auto_escape_callback(|name: &str| {
        if name.ends_with(".html") {
            minijinja::AutoEscape::Html
        } else {
            minijinja::AutoEscape::None
        }
    });
    env.add_template("gallery.html", include_str!("../templates/gallery.html"))
        .unwrap();
    env.add_template("cards.html", include_str!("../templates/cards.html"))
        .unwrap();
    env.add_template(
        "pagination.html",
        include_str!("../templates/pagination.html"),
    )
    .unwrap();
    env.add_template("detail.html", include_str!("../templates/detail.html"))
        .unwrap();
    env.add_template("projects.html", include_str!("../templates/projects.html"))
        .unwrap();
    env.add_template(
        "project_widget.html",
        include_str!("../templates/project_widget.html"),
    )
    .unwrap();
    env.add_template("settings.html", include_str!("../templates/settings.html"))
        .unwrap();
    env
}

pub fn make_router(state: SharedState) -> Router {
    let thumb_dir = state.thumb_dir.clone();
    Router::new()
        .route("/health_check", get(health_check))
        .route("/", get(gallery))
        .route("/projects", get(projects_page))
        .route("/settings", get(settings_page).post(save_settings))
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
        .with_state(state)
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn health_check() -> StatusCode {
    StatusCode::OK
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

async fn gallery(
    State(state): State<SharedState>,
    Query(params): Query<GalleryQuery>,
) -> impl IntoResponse {
    let page = params.page.unwrap_or(0).max(0);
    let conn = state.conn.lock().unwrap();

    let sp = db::SearchParams {
        query: nonempty(params.q.as_deref()),
        designer: None,
        folder: None,
        format: nonempty(params.format.as_deref()),
        project: parse_project(params.project.as_deref()),
        limit: PAGE_SIZE,
        offset: page * PAGE_SIZE,
    };
    let models: Vec<ModelCtx> = db::search(&conn, &sp)
        .unwrap_or_default()
        .iter()
        .map(ModelCtx::from)
        .collect();
    let projects = db::list_projects(&conn).unwrap_or_default();
    let total = db::search_count(&conn, &sp).unwrap_or(0);

    let html = state
        .env
        .get_template("gallery.html")
        .unwrap()
        .render(minijinja::context! {
            total, models, projects,
            q       => params.q.as_deref().unwrap_or(""),
            format  => params.format.as_deref().unwrap_or(""),
            project => parse_project(params.project.as_deref()),
            page,
            prev_page => if page > 0 { Some(page - 1) } else { None::<i64> },
            next_page => page + 1,
        })
        .unwrap_or_else(|e| format!("template error: {e}"));

    Html(html)
}

async fn search_results(
    State(state): State<SharedState>,
    Query(params): Query<GalleryQuery>,
) -> impl IntoResponse {
    let page = params.page.unwrap_or(0).max(0);
    let q = params.q.as_deref().unwrap_or("").to_string();
    let format = params.format.as_deref().unwrap_or("").to_string();
    let project = parse_project(params.project.as_deref());
    let conn = state.conn.lock().unwrap();

    let sp = db::SearchParams {
        query: nonempty(params.q.as_deref()),
        designer: None,
        folder: None,
        format: nonempty(params.format.as_deref()),
        project,
        limit: PAGE_SIZE,
        offset: page * PAGE_SIZE,
    };
    let models: Vec<ModelCtx> = db::search(&conn, &sp)
        .unwrap_or_default()
        .iter()
        .map(ModelCtx::from)
        .collect();
    let total = db::search_count(&conn, &sp).unwrap_or(0);

    let cards = state
        .env
        .get_template("cards.html")
        .unwrap()
        .render(minijinja::context! { models })
        .unwrap_or_else(|e| format!("template error: {e}"));

    let prev_page: Option<i64> = if page > 0 { Some(page - 1) } else { None };
    let next_page = page + 1;
    let pagination = state
        .env
        .get_template("pagination.html")
        .unwrap()
        .render(minijinja::context! { total, page, prev_page, next_page, q, format, project })
        .unwrap_or_default();

    Html(format!("{cards}\n{pagination}"))
}

async fn model_detail(State(state): State<SharedState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.conn.lock().unwrap();
    match db::get_by_id(&conn, id) {
        Ok(Some(row)) => {
            let project_name = row
                .project_id
                .and_then(|pid| db::get_project_name(&conn, pid).ok().flatten());
            let projects = db::list_projects(&conn).unwrap_or_default();
            let m = ModelCtx::from(&row);
            let preferred_slicer = state.preferred_slicer.read().ok().and_then(|g| g.clone());
            let html = state
                .env
                .get_template("detail.html")
                .unwrap()
                .render(minijinja::context! { m, project_name, projects, preferred_slicer })
                .unwrap_or_else(|e| format!("template error: {e}"));
            Html(html).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "model not found").into_response(),
    }
}

async fn projects_page(State(state): State<SharedState>) -> impl IntoResponse {
    let conn = state.conn.lock().unwrap();
    let projects = db::list_projects(&conn).unwrap_or_default();
    let html = state
        .env
        .get_template("projects.html")
        .unwrap()
        .render(minijinja::context! { projects })
        .unwrap_or_else(|e| format!("template error: {e}"));
    Html(html)
}

async fn serve_as_stl(State(state): State<SharedState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.conn.lock().unwrap();
    match db::get_by_id(&conn, id) {
        Ok(Some(m)) => {
            let bytes = if m.format == "STL" {
                std::fs::read(&m.path).ok()
            } else {
                crate::formats::threemf::to_binary_stl(std::path::Path::new(&m.path)).ok()
            };
            match bytes {
                Some(b) => ([(header::CONTENT_TYPE, "model/stl")], b).into_response(),
                None => (StatusCode::NOT_FOUND, "could not read model").into_response(),
            }
        }
        _ => (StatusCode::NOT_FOUND, "model not found").into_response(),
    }
}

async fn upload_thumbnail(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    body: Bytes,
) -> StatusCode {
    if body.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    let thumb_name = format!("cap_{id}.png");
    let thumb_path = std::path::PathBuf::from(&state.thumb_dir).join(&thumb_name);
    if std::fs::write(&thumb_path, &body).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    let conn = state.conn.lock().unwrap();
    let _ = db::update_thumbnail(&conn, id, &format!("/thumbs/{thumb_name}"));
    StatusCode::OK
}

#[derive(Deserialize)]
struct SetProjectBody {
    name: String,
}

async fn set_project(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    axum::Json(body): axum::Json<SetProjectBody>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let conn = state.conn.lock().unwrap();
    match db::find_or_create_project(&conn, &name)
        .and_then(|pid| db::assign_project(&conn, id, pid))
    {
        Ok(_) => {
            let project_name: Option<String> = Some(name);
            let projects = db::list_projects(&conn).unwrap_or_default();
            let html = state
                .env
                .get_template("project_widget.html")
                .unwrap()
                .render(minijinja::context! { m_id => id, project_name, projects })
                .unwrap_or_else(|e| format!("error: {e}"));
            Html(html).into_response()
        }
        Err(e) => {
            tracing::warn!("set_project error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn clear_project(State(state): State<SharedState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.conn.lock().unwrap();
    let _ = db::remove_from_project(&conn, id);
    let projects = db::list_projects(&conn).unwrap_or_default();
    let html = state
        .env
        .get_template("project_widget.html")
        .unwrap()
        .render(minijinja::context! { m_id => id, project_name => None::<String>, projects })
        .unwrap_or_else(|e| format!("error: {e}"));
    Html(html)
}

async fn settings_page(State(state): State<SharedState>) -> impl IntoResponse {
    let settings = crate::settings::Settings::load();
    let data_dir = &state.data_dir;
    let html = state
        .env
        .get_template("settings.html")
        .unwrap()
        .render(minijinja::context! { settings, saved => false, data_dir })
        .unwrap_or_else(|e| format!("template error: {e}"));
    Html(html)
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
) -> impl IntoResponse {
    let preferred_slicer = match form.preferred_slicer.as_str() {
        "none" | "" => None,
        s => Some(s.to_string()),
    };
    let base_folder = form.base_folder.trim().to_string();
    let settings = crate::settings::Settings {
        base_folder: base_folder.clone(),
        port: form.port.trim().parse::<u16>().unwrap_or(0),
        db_path: form.db_path.trim().to_string(),
        thumb_dir: form.thumb_dir.trim().to_string(),
        preferred_slicer: preferred_slicer.clone(),
        rescan_on_startup: form.rescan_on_startup.as_deref() == Some("on"),
    };
    if let Ok(mut guard) = state.preferred_slicer.write() {
        *guard = preferred_slicer;
    }
    if let Ok(mut guard) = state.files_root.write() {
        *guard = base_folder.clone();
    }
    let saved = settings.save().is_ok();

    let scan_started = !base_folder.is_empty() && !state.scanning.load(Ordering::Relaxed);
    if scan_started {
        let db_path = state.db_path.clone();
        let thumb_dir = state.thumb_dir.clone();
        let scanning = state.scanning.clone();
        scanning.store(true, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = crate::db::open(&db_path) {
                let _ = crate::scanner::scan(
                    &conn,
                    std::path::Path::new(&base_folder),
                    std::path::Path::new(&thumb_dir),
                );
            }
            scanning.store(false, Ordering::Relaxed);
        });
    }

    let data_dir = &state.data_dir;
    let html = state
        .env
        .get_template("settings.html")
        .unwrap()
        .render(minijinja::context! { settings, saved, scan_started, data_dir })
        .unwrap_or_else(|e| format!("template error: {e}"));
    Html(html)
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
    let app_name = match KNOWN_SLICERS.iter().find(|(key, _)| *key == app.as_str()) {
        Some((_, name)) => *name,
        None => return StatusCode::BAD_REQUEST,
    };
    let conn = state.conn.lock().unwrap();
    let path = match db::get_by_id(&conn, id) {
        Ok(Some(m)) => m.path,
        _ => return StatusCode::NOT_FOUND,
    };
    drop(conn);
    match std::process::Command::new("open")
        .args(["-a", app_name, &path])
        .status()
    {
        Ok(s) if s.success() => StatusCode::OK,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn serve_model_file(
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = state.conn.lock().unwrap();
    match db::get_by_id(&conn, id) {
        Ok(Some(m)) => match std::fs::read(&m.path) {
            Ok(bytes) => {
                let ct = if m.format == "3MF" {
                    "model/3mf"
                } else {
                    "application/octet-stream"
                };
                ([(header::CONTENT_TYPE, ct)], bytes).into_response()
            }
            Err(_) => (StatusCode::NOT_FOUND, "file not on disk").into_response(),
        },
        _ => (StatusCode::NOT_FOUND, "model not found").into_response(),
    }
}

async fn rescan_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let files_root = state
        .files_root
        .read()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    if files_root.is_empty() {
        return Html(
            r#"<span style="color:var(--muted)">No base folder configured.</span>"#.to_string(),
        );
    }
    if state.scanning.load(Ordering::Relaxed) {
        return Html(
            r#"<span style="color:var(--muted)">Scan already in progress.</span>"#.to_string(),
        );
    }
    let db_path = state.db_path.clone();
    let thumb_dir = state.thumb_dir.clone();
    let scanning = state.scanning.clone();
    scanning.store(true, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        if let Ok(conn) = crate::db::open(&db_path) {
            let _ = crate::scanner::scan(
                &conn,
                std::path::Path::new(&files_root),
                std::path::Path::new(&thumb_dir),
            );
        }
        scanning.store(false, Ordering::Relaxed);
    });
    Html(r#"<span style="color:#80c080">Scan started — check the <a href="/" style="color:inherit">Gallery</a> for progress.</span>"#.to_string())
}

async fn extract_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let files_root = state
        .files_root
        .read()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    if files_root.is_empty() {
        return Html(
            r#"<span style="color:var(--muted)">No base folder configured.</span>"#.to_string(),
        );
    }
    if state.scanning.load(Ordering::Relaxed) {
        return Html(r#"<span style="color:var(--muted)">A scan is already in progress — try again after it finishes.</span>"#.to_string());
    }
    let db_path = state.db_path.clone();
    let thumb_dir = state.thumb_dir.clone();
    let scanning = state.scanning.clone();
    scanning.store(true, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        let results =
            crate::extract::extract_all(std::path::Path::new(&files_root)).unwrap_or_default();
        let zips: usize = results.iter().filter(|r| !r.skipped).count();
        let files: usize = results.iter().map(|r| r.files_extracted).sum();
        if zips > 0
            && let Ok(conn) = crate::db::open(&db_path)
        {
            let _ = crate::scanner::scan(
                &conn,
                std::path::Path::new(&files_root),
                std::path::Path::new(&thumb_dir),
            );
        }
        scanning.store(false, Ordering::Relaxed);
        (zips, files)
    });
    Html(r#"<span style="color:#80c080">Extracting ZIPs and scanning — check the <a href="/" style="color:inherit">Gallery</a> for progress.</span>"#.to_string())
}

async fn autogroup_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let conn = state.conn.lock().unwrap();
    match crate::autogroup::run(&conn) {
        Ok(s) if s.models_assigned == 0 => Html(
            r#"<span style="color:var(--muted)">Nothing to group — run a scan first or all models are already assigned.</span>"#.to_string()
        ),
        Ok(s) => Html(format!(
            r#"<span style="color:#80c080">Created {} project{}, assigned {} model{}.</span>"#,
            s.projects_created, if s.projects_created == 1 { "" } else { "s" },
            s.models_assigned,  if s.models_assigned  == 1 { "" } else { "s" },
        )),
        Err(e) => Html(format!(r#"<span style="color:#c08080">Error: {e}</span>"#)),
    }
}

async fn scan_status(State(state): State<SharedState>) -> impl IntoResponse {
    let is_scanning = state.scanning.load(Ordering::Relaxed);
    let count = {
        let conn = state.conn.lock().unwrap();
        crate::db::count(&conn).unwrap_or(0)
    };
    if is_scanning {
        Html(format!(
            r#"<span id="scan-status" class="count" hx-get="/api/scan/status" hx-trigger="every 1s" hx-swap="outerHTML"><span class="spinner"></span> {} models</span>"#,
            count
        ))
    } else {
        Html(format!(
            r#"<span id="scan-status" class="count">{} models</span>"#,
            count
        ))
    }
}

async fn update_status_handler(State(state): State<SharedState>) -> impl IntoResponse {
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
