use org3d::{server, settings::Settings};
use tauri::Manager;

#[tauri::command]
async fn pick_folder() -> Option<String> {
    rfd::AsyncFileDialog::new()
        .pick_folder()
        .await
        .map(|h| h.path().to_string_lossy().into_owned())
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "org3d=info".parse().unwrap()),
        )
        .init();

    let settings_exists = Settings::config_file().exists();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![pick_folder])
        .setup(move |app| {
            let app_data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&app_data_dir)?;

            let mut settings = Settings::load();

            // Resolve empty paths to the Tauri app data directory.
            if settings.db_path.is_empty() {
                settings.db_path = app_data_dir.join("org3d.db").to_string_lossy().into_owned();
            }
            if settings.thumb_dir.is_empty() {
                settings.thumb_dir = app_data_dir.join("thumbs").to_string_lossy().into_owned();
            }
            std::fs::create_dir_all(&settings.thumb_dir)?;

            let data_dir = app_data_dir.to_string_lossy().into_owned();
            let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();

            tauri::async_runtime::spawn(async move {
                // Background scan if base folder is set.
                if settings.is_configured() {
                    let db_path = settings.db_path.clone();
                    let base = settings.base_folder.clone();
                    let thumbs = settings.thumb_dir.clone();
                    let rescan = settings.rescan_on_startup;
                    tokio::task::spawn_blocking(move || {
                        if let Ok(conn) = org3d::db::open(&db_path) {
                            let existing = org3d::db::count(&conn).unwrap_or(0);
                            if rescan || existing == 0 {
                                let _ = org3d::scanner::scan(
                                    &conn,
                                    std::path::Path::new(&base),
                                    std::path::Path::new(&thumbs),
                                );
                            }
                        }
                    });
                }

                let cfg = server::AppConfig {
                    db_path: settings.db_path,
                    thumb_dir: settings.thumb_dir,
                    files_root: settings.base_folder,
                    port: settings.port,
                    data_dir,
                };

                match server::Application::build(cfg).await {
                    Ok(server_app) => {
                        port_tx.send(server_app.port()).ok();
                        if let Err(e) = server_app.run().await {
                            tracing::error!(error = %e, "org3d server stopped");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to start org3d server");
                        port_tx.send(0).ok();
                    }
                }
            });

            let port = port_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or(0);

            if port == 0 {
                return Err("org3d server failed to start".into());
            }

            let startup_path = if settings_exists { "/" } else { "/settings" };
            let url: url::Url = format!("http://127.0.0.1:{port}{startup_path}")
                .parse()
                .expect("valid startup URL");

            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
                .title("org3d")
                .inner_size(1280.0, 820.0)
                .min_inner_size(800.0, 600.0)
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running org3d");
}
