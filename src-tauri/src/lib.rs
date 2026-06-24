use org3d::{server, settings::Settings};
use std::sync::{Arc, Mutex};
use tauri::Manager;

#[tauri::command]
async fn pick_folder() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        rfd::AsyncFileDialog::new()
            .pick_folder()
            .await
            .map(|h| h.path().to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

// ── Self-updater (macOS only) ─────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> (u64, u64, u64) {
        let mut p = v.split('.').filter_map(|s| s.parse().ok());
        (
            p.next().unwrap_or(0),
            p.next().unwrap_or(0),
            p.next().unwrap_or(0),
        )
    }
    parse(latest) > parse(current)
}

#[cfg(target_os = "macos")]
async fn check_and_update(banner: Arc<Mutex<Option<String>>>) {
    let set = |msg: Option<String>| {
        if let Ok(mut b) = banner.lock() {
            *b = msg;
        }
    };

    // Give the UI a moment to load before we start the network call.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let api_out = tokio::process::Command::new("curl")
        .args([
            "-sf",
            "--max-time",
            "10",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: org3d-updater",
            "https://api.github.com/repos/sbeckeriv/org3d/releases/latest",
        ])
        .output()
        .await;

    let api_out = match api_out {
        Ok(o) if o.status.success() => o,
        _ => return,
    };

    let json: serde_json::Value = match serde_json::from_slice(&api_out.stdout) {
        Ok(v) => v,
        Err(_) => return,
    };

    let tag = json["tag_name"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches('v');
    let current = env!("CARGO_PKG_VERSION");

    if !is_newer(tag, current) {
        return;
    }

    let version = tag.to_string();

    let dmg_url = json["assets"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(".dmg")))
                .and_then(|a| a["browser_download_url"].as_str())
        })
        .map(String::from);

    let Some(dmg_url) = dmg_url else { return };

    set(Some(format!("Downloading update v{version}…")));

    let tmp_dmg = std::env::temp_dir().join("org3d_update.dmg");
    let dl_ok = tokio::process::Command::new("curl")
        .args(["-sfL", "--max-time", "300", "-o"])
        .arg(&tmp_dmg)
        .arg(&dmg_url)
        .status()
        .await
        .is_ok_and(|s| s.success());

    if !dl_ok {
        set(None);
        return;
    }

    set(Some(format!("Installing update v{version}…")));

    let mount_out = tokio::process::Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-quiet"])
        .arg(&tmp_dmg)
        .output()
        .await;

    let mount_out = match mount_out {
        Ok(o) if o.status.success() => o,
        _ => {
            set(None);
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&mount_out.stdout);
    let mount_point = stdout
        .lines()
        .filter_map(|l| l.split('\t').next_back())
        .map(str::trim)
        .find(|s| s.starts_with("/Volumes/"))
        .map(String::from);

    let Some(mount_point) = mount_point else {
        set(None);
        return;
    };

    let mounted_app = std::path::Path::new(&mount_point)
        .read_dir()
        .ok()
        .and_then(|mut d| {
            d.find_map(|e| {
                let e = e.ok()?;
                let p = e.path();
                (p.extension()? == "app").then_some(p)
            })
        });

    let detach = || async {
        let _ = tokio::process::Command::new("hdiutil")
            .args(["detach", "-quiet", &mount_point])
            .status()
            .await;
    };

    let Some(mounted_app) = mounted_app else {
        detach().await;
        set(None);
        return;
    };

    let current_app = std::env::current_exe().ok().and_then(|exe| {
        exe.ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(std::path::Path::to_path_buf)
    });

    let Some(current_app) = current_app else {
        detach().await;
        set(None);
        return;
    };

    // ditto preserves .app bundle structure and extended attributes.
    let copy_ok = tokio::process::Command::new("ditto")
        .arg(&mounted_app)
        .arg(&current_app)
        .status()
        .await
        .is_ok_and(|s| s.success());

    detach().await;

    if !copy_ok {
        // Can't write to the .app location (likely /Applications without sudo).
        // Fall back to a clickable download link.
        set(Some(format!(
            r#"Update v{version} available — <a href="{dmg_url}" style="color:inherit">Download</a>"#
        )));
        return;
    }

    // Remove quarantine from the freshly-copied .app.
    let _ = tokio::process::Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&current_app)
        .status()
        .await;

    set(Some(format!("Update v{version} installed — restarting…")));
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let _ = tokio::process::Command::new("open")
        .arg(&current_app)
        .spawn();

    std::process::exit(0);
}

// ─────────────────────────────────────────────────────────────────────────────

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
                        let update_banner = server_app.update_banner();
                        port_tx.send(server_app.port()).ok();
                        #[cfg(target_os = "macos")]
                        tokio::spawn(check_and_update(update_banner));
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
