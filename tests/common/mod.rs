use org3d::{db, server};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

pub struct TestApp {
    pub address: String,
    pub client: reqwest::Client,
    /// Empty per-test library folder the app scans.
    #[allow(dead_code)]
    pub library: PathBuf,
    db_path: PathBuf,
    scratch: Vec<PathBuf>,
}

impl TestApp {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.address, path)
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        for d in &self.scratch {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Spawn the app bound to an OS-assigned port. Seed the DB with the provided closure.
pub async fn spawn_app<F>(seed: F) -> TestApp
where
    F: FnOnce(&rusqlite::Connection),
{
    spawn_app_with(|_| {}, seed).await
}

/// Like `spawn_app`, but lets the test adjust the config (e.g. enable auth).
pub async fn spawn_app_with<C, F>(configure: C, seed: F) -> TestApp
where
    C: FnOnce(&mut server::AppConfig),
    F: FnOnce(&rusqlite::Connection),
{
    let id = TEST_ID.fetch_add(1, Ordering::SeqCst);
    let tmp = std::env::temp_dir();
    let tag = format!("{}_{id}", std::process::id());
    let db_path = tmp.join(format!("org3d_test_{tag}.db"));
    let library = tmp.join(format!("org3d_test_lib_{tag}"));
    let thumbs = tmp.join(format!("org3d_test_thumbs_{tag}"));
    let cache = tmp.join(format!("org3d_test_cache_{tag}"));
    let _ = std::fs::remove_dir_all(&library);
    std::fs::create_dir_all(&library).unwrap();

    let conn = db::open(db_path.to_str().unwrap()).unwrap();
    seed(&conn);
    drop(conn);

    let s = |p: &PathBuf| p.to_string_lossy().into_owned();
    let mut cfg = server::AppConfig {
        db_path: s(&db_path),
        thumb_dir: s(&thumbs),
        cache_dir: s(&cache),
        // Never point this at the shared temp dir: a rescan would walk it all.
        files_root: s(&library),
        data_dir: s(&tmp),
        bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port: 0,
        ..Default::default()
    };

    configure(&mut cfg);
    let app = server::Application::build(cfg).await.unwrap();
    let port = app.port();
    tokio::spawn(app.run());

    TestApp {
        address: format!("http://127.0.0.1:{port}"),
        client: reqwest::Client::new(),
        scratch: vec![library.clone(), thumbs, cache],
        library,
        db_path,
    }
}

/// Spawn with two seeded models (one STL, one 3MF).
#[allow(dead_code)]
pub async fn spawn_seeded() -> TestApp {
    spawn_app(|conn| {
        db::upsert(
            conn,
            &db::ModelRow {
                id: 0,
                path: "/tmp/dragon.stl".to_string(),
                filename: "dragon.stl".to_string(),
                folder: "/miniatures".to_string(),
                format: "STL".to_string(),
                file_size: 2048,
                title: Some("Dragon".to_string()),
                designer: Some("Alice".to_string()),
                description: Some("A fearsome dragon".to_string()),
                application: None,
                license: None,
                created_at: None,
                object_count: Some(1),
                triangle_count: Some(500),
                dim_x: Some(80.0),
                dim_y: Some(60.0),
                dim_z: Some(40.0),
                thumbnail_path: None,
                project_id: None,
                mtime: None,
            },
        )
        .unwrap();

        db::upsert(
            conn,
            &db::ModelRow {
                id: 0,
                path: "/tmp/bunny.3mf".to_string(),
                filename: "bunny.3mf".to_string(),
                folder: "/cute".to_string(),
                format: "3MF".to_string(),
                file_size: 512,
                title: Some("Bunny".to_string()),
                designer: Some("Bob".to_string()),
                description: Some("An adorable bunny".to_string()),
                application: Some("BambuStudio".to_string()),
                license: None,
                created_at: Some("2025-01-01".to_string()),
                object_count: Some(1),
                triangle_count: Some(200),
                dim_x: Some(30.0),
                dim_y: Some(25.0),
                dim_z: Some(20.0),
                thumbnail_path: None,
                project_id: None,
                mtime: None,
            },
        )
        .unwrap();
    })
    .await
}
