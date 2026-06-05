mod common;
use common::spawn_seeded;

// ── Gallery page ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn gallery_returns_200() {
    let app = spawn_seeded().await;
    let resp = app.get("/").await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn gallery_is_html() {
    let app = spawn_seeded().await;
    let resp = app.get("/").await;
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.contains("text/html"), "expected HTML content-type, got {ct}");
}

#[tokio::test]
async fn gallery_shows_both_models() {
    let app = spawn_seeded().await;
    let body = app.get("/").await.text().await.unwrap();
    assert!(body.contains("Dragon"), "gallery should list Dragon");
    assert!(body.contains("Bunny"),  "gallery should list Bunny");
}

#[tokio::test]
async fn gallery_shows_total_count() {
    let app = spawn_seeded().await;
    let body = app.get("/").await.text().await.unwrap();
    assert!(body.contains("2 models"), "should show total indexed count");
}

// ── Search / filter ──────────────────────────────────────────────────────────

#[tokio::test]
async fn search_returns_partial_html_not_full_page() {
    let app = spawn_seeded().await;
    let body = app.get("/search").await.text().await.unwrap();
    assert!(!body.contains("<!DOCTYPE"), "search should return a fragment");
    assert!(body.contains("class=\"card\""), "fragment should contain cards");
}

#[tokio::test]
async fn search_by_title_filters_correctly() {
    let app = spawn_seeded().await;
    let body = app.get("/search?q=dragon").await.text().await.unwrap();
    assert!(body.contains("Dragon"), "dragon search should return Dragon");
    assert!(!body.contains("Bunny"),  "dragon search should not return Bunny");
}

#[tokio::test]
async fn search_by_format_stl() {
    let app = spawn_seeded().await;
    let body = app.get("/search?format=STL").await.text().await.unwrap();
    assert!(body.contains("Dragon"), "STL filter should return Dragon");
    assert!(!body.contains("Bunny"),  "STL filter should not return Bunny");
}

#[tokio::test]
async fn search_by_format_3mf() {
    let app = spawn_seeded().await;
    let body = app.get("/search?format=3MF").await.text().await.unwrap();
    assert!(body.contains("Bunny"),   "3MF filter should return Bunny");
    assert!(!body.contains("Dragon"), "3MF filter should not return Dragon");
}

#[tokio::test]
async fn search_empty_query_returns_all() {
    let app = spawn_seeded().await;
    let body = app.get("/search?q=").await.text().await.unwrap();
    assert!(body.contains("Dragon"));
    assert!(body.contains("Bunny"));
}

// ── Model detail ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn model_detail_returns_200_and_content() {
    let app = spawn_seeded().await;
    let body = app.get("/model/1").await.text().await.unwrap();
    assert!(body.contains("Dragon"),  "detail should show model title");
    assert!(body.contains("Alice"),   "detail should show designer");
}

#[tokio::test]
async fn model_detail_not_found_returns_404() {
    let app = spawn_seeded().await;
    let resp = app.get("/model/99999").await;
    assert_eq!(resp.status(), 404);
}

// ── File serving ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn file_unknown_id_returns_404() {
    let app = spawn_seeded().await;
    let resp = app.get("/file/99999").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn file_missing_on_disk_returns_404() {
    // DB has the model but /tmp/dragon.stl doesn't actually exist
    let app = spawn_seeded().await;
    let resp = app.get("/file/1").await;
    assert_eq!(resp.status(), 404);
}
