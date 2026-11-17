mod common;
use common::{TestApp, spawn_app, spawn_app_with};
use org3d::auth::Credentials;

async fn spawn_locked() -> TestApp {
    spawn_app_with(
        |cfg| cfg.auth = Some(Credentials::parse("printer:hunter22").unwrap()),
        |_| {},
    )
    .await
}

#[tokio::test]
async fn everything_requires_login_except_health_and_static() {
    let app = spawn_locked().await;
    for path in [
        "/",
        "/settings",
        "/search?q=x",
        "/stl/1",
        "/file/1",
        "/thumbs/x.png",
        "/api/scan/report",
    ] {
        let r = app.get(path).await;
        assert_eq!(r.status(), 401, "{path}");
        assert!(
            r.headers()["www-authenticate"]
                .to_str()
                .unwrap()
                .starts_with("Basic"),
            "{path}"
        );
    }
    let r = app
        .client
        .post(app.url("/api/rescan"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "writes too");

    assert_eq!(app.get("/health_check").await.status(), 200);
    assert_eq!(app.get("/static/htmx-2.0.3.min.js").await.status(), 200);
}

#[tokio::test]
async fn correct_credentials_get_in() {
    let app = spawn_locked().await;
    let r = app
        .client
        .get(app.url("/settings"))
        .basic_auth("printer", Some("hunter22"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(
        body.contains("required (user printer)"),
        "settings shows login state"
    );
}

#[tokio::test]
async fn wrong_credentials_are_slowed_down() {
    let app = spawn_locked().await;
    let t = std::time::Instant::now();
    let r = app
        .client
        .get(app.url("/"))
        .basic_auth("printer", Some("hunter23"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(t.elapsed() >= std::time::Duration::from_millis(900));
}

#[tokio::test]
async fn settings_warn_when_login_is_off() {
    let app = spawn_app(|_| {}).await;
    let body = app.get("/settings").await.text().await.unwrap();
    assert!(body.contains("anyone on the network"), "{body}");
}
