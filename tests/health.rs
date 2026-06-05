mod common;

#[tokio::test]
async fn health_check_returns_200() {
    let app = common::spawn_app(|_| {}).await;
    let resp = app.get("/health_check").await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn health_check_has_empty_body() {
    let app = common::spawn_app(|_| {}).await;
    let resp = app.get("/health_check").await;
    let body = resp.text().await.unwrap();
    assert!(body.is_empty());
}
