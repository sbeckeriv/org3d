//! Duplicate finder: identical files are grouped, and copies can be moved to
//! the library's trash folder without losing curation.

mod common;
use common::{TestApp, spawn_app};
use org3d::db;
use std::path::Path;

/// Binary STL with `tris` empty triangles. `tag` changes the bytes but not
/// the size.
fn write_stl(path: &Path, tag: u8, tris: u32) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut buf = vec![tag; 80];
    buf.extend_from_slice(&tris.to_le_bytes());
    buf.extend(std::iter::repeat_n(0u8, 50 * tris as usize));
    std::fs::write(path, buf).unwrap();
}

async fn post(app: &TestApp, path: &str, body: &str) -> reqwest::Response {
    app.client
        .post(app.url(path))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

async fn wait_for_report(app: &TestApp) -> String {
    for _ in 0..200 {
        let body = app.get("/api/scan/report").await.text().await.unwrap();
        if !body.contains("every 2s") {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("job did not finish");
}

async fn find_duplicates(app: &TestApp) -> String {
    let r = post(app, "/api/duplicates/scan", "").await;
    assert_eq!(r.headers()["hx-trigger"], "scan-started");
    wait_for_report(app).await
}

fn id_of(app: &TestApp, rel: &str) -> i64 {
    let conn = db::open(app.db_path.to_str().unwrap()).unwrap();
    let path = app.library.join(rel);
    conn.query_row(
        "SELECT id FROM models WHERE path = ?1",
        [path.to_str().unwrap()],
        |r| r.get(0),
    )
    .unwrap()
}

fn library_with_copies(app: &TestApp) {
    write_stl(&app.library.join("a/cube.stl"), 1, 1);
    write_stl(&app.library.join("b/deeper/cube.stl"), 1, 1);
    write_stl(&app.library.join("c/other.stl"), 2, 1); // same size, other bytes
    write_stl(&app.library.join("d/big.stl"), 1, 3); // unique size
}

#[tokio::test]
async fn finds_identical_files_only() {
    let app = spawn_app(|_| {}).await;
    library_with_copies(&app);

    let report = find_duplicates(&app).await;
    assert!(report.contains("Find duplicates"), "{report}");
    assert!(
        report.contains("Compared 3 same-size files: read 3"),
        "{report}"
    );
    assert!(report.contains("1 extra copy in 1 group"), "{report}");

    let page = app.get("/duplicates").await.text().await.unwrap();
    assert!(page.contains("cube.stl"), "{page}");
    assert!(!page.contains("other.stl"), "{page}");
    assert!(!page.contains("big.stl"), "{page}");
    // Shortest path is suggested as the keeper; the other copy is pre-checked.
    let keep = id_of(&app, "a/cube.stl");
    let extra = id_of(&app, "b/deeper/cube.stl");
    assert!(
        page.contains(&format!(r#"value="{keep}">"#)),
        "keeper unchecked"
    );
    assert!(
        page.contains(&format!(r#"value="{extra}" checked>"#)),
        "{page}"
    );

    // Second run reads nothing: hashes are remembered.
    let report = find_duplicates(&app).await;
    assert!(report.contains("read 0, 3 already known"), "{report}");
}

#[tokio::test]
async fn trashing_moves_file_and_keeps_project() {
    let app = spawn_app(|_| {}).await;
    library_with_copies(&app);
    find_duplicates(&app).await;

    // Put the copy we're about to trash in a project.
    let keep = id_of(&app, "a/cube.stl");
    let extra = id_of(&app, "b/deeper/cube.stl");
    {
        let conn = db::open(app.db_path.to_str().unwrap()).unwrap();
        let p = db::find_or_create_project(&conn, "Cubes").unwrap();
        db::assign_project(&conn, extra, p).unwrap();
    }

    let body = post(&app, "/api/duplicates/trash", &format!("id={extra}"))
        .await
        .text()
        .await
        .unwrap();
    assert!(body.contains("Moved 1 file"), "{body}");
    assert!(body.contains("No duplicates found"), "{body}");

    assert!(app.library.join("a/cube.stl").exists());
    assert!(!app.library.join("b/deeper/cube.stl").exists());
    assert!(app.library.join(".org3d-trash/b/deeper/cube.stl").exists());

    let conn = db::open(app.db_path.to_str().unwrap()).unwrap();
    let kept = db::get_by_id(&conn, keep).unwrap().unwrap();
    assert!(kept.project_id.is_some(), "project carried over");
    assert!(db::get_by_id(&conn, extra).unwrap().is_none());
    drop(conn);

    // The trash folder is never indexed.
    post(&app, "/api/rescan", "").await;
    let report = wait_for_report(&app).await;
    assert!(report.contains("<th>Found</th><td>3</td>"), "{report}");
}

#[tokio::test]
async fn refuses_to_trash_every_copy() {
    let app = spawn_app(|_| {}).await;
    library_with_copies(&app);
    find_duplicates(&app).await;
    let a = id_of(&app, "a/cube.stl");
    let b = id_of(&app, "b/deeper/cube.stl");

    let body = post(&app, "/api/duplicates/trash", &format!("id={a}&id={b}"))
        .await
        .text()
        .await
        .unwrap();
    assert!(body.contains("Moved 0 files"), "{body}");
    assert!(body.contains("keep at least one"), "{body}");
    assert!(app.library.join("a/cube.stl").exists());
    assert!(app.library.join("b/deeper/cube.stl").exists());
}

#[tokio::test]
async fn refuses_when_file_changed_since_compared() {
    let app = spawn_app(|_| {}).await;
    library_with_copies(&app);
    find_duplicates(&app).await;
    let b = id_of(&app, "b/deeper/cube.stl");

    // Same size, different bytes and mtime, and no rescan yet.
    let path = app.library.join("b/deeper/cube.stl");
    write_stl(&path, 9, 1);
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
        .unwrap();
    drop(f);

    let body = post(&app, "/api/duplicates/trash", &format!("id={b}"))
        .await
        .text()
        .await
        .unwrap();
    assert!(body.contains("changed on disk"), "{body}");
    assert!(path.exists());

    // A rescan clears the stale hash, so the pair is no longer listed.
    find_duplicates(&app).await;
    let groups = app.get("/api/duplicates").await.text().await.unwrap();
    assert!(groups.contains("No duplicates found"), "{groups}");
}

#[tokio::test]
async fn page_offers_comparison_when_unchecked_files_exist() {
    let app = spawn_app(|_| {}).await;
    library_with_copies(&app);
    post(&app, "/api/rescan", "").await;
    wait_for_report(&app).await;

    let groups = app.get("/api/duplicates").await.text().await.unwrap();
    assert!(
        groups.contains("3 files of matching size haven&#x27;t been compared"),
        "{groups}"
    );
}

#[tokio::test]
async fn polling_report_signals_when_job_ends() {
    let app = spawn_app(|_| {}).await;
    let r = app.get("/api/scan/report?poll=1").await;
    assert_eq!(r.headers()["hx-trigger"], "scan-finished");
    let r = app.get("/api/scan/report").await;
    assert!(r.headers().get("hx-trigger").is_none());
}
