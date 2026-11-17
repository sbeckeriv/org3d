mod common;
use common::{spawn_app, spawn_seeded};

fn stl_bytes() -> Vec<u8> {
    let mut buf = vec![0u8; 80];
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 50]);
    buf
}

/// Spawn with one model whose file really exists on disk.
async fn spawn_with_file() -> (common::TestApp, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "org3d_srv_{}_{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cube ü.stl");
    std::fs::write(&path, stl_bytes()).unwrap();
    let p = path.clone();
    let app = spawn_app(move |conn| {
        org3d::db::upsert(
            conn,
            &org3d::db::ModelRow {
                id: 0,
                path: p.to_string_lossy().into_owned(),
                filename: "cube ü.stl".into(),
                folder: "/".into(),
                format: "STL".into(),
                file_size: 134,
                title: Some("Cube".into()),
                designer: None,
                description: None,
                application: None,
                license: None,
                created_at: None,
                object_count: Some(1),
                triangle_count: Some(1),
                dim_x: None,
                dim_y: None,
                dim_z: None,
                thumbnail_path: None,
                project_id: None,
                mtime: Some(42),
            },
        )
        .unwrap();
    })
    .await;
    (app, path)
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

#[tokio::test]
async fn preview_is_cached_and_revalidates_without_the_library() {
    let (app, path) = spawn_with_file().await;
    let r = app.get("/stl/1").await;
    assert_eq!(r.status(), 200);
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(
        r.headers()["content-length"],
        "134",
        "streamed with a length"
    );
    assert_eq!(r.bytes().await.unwrap().len(), 134);

    // Library disk "asleep"/gone: both a cache hit and a 304 must still work.
    std::fs::remove_file(&path).unwrap();
    let r = app.get("/stl/1").await;
    assert_eq!(r.status(), 200, "served from preview cache");
    let r = app
        .client
        .get(app.url("/stl/1"))
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
}

#[tokio::test]
async fn download_sets_attachment_filename() {
    let (app, _path) = spawn_with_file().await;
    let r = app.get("/file/1?download=1").await;
    assert_eq!(r.status(), 200);
    let cd = r.headers()["content-disposition"].to_str().unwrap();
    assert!(cd.starts_with("attachment;"), "{cd}");
    assert!(cd.contains("filename*=UTF-8''cube%20%C3%BC.stl"), "{cd}");
}

#[tokio::test]
async fn cross_site_post_is_rejected() {
    let app = spawn_seeded().await;
    let r = app
        .client
        .post(app.url("/api/rescan"))
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    let r = app
        .client
        .post(app.url("/api/autogroup"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    let r = app
        .client
        .post(app.url("/api/autogroup"))
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn thumbnail_upload_requires_png() {
    let app = spawn_seeded().await;
    let r = app
        .client
        .post(app.url("/api/model/1/thumbnail"))
        .body("<script>alert(1)</script>")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0u8; 16]);
    let r = app
        .client
        .post(app.url("/api/model/999/thumbnail"))
        .body(png)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "unknown model");
}

#[tokio::test]
async fn settings_are_read_only_in_server_mode() {
    let app = spawn_seeded().await;
    let body = app.get("/settings").await.text().await.unwrap();
    assert!(body.contains("ORG3D_"), "should explain env configuration");
    assert!(!body.contains(r#"action="/settings""#), "no editable form");
    let r = app
        .client
        .post(app.url("/settings"))
        .form(&[
            ("base_folder", "/"),
            ("port", "1"),
            ("db_path", ""),
            ("thumb_dir", ""),
            ("preferred_slicer", "none"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn slicer_launch_disabled_in_server_mode() {
    let app = spawn_seeded().await;
    let r = app
        .client
        .post(app.url("/api/model/1/open/bambu"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let body = app.get("/model/1").await.text().await.unwrap();
    assert!(body.contains("Download"));
    assert!(!body.contains("/open/bambu"));
}

#[tokio::test]
async fn fts_syntax_characters_do_not_break_search() {
    let app = spawn_seeded().await;
    for q in ["%22", "a%3Ab", "(x", "NEAR(", "-", "dra*"] {
        let r = app.get(&format!("/search?q={q}")).await;
        assert_eq!(r.status(), 200, "q={q}");
    }
    let body = app.get("/search?q=drag").await.text().await.unwrap();
    assert!(body.contains("Dragon"));
}

#[tokio::test]
async fn vendored_js_is_served_locally() {
    let app = spawn_seeded().await;
    for name in ["htmx-2.0.3.min.js", "three-r128.min.js"] {
        let r = app.get(&format!("/static/{name}")).await;
        assert_eq!(r.status(), 200, "{name}");
        assert!(
            r.headers()["cache-control"]
                .to_str()
                .unwrap()
                .contains("immutable")
        );
    }
    let body = app.get("/").await.text().await.unwrap();
    assert!(!body.contains("unpkg.com") && !body.contains("cdnjs"));
}

#[tokio::test]
async fn huge_page_number_does_not_panic() {
    let app = spawn_seeded().await;
    let r = app.get(&format!("/?page={}", i64::MAX)).await;
    assert_eq!(r.status(), 200);
}
