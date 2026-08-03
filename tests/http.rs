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
use toolsite::{build_router, platform::upload::UploadTicket, runtime::wasm::Runtime, Config};
use tower::ServiceExt;

const TOKEN: &str = "test-token";

fn server() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), TOKEN, true));
    (dir, config)
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String, Vec<(String, String)>) {
    let response = build_router(config.clone(), Runtime::new().unwrap())
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

fn publish_handler(config: &Config, app: &str) {
    let dir = config.data_dir.join(app);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("handler.wasm"), HANDLER).unwrap();
}

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

#[tokio::test]
async fn api_requests_reach_the_apps_handler() {
    let (_dir, config) = server();
    publish_handler(&config, "app");

    let (status, body, _) = send(&config, get("/p/app/api/echo")).await;
    assert_eq!(status, StatusCode::OK);
    // The guest sees a path relative to its own app, not the mount point.
    assert_eq!(body, "GET /api/echo?");
}

#[tokio::test]
async fn a_handler_can_use_its_apps_database_over_http() {
    let (_dir, config) = server();
    publish_handler(&config, "counter");

    let (_, first, _) = send(&config, get("/p/counter/api/count")).await;
    let (_, second, _) = send(&config, get("/p/counter/api/count")).await;
    assert_eq!((first.as_str(), second.as_str()), ("1", "2"));
}

#[tokio::test]
async fn api_requests_carry_method_and_body_through() {
    let (_dir, config) = server();
    publish_handler(&config, "app");

    let request = Request::builder()
        .method("POST")
        .uri("/p/app/api/echo-param")
        .body(Body::from("hello from the browser"))
        .unwrap();
    let (status, body, _) = send(&config, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "hello from the browser");
}

#[tokio::test]
async fn static_files_win_over_the_handler_but_api_never_does() {
    let (_dir, config) = server();
    publish_handler(&config, "app");
    std::fs::write(config.data_dir.join("app/index.html"), "<h1>static</h1>").unwrap();
    // A file that would otherwise shadow the reserved prefix.
    std::fs::create_dir_all(config.data_dir.join("app/api")).unwrap();
    std::fs::write(config.data_dir.join("app/api/echo"), "STATIC SHADOW").unwrap();

    let (_, body, _) = send(&config, get("/p/app/")).await;
    assert!(body.contains("static"), "handler answered for a real file");

    let (_, body, _) = send(&config, get("/p/app/api/echo")).await;
    assert_eq!(body, "GET /api/echo?", "a file shadowed the handler");
}

