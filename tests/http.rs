//! End-to-end tests over the real router. Requests go through every layer —
//! auth middleware, routing, handlers — without binding a socket.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use toolsite::{build_router, upload::UploadTicket, Config};
use tower::ServiceExt;

const TOKEN: &str = "test-token";

fn server() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), TOKEN, true));
    (dir, config)
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String, Vec<(String, String)>) {
    let response = build_router(config.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string(), headers)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn write_page(config: &Config, slug: &str, html: &str) {
    let path = config.data_dir.join(format!("{slug}.html"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, html).unwrap();
}

fn ticket(config: &Config, slug: &str, ttl: Duration) -> String {
    let token = format!("ticket{}", config.uploads.lock().unwrap().len());
    config.uploads.lock().unwrap().insert(
        token.clone(),
        UploadTicket {
            slug: slug.to_string(),
            expires_at: Instant::now() + ttl,
        },
    );
    token
}

#[tokio::test]
async fn mcp_requires_a_token() {
    let (_dir, config) = server();
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;

    let unauthenticated = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(initialize))
        .unwrap();
    let (status, ..) = send(&config, unauthenticated).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let wrong_token = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("authorization", "Bearer nope")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(initialize))
        .unwrap();
    let (status, ..) = send(&config, wrong_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn both_bearer_and_x_api_key_are_accepted() {
    let (_dir, config) = server();
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;

    for (name, value) in [
        ("authorization", format!("Bearer {TOKEN}")),
        ("authorization", format!("bearer {TOKEN}")),
        ("x-api-key", TOKEN.to_string()),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
        .header("host", "localhost")
            .header(name, value.clone())
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(initialize))
            .unwrap();
        let (status, body, _) = send(&config, request).await;
        assert_eq!(status, StatusCode::OK, "{name}: {value} was rejected: {body}");
    }
}

#[tokio::test]
async fn pages_are_public_but_traversal_is_not_reachable() {
    let (_dir, config) = server();
    write_page(&config, "hello", "<h1>hi</h1>");

    let (status, body, _) = send(&config, get("/p/hello")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1>hi</h1>"));

    for attempt in [
        "/p/../../etc/passwd",
        "/p/..%2f..%2fetc%2fpasswd",
        "/p/hello/../../../etc/passwd",
    ] {
        let (status, ..) = send(&config, get(attempt)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{attempt} was not rejected");
    }
}

#[tokio::test]
async fn an_app_root_redirects_so_relative_links_resolve() {
    let (_dir, config) = server();
    write_page(&config, "app/index", "<h1>app</h1>");

    let (status, _, headers) = send(&config, get("/p/app")).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    let location = headers.iter().find(|(k, _)| k == "location").unwrap();
    assert_eq!(location.1, "/p/app/");

    let (status, body, _) = send(&config, get("/p/app/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1>app</h1>"));
}

#[tokio::test]
async fn hiding_a_page_takes_down_its_url_without_deleting_it() {
    let (_dir, config) = server();
    write_page(&config, "secret", "<h1>classified</h1>");
    std::fs::write(
        config.data_dir.join("secret.meta"),
        r#"{"listed":false,"hidden":true,"spa":false}"#,
    )
    .unwrap();

    let (status, ..) = send(&config, get("/p/secret")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The file is untouched, which is what makes it reversible.
    assert!(config.data_dir.join("secret.html").exists());

    let (_, index, _) = send(&config, get("/")).await;
    assert!(!index.contains("secret"), "hidden page appeared on the index");
}

#[tokio::test]
async fn unlisted_pages_still_serve() {
    let (_dir, config) = server();
    write_page(&config, "quiet", "<h1>quiet</h1>");
    std::fs::write(
        config.data_dir.join("quiet.meta"),
        r#"{"listed":false,"hidden":false,"spa":false}"#,
    )
    .unwrap();

    let (status, ..) = send(&config, get("/p/quiet")).await;
    assert_eq!(status, StatusCode::OK);

    let (_, index, _) = send(&config, get("/")).await;
    assert!(!index.contains("quiet"), "unlisted page appeared on the index");
}

#[tokio::test]
async fn uploads_need_a_live_ticket() {
    let (_dir, config) = server();
    let good = ticket(&config, "uploaded", Duration::from_secs(60));
    let expired = ticket(&config, "stale", Duration::from_millis(0));

    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/{good}"))
        .body(Body::from("<h1>via ticket</h1>"))
        .unwrap();
    let (status, body, _) = send(&config, request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/p/uploaded"));

    let (status, body, _) = send(&config, get("/p/uploaded")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("via ticket"));

    for bad in [expired.as_str(), "never-existed"] {
        let request = Request::builder()
            .method("PUT")
            .uri(format!("/upload/{bad}"))
            .body(Body::from("<h1>nope</h1>"))
            .unwrap();
        let (status, ..) = send(&config, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "ticket {bad} was accepted");
    }
}

#[tokio::test]
async fn a_ticket_cannot_write_outside_its_own_slug() {
    let (_dir, config) = server();
    let token = ticket(&config, "mine", Duration::from_secs(60));

    for attempt in ["../yours", "..%2fyours", "../../etc/passwd"] {
        let request = Request::builder()
            .method("PUT")
            .uri(format!("/upload/{token}/{attempt}"))
            .body(Body::from("<h1>pwned</h1>"))
            .unwrap();
        let (status, ..) = send(&config, request).await;
        assert_ne!(status, StatusCode::OK, "{attempt} was accepted");
    }
    assert!(!config.data_dir.join("yours.html").exists());
}

#[tokio::test]
async fn bundle_assets_are_served_with_a_real_content_type() {
    let (_dir, config) = server();
    let app = config.data_dir.join("bundle/assets");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(config.data_dir.join("bundle/index.html"), "<h1>app</h1>").unwrap();
    std::fs::write(app.join("main-4f2a.js"), "console.log(1)").unwrap();
    std::fs::write(app.join("main-4f2a.css"), "body{}").unwrap();

    for (path, expected) in [
        ("/p/bundle/assets/main-4f2a.js", "text/javascript"),
        ("/p/bundle/assets/main-4f2a.css", "text/css"),
    ] {
        let (status, _, headers) = send(&config, get(path)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let content_type = headers.iter().find(|(k, _)| k == "content-type").unwrap();
        assert!(
            content_type.1.starts_with(expected),
            "{path} served as {}",
            content_type.1
        );
    }
}

#[tokio::test]
async fn client_routes_only_fall_back_to_index_when_the_app_asked_for_it() {
    let (_dir, config) = server();
    std::fs::create_dir_all(config.data_dir.join("spa")).unwrap();
    std::fs::write(config.data_dir.join("spa/index.html"), "<h1>spa</h1>").unwrap();
    std::fs::create_dir_all(config.data_dir.join("static")).unwrap();
    std::fs::write(config.data_dir.join("static/index.html"), "<h1>static</h1>").unwrap();
    std::fs::write(
        config.data_dir.join("spa/index.meta"),
        r#"{"listed":true,"hidden":false,"spa":true}"#,
    )
    .unwrap();

    let (status, body, _) = send(&config, get("/p/spa/deep/route")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1>spa</h1>"));

    let (status, ..) = send(&config, get("/p/static/deep/route")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_index_lists_an_app_once_at_its_root() {
    let (_dir, config) = server();
    write_page(&config, "app/index", "<!doctype html><title>My App</title>");
    write_page(&config, "app/about", "<!doctype html><title>About</title>");
    write_page(&config, "loose", "<!doctype html><title>Loose Page</title>");

    let (status, body, _) = send(&config, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.matches("/p/app\"").count(), 1);
    assert!(!body.contains("/p/app/about"), "inner page was listed");
    assert!(body.contains("My App"), "title was not picked up");
    assert!(body.contains("Loose Page"));
}
