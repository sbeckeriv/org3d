//! Scan results must be visible in the UI, not only in the container log.

mod common;
use common::{TestApp, spawn_app};

fn write_stl(path: &std::path::Path) {
    let mut buf = vec![0u8; 80];
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 50]);
    std::fs::write(path, buf).unwrap();
}

async fn post(app: &TestApp, path: &str) -> reqwest::Response {
    app.client.post(app.url(path)).send().await.unwrap()
}

/// Poll the Last scan panel until the scan finishes; return its HTML.
async fn wait_for_report(app: &TestApp) -> String {
    for _ in 0..100 {
        let body = app.get("/api/scan/report").await.text().await.unwrap();
        if !body.contains("every 2s") {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("scan did not finish");
}

#[tokio::test]
async fn panel_says_when_nothing_has_run() {
    let app = spawn_app(|_| {}).await;
    let body = app.get("/api/scan/report").await.text().await.unwrap();
    assert!(body.contains("No scan has run"), "{body}");
    assert!(
        body.contains("scan-started from:body"),
        "waits for a scan to start"
    );
}

#[tokio::test]
async fn clean_rescan_reports_counts_and_no_problems() {
    let app = spawn_app(|_| {}).await;
    write_stl(&app.library.join("cube.stl"));

    let r = post(&app, "/api/rescan").await;
    assert_eq!(r.headers()["hx-trigger"], "scan-started");
    let body = wait_for_report(&app).await;
    assert!(body.contains("Manual rescan"), "{body}");
    assert!(body.contains("sr-ok"), "green status: {body}");
    assert!(!body.contains("couldn"), "{body}");

    let status = app.get("/api/scan/status").await.text().await.unwrap();
    assert!(status.contains("1 models"), "{status}");
    assert!(!status.contains("problems"), "{status}");
}

#[tokio::test]
async fn unreadable_files_are_listed_and_flagged_in_gallery() {
    let app = spawn_app(|_| {}).await;
    write_stl(&app.library.join("good.stl"));
    std::fs::write(app.library.join("broken.3mf"), b"not a zip").unwrap();

    post(&app, "/api/rescan").await;
    let body = wait_for_report(&app).await;
    assert!(body.contains("sr-warn"), "{body}");
    assert!(body.contains("1 file couldn"), "{body}");
    assert!(body.contains("broken.3mf"), "{body}");

    let status = app.get("/api/scan/status").await.text().await.unwrap();
    assert!(status.contains("last scan had problems"), "{status}");
    assert!(status.contains("/settings#last-scan"), "{status}");
}

#[tokio::test]
async fn missing_library_folder_is_shown_as_an_error() {
    let app = spawn_app(|_| {}).await;
    std::fs::remove_dir_all(&app.library).unwrap();

    post(&app, "/api/rescan").await;
    let body = wait_for_report(&app).await;
    assert!(body.contains("sr-bad"), "{body}");
    assert!(body.contains("not accessible"), "{body}");
}

#[tokio::test]
async fn failed_zip_extraction_is_reported() {
    let app = spawn_app(|_| {}).await;
    std::fs::write(app.library.join("pack.zip"), b"definitely not a zip").unwrap();

    let r = post(&app, "/api/extract").await;
    assert_eq!(r.headers()["hx-trigger"], "scan-started");
    let body = wait_for_report(&app).await;
    assert!(body.contains("Extract ZIPs"), "{body}");
    assert!(body.contains("1 ZIP couldn"), "{body}");
    assert!(body.contains("pack.zip"), "{body}");
    assert!(body.contains("No new ZIPs"), "{body}");
}

#[tokio::test]
async fn settings_page_hosts_the_panel() {
    let app = spawn_app(|_| {}).await;
    let body = app.get("/settings").await.text().await.unwrap();
    assert!(body.contains(r#"id="last-scan""#));
    assert!(body.contains(r#"hx-get="/api/scan/report""#));
}