#[tokio::test]
async fn an_app_without_a_handler_says_so_rather_than_erroring() {
    let (_dir, config) = server();
    std::fs::create_dir_all(config.data_dir.join("static")).unwrap();
    std::fs::write(config.data_dir.join("static/index.html"), "<h1>hi</h1>").unwrap();

    let (status, ..) = send(&config, get("/p/static/api/anything")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_handler_answers_for_routes_with_no_file_behind_them() {
    let (_dir, config) = server();
    publish_handler(&config, "app");
    std::fs::write(config.data_dir.join("app/index.html"), "<h1>static</h1>").unwrap();

    // Not a file, not /api — the handler gets a chance before the 404.
    let (status, body, _) = send(&config, get("/p/app/echo")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "GET /echo?");
}

#[tokio::test]
async fn a_trapping_handler_returns_500_and_leaves_the_server_up() {
    let (_dir, config) = server();
    publish_handler(&config, "app");

    let (status, body, _) = send(&config, get("/p/app/api/spin")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!body.contains("wasm"), "internals leaked to the visitor: {body}");

    let (status, ..) = send(&config, get("/p/app/api/echo")).await;
    assert_eq!(status, StatusCode::OK, "server did not survive the trap");
}

#[tokio::test]
async fn hiding_an_app_takes_its_handler_down_too() {
    let (_dir, config) = server();
    publish_handler(&config, "app");
    std::fs::write(config.data_dir.join("app/index.html"), "<h1>hi</h1>").unwrap();
    std::fs::write(
        config.data_dir.join("app/index.meta"),
        r#"{"listed":false,"hidden":true,"spa":false}"#,
    )
    .unwrap();

    let (status, ..) = send(&config, get("/p/app/api/echo")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_handler_is_validated_when_it_is_uploaded() {
    let (_dir, config) = server();
    let token = ticket(&config, "app", Duration::from_secs(60));

    let bad = Request::builder()
        .method("PUT")
        .uri(format!("/upload/{token}?handler"))
        .body(Body::from("this is not a wasm component"))
        .unwrap();
    let (status, body, _) = send(&config, bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("not a valid handler"), "{body}");
    assert!(!config.data_dir.join("app/handler.wasm").exists());

    let good = Request::builder()
        .method("PUT")
        .uri(format!("/upload/{token}?handler"))
        .body(Body::from(HANDLER))
        .unwrap();
    let (status, body, _) = send(&config, good).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(config.data_dir.join("app/handler.wasm").exists());

    let (status, body, _) = send(&config, get("/p/app/api/echo")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "GET /api/echo?");
}

// --- accounts and gates -------------------------------------------------

fn account(config: &Config, email: &str, password: &str) {
    toolsite::accounts::users::sign_up(config, email, password).unwrap();
}

fn sign_in(config: &Arc<Config>, email: &str, password: &str) -> String {
    let (_, token) = toolsite::accounts::users::log_in(config, email, password).unwrap();
    token
}

fn get_as(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", format!("ts_session={token}"))
        .body(Body::empty())
        .unwrap()
}

fn gate(config: &Config, app: &str, gate: &str) {
    std::fs::create_dir_all(config.data_dir.join(app)).unwrap();
    std::fs::write(
        config.data_dir.join(app).join("index.meta"),
        format!(r#"{{"listed":true,"hidden":false,"spa":false,"gate":"{gate}"}}"#),
    )
    .unwrap();
}

#[tokio::test]
async fn a_public_app_needs_no_account() {
    let (_dir, config) = server();
    write_page(&config, "open/index", "<h1>open</h1>");

    let (status, ..) = send(&config, get("/p/open/")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_authenticated_gate_sends_a_visitor_to_sign_in() {
    let (_dir, config) = server();
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");

    let (status, _, headers) = send(&config, get("/p/members/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers.iter().find(|(k, _)| k == "location").unwrap();
    assert!(location.1.starts_with("/auth/login?next="), "{}", location.1);

    account(&config, "someone@example.com", "correct horse battery");
    let token = sign_in(&config, "someone@example.com", "correct horse battery");
    let (status, body, _) = send(&config, get_as("/p/members/", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("members"));
}

#[tokio::test]
async fn a_granted_gate_needs_that_specific_grant() {
    let (_dir, config) = server();
    write_page(&config, "private/index", "<h1>private</h1>");
    gate(&config, "private", "granted");
    account(&config, "allowed@example.com", "correct horse battery");
    account(&config, "outsider@example.com", "correct horse battery");

    let outsider = sign_in(&config, "outsider@example.com", "correct horse battery");
    let (status, ..) = send(&config, get_as("/p/private/", &outsider)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a signed-in stranger got in");

    toolsite::accounts::users::grant(&config, "allowed@example.com", "private", "viewer").unwrap();
    let allowed = sign_in(&config, "allowed@example.com", "correct horse battery");
    let (status, ..) = send(&config, get_as("/p/private/", &allowed)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_gate_covers_the_handler_and_assets_not_just_the_page() {
    let (_dir, config) = server();
    publish_handler(&config, "members");
    gate(&config, "members", "authenticated");
    std::fs::write(config.data_dir.join("members/secret.txt"), "classified").unwrap();

    // An API call gets a status, not a redirect into an HTML form.
    let (status, ..) = send(&config, get("/p/members/api/echo")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, ..) = send(&config, get("/p/members/secret.txt")).await;
    assert_ne!(status, StatusCode::OK, "an asset leaked past the gate");
}

#[tokio::test]
async fn a_handler_learns_who_is_signed_in() {
    let (_dir, config) = server();
    publish_handler(&config, "app");

    // Anonymous by default.
    let (status, ..) = send(&config, get("/p/app/api/whoami")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    account(&config, "someone@example.com", "correct horse battery");
    let token = sign_in(&config, "someone@example.com", "correct horse battery");
    let (status, body, _) = send(&config, get_as("/p/app/api/whoami", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.ends_with(":someone@example.com"), "got {body}");
}

#[tokio::test]
async fn an_invented_cookie_buys_nothing() {
    let (_dir, config) = server();
    publish_handler(&config, "app");
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");

    let (status, ..) = send(&config, get_as("/p/members/", "forged-token")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, ..) = send(&config, get_as("/p/app/api/whoami", "forged-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signing_in_sets_a_session_and_signing_out_clears_it() {
    let (_dir, config) = server();
    account(&config, "someone@example.com", "correct horse battery");

    let request = Request::builder()
        .method("POST")
        .uri("/auth/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "email=someone@example.com&password=correct+horse+battery&next=/p/somewhere",
        ))
        .unwrap();
    let (status, _, headers) = send(&config, request).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = headers.iter().find(|(k, _)| k == "set-cookie").unwrap();
    assert!(cookie.1.contains("HttpOnly") && cookie.1.contains("Secure"), "{}", cookie.1);
    let location = headers.iter().find(|(k, _)| k == "location").unwrap();
    assert_eq!(location.1, "/p/somewhere");

    let token = cookie.1.split(';').next().unwrap().trim_start_matches("ts_session=");
    let (status, body, _) = send(&config, get_as("/auth/me", token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("someone@example.com"));

    let logout = Request::builder()
        .method("POST")
        .uri("/auth/logout")
        .header("cookie", format!("ts_session={token}"))
        .body(Body::empty())
        .unwrap();
    send(&config, logout).await;

    let (status, ..) = send(&config, get_as("/auth/me", token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "session survived sign-out");
}

#[tokio::test]
async fn a_bad_password_does_not_hand_out_a_session() {
    let (_dir, config) = server();
    account(&config, "someone@example.com", "correct horse battery");

    let request = Request::builder()
        .method("POST")
        .uri("/auth/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("email=someone@example.com&password=wrong"))
        .unwrap();
    let (status, _, headers) = send(&config, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.iter().all(|(k, _)| k != "set-cookie"));
}

#[tokio::test]
async fn the_next_parameter_cannot_bounce_a_visitor_off_site() {
    let (_dir, config) = server();
    account(&config, "someone@example.com", "correct horse battery");

    for hostile in ["https://evil.example.com/", "//evil.example.com/"] {
        let request = Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "email=someone@example.com&password=correct+horse+battery&next={hostile}"
            )))
            .unwrap();
        let (_, _, headers) = send(&config, request).await;
        let location = headers.iter().find(|(k, _)| k == "location").unwrap();
        assert_eq!(location.1, "/", "open redirect via {hostile}");
    }
}
