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
    let config = Arc::new(Config::local(dir.path().to_path_buf(), TOKEN));
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

/// Like `send`, but keeps the body as bytes — a gzip does not survive being
/// read as lossy UTF-8.
async fn send_bytes(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = build_router(config.clone(), Runtime::new().unwrap())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, bytes.to_vec())
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
            user: None,
            project: None,
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
    assert!(!index.contains("/p/secret"), "hidden page appeared on the index");
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
    // Match the link, not the word: the stylesheet has classes too.
    assert!(!index.contains("/p/quiet"), "unlisted page appeared on the index");
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

/// A request carrying the site session cookie, which the browser sends to
/// every path on the origin.
fn get_as(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", format!("ts_session={token}"))
        .body(Body::empty())
        .unwrap()
}

/// A request carrying one app's session cookie. The browser would only attach
/// this under `/p/<app>/`; the tests attach it by hand so they can also ask
/// what happens when it turns up somewhere it should not.
fn get_as_app(uri: &str, app: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", format!("ts_app_{app}={token}"))
        .body(Body::empty())
        .unwrap()
}

/// Walks a signed-in visitor through the handoff and returns the app session
/// token the browser would have stored, plus the Set-Cookie it came in.
async fn hand_off(config: &Arc<Config>, site_token: &str, app: &str) -> (String, String) {
    let request = Request::builder()
        .uri(format!("/auth/handoff?app={app}&next=/p/{app}/"))
        .header("cookie", format!("ts_session={site_token}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, headers) = send(config, request).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "handoff did not redirect");
    let cookie = headers
        .iter()
        .find(|(k, _)| k == "set-cookie")
        .expect("handoff set no cookie")
        .1
        .clone();
    let token = cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches(&format!("ts_app_{app}="))
        .to_string();
    (token, cookie)
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
    let site = sign_in(&config, "someone@example.com", "correct horse battery");
    // Signing in is not itself entry: the visitor is sent to collect a
    // credential for this app first.
    let (status, _, headers) = send(&config, get_as("/p/members/", &site)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers.iter().find(|(k, _)| k == "location").unwrap();
    assert!(location.1.starts_with("/auth/handoff?app=members"), "{}", location.1);

    let (app_token, _) = hand_off(&config, &site, "members").await;
    let (status, body, _) = send(&config, get_as_app("/p/members/", "members", &app_token)).await;
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

    // A stranger is refused outright rather than sent round the handoff: the
    // site session would not satisfy this gate either.
    let outsider = sign_in(&config, "outsider@example.com", "correct horse battery");
    let (status, ..) = send(&config, get_as("/p/private/", &outsider)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a signed-in stranger got in");
    let (outsider_app, _) = hand_off(&config, &outsider, "private").await;
    let (status, ..) = send(&config, get_as_app("/p/private/", "private", &outsider_app)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the handoff granted the app itself");

    toolsite::accounts::users::grant(&config, "allowed@example.com", "private", "viewer").unwrap();
    let allowed = sign_in(&config, "allowed@example.com", "correct horse battery");
    let (allowed_app, _) = hand_off(&config, &allowed, "private").await;
    let (status, ..) = send(&config, get_as_app("/p/private/", "private", &allowed_app)).await;
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
    let site = sign_in(&config, "someone@example.com", "correct horse battery");
    // Being signed in to the site tells this app nothing — identity reaches a
    // guest through the app's own session, or not at all.
    let (status, ..) = send(&config, get_as("/p/app/api/whoami", &site)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the site cookie leaked into a guest");

    let (app_token, _) = hand_off(&config, &site, "app").await;
    let (status, body, _) =
        send(&config, get_as_app("/p/app/api/whoami", "app", &app_token)).await;
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

    // Nor does inventing the app-scoped one, which is the cookie that counts.
    let (status, ..) = send(&config, get_as_app("/p/members/", "members", "forged-token")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, ..) = send(&config, get_as_app("/p/app/api/whoami", "app", "forged")).await;
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

#[tokio::test]
async fn a_hostile_page_title_cannot_inject_script_into_the_index() {
    let (_dir, config) = server();
    // A title is attacker-influenced content: it comes from a published page.
    write_page(
        &config,
        "nasty",
        r#"<!doctype html><title><script>alert(1)</script></title>"#,
    );

    let (status, body, _) = send(&config, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("<script>alert(1)</script>"),
        "the title was written into the index unescaped"
    );
    assert!(body.contains("&lt;script&gt;"), "expected an escaped title");
}

#[tokio::test]
async fn a_hostile_icon_cannot_break_out_of_its_attribute() {
    let (_dir, config) = server();
    write_page(&config, "nasty", "<!doctype html><title>ok</title>");
    // data: URIs are rendered as an img src, so a quote here would escape the
    // attribute if it were interpolated rather than escaped.
    std::fs::write(
        config.data_dir.join("nasty.icon"),
        r#"data:image/svg+xml,x" onerror="alert(1)"#,
    )
    .unwrap();

    let (_, body, _) = send(&config, get("/")).await;
    assert!(!body.contains(r#"onerror="alert(1)"#), "attribute was broken out of");
}

// --- one origin, many apps ----------------------------------------------
//
// Every app is served from the same host, so the browser is no help: it will
// hand any cookie it holds to whichever app asks for the path. Isolation is
// the cookie's `Path`, and these tests are what says so.

#[tokio::test]
async fn a_site_session_alone_opens_no_app() {
    let (_dir, config) = server();
    publish_handler(&config, "members");
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");

    // The page: not served, only an offer to go and earn a credential.
    let (status, body, headers) = send(&config, get_as("/p/members/", &site)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "the site cookie opened the page");
    assert!(!body.contains("members</h1>"));
    let location = headers.iter().find(|(k, _)| k == "location").unwrap();
    assert!(location.1.starts_with("/auth/handoff?app=members"), "{}", location.1);

    // The API: refused with a status, never redirected into a sign-in page,
    // and never quietly upgraded to an app session on a script's say-so.
    let (status, _, headers) = send(&config, get_as("/p/members/api/echo", &site)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the site cookie opened the API");
    assert!(headers.iter().all(|(k, _)| k != "set-cookie"), "the API minted a session");
}

#[tokio::test]
async fn the_handoff_admits_the_visitor_the_site_session_names() {
    let (_dir, config) = server();
    publish_handler(&config, "members");
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");

    let (app_token, _) = hand_off(&config, &site, "members").await;

    let (status, body, _) = send(&config, get_as_app("/p/members/", "members", &app_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("members"));

    // And the app learns who it is talking to.
    let (status, body, _) =
        send(&config, get_as_app("/p/members/api/whoami", "members", &app_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.ends_with(":someone@example.com"), "got {body}");
}

#[tokio::test]
async fn the_handoff_cookie_is_confined_to_one_apps_path() {
    let (_dir, config) = server();
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");

    let (_, cookie) = hand_off(&config, &site, "members").await;

    // The Path is the isolation: the browser will not send this cookie to
    // /p/anything-else/, so no other app can spend it.
    assert!(cookie.contains("Path=/p/members/"), "{cookie}");
    assert!(cookie.starts_with("ts_app_members="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("Secure"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
}

/// The vulnerability itself: one origin, so the only thing standing between
/// app A and the visitor's standing with app B is that the token is scoped.
#[tokio::test]
async fn one_apps_session_is_worthless_against_another_app() {
    let (_dir, config) = server();
    for app in ["alpha", "beta"] {
        publish_handler(&config, app);
        write_page(&config, &format!("{app}/index"), &format!("<h1>{app}</h1>"));
        gate(&config, app, "authenticated");
    }
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");
    let (alpha, _) = hand_off(&config, &site, "alpha").await;

    // Alpha's own app works.
    let (status, ..) = send(&config, get_as_app("/p/alpha/", "alpha", &alpha)).await;
    assert_eq!(status, StatusCode::OK);

    // Alpha's token, presented for beta under beta's own cookie name, buys
    // nothing — a scope is checked, not just a cookie's presence.
    let (status, ..) = send(&config, get_as_app("/p/beta/", "beta", &alpha)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "alpha's session opened beta");
    let (status, ..) = send(&config, get_as_app("/p/beta/api/whoami", "beta", &alpha)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "alpha's session reached beta's API");

    // And the cookie alpha's script actually holds is not beta's to begin
    // with, so beta sees an anonymous stranger.
    let request = Request::builder()
        .uri("/p/beta/api/whoami")
        .header("cookie", format!("ts_app_alpha={alpha}"))
        .body(Body::empty())
        .unwrap();
    let (status, ..) = send(&config, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "beta honoured alpha's cookie");
}

/// A scoped cookie must not outlive the sign-out that ended the session it
/// came from — the browser will not send it to /auth/logout to be cleared, so
/// the server has to be the one that kills it.
#[tokio::test]
async fn signing_out_ends_the_app_sessions_too() {
    let (_dir, config) = server();
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");
    let (app_token, _) = hand_off(&config, &site, "members").await;

    let (status, ..) = send(&config, get_as_app("/p/members/", "members", &app_token)).await;
    assert_eq!(status, StatusCode::OK);

    let logout = Request::builder()
        .method("POST")
        .uri("/auth/logout")
        .header("cookie", format!("ts_session={site}"))
        .body(Body::empty())
        .unwrap();
    send(&config, logout).await;

    let (status, ..) = send(&config, get_as_app("/p/members/", "members", &app_token)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "an app session survived sign-out");
}

#[tokio::test]
async fn a_public_app_serves_with_no_cookies_at_all() {
    let (_dir, config) = server();
    publish_handler(&config, "open");
    write_page(&config, "open/index", "<h1>open</h1>");

    let (status, body, _) = send(&config, get("/p/open/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("open"));
    let (status, body, _) = send(&config, get("/p/open/api/echo")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Nobody is signed in, so the guest is told nobody is.
    let (status, ..) = send(&config, get("/p/open/api/whoami")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A visitor who has been through the handoff still gets identity, even
    // though this app never required it.
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");
    let (app_token, _) = hand_off(&config, &site, "open").await;
    let (status, body, _) = send(&config, get_as_app("/p/open/api/whoami", "open", &app_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.ends_with(":someone@example.com"), "got {body}");
}

#[tokio::test]
async fn the_handoff_refuses_an_app_name_that_is_not_one() {
    let (_dir, config) = server();
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");

    for hostile in ["../etc/passwd", "a%2Fb", ".site", "with%20space", ""] {
        let request = Request::builder()
            .uri(format!("/auth/handoff?app={hostile}&next=/"))
            .header("cookie", format!("ts_session={site}"))
            .body(Body::empty())
            .unwrap();
        let (status, _, headers) = send(&config, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "minted a session for {hostile:?}");
        assert!(
            headers.iter().all(|(k, _)| k != "set-cookie"),
            "set a cookie for {hostile:?}"
        );
    }
}

#[tokio::test]
async fn the_handoff_cannot_bounce_a_visitor_off_site() {
    let (_dir, config) = server();
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");

    for hostile in ["https://evil.example.com/", "//evil.example.com/"] {
        let request = Request::builder()
            .uri(format!(
                "/auth/handoff?app=members&next={}",
                urlencoding::encode(hostile)
            ))
            .header("cookie", format!("ts_session={site}"))
            .body(Body::empty())
            .unwrap();
        let (_, _, headers) = send(&config, request).await;
        let location = headers.iter().find(|(k, _)| k == "location").unwrap();
        assert_eq!(location.1, "/", "open redirect via {hostile}");
    }
}

/// The handoff is where the two tiers meet, so it is also where a hostile app
/// would try to mint itself a neighbour's cookie. Fetch metadata is the only
/// thing that distinguishes the visitor navigating from a script asking on
/// their behalf, and script cannot forge it.
#[tokio::test]
async fn a_script_cannot_mint_itself_a_session_for_a_neighbour() {
    let (_dir, config) = server();
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");

    // What `fetch('/auth/handoff?app=members')` from another app looks like.
    let fetched = Request::builder()
        .uri("/auth/handoff?app=members&next=/p/members/")
        .header("cookie", format!("ts_session={site}"))
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-dest", "empty")
        .body(Body::empty())
        .unwrap();
    let (status, _, headers) = send(&config, fetched).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(headers.iter().all(|(k, _)| k != "set-cookie"), "a script got a cookie");

    // And the gate does not send one there either, so following redirects
    // gains nothing.
    let fetched = Request::builder()
        .uri("/p/members/")
        .header("cookie", format!("ts_session={site}"))
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-dest", "empty")
        .body(Body::empty())
        .unwrap();
    let (status, ..) = send(&config, fetched).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a background fetch was sent to the handoff");

    // The same request as a navigation is the visitor, and is let through.
    let navigated = Request::builder()
        .uri("/p/members/")
        .header("cookie", format!("ts_session={site}"))
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-dest", "document")
        .body(Body::empty())
        .unwrap();
    let (status, _, headers) = send(&config, navigated).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers.iter().find(|(k, _)| k == "location").unwrap();
    assert!(location.1.starts_with("/auth/handoff?app=members"), "{}", location.1);
}

/// The cookie's Path ends in a slash, and a browser will not send it to the
/// bare `/p/<app>`, so the gate has to send the visitor somewhere the cookie
/// will actually come back — or the two bounce off each other forever.
#[tokio::test]
async fn the_app_root_without_a_slash_does_not_loop() {
    let (_dir, config) = server();
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");
    account(&config, "someone@example.com", "correct horse battery");
    let site = sign_in(&config, "someone@example.com", "correct horse battery");

    let (_, _, headers) = send(&config, get_as("/p/members", &site)).await;
    let location = headers.iter().find(|(k, _)| k == "location").unwrap();
    assert_eq!(
        location.1, "/auth/handoff?app=members&next=%2Fp%2Fmembers%2F",
        "the handoff would return to a path the cookie is not sent to"
    );
}

#[tokio::test]
async fn the_login_form_cannot_be_used_to_inject_markup() {
    let (_dir, config) = server();
    // ?next= is attacker-controlled and lands in a value attribute.
    let (status, body, _) = send(
        &config,
        // Percent-encoded so it is a legal URI; axum hands the handler the
        // raw characters, which is the point.
        get("/auth/login?next=%2Fp%2Fx%22%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<script>alert(1)</script>"), "markup was injected");
}

// --- admin --------------------------------------------------------------

fn admin_account(config: &Config, email: &str, password: &str) {
    toolsite::accounts::users::sign_up_as(config, email, password, true).unwrap();
}

/// The token an admin's own forms carry. Pulled from a rendered page rather
/// than computed, so the test exercises what a browser would actually send.
fn form_token_from(body: &str) -> String {
    let marker = r#"name="token" value=""#;
    let start = body.find(marker).expect("no form token on the page") + marker.len();
    body[start..].split('"').next().unwrap().to_string()
}

fn post_form(uri: &str, token: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("cookie", format!("ts_session={token}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn the_admin_page_is_for_admins_only() {
    let (_dir, config) = server();
    account(&config, "ordinary@example.com", "correct horse battery");
    admin_account(&config, "boss@example.com", "correct horse battery");

    // Signed out: sent to sign in.
    let (status, ..) = send(&config, get("/admin")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Signed in but ordinary: refused, and not bounced into a login loop.
    let ordinary = sign_in(&config, "ordinary@example.com", "correct horse battery");
    let (status, ..) = send(&config, get_as("/admin", &ordinary)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (status, body, _) = send(&config, get_as("/admin", &boss)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("ordinary@example.com"), "accounts were not listed");
}

#[tokio::test]
async fn an_admin_can_create_an_account_and_it_can_sign_in() {
    let (_dir, config) = server();
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");

    let (_, page, _) = send(&config, get_as("/admin", &boss)).await;
    let token = form_token_from(&page);

    let (status, ..) = send(
        &config,
        post_form(
            "/admin/users",
            &boss,
            format!("token={token}&email=new@example.com&password=correct+horse+battery"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // The proof it worked is that the account can actually sign in.
    assert!(toolsite::accounts::users::log_in(&config, "new@example.com", "correct horse battery").is_ok());
}

#[tokio::test]
async fn an_admin_can_gate_an_app_and_grant_access_to_it() {
    let (_dir, config) = server();
    write_page(&config, "reports/index", "<h1>reports</h1>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    account(&config, "reader@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");

    let (_, page, _) = send(&config, get_as("/admin", &boss)).await;
    let token = form_token_from(&page);

    send(
        &config,
        post_form("/admin/gate", &boss, format!("token={token}&app=reports&gate=granted")),
    )
    .await;
    let (status, ..) = send(&config, get("/p/reports/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "gate did not take effect");

    send(
        &config,
        post_form(
            "/admin/access",
            &boss,
            format!("token={token}&app=reports&email=reader@example.com&allow=1"),
        ),
    )
    .await;
    let reader = toolsite::accounts::users::log_in(&config, "reader@example.com", "correct horse battery")
        .unwrap()
        .0;
    assert!(toolsite::accounts::users::has_grant(&config, &reader, "reports"));
}

#[tokio::test]
async fn an_admin_action_needs_the_form_token_from_this_session() {
    let (_dir, config) = server();
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");

    // A page on this origin can make the admin's browser POST, since the
    // cookie is same-site. The token is what stops it landing.
    for forged in ["", "guessed-token"] {
        let (status, ..) = send(
            &config,
            post_form(
                "/admin/users",
                &boss,
                format!("token={forged}&email=sneak@example.com&password=correct+horse+battery"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "token {forged:?} was accepted");
    }
    assert!(
        toolsite::accounts::users::log_in(&config, "sneak@example.com", "correct horse battery")
            .is_err(),
        "an account was created without a valid form token"
    );
}

#[tokio::test]
async fn an_ordinary_account_cannot_drive_admin_actions_directly() {
    let (_dir, config) = server();
    admin_account(&config, "boss@example.com", "correct horse battery");
    account(&config, "ordinary@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let ordinary = sign_in(&config, "ordinary@example.com", "correct horse battery");

    let (_, page, _) = send(&config, get_as("/admin", &boss)).await;
    let token = form_token_from(&page);

    // Even holding a real admin's form token, the session decides.
    let (status, ..) = send(
        &config,
        post_form(
            "/admin/users",
            &ordinary,
            format!("token={token}&email=sneak@example.com&password=correct+horse+battery&admin=1"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn disabling_an_account_ends_the_sessions_it_already_has() {
    let (_dir, config) = server();
    write_page(&config, "members/index", "<h1>members</h1>");
    gate(&config, "members", "authenticated");
    account(&config, "someone@example.com", "correct horse battery");

    // Signed in and working before anything changes.
    let token = sign_in(&config, "someone@example.com", "correct horse battery");
    let (status, ..) = send(&config, get_as("/p/members/", &token)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "expected the handoff");

    toolsite::accounts::users::set_active(&config, "someone@example.com", false).unwrap();

    // The point: a live session stops working, rather than lasting until it
    // expires. Checking the flag only at sign-in would miss this.
    let (status, ..) = send(&config, get_as("/auth/me", &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    assert!(
        toolsite::accounts::users::log_in(&config, "someone@example.com", "correct horse battery")
            .is_err(),
        "a disabled account signed in"
    );
}

#[tokio::test]
async fn enabling_an_account_lets_it_back_in() {
    let (_dir, config) = server();
    account(&config, "someone@example.com", "correct horse battery");
    toolsite::accounts::users::set_active(&config, "someone@example.com", false).unwrap();
    toolsite::accounts::users::set_active(&config, "someone@example.com", true).unwrap();

    // Nothing was destroyed, so the same password still works.
    let (_, token) =
        toolsite::accounts::users::log_in(&config, "someone@example.com", "correct horse battery")
            .unwrap();
    let (status, ..) = send(&config, get_as("/auth/me", &token)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_disabled_admin_loses_the_admin_page() {
    let (_dir, config) = server();
    admin_account(&config, "boss@example.com", "correct horse battery");
    admin_account(&config, "other@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    assert_eq!(send(&config, get_as("/admin", &boss)).await.0, StatusCode::OK);

    toolsite::accounts::users::set_active(&config, "boss@example.com", false).unwrap();
    let (status, ..) = send(&config, get_as("/admin", &boss)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a disabled admin kept the page");
}

#[tokio::test]
async fn an_admin_cannot_disable_itself_and_lock_everyone_out() {
    let (_dir, config) = server();
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/admin", &boss)).await;
    let token = form_token_from(&page);

    let (status, _, headers) = send(
        &config,
        post_form(
            "/admin/active",
            &boss,
            format!("token={token}&email=boss@example.com&active=0"),
        ),
    )
    .await;
    // Refused with a message on the page they came from, not a bare 400.
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, page) = follow(&config, &boss, &headers).await;
    assert_eq!(status, StatusCode::OK, "the admin locked themselves out");
    assert!(page.contains("cannot disable your own account"), "no explanation shown");
}

// --- invitations --------------------------------------------------------

#[tokio::test]
async fn an_invited_account_sets_its_own_password_and_is_signed_in() {
    let (_dir, config) = server();
    let (_, token) =
        toolsite::accounts::users::invite(&config, "new@example.com", false).unwrap();

    // The form names who it is for, so the person knows what they are joining.
    let (status, body, _) = send(&config, get(&format!("/auth/setup?token={token}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("new@example.com"));

    let (status, _, headers) = send(
        &config,
        Request::builder()
            .method("POST")
            .uri("/auth/setup")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "token={token}&password=correct+horse+battery"
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Signed in already: they just proved they hold the link and chose the
    // password, so asking for it again would be theatre.
    let cookie = headers.iter().find(|(k, _)| k == "set-cookie").unwrap();
    let session = cookie.1.split(';').next().unwrap().trim_start_matches("ts_session=");
    let (status, me, _) = send(&config, get_as("/auth/me", session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(me.contains("new@example.com"));

    // And the password works from the front door too.
    assert!(
        toolsite::accounts::users::log_in(&config, "new@example.com", "correct horse battery")
            .is_ok()
    );
}

#[tokio::test]
async fn an_invitation_works_exactly_once() {
    let (_dir, config) = server();
    let (_, token) =
        toolsite::accounts::users::invite(&config, "new@example.com", false).unwrap();

    let accept = |password: &str| {
        Request::builder()
            .method("POST")
            .uri("/auth/setup")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!("token={token}&password={password}")))
            .unwrap()
    };

    assert_eq!(send(&config, accept("correct+horse+battery")).await.0, StatusCode::SEE_OTHER);
    // A second use must not let anyone reset the password out from under them.
    assert_eq!(send(&config, accept("someone+elses+choice")).await.0, StatusCode::BAD_REQUEST);
    assert!(
        toolsite::accounts::users::log_in(&config, "new@example.com", "correct horse battery")
            .is_ok(),
        "the password was changed by a replayed link"
    );
}

#[tokio::test]
async fn an_account_awaiting_its_password_cannot_sign_in() {
    let (_dir, config) = server();
    toolsite::accounts::users::invite(&config, "new@example.com", false).unwrap();

    // No password is set yet, so nothing should get past the login form.
    for attempt in ["", "correct horse battery", "anything"] {
        assert!(
            toolsite::accounts::users::log_in(&config, "new@example.com", attempt).is_err(),
            "signed in with {attempt:?} before a password existed"
        );
    }
}

#[tokio::test]
async fn a_made_up_or_expired_invitation_is_refused() {
    let (_dir, config) = server();
    let (status, ..) = send(&config, get("/auth/setup?token=not-a-real-invitation")).await;
    assert_eq!(status, StatusCode::GONE);

    let (_, token) =
        toolsite::accounts::users::invite(&config, "new@example.com", false).unwrap();
    // Re-inviting replaces the outstanding link, so the first one dies.
    toolsite::accounts::users::reinvite(&config, "new@example.com").unwrap();
    let (status, ..) = send(&config, get(&format!("/auth/setup?token={token}"))).await;
    assert_eq!(status, StatusCode::GONE, "a replaced link still worked");
}

#[tokio::test]
async fn public_and_gated_apps_coexist_without_leaking_into_each_other() {
    let (_dir, config) = server();
    write_page(&config, "openapp/index", "<!doctype html><title>Public Thing</title>");
    write_page(&config, "members/index", "<!doctype html><title>Members Only</title>");
    write_page(&config, "secretapp/index", "<!doctype html><title>Salary Review 2026</title>");
    gate(&config, "members", "authenticated");
    gate(&config, "secretapp", "granted");

    account(&config, "someone@example.com", "correct horse battery");
    toolsite::accounts::users::grant(&config, "someone@example.com", "secretapp", "viewer").unwrap();

    // Anonymous: the public app only. A gated app's *title* is as sensitive
    // as its contents, so it must not appear either.
    let (_, index, _) = send(&config, get("/")).await;
    assert!(index.contains("Public Thing"));
    assert!(!index.contains("Members Only"), "an authenticated app was advertised");
    assert!(!index.contains("Salary Review 2026"), "a granted app was advertised");
    assert_eq!(send(&config, get("/p/openapp/")).await.0, StatusCode::OK);
    assert_eq!(send(&config, get("/p/members/")).await.0, StatusCode::SEE_OTHER);

    // Signed in: the authenticated app appears, and so does the one granted.
    let token = sign_in(&config, "someone@example.com", "correct horse battery");
    let (_, index, _) = send(&config, get_as("/", &token)).await;
    assert!(index.contains("Public Thing"));
    assert!(index.contains("Members Only"));
    assert!(index.contains("Salary Review 2026"));
}

#[tokio::test]
async fn a_grant_on_one_app_does_not_reveal_another() {
    let (_dir, config) = server();
    write_page(&config, "mine/index", "<!doctype html><title>Mine</title>");
    write_page(&config, "theirs/index", "<!doctype html><title>Theirs</title>");
    gate(&config, "mine", "granted");
    gate(&config, "theirs", "granted");

    account(&config, "someone@example.com", "correct horse battery");
    toolsite::accounts::users::grant(&config, "someone@example.com", "mine", "viewer").unwrap();
    let token = sign_in(&config, "someone@example.com", "correct horse battery");

    let (_, index, _) = send(&config, get_as("/", &token)).await;
    assert!(index.contains("Mine"));
    assert!(!index.contains("Theirs"), "an app they cannot open was listed");
}

#[tokio::test]
async fn the_password_form_names_the_account_so_a_manager_can_save_it() {
    let (_dir, config) = server();
    let (_, invite) =
        toolsite::accounts::users::invite(&config, "new@example.com", false).unwrap();

    let (status, body, _) = send(&config, get(&format!("/auth/setup?token={invite}"))).await;
    assert_eq!(status, StatusCode::OK);

    // A password manager needs a username field in the same form, or it saves
    // a password with nothing to associate it with.
    assert!(body.contains(r#"autocomplete="username""#), "no username field");
    assert!(body.contains(r#"value="new@example.com""#), "the email was not filled in");
    assert!(body.contains(r#"autocomplete="new-password""#));
}

#[tokio::test]
async fn every_page_toolsite_serves_itself_shares_one_stylesheet() {
    let (_dir, config) = server();
    write_page(&config, "page", "<!doctype html><title>A page</title>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (_, invite) = toolsite::accounts::users::invite(&config, "new@example.com", false).unwrap();

    // A token from the shared theme, present only if the page uses it.
    let marker = "--accent:";
    for (name, request) in [
        ("index", get("/")),
        ("sign in", get("/auth/login")),
        ("choose a password", get(&format!("/auth/setup?token={invite}"))),
        ("admin", get_as("/admin", &boss)),
    ] {
        let (status, body, _) = send(&config, request).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert!(body.contains(marker), "{name} does not use the shared theme");
    }
}

#[tokio::test]
async fn an_agent_can_fetch_the_contract_and_a_crate_that_builds_against_it() {
    let (_dir, config) = server();

    // Public: the WIT describes an interface and the scaffold is a template.
    // Neither says anything about what is published here.
    let (status, wit, _) = send(&config, get("/wit/toolsite.wit")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(wit.contains("world app"), "not the contract");
    assert!(wit.contains("export handle:"), "the export is what a guest must satisfy");

    let (status, body) = send_bytes(&config, get("/scaffold/notes")).await;
    assert_eq!(status, StatusCode::OK);

    // The archive must carry everything needed to build without the repo.
    let decoder = flate2::read::GzDecoder::new(&body[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut names: Vec<String> = archive
        .entries()
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().unwrap().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "notes-handler/Cargo.toml",
            "notes-handler/README.md",
            "notes-handler/migrations/001_initial.sql",
            "notes-handler/src/lib.rs",
            "notes-handler/wit/toolsite.wit",
        ]
    );
}

#[tokio::test]
async fn a_scaffold_cannot_be_asked_for_under_a_bad_name() {
    let (_dir, config) = server();
    let (status, ..) = send(&config, get("/scaffold/../../etc")).await;
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test]
async fn sidecars_are_not_reachable_from_the_public_route() {
    let (_dir, config) = server();
    write_page(&config, "page", "<!doctype html><title>A page</title>");
    std::fs::write(
        config.data_dir.join("page.meta"),
        r#"{"listed":false,"hidden":false,"spa":false,"gate":"public"}"#,
    )
    .unwrap();
    std::fs::write(config.data_dir.join("page.notes"), "internal: rotate the key").unwrap();
    std::fs::write(config.data_dir.join("page.icon"), "🔑").unwrap();

    // .meta would say whether a page is hidden and what gate it is behind;
    // .notes is written for the next agent session, not for visitors.
    for sidecar in ["/p/page.meta", "/p/page.notes", "/p/page.icon"] {
        let (status, body, _) = send(&config, get(sidecar)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{sidecar} was served: {body}");
    }

    // The page itself is unaffected, and icons still have their own route.
    assert_eq!(send(&config, get("/p/page")).await.0, StatusCode::OK);
    assert_eq!(send(&config, get("/icon/page")).await.0, StatusCode::OK);
}

#[tokio::test]
async fn notes_survive_for_the_next_session_but_never_reach_a_visitor() {
    let (_dir, config) = server();
    write_page(&config, "notes-app/index", "<!doctype html><title>App</title>");
    let markdown = "## schema\n\ntodos(id, user_id, text)\n\nTODO: pagination is unfinished.";
    toolsite::content::store::write_notes(&config, "notes-app", markdown)
        .await
        .unwrap();

    // A later session reads them back verbatim.
    assert_eq!(
        toolsite::content::store::read_notes(&config, "notes-app").await.unwrap(),
        markdown
    );

    // They are not part of what the app serves, by any spelling.
    for path in [
        "/p/notes-app/index.notes",
        "/p/notes-app.notes",
        "/p/notes-app/notes",
    ] {
        let (status, body, _) = send(&config, get(path)).await;
        assert!(
            status != StatusCode::OK || !body.contains("pagination"),
            "{path} served the notes"
        );
    }
}

#[tokio::test]
async fn an_agent_can_fetch_back_the_project_it_published() {
    let (_dir, config) = server();
    let token = ticket(&config, "myapp", Duration::from_secs(60));

    // Nothing stored yet: say so, and say what to do about it.
    let (status, body, _) = send(&config, get(&format!("/upload/{token}?source"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("PUT ?source"), "the error should say how to fix it");

    // A project archive, whatever the agent decides that means.
    let project = b"\x1f\x8b\x08\x00pretend this is a tar.gz of src/".to_vec();
    let (status, body, _) = send(
        &config,
        Request::builder()
            .method("PUT")
            .uri(format!("/upload/{token}?source"))
            .body(Body::from(project.clone()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, returned) = send_bytes(&config, get(&format!("/upload/{token}?source"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(returned, project, "what came back was not what went in");
}

#[tokio::test]
async fn source_is_never_served_to_a_visitor() {
    let (_dir, config) = server();
    let token = ticket(&config, "myapp", Duration::from_secs(60));
    send(
        &config,
        Request::builder()
            .method("PUT")
            .uri(format!("/upload/{token}?source"))
            .body(Body::from("SECRET SOURCE"))
            .unwrap(),
    )
    .await;

    // The whole point: a visitor sees the built output, never the project.
    for path in ["/p/myapp.source", "/p/myapp/source", "/p/myapp/.source"] {
        let (status, body, _) = send(&config, get(path)).await;
        assert!(
            status != StatusCode::OK || !body.contains("SECRET SOURCE"),
            "{path} served the source"
        );
    }
}

#[tokio::test]
async fn a_ticket_reads_only_its_own_app() {
    let (_dir, config) = server();
    let mine = ticket(&config, "mine", Duration::from_secs(60));
    let theirs = ticket(&config, "theirs", Duration::from_secs(60));

    send(
        &config,
        Request::builder()
            .method("PUT")
            .uri(format!("/upload/{theirs}?source"))
            .body(Body::from("THEIR SOURCE"))
            .unwrap(),
    )
    .await;

    // A ticket is scoped to one slug in both directions.
    let (status, body) = send_bytes(&config, get(&format!("/upload/{mine}?source"))).await;
    assert!(
        status != StatusCode::OK || !String::from_utf8_lossy(&body).contains("THEIR SOURCE"),
        "one app's ticket read another's source"
    );

    let (status, ..) = send(&config, get("/upload/not-a-ticket?source")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- settings -----------------------------------------------------------

#[tokio::test]
async fn an_owner_pastes_settings_through_a_link_the_agent_never_reads() {
    let (_dir, config) = server();
    let link = toolsite::platform::secrets::create_entry(&config, "scraper").unwrap();
    let token = link.rsplit('/').next().unwrap().to_string();

    let (status, form, _) = send(&config, get(&format!("/settings/{token}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("Settings for"), "not the entry form");

    let (status, ..) = send(
        &config,
        Request::builder()
            .method("POST")
            .uri("/settings")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "token={token}&pasted=%23+comment%0Aexport+API_KEY%3D%22hunter2%22%0AENDPOINT%3Dhttps%3A%2F%2Fexample.com"
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Names come back; values never do.
    let listed = toolsite::platform::secrets::names(&config, "scraper");
    assert_eq!(listed, ["API_KEY", "ENDPOINT"]);
    let (_, form, _) = send(&config, get(&format!("/settings/{token}"))).await;
    assert!(form.contains("API_KEY"), "the form should say what is set");
    assert!(!form.contains("hunter2"), "the form showed a value back");

    // Only the app's own code can read one.
    assert_eq!(
        toolsite::platform::secrets::get(&config, "scraper", "API_KEY").as_deref(),
        Some("hunter2")
    );
}

#[tokio::test]
async fn a_settings_link_is_scoped_and_expires() {
    let (_dir, config) = server();
    toolsite::platform::secrets::create_entry(&config, "mine").unwrap();

    let (status, ..) = send(&config, get("/settings/not-a-real-token")).await;
    assert_eq!(status, StatusCode::GONE);

    let (status, ..) = send(
        &config,
        Request::builder()
            .method("POST")
            .uri("/settings")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("token=not-a-real-token&pasted=API_KEY%3Dx"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn settings_are_absent_from_everything_a_visitor_or_agent_can_fetch() {
    let (_dir, config) = server();
    write_page(&config, "scraper/index", "<!doctype html><title>Scraper</title>");
    toolsite::platform::secrets::set(&config, "scraper", "API_KEY", Some("hunter2")).unwrap();

    // Not under /p/, by any spelling.
    for path in ["/p/scraper.secrets", "/p/scraper/secrets", "/p/scraper/.secrets"] {
        let (status, body, _) = send(&config, get(path)).await;
        assert!(
            status != StatusCode::OK || !body.contains("hunter2"),
            "{path} served a value"
        );
    }

    // Not in the source archive an agent pulls back either.
    let ticket = ticket(&config, "scraper", Duration::from_secs(60));
    send(
        &config,
        Request::builder()
            .method("PUT")
            .uri(format!("/upload/{ticket}?source"))
            .body(Body::from("the project, without its secrets"))
            .unwrap(),
    )
    .await;
    let (_, archive) = send_bytes(&config, get(&format!("/upload/{ticket}?source"))).await;
    assert!(
        !String::from_utf8_lossy(&archive).contains("hunter2"),
        "a value rode along in the source archive"
    );
}

// --- gates on part of an app -------------------------------------------

fn rules(config: &Config, app: &str, gate: &str, rules: &str) {
    std::fs::create_dir_all(config.data_dir.join(app)).unwrap();
    std::fs::write(
        config.data_dir.join(app).join("index.meta"),
        format!(r#"{{"listed":true,"hidden":false,"spa":false,"gate":"{gate}","rules":{rules}}}"#),
    )
    .unwrap();
}

#[tokio::test]
async fn a_public_app_can_have_a_private_corner() {
    let (_dir, config) = server();
    write_page(&config, "board/index", "<!doctype html><title>Board</title>");
    write_page(&config, "board/triage", "<!doctype html><title>Triage</title>");
    rules(
        &config,
        "board",
        "public",
        r#"[{"prefix":"/triage","gate":"authenticated"}]"#,
    );
    account(&config, "someone@example.com", "correct horse battery");

    // The front page is open to anyone…
    assert_eq!(send(&config, get("/p/board/")).await.0, StatusCode::OK);
    // …while the corner behind the rule is not.
    assert_eq!(send(&config, get("/p/board/triage")).await.0, StatusCode::SEE_OTHER);

    let token = sign_in(&config, "someone@example.com", "correct horse battery");
    let (status, ..) = send(&config, get_as("/p/board/triage", &token)).await;
    // Signed in, the handoff sends them on rather than turning them away.
    assert!(
        status == StatusCode::OK || status == StatusCode::SEE_OTHER,
        "a signed-in visitor was refused outright: {status}"
    );
}

#[tokio::test]
async fn a_private_app_can_have_a_public_front_page() {
    let (_dir, config) = server();
    write_page(&config, "members/index", "<!doctype html><title>Members</title>");
    write_page(&config, "members/inside", "<!doctype html><title>Inside</title>");
    // The reverse arrangement, from the same mechanism.
    rules(
        &config,
        "members",
        "authenticated",
        r#"[{"prefix":"/","gate":"public"},{"prefix":"/inside","gate":"authenticated"}]"#,
    );

    assert_eq!(send(&config, get("/p/members/")).await.0, StatusCode::OK);
    assert_eq!(send(&config, get("/p/members/inside")).await.0, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_longest_matching_rule_wins() {
    let (_dir, config) = server();
    write_page(&config, "app/index", "<!doctype html><title>App</title>");
    std::fs::create_dir_all(config.data_dir.join("app/admin")).unwrap();
    std::fs::write(
        config.data_dir.join("app/admin/index.html"),
        "<!doctype html><title>Admin</title>",
    )
    .unwrap();
    std::fs::write(
        config.data_dir.join("app/admin/help.html"),
        "<!doctype html><title>Help</title>",
    )
    .unwrap();
    rules(
        &config,
        "app",
        "public",
        r#"[{"prefix":"/admin","gate":"authenticated"},{"prefix":"/admin/help","gate":"public"}]"#,
    );

    // Order in the file must not matter, only specificity.
    assert_eq!(send(&config, get("/p/app/admin/help")).await.0, StatusCode::OK);
    assert_eq!(send(&config, get("/p/app/admin/")).await.0, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_rule_covers_the_api_as_well_as_the_pages() {
    let (_dir, config) = server();
    publish_handler(&config, "board");
    rules(
        &config,
        "board",
        "public",
        r#"[{"prefix":"/api/all","gate":"authenticated"}]"#,
    );

    // The open route stays open…
    assert_eq!(send(&config, get("/p/board/api/echo")).await.0, StatusCode::OK);
    // …and the guarded one answers with a status, not a redirect into a form.
    assert_eq!(
        send(&config, get("/p/board/api/all")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_scaffold_carries_the_schema_its_handler_expects() {
    let (_dir, config) = server();
    let (status, body) = send_bytes(&config, get("/scaffold/notes")).await;
    assert_eq!(status, StatusCode::OK);

    let decoder = flate2::read::GzDecoder::new(&body[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut files = std::collections::BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().to_string();
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut entry, &mut contents).unwrap();
        files.insert(name, contents);
    }

    // The handler it ships writes to a table, so a migration creating that
    // table has to travel with it — otherwise the first request 500s.
    let handler = &files["notes-handler/src/lib.rs"];
    let migration = files
        .get("notes-handler/migrations/001_initial.sql")
        .expect("the scaffold has no migration");
    assert!(handler.contains("visits"), "the template stopped using the table");
    assert!(
        migration.contains("create table visits"),
        "the migration does not create the table the handler uses"
    );
    assert!(
        !handler.contains("create table"),
        "the handler is doing its own DDL again"
    );
}

// --- what a real deploy got wrong --------------------------------------

#[tokio::test]
async fn an_unknown_upload_flag_is_refused_not_published() {
    let (_dir, config) = server();
    let token = ticket(&config, "app", Duration::from_secs(60));
    std::fs::create_dir_all(config.data_dir.join("app")).unwrap();
    std::fs::write(config.data_dir.join("app/index.html"), "<h1>the app</h1>").unwrap();

    // Probing for a flag that does not exist used to publish the probe as the
    // app's front page.
    for flag in ["config", "toolsite", "settings"] {
        let (status, body, _) = send(
            &config,
            Request::builder()
                .method("PUT")
                .uri(format!("/upload/{token}?{flag}"))
                .body(Body::from("slug = \"app\"\ngate = \"public\"\n"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "?{flag} was accepted");
        assert!(body.contains("manifest"), "the error should list the real flags");
    }

    let (_, page, _) = send(&config, get("/p/app/")).await;
    assert!(page.contains("the app"), "a probe replaced the page: {page}");
}

#[tokio::test]
async fn a_client_cannot_claim_a_request_came_from_the_scheduler() {
    let (_dir, config) = server();
    publish_handler(&config, "app");

    // The handler reports the headers it was given; the host's own marker
    // must not be among them when a visitor sends it.
    let request = Request::builder()
        .uri("/p/app/api/echo")
        .header("x-toolsite-scheduled", "nightly")
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = send(&config, request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("nightly"),
        "a forged x-toolsite header reached the handler: {body}"
    );
}

#[tokio::test]
async fn publishing_an_app_replaces_a_page_of_the_same_name() {
    let (_dir, config) = server();
    // The shape that produced two identical entries on the live site: a page
    // pushed first, then a bundle at the same slug.
    write_page(&config, "releases", "<!doctype html>slug = \"releases\"");

    let token = ticket(&config, "releases", Duration::from_secs(60));
    let mut builder = tar::Builder::new(Vec::new());
    let body = b"<!doctype html><title>Release watcher</title>";
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, "index.html", &body[..]).unwrap();
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut encoder, &tar).unwrap();
    let archive = encoder.finish().unwrap();

    let (status, report, _) = send(
        &config,
        Request::builder()
            .method("PUT")
            .uri(format!("/upload/{token}?bundle"))
            .body(Body::from(archive))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(report.contains("replaced"), "the replacement was silent: {report}");

    // The app is what serves, and the index names it once.
    let (_, page, _) = send(&config, get("/p/releases/")).await;
    assert!(page.contains("Release watcher"), "the page still shadows the app");
    let (_, index, _) = send(&config, get("/")).await;
    assert_eq!(index.matches("/p/releases\"").count(), 1, "listed twice");
}

#[tokio::test]
async fn the_platform_explains_itself_without_reading_someone_elses_app() {
    let (_dir, config) = server();
    let (status, guide, headers) = send(&config, get("/guide")).await;
    assert_eq!(status, StatusCode::OK);
    let content_type = headers.iter().find(|(k, _)| k == "content-type").unwrap();
    assert!(content_type.1.starts_with("text/markdown"), "{}", content_type.1);

    // The things an agent otherwise learns by trial, or from a neighbour's
    // notes where they go stale.
    for fact in [
        "std::time",          // no clock
        "?manifest",          // the flag it probed for
        "create table if not exists",
        "allow_http",
        "/p/<app>/api/",
    ] {
        assert!(guide.contains(fact), "the guide never mentions {fact}");
    }
}

// --- signing an MCP client in -------------------------------------------
//
// A person connects Claude (or Claude Code) to the site by signing in with
// the admin account they already have. Nothing is pasted: the client
// registers itself, the person consents, and the token that comes out is
// theirs, not a shared secret.

const BASE: &str = "https://site.test";
const CALLBACK: &str = "https://client.test/callback";
// RFC 7636's own test vector.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// A deployment that knows its own address, which is all the OAuth server
/// needs to exist. No static token at all.
fn public_server() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        data_dir: dir.path().to_path_buf(),
        base_url: Some(BASE.to_string()),
        local_base: "http://localhost:8080".to_string(),
        valid_tokens: Vec::new(),
        uploads: std::sync::Mutex::new(std::collections::HashMap::new()),
        ..Config::local(dir.path().to_path_buf(), "unused")
    });
    (dir, config)
}

fn admin(config: &Config, email: &str, password: &str) {
    toolsite::accounts::users::sign_up_as(config, email, password, true).unwrap();
}

fn json_post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn form_post(uri: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(token) = cookie {
        builder = builder.header("cookie", format!("ts_session={token}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn location(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.clone())
        .expect("no Location header")
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| urlencoding::decode(v).unwrap().into_owned())
}

/// What the client's first request does: register, and get an id back.
async fn register(config: &Arc<Config>, redirect_uri: &str) -> String {
    let body = format!(
        r#"{{"client_name":"Test Client","redirect_uris":["{redirect_uri}"],"token_endpoint_auth_method":"none"}}"#
    );
    let (status, body, _) = send(config, json_post("/register", &body)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json.get("client_secret").is_none(), "a public client was given a secret");
    json["client_id"].as_str().unwrap().to_string()
}

fn authorize_url(client_id: &str, redirect_uri: &str) -> String {
    format!(
        "/authorize?response_type=code&client_id={client_id}&redirect_uri={}&state=xyz&code_challenge={CHALLENGE}&code_challenge_method=S256&resource={}",
        urlencoding::encode(redirect_uri),
        urlencoding::encode(&format!("{BASE}/mcp")),
    )
}

/// The hidden fields of the consent form, plus the answer.
fn consent_body(page: &str, client_id: &str, redirect_uri: &str, decision: &str) -> String {
    let token = page
        .split("name=\"token\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("consent form carries no form token");
    format!(
        "token={token}&response_type=code&client_id={client_id}&redirect_uri={}&state=xyz&code_challenge={CHALLENGE}&code_challenge_method=S256&decision={decision}",
        urlencoding::encode(redirect_uri),
    )
}

/// Walks a signed-in admin through consent and returns the code the browser
/// would have carried back to the client.
async fn consent(config: &Arc<Config>, session: &str, client_id: &str, redirect_uri: &str) -> String {
    let (status, page, _) = send(config, get_as(&authorize_url(client_id, redirect_uri), session)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let body = consent_body(&page, client_id, redirect_uri, "allow");
    let (status, _, headers) = send(config, form_post("/authorize", &body, Some(session))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let target = location(&headers);
    assert!(target.starts_with(redirect_uri), "sent somewhere else: {target}");
    assert_eq!(query_param(&target, "state").as_deref(), Some("xyz"));
    query_param(&target, "code").expect("no code in the redirect")
}

fn exchange_body(client_id: &str, code: &str, redirect_uri: &str, verifier: &str) -> String {
    format!(
        "grant_type=authorization_code&client_id={client_id}&code={code}&redirect_uri={}&code_verifier={verifier}",
        urlencoding::encode(redirect_uri),
    )
}

async fn exchange(config: &Arc<Config>, body: &str) -> (StatusCode, serde_json::Value) {
    let (status, body, _) = send(config, form_post("/token", body, None)).await;
    (status, serde_json::from_str(&body).unwrap_or_default())
}

fn mcp_initialize(token: &str) -> Request<Body> {
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(initialize))
        .unwrap()
}

#[tokio::test]
async fn discovery_tells_a_client_it_can_register_and_must_use_pkce() {
    let (_dir, config) = public_server();

    let (status, body, _) = send(&config, get("/.well-known/oauth-protected-resource")).await;
    assert_eq!(status, StatusCode::OK);
    let resource: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(resource["resource"], format!("{BASE}/mcp"));
    assert_eq!(resource["authorization_servers"][0], BASE);

    let (status, body, _) = send(&config, get("/.well-known/oauth-authorization-server")).await;
    assert_eq!(status, StatusCode::OK);
    let server: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(server["issuer"], BASE);
    assert_eq!(server["registration_endpoint"], format!("{BASE}/register"));
    assert_eq!(server["code_challenge_methods_supported"], serde_json::json!(["S256"]));
    assert_eq!(server["token_endpoint_auth_methods_supported"], serde_json::json!(["none"]));

    // And a bare request to /mcp is pointed at all of this.
    let (status, _, headers) = send(&config, mcp_initialize("")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers.iter().find(|(k, _)| k == "www-authenticate").unwrap();
    assert!(challenge.1.contains("oauth-protected-resource"), "{}", challenge.1);
}

#[tokio::test]
async fn without_a_public_address_there_is_no_oauth_server() {
    let (_dir, config) = server();
    let (status, ..) = send(&config, get("/.well-known/oauth-authorization-server")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&config, json_post("/register", r#"{"redirect_uris":["https://c.test/cb"]}"#)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_admin_signs_a_client_in_and_it_can_then_use_mcp() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");

    let client_id = register(&config, CALLBACK).await;
    let code = consent(&config, &session, &client_id, CALLBACK).await;

    let (status, tokens) = exchange(&config, &exchange_body(&client_id, &code, CALLBACK, VERIFIER)).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["token_type"], "Bearer");
    let access = tokens["access_token"].as_str().unwrap();
    assert!(tokens["refresh_token"].as_str().is_some());

    let (status, body, _) = send(&config, mcp_initialize(access)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_consent_screen_names_where_the_answer_goes_and_who_it_acts_as() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;

    let (status, page, _) = send(&config, get_as(&authorize_url(&client_id, CALLBACK), &session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("client.test"), "the redirect host is not shown");
    assert!(page.contains("Test Client"), "the client's name is not shown");
    assert!(page.contains("owner@example.com"), "whose standing it acts with is not shown");
}

#[tokio::test]
async fn someone_not_signed_in_is_sent_to_sign_in_and_comes_back() {
    let (_dir, config) = public_server();
    let client_id = register(&config, CALLBACK).await;
    let url = authorize_url(&client_id, CALLBACK);

    let (status, _, headers) = send(&config, get(&url)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let target = location(&headers);
    assert!(target.starts_with("/auth/login?next="), "{target}");
    assert_eq!(query_param(&target, "next").as_deref(), Some(url.as_str()));
}

#[tokio::test]
async fn a_visitor_account_connects_a_reading_client_but_not_a_publishing_one() {
    let (_dir, config) = public_server();
    account(&config, "reader@example.com", "correct horse");
    let session = sign_in(&config, "reader@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;

    // The consent page says what a reader's client may do.
    let (status, page, _) = send(&config, get_as(&authorize_url(&client_id, CALLBACK), &session)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("cannot publish"), "{page}");

    let code = consent(&config, &session, &client_id, CALLBACK).await;
    let (status, tokens) = exchange(&config, &exchange_body(&client_id, &code, CALLBACK, VERIFIER)).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    let access = tokens["access_token"].as_str().unwrap();

    // Publishing: no. Reading as themselves: yes.
    let (status, body, _) = send(&config, mcp_initialize(access)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a visitor's token published");
    assert!(body.contains("/me/mcp"), "{body}");
    let (status, body, _) = send(&config, me_initialize(access)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The form token still guards the decision.
    let body = format!(
        "token=whatever&response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&decision=allow",
        urlencoding::encode(CALLBACK)
    );
    let (status, ..) = send(&config, form_post("/authorize", &body, Some(&session))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

fn me_initialize(token: &str) -> Request<Body> {
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    Request::builder()
        .method("POST")
        .uri("/me/mcp")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(initialize))
        .unwrap()
}

#[tokio::test]
async fn declining_sends_the_client_away_with_no_code() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;

    let (_, page, _) = send(&config, get_as(&authorize_url(&client_id, CALLBACK), &session)).await;
    let body = consent_body(&page, &client_id, CALLBACK, "deny");
    let (status, _, headers) = send(&config, form_post("/authorize", &body, Some(&session))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let target = location(&headers);
    assert_eq!(query_param(&target, "error").as_deref(), Some("access_denied"));
    assert!(query_param(&target, "code").is_none());
}

#[tokio::test]
async fn consent_cannot_be_forged_by_a_page_the_admin_has_open() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;

    // A cross-site form post carries the cookie but cannot know the form
    // token, which only the rendered consent page contains.
    let body = format!(
        "token=guess&response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&decision=allow",
        urlencoding::encode(CALLBACK)
    );
    let (status, ..) = send(&config, form_post("/authorize", &body, Some(&session))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_code_only_goes_where_the_client_registered() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;

    // Answered on the page, not by redirecting to the attacker's URI.
    let elsewhere = authorize_url(&client_id, "https://evil.test/steal");
    let (status, _, headers) = send(&config, get_as(&elsewhere, &session)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!headers.iter().any(|(k, _)| k == "location"));

    // An unknown client likewise.
    let (status, _, headers) = send(&config, get_as(&authorize_url("nobody", CALLBACK), &session)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!headers.iter().any(|(k, _)| k == "location"));
}

#[tokio::test]
async fn registering_a_plaintext_redirect_off_this_machine_is_refused() {
    let (_dir, config) = public_server();
    let (status, ..) = send(
        &config,
        json_post("/register", r#"{"redirect_uris":["http://attacker.test/cb"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, ..) = send(&config, json_post("/register", r#"{"redirect_uris":[]}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Loopback over plain HTTP is how a program on the person's machine
    // (Claude Code) receives its code, so that one is allowed.
    let (status, ..) = send(
        &config,
        json_post("/register", r#"{"redirect_uris":["http://localhost:3000/cb"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn pkce_is_required_and_a_wrong_verifier_spends_the_code() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;

    // No challenge at all: refused before anyone is asked anything.
    let bare = format!(
        "/authorize?response_type=code&client_id={client_id}&redirect_uri={}",
        urlencoding::encode(CALLBACK)
    );
    let (status, _, headers) = send(&config, get_as(&bare, &session)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(query_param(&location(&headers), "error").as_deref(), Some("invalid_request"));

    // A code, exchanged with the wrong verifier.
    let code = consent(&config, &session, &client_id, CALLBACK).await;
    let (status, body) = exchange(&config, &exchange_body(&client_id, &code, CALLBACK, "not-the-verifier")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // The right verifier no longer helps: the attempt spent it.
    let (status, body) = exchange(&config, &exchange_body(&client_id, &code, CALLBACK, VERIFIER)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_code_is_bound_to_the_client_and_redirect_that_asked_for_it() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;
    let other = register(&config, CALLBACK).await;

    let code = consent(&config, &session, &client_id, CALLBACK).await;
    let (status, ..) = exchange(&config, &exchange_body(&other, &code, CALLBACK, VERIFIER)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "another client redeemed the code");

    let code = consent(&config, &session, &client_id, CALLBACK).await;
    let (status, ..) = exchange(&config, &exchange_body(&client_id, &code, "https://client.test/other", VERIFIER)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a different redirect_uri was accepted");
}

#[tokio::test]
async fn a_code_is_exchanged_once() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;
    let code = consent(&config, &session, &client_id, CALLBACK).await;

    let body = exchange_body(&client_id, &code, CALLBACK, VERIFIER);
    let (status, ..) = exchange(&config, &body).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ..) = exchange(&config, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the code was replayed");
}

#[tokio::test]
async fn a_refresh_token_rotates_and_the_old_one_dies() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;
    let code = consent(&config, &session, &client_id, CALLBACK).await;
    let (_, first) = exchange(&config, &exchange_body(&client_id, &code, CALLBACK, VERIFIER)).await;
    let refresh = first["refresh_token"].as_str().unwrap();

    let body = format!("grant_type=refresh_token&client_id={client_id}&refresh_token={refresh}");
    let (status, second) = exchange(&config, &body).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_ne!(second["access_token"], first["access_token"]);
    assert_ne!(second["refresh_token"], first["refresh_token"]);

    let (status, ..) = exchange(&config, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a retired refresh token still worked");

    let (status, ..) = send(&config, mcp_initialize(second["access_token"].as_str().unwrap())).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn disabling_the_account_ends_its_clients_access_now() {
    let (_dir, config) = public_server();
    admin(&config, "owner@example.com", "correct horse");
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let client_id = register(&config, CALLBACK).await;
    let code = consent(&config, &session, &client_id, CALLBACK).await;
    let (_, tokens) = exchange(&config, &exchange_body(&client_id, &code, CALLBACK, VERIFIER)).await;
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();

    let (status, ..) = send(&config, mcp_initialize(access)).await;
    assert_eq!(status, StatusCode::OK);

    toolsite::accounts::users::set_active(&config, "owner@example.com", false).unwrap();

    let (status, ..) = send(&config, mcp_initialize(access)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a disabled account's token still worked");
    let body = format!("grant_type=refresh_token&client_id={client_id}&refresh_token={refresh}");
    let (status, ..) = exchange(&config, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a disabled account refreshed its way back in");
}

#[tokio::test]
async fn an_issued_token_is_not_a_static_token_and_a_made_up_one_is_nothing() {
    let (_dir, config) = public_server();
    let (status, ..) = send(&config, mcp_initialize("made-up")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, ..) = send(&config, mcp_initialize("")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- an app's files -------------------------------------------------------
//
// Blobs are the app's own, like its database: one namespace per app, keys
// that cannot leave it, and bytes that move between a browser and storage
// without ever passing through the handler.

fn put_bytes(uri: &str, content_type: &str, body: &[u8]) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", content_type)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

#[tokio::test]
async fn a_handler_stores_reads_lists_and_deletes_its_own_files() {
    let (_dir, config) = server();
    publish_handler(&config, "gallery");

    let (status, body, _) = send(
        &config,
        put_bytes("/p/gallery/api/blob-put?key=photos/cat.jpg", "image/jpeg", b"JPEG"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    send(&config, put_bytes("/p/gallery/api/blob-put?key=photos/dog.jpg", "image/jpeg", b"DOG")).await;
    send(&config, put_bytes("/p/gallery/api/blob-put?key=notes.txt", "text/plain", b"hi")).await;

    let (status, body, headers) = send(&config, get("/p/gallery/api/blob-get?key=photos/cat.jpg")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "JPEG");
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "image/jpeg"));

    let (_, body, _) = send(&config, get("/p/gallery/api/blob-stat?key=photos/cat.jpg")).await;
    assert_eq!(body, "4:image/jpeg");

    let (_, body, _) = send(&config, get("/p/gallery/api/blob-list?prefix=photos/")).await;
    assert_eq!(body, "photos/cat.jpg,photos/dog.jpg");
    let (_, body, _) = send(&config, get("/p/gallery/api/blob-list?prefix=")).await;
    assert_eq!(body, "notes.txt,photos/cat.jpg,photos/dog.jpg");

    let (status, ..) = send(&config, get("/p/gallery/api/blob-delete?key=photos/cat.jpg")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ..) = send(&config, get("/p/gallery/api/blob-get?key=photos/cat.jpg")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&config, get("/p/gallery/api/blob-stat?key=photos/cat.jpg")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn one_apps_files_are_not_another_apps() {
    let (_dir, config) = server();
    publish_handler(&config, "alpha");
    publish_handler(&config, "beta");

    send(&config, put_bytes("/p/alpha/api/blob-put?key=private.txt", "text/plain", b"alpha's")).await;
    let (status, ..) = send(&config, get("/p/beta/api/blob-get?key=private.txt")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "beta read alpha's file");
    let (_, body, _) = send(&config, get("/p/beta/api/blob-list?prefix=")).await;
    assert_eq!(body, "", "beta listed alpha's files");
    // Nor by naming the neighbour in the key.
    let (status, ..) = send(&config, get("/p/beta/api/blob-get?key=../alpha/.blobs/data/private.txt")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_file_key_cannot_leave_the_apps_directory() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    write_page(&config, "victim/index", "<h1>victim</h1>");

    for key in ["../victim/index.html", "../../outside", ".secret", "a/../b"] {
        let uri = format!("/p/app/api/blob-put?key={}", urlencoding::encode(key));
        let (status, body, _) = send(&config, put_bytes(&uri, "text/plain", b"pwned")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key} accepted: {body}");
        let uri = format!("/p/app/api/blob-upload-url?key={}", urlencoding::encode(key));
        let (status, ..) = send(&config, get(&uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "upload URL minted for {key}");
    }
    assert_eq!(std::fs::read_to_string(dir.path().join("victim/index.html")).unwrap(), "<h1>victim</h1>");
    assert!(!dir.path().join("outside").exists());
}

#[tokio::test]
async fn a_browser_uploads_straight_to_storage_and_the_handler_serves_it_back() {
    let (_dir, config) = server();
    publish_handler(&config, "drive");

    // The handler hands out a URL; the bytes never reach it.
    let (status, url, _) = send(&config, get("/p/drive/api/blob-upload-url?key=uploads/report.pdf")).await;
    assert_eq!(status, StatusCode::OK, "{url}");
    let path = url.strip_prefix("http://localhost:8080").expect("an absolute upload URL");
    assert!(path.starts_with("/blob/"), "{url}");

    let big = vec![b'x'; 3 * 1024 * 1024];
    let (status, body, _) = send(&config, put_bytes(path, "application/pdf", &big)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The same URL is spent.
    let (status, ..) = send(&config, put_bytes(path, "application/pdf", b"again")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, ..) = send(&config, put_bytes("/blob/made-up", "application/pdf", b"x")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (_, body, _) = send(&config, get("/p/drive/api/blob-stat?key=uploads/report.pdf")).await;
    assert_eq!(body, format!("{}:application/pdf", big.len()));

    // Served by pointing at it: the host streams the bytes, keeps the
    // handler's other headers, and fills in type and length.
    let (status, body, headers) = send(&config, get("/p/drive/api/blob-serve?key=uploads/report.pdf")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), big.len());
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    assert_eq!(header("content-type"), Some("application/pdf"));
    assert_eq!(header("content-length"), Some(big.len().to_string().as_str()));
    assert_eq!(header("content-disposition"), Some("attachment; filename=\"uploads/report.pdf\""));
    assert_eq!(header("x-toolsite-blob"), None, "the pointer leaked to the visitor");

    // Pointing at nothing is a 404, not a crash.
    let (status, ..) = send(&config, get("/p/drive/api/blob-serve?key=uploads/missing.pdf")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_browser_upload_stops_at_the_handlers_limit() {
    let (dir, config) = server();
    publish_handler(&config, "drive");
    let (_, url, _) = send(&config, get("/p/drive/api/blob-upload-url?key=small.bin&max=10")).await;
    let path = url.strip_prefix("http://localhost:8080").unwrap();

    let (status, ..) = send(&config, put_bytes(path, "application/octet-stream", &[0u8; 11])).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, ..) = send(&config, get("/p/drive/api/blob-stat?key=small.bin")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a refused upload was stored anyway");
    assert!(
        std::fs::read_dir(dir.path().join(".tmp")).map(|d| d.count()).unwrap_or(0) == 0,
        "a spool file was left behind"
    );
}

#[tokio::test]
async fn stored_files_are_not_reachable_as_assets_only_through_the_handler() {
    let (dir, config) = server();
    publish_handler(&config, "app");
    send(&config, put_bytes("/p/app/api/blob-put?key=secret.txt", "text/plain", b"shh")).await;
    assert!(dir.path().join("app/.blobs/data/secret.txt").exists());

    for path in ["/p/app/.blobs/data/secret.txt", "/p/app/.blobs/meta/secret.txt", "/p/app/.blobs/"] {
        let (status, ..) = send(&config, get(path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} was served");
    }
}

#[tokio::test]
async fn an_agent_seeds_a_file_through_its_upload_ticket() {
    let (_dir, config) = server();
    publish_handler(&config, "app");
    let ticket = ticket(&config, "app", Duration::from_secs(60));

    let (status, body, _) = send(
        &config,
        put_bytes(&format!("/upload/{ticket}?blob=data/seed.csv"), "text/csv", b"a,b\n1,2\n"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body, headers) = send(&config, get("/p/app/api/blob-get?key=data/seed.csv")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "a,b\n1,2\n");
    // The type came from the key, since this path carries none.
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("text/csv")), "{headers:?}");

    // The ticket is for that app alone, and the key rules still hold.
    let (status, ..) = send(
        &config,
        put_bytes(&format!("/upload/{ticket}?blob=../other/x"), "text/plain", b"x"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- exporting a database -------------------------------------------------
//
// A reporting tool pulls one app's SQLite file with a token that opens that
// app and nothing else. The publish token is never good here, and the file
// is a snapshot, never the live WAL set.

fn bearer_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn seed_db(config: &Config, app: &str) {
    toolsite::runtime::db::run(config, app, "create table orders (id integer, total real)", &[]).unwrap();
    toolsite::runtime::db::run(config, app, "insert into orders values (1, 9.5), (2, 20)", &[]).unwrap();
}

#[tokio::test]
async fn an_export_token_downloads_a_working_copy_of_that_apps_database() {
    let (dir, config) = server();
    seed_db(&config, "sales");
    let (_, token) = toolsite::platform::export::create(&config, "sales", "reporting").unwrap();

    let (status, body, headers) = send_bytes_with_headers(&config, bearer_get("/export/sales.sqlite", &token)).await;
    assert_eq!(status, StatusCode::OK);
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    assert_eq!(header("content-type"), Some("application/vnd.sqlite3"));
    assert_eq!(header("content-length"), Some(body.len().to_string().as_str()));

    // What came down opens as a database and holds the rows.
    let copy = dir.path().join("downloaded.sqlite");
    std::fs::write(&copy, &body).unwrap();
    let conn = rusqlite::Connection::open(&copy).unwrap();
    let n: i64 = conn.query_row("select count(*) from orders", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);

    // The snapshot did not linger.
    let leftovers = std::fs::read_dir(dir.path().join(".tmp")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftovers, 0, "a snapshot file was left behind");
}

/// `send_bytes` plus the headers, which the export test needs both of.
async fn send_bytes_with_headers(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, Vec<u8>, Vec<(String, String)>) {
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
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, bytes.to_vec(), headers)
}

#[tokio::test]
async fn an_export_token_opens_one_app_and_the_publish_token_opens_none() {
    let (_dir, config) = server();
    seed_db(&config, "sales");
    seed_db(&config, "hr");
    let (_, token) = toolsite::platform::export::create(&config, "sales", "x").unwrap();

    let (status, ..) = send(&config, bearer_get("/export/hr.sqlite", &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "sales's token read hr");
    let (status, ..) = send(&config, bearer_get("/export/sales.sqlite", TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the publish token exported a database");
    let (status, ..) = send(&config, get("/export/sales.sqlite")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Unknown app and wrong token read the same, so a token cannot probe
    // for which apps exist.
    let (status, body, _) = send(&config, bearer_get("/export/nothing.sqlite", &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("no export token"));
}

#[tokio::test]
async fn a_revoked_export_token_stops_working_at_once() {
    let (_dir, config) = server();
    seed_db(&config, "sales");
    let (entry, token) = toolsite::platform::export::create(&config, "sales", "x").unwrap();
    let (status, ..) = send(&config, bearer_get("/export/sales.sqlite", &token)).await;
    assert_eq!(status, StatusCode::OK);
    toolsite::platform::export::revoke(&config, "sales", &entry.id).unwrap();
    let (status, ..) = send(&config, bearer_get("/export/sales.sqlite", &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_token_file_is_never_served_and_the_platform_db_is_never_exported() {
    let (_dir, config) = server();
    account(&config, "someone@example.com", "correct horse");
    let (_, token) = toolsite::platform::export::create(&config, "sales", "x").unwrap();

    let (status, ..) = send(&config, get("/p/sales.exports")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for app in [".site", "..", ".site%2Fauth", "sales%2F..%2F.site"] {
        let (status, ..) = send(&config, bearer_get(&format!("/export/{app}.sqlite"), &token)).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::UNAUTHORIZED || status == StatusCode::BAD_REQUEST,
            "{app}: {status}"
        );
    }
}

#[tokio::test]
async fn an_admin_mints_a_token_on_the_exports_page_and_sees_it_once() {
    let (_dir, config) = server();
    seed_db(&config, "sales");
    write_page(&config, "sales/index", "<h1>sales</h1>");
    toolsite::accounts::users::sign_up_as(&config, "owner@example.com", "correct horse", true).unwrap();
    let session = sign_in(&config, "owner@example.com", "correct horse");

    let (status, page, _) = send(&config, get_as("/admin/exports", &session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("No export tokens"));
    // Minting happens on the app's own Exports tab.
    let (status, page, _) = send(&config, get_as("/admin/apps/sales/exports", &session)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let form_token = page
        .split("name=\"token\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();

    let body = format!("token={form_token}&action=create&app=sales&label=reporting");
    let (status, page, _) = send(&config, form_post("/admin/exports", &body, Some(&session))).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let token = page
        .split("id=\"fresh-token\">tse_")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .map(|rest| format!("tse_{rest}"))
        .expect("the new token is shown");
    assert!(page.contains("/export/sales.sqlite"), "the URL to paste is not shown");

    let (status, ..) = send(&config, bearer_get("/export/sales.sqlite", &token)).await;
    assert_eq!(status, StatusCode::OK, "the token from the page does not work");

    // Listed by label, never by value; revocable from the same page.
    let (_, page, _) = send(&config, get_as("/admin/exports", &session)).await;
    assert!(page.contains("reporting"));
    assert!(!page.contains(&token), "the token is shown again on a later visit");
    let id = toolsite::platform::export::list(&config, "sales")[0].id.clone();
    let body = format!("token={form_token}&action=revoke&app=sales&id={id}");
    let (status, ..) = send(&config, form_post("/admin/exports", &body, Some(&session))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, ..) = send(&config, bearer_get("/export/sales.sqlite", &token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A visitor account gets nowhere near it.
    account(&config, "reader@example.com", "correct horse");
    let reader = sign_in(&config, "reader@example.com", "correct horse");
    let (status, ..) = send(&config, get_as("/admin/exports", &reader)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// --- signing in through a provider ----------------------------------------
//
// The provider proves an email; toolsite decides which account that is. These
// run the whole dance against a small OpenID provider in this process, bound
// to 127.0.0.1 on a port nobody else has, with a real RS256 key — so the
// signature, issuer, audience and nonce checks are the real ones.

mod fake_idp {
    use axum::{
        extract::{Form, Query, State},
        response::{IntoResponse, Redirect},
        routing::{get, post},
        Json, Router,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    pub struct Fake {
        pub base: String,
        pub sub: String,
        pub email: String,
        pub email_verified: Option<bool>,
        /// Put this nonce in the token instead of the one the request carried.
        pub wrong_nonce: Option<String>,
        /// Leave the email out of the id token, so userinfo has to answer.
        pub email_in_userinfo_only: bool,
        codes: HashMap<String, String>,
    }

    pub type Shared = Arc<Mutex<Fake>>;

    const KEY_PEM: &[u8] = include_bytes!("fixtures/oidc-test-key.pem");
    const JWKS: &str = include_str!("fixtures/oidc-test-jwks.json");

    pub async fn start() -> (Shared, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let fake: Shared = Arc::new(Mutex::new(Fake {
            base: base.clone(),
            sub: "sub-alice".into(),
            email: "alice@example.com".into(),
            email_verified: Some(true),
            wrong_nonce: None,
            email_in_userinfo_only: false,
            codes: HashMap::new(),
        }));
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(|| async { ([("content-type", "application/json")], JWKS) }))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .route("/userinfo", get(userinfo))
            .with_state(fake.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (fake, base)
    }

    async fn discovery(State(fake): State<Shared>) -> Json<serde_json::Value> {
        let base = fake.lock().unwrap().base.clone();
        Json(serde_json::json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "jwks_uri": format!("{base}/jwks"),
            "userinfo_endpoint": format!("{base}/userinfo"),
        }))
    }

    async fn authorize(
        State(fake): State<Shared>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let code = format!("code-{}", q.get("state").cloned().unwrap_or_default());
        fake.lock()
            .unwrap()
            .codes
            .insert(code.clone(), q.get("nonce").cloned().unwrap_or_default());
        let back = format!(
            "{}?code={code}&state={}",
            q["redirect_uri"],
            q.get("state").cloned().unwrap_or_default()
        );
        Redirect::to(&back).into_response()
    }

    async fn token(
        State(fake): State<Shared>,
        Form(form): Form<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        let (claims, access) = {
            let mut fake = fake.lock().unwrap();
            let Some(nonce) = fake.codes.remove(form.get("code").map(String::as_str).unwrap_or("")) else {
                return Json(serde_json::json!({ "error": "invalid_grant" }));
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let mut claims = serde_json::json!({
                "iss": fake.base,
                "aud": form.get("client_id").cloned().unwrap_or_default(),
                "sub": fake.sub,
                "iat": now,
                "exp": now + 300,
                "nonce": fake.wrong_nonce.clone().unwrap_or(nonce),
            });
            if !fake.email_in_userinfo_only {
                claims["email"] = serde_json::json!(fake.email);
                if let Some(verified) = fake.email_verified {
                    claims["email_verified"] = serde_json::json!(verified);
                }
            }
            (claims, format!("access-{}", fake.sub))
        };
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("test-key-1".into());
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(KEY_PEM).unwrap();
        let id_token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
        Json(serde_json::json!({
            "access_token": access,
            "token_type": "Bearer",
            "id_token": id_token,
        }))
    }

    async fn userinfo(State(fake): State<Shared>) -> Json<serde_json::Value> {
        let fake = fake.lock().unwrap();
        Json(serde_json::json!({
            "sub": fake.sub,
            "email": fake.email,
            "email_verified": fake.email_verified,
        }))
    }
}

/// A deployment with one OIDC provider pointing at the fake, which issues
/// tokens for `allow_domain` if given.
async fn server_with_provider(allow_domain: Option<&str>) -> (TempDir, Arc<Config>, fake_idp::Shared) {
    use toolsite::accounts::providers::{IssuerRule, Kind, Provider};
    let (fake, base) = fake_idp::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        base_url: Some("http://localhost:8080".to_string()),
        providers: vec![Provider {
            slug: "fake".into(),
            name: "Fake IdP".into(),
            kind: Kind::Oidc {
                issuer: base.clone(),
                issuer_claim: IssuerRule::Exactly(base.clone()),
            },
            client_id: "toolsite-test".into(),
            client_secret: "shh".into(),
            allow_domain: allow_domain.map(str::to_string),
        }],
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    (dir, config, fake)
}

/// Does what the browser would: leaves through toolsite, visits the
/// provider, and brings its answer back. Returns the callback's response.
async fn sign_in_through(config: &Arc<Config>, next: &str) -> (StatusCode, String, Vec<(String, String)>) {
    let (status, _, headers) = send(config, get(&format!("/auth/login/fake?next={}", urlencoding::encode(next)))).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "no redirect to the provider");
    let to_provider = location(&headers);
    assert!(to_provider.contains("code_challenge_method=S256"), "{to_provider}");
    assert!(to_provider.contains("nonce="), "{to_provider}");

    // The provider answers with a redirect back to toolsite.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let answer = client.get(&to_provider).send().await.unwrap();
    assert_eq!(answer.status(), reqwest::StatusCode::SEE_OTHER);
    let back = answer.headers()["location"].to_str().unwrap().to_string();
    let path = back.strip_prefix("http://localhost:8080").expect("the provider was given our redirect_uri");
    send(config, get(path)).await
}

fn session_cookie(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(k, v)| k == "set-cookie" && v.starts_with("ts_session="))
        .map(|(_, v)| v.split(';').next().unwrap().trim_start_matches("ts_session=").to_string())
}

#[tokio::test]
async fn the_sign_in_page_offers_each_configured_provider() {
    let (_dir, config, _) = server_with_provider(None).await;
    let (status, page, _) = send(&config, get("/auth/login?next=/admin")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Sign in with Fake IdP"), "{page}");
    assert!(page.contains("/auth/login/fake?next=%2Fadmin"), "next is not carried: {page}");

    let (_dir, plain) = server();
    let (_, page, _) = send(&plain, get("/auth/login")).await;
    assert!(!page.contains("Sign in with"), "a button with no provider behind it");
}

#[tokio::test]
async fn a_provider_login_signs_in_the_account_that_holds_that_email_and_links_it() {
    let (_dir, config, fake) = server_with_provider(None).await;
    account(&config, "alice@example.com", "correct horse");

    let (status, _, headers) = sign_in_through(&config, "/p/somewhere/").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), "/p/somewhere/");
    let token = session_cookie(&headers).expect("no session cookie");
    let (_, me, _) = send(&config, get_as("/auth/me", &token)).await;
    assert!(me.contains("alice@example.com"), "{me}");

    // From now on it is the identity that counts, not the email: the
    // provider renaming the person still lands on the same account.
    fake.lock().unwrap().email = "alice.renamed@example.com".into();
    let (status, _, headers) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let token = session_cookie(&headers).unwrap();
    let (_, me, _) = send(&config, get_as("/auth/me", &token)).await;
    assert!(me.contains("alice@example.com"), "{me}");
}

#[tokio::test]
async fn an_unknown_email_is_refused_and_no_account_appears() {
    let (_dir, config, fake) = server_with_provider(None).await;
    fake.lock().unwrap().email = "stranger@example.com".into();

    let (status, page, headers) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("No account"), "{page}");
    assert!(session_cookie(&headers).is_none(), "a cookie was set for a refused sign-in");
    assert!(matches!(
        toolsite::accounts::users::account_at_email(&config, "stranger@example.com").unwrap(),
        toolsite::accounts::users::AtEmail::Nobody
    ));
}

#[tokio::test]
async fn an_allowed_domain_gets_an_account_made_but_never_an_admin_one() {
    let (_dir, config, fake) = server_with_provider(Some("example.com")).await;
    fake.lock().unwrap().email = "newcomer@example.com".into();

    let (status, _, headers) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let token = session_cookie(&headers).unwrap();
    let (_, me, _) = send(&config, get_as("/auth/me", &token)).await;
    assert!(me.contains("newcomer@example.com"), "{me}");
    let (status, ..) = send(&config, get_as("/admin", &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a provider-made account was an admin");

    // A neighbouring domain is not that domain.
    fake.lock().unwrap().email = "other@notexample.com".into();
    fake.lock().unwrap().sub = "sub-other".into();
    let (status, ..) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_token_for_a_different_sign_in_or_a_forged_callback_is_refused() {
    let (_dir, config, fake) = server_with_provider(None).await;
    account(&config, "alice@example.com", "correct horse");

    fake.lock().unwrap().wrong_nonce = Some("not-this-one".into());
    let (status, _, headers) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "a token with the wrong nonce was accepted");
    assert!(session_cookie(&headers).is_none());
    fake.lock().unwrap().wrong_nonce = None;

    // A callback nobody started.
    let (status, ..) = send(&config, get("/auth/callback/fake?code=whatever&state=made-up")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // And one for a provider that does not exist.
    let (status, ..) = send(&config, get("/auth/callback/nope?code=x&state=y")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&config, get("/auth/login/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_state_is_spent_by_its_first_use() {
    let (_dir, config, _) = server_with_provider(None).await;
    account(&config, "alice@example.com", "correct horse");
    let (status, _, headers) = send(&config, get("/auth/login/fake")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let to_provider = location(&headers);
    let state = query_param(&to_provider, "state").unwrap();
    // Reusing the state without going through the provider: the first try
    // spends it (and fails at the token step, since the code is junk), and
    // the second is already unknown.
    let (first, ..) = send(&config, get(&format!("/auth/callback/fake?code=junk&state={state}"))).await;
    assert_eq!(first, StatusCode::BAD_GATEWAY);
    let (second, ..) = send(&config, get(&format!("/auth/callback/fake?code=junk&state={state}"))).await;
    assert_eq!(second, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_disabled_account_cannot_come_in_through_a_provider_either() {
    let (_dir, config, _) = server_with_provider(Some("example.com")).await;
    account(&config, "alice@example.com", "correct horse");
    toolsite::accounts::users::set_active(&config, "alice@example.com", false).unwrap();

    let (status, page, headers) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("disabled"), "{page}");
    assert!(session_cookie(&headers).is_none());
}

#[tokio::test]
async fn an_email_the_provider_will_not_vouch_for_is_refused() {
    let (_dir, config, fake) = server_with_provider(Some("example.com")).await;
    fake.lock().unwrap().email_verified = Some(false);
    let (status, ..) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(matches!(
        toolsite::accounts::users::account_at_email(&config, "alice@example.com").unwrap(),
        toolsite::accounts::users::AtEmail::Nobody
    ));
}

#[tokio::test]
async fn an_email_missing_from_the_token_is_fetched_from_userinfo() {
    let (_dir, config, fake) = server_with_provider(None).await;
    account(&config, "alice@example.com", "correct horse");
    fake.lock().unwrap().email_in_userinfo_only = true;
    let (status, _, headers) = sign_in_through(&config, "/").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(session_cookie(&headers).is_some());
}

// --- the app page -----------------------------------------------------------

/// Follows one redirect the way a browser would: carries the Set-Cookie it
/// came with (the flash) into the next request alongside the session.
async fn follow(config: &Arc<Config>, session: &str, headers: &[(String, String)]) -> (StatusCode, String) {
    let location = headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone()).expect("no redirect");
    let flash = headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.split(';').next().unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let request = Request::builder()
        .uri(&location)
        .header("cookie", format!("ts_session={session}; {flash}"))
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = send(config, request).await;
    (status, body)
}

#[tokio::test]
async fn an_apps_page_is_for_admins_and_only_for_apps_that_exist() {
    let (_dir, config) = server();
    write_page(&config, "reports/index", "<title>Reports</title>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    account(&config, "reader@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let reader = sign_in(&config, "reader@example.com", "correct horse battery");

    let (status, page, _) = send(&config, get_as("/admin/apps/reports", &boss)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Reports"), "the title is not shown");
    for tab in ["access", "exports", "settings", "jobs", "notes"] {
        let (status, ..) = send(&config, get_as(&format!("/admin/apps/reports/{tab}"), &boss)).await;
        assert_eq!(status, StatusCode::OK, "{tab}");
    }
    let (status, ..) = send(&config, get_as("/admin/apps/reports/bogus", &boss)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&config, get_as("/admin/apps/nothing", &boss)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&config, get_as("/admin/apps/..%2F.site", &boss)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&config, get_as("/admin/apps/reports", &reader)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn saving_a_gate_returns_to_the_tab_with_the_outcome_shown_once() {
    let (_dir, config) = server();
    write_page(&config, "reports/index", "<h1>r</h1>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/admin/apps/reports/access", &boss)).await;
    let token = form_token_from(&page);

    let (status, _, headers) = send(
        &config,
        post_form(
            "/admin/gate",
            &boss,
            format!("token={token}&app=reports&gate=granted&back=/admin/apps/reports/access"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, page) = follow(&config, &boss, &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Access for reports is granted"), "no flash: {page}");
    assert!(page.contains(r#"value="granted" checked"#), "the radio does not reflect the save");
    // Shown once: the next visit is quiet.
    let (_, page, _) = send(&config, get_as("/admin/apps/reports/access", &boss)).await;
    assert!(!page.contains("Access for reports is granted"));

    // A `back` that is not one of ours is ignored, not followed.
    let (_, _, headers) = send(
        &config,
        post_form("/admin/gate", &boss, format!("token={token}&app=reports&gate=public&back=https://evil.test/")),
    )
    .await;
    let location = headers.iter().find(|(k, _)| k == "location").unwrap().1.clone();
    assert!(location.starts_with("/admin"), "{location}");
}

#[tokio::test]
async fn route_rules_are_added_and_removed_from_the_access_tab() {
    let (_dir, config) = server();
    write_page(&config, "reports/index", "<h1>r</h1>");
    write_page(&config, "reports/admin/index", "<h1>secret</h1>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/admin/apps/reports/access", &boss)).await;
    let token = form_token_from(&page);

    let (status, ..) = send(
        &config,
        post_form("/admin/rule", &boss, format!("token={token}&app=reports&action=add&prefix=/admin&gate=authenticated")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    // The rule does what the page says it does.
    let (status, ..) = send(&config, get("/p/reports/")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ..) = send(&config, get("/p/reports/admin/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "the rule did not gate the corner");

    let (_, page, _) = send(&config, get_as("/admin/apps/reports/access", &boss)).await;
    assert!(page.contains("/admin</code>"), "the rule is not listed");
    let (status, ..) = send(
        &config,
        post_form("/admin/rule", &boss, format!("token={token}&app=reports&action=remove&prefix=/admin")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, ..) = send(&config, get("/p/reports/admin/")).await;
    assert_eq!(status, StatusCode::OK);

    // A prefix that is not a path within the app is refused.
    let (_, _, headers) = send(
        &config,
        post_form("/admin/rule", &boss, format!("token={token}&app=reports&action=add&prefix=../x&gate=public")),
    )
    .await;
    assert!(headers.iter().any(|(k, v)| k == "set-cookie" && v.contains("ts_flash=error")));
}

// --- the site default for access ---------------------------------------------

#[tokio::test]
async fn an_app_that_names_no_access_follows_the_site_default() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        default_gate: "granted".to_string(),
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    write_page(&config, "internal/index", "<title>Internal</title>");
    write_page(&config, "brochure/index", "<title>Brochure</title>");
    std::fs::write(
        dir.path().join("brochure.meta"),
        r#"{"listed":true,"hidden":false,"spa":false,"gate":"public","allow_http":[],"rules":[]}"#,
    )
    .unwrap();

    // Nothing said: closed, as the site asked.
    let (status, ..) = send(&config, get("/p/internal/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "an unconfigured app was open on a granted-by-default site");
    // Said public: open.
    let (status, ..) = send(&config, get("/p/brochure/")).await;
    assert_eq!(status, StatusCode::OK);
    // And the index agrees.
    let (_, index, _) = send(&config, get("/")).await;
    assert!(index.contains("Brochure"));
    assert!(!index.contains("Internal"), "a closed app was listed to a stranger");

    // The same files on an open-by-default site are open.
    let open = Arc::new(Config::local(dir.path().to_path_buf(), TOKEN));
    let (status, ..) = send(&open, get("/p/internal/")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_admin_can_put_an_app_back_on_the_site_default() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        default_gate: "authenticated".to_string(),
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    write_page(&config, "reports/index", "<h1>r</h1>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/admin/apps/reports/access", &boss)).await;
    let token = form_token_from(&page);
    assert!(page.contains(r#"value="default" checked"#), "a fresh app is not shown on the default");

    send(&config, post_form("/admin/gate", &boss, format!("token={token}&app=reports&gate=public"))).await;
    let (status, ..) = send(&config, get("/p/reports/")).await;
    assert_eq!(status, StatusCode::OK);
    let (_, page, _) = send(&config, get_as("/admin/apps", &boss)).await;
    assert!(!page.contains("site default"), "an app with its own setting was marked as following the default");

    send(&config, post_form("/admin/gate", &boss, format!("token={token}&app=reports&gate=default"))).await;
    let (status, ..) = send(&config, get("/p/reports/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "back on the default, the app should be closed again");
    // Wherever the sidecar landed, it names no gate.
    let meta = std::fs::read_to_string(dir.path().join("reports.meta"))
        .or_else(|_| std::fs::read_to_string(dir.path().join("reports/index.meta")))
        .unwrap();
    assert!(!meta.contains("gate"), "following the default should leave no gate in the file: {meta}");
    let (_, page, _) = send(&config, get_as("/admin/apps", &boss)).await;
    assert!(page.contains("site default"));
}

// --- the second pass on the admin pages ----------------------------------------

#[tokio::test]
async fn the_index_is_called_apps_and_offers_cards_or_a_list() {
    let (_dir, config) = server();
    write_page(&config, "one", "<title>One</title>");
    let (status, page, _) = send(&config, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<h1>Apps</h1>"), "the index is not titled Apps");
    assert!(page.contains("1 app"), "{page}");
    assert!(page.contains(r#"data-view="cards""#) && page.contains(r#"data-view="list""#), "no view control");
    assert!(!page.contains("Pages"), "the old name is still on the page");
}

#[tokio::test]
async fn the_apps_list_pages_and_searches_instead_of_dumping_everything() {
    let (_dir, config) = server();
    for n in 0..60 {
        write_page(&config, &format!("app-{n:02}/index"), &format!("<title>Number {n}</title>"));
    }
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");

    let (status, page, _) = send(&config, get_as("/admin/apps", &boss)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.matches("data-slug=\"").count(), 50, "the first page is not capped at 50");
    assert!(page.contains("Showing 1–50 of 60"), "{page}");
    assert!(page.contains("page=2"), "no way to the second page");

    let (_, page, _) = send(&config, get_as("/admin/apps?page=2", &boss)).await;
    assert_eq!(page.matches("data-slug=\"").count(), 10);
    assert!(page.contains("Showing 51–60 of 60"));

    // The server-side search finds by slug and by title, across pages.
    let (_, page, _) = send(&config, get_as("/admin/apps?q=app-57", &boss)).await;
    assert_eq!(page.matches("data-slug=\"").count(), 1);
    let (_, page, _) = send(&config, get_as("/admin/apps?q=number+3", &boss)).await;
    assert_eq!(page.matches("data-slug=\"").count(), 11, "Number 3 and 30-39");
    let (_, page, _) = send(&config, get_as("/admin/apps?q=zzz", &boss)).await;
    assert!(page.contains("No match"));
}

#[tokio::test]
async fn an_account_page_shows_grants_and_lets_an_admin_change_them() {
    let (_dir, config) = server();
    write_page(&config, "reports/index", "<title>Reports</title>");
    write_page(&config, "board/index", "<title>Board</title>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    account(&config, "reader@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let reader = sign_in(&config, "reader@example.com", "correct horse battery");

    let (status, ..) = send(&config, get_as("/admin/accounts/reader%40example.com", &reader)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ..) = send(&config, get_as("/admin/accounts/nobody%40example.com", &boss)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, page, _) = send(&config, get_as("/admin/accounts/reader%40example.com", &boss)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("No grants"));
    assert!(page.contains(r#"data-search="/admin/apps/search""#), "the app picker is not a combobox");
    assert!(page.contains(r#"role="combobox""#), "the picker is not a real combobox");
    assert!(!page.contains("<datalist"), "a native datalist is still in use");
    assert!(!page.contains("<select"), "a select lists every app");
    let token = form_token_from(&page);

    // Add from the account page, with a role, and come back to it.
    let back = "/admin/accounts/reader%40example.com";
    let (status, _, headers) = send(
        &config,
        post_form("/admin/access", &boss, format!("token={token}&app=reports&email=reader@example.com&allow=1&role=editor&back={back}")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, page) = follow(&config, &boss, &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("reports") && page.contains("editor"), "the grant is not listed with its role: {page}");
    let who = toolsite::accounts::users::log_in(&config, "reader@example.com", "correct horse battery").unwrap().0;
    assert_eq!(toolsite::accounts::users::role_for(&config, &who.id, "reports").as_deref(), Some("editor"));

    // A role is one word.
    let (_, _, headers) = send(
        &config,
        post_form("/admin/access", &boss, format!("token={token}&app=board&email=reader@example.com&allow=1&role=drop+table&back={back}")),
    )
    .await;
    assert!(headers.iter().any(|(k, v)| k == "set-cookie" && v.contains("ts_flash=error")));
    assert!(!toolsite::accounts::users::has_grant(&config, &who, "board"));

    // Revoke from the same page.
    send(
        &config,
        post_form("/admin/access", &boss, format!("token={token}&app=reports&email=reader@example.com&allow=0&back={back}")),
    )
    .await;
    assert!(!toolsite::accounts::users::has_grant(&config, &who, "reports"));

    // A fresh setup link, shown once and usable.
    let (status, page, _) = send(
        &config,
        post_form("/admin/reinvite", &boss, format!("token={token}&email=reader@example.com&back={back}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let link = page
        .split("id=\"setup-link\">")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("no setup link shown");
    assert!(link.contains("/auth/setup?token="), "{link}");
    let (_, page, _) = send(&config, get_as("/admin/accounts/reader%40example.com", &boss)).await;
    assert!(!page.contains("/auth/setup?token="), "the link is shown again on a later visit");
}

#[tokio::test]
async fn the_pickers_search_the_server_and_never_list_everyone() {
    let (_dir, config) = server();
    write_page(&config, "reports/index", "<title>Quarterly Numbers</title>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    for n in 0..15 {
        account(&config, &format!("person{n:02}@example.com"), "correct horse battery");
    }
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let reader = sign_in(&config, "person00@example.com", "correct horse battery");

    // Admin only.
    let (status, ..) = send(&config, get("/admin/accounts/search?q=person")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, ..) = send(&config, get_as("/admin/accounts/search?q=person", &reader)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ..) = send(&config, get_as("/admin/apps/search?q=rep", &reader)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Capped at ten, nothing for nothing.
    let (status, body, _) = send(&config, get_as("/admin/accounts/search?q=person", &boss)).await;
    assert_eq!(status, StatusCode::OK);
    let found: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(found.len(), 10, "{body}");
    let (_, body, _) = send(&config, get_as("/admin/accounts/search?q=", &boss)).await;
    assert_eq!(body, "[]");
    let (_, body, _) = send(&config, get_as("/admin/accounts/search?q=person1", &boss)).await;
    let found: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(found.len(), 5);

    // Apps match by slug or title.
    let (_, body, _) = send(&config, get_as("/admin/apps/search?q=quarterly", &boss)).await;
    assert!(body.contains(r#""value":"reports""#), "{body}");
    let (_, body, _) = send(&config, get_as("/admin/apps/search?q=", &boss)).await;
    assert_eq!(body, "[]");

    // The Access tab carries the picker, not the directory.
    let (_, page, _) = send(&config, get_as("/admin/apps/reports/access", &boss)).await;
    assert!(page.contains("Add an account"));
    assert!(page.contains(r#"data-search="/admin/accounts/search""#));
    for n in 0..15 {
        assert!(!page.contains(&format!("person{n:02}@example.com")), "the page lists every account");
    }
    assert!(!page.contains("<select name=\"email\""));
}

#[tokio::test]
async fn the_global_access_page_is_gone_but_its_action_remains() {
    let (_dir, config) = server();
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (status, ..) = send(&config, get_as("/admin/access", &boss)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let (_, page, _) = send(&config, get_as("/admin/apps", &boss)).await;
    assert!(!page.contains(r#"href="/admin/access""#), "the sidebar still links to it");
}

#[tokio::test]
async fn an_admin_opens_a_granted_app_without_a_grant_and_a_visitor_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        default_gate: "granted".to_string(),
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    write_page(&config, "internal/index", "<title>Internal</title>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    account(&config, "reader@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let reader = sign_in(&config, "reader@example.com", "correct horse battery");

    let (_, index, _) = send(&config, get_as("/", &boss)).await;
    assert!(index.contains("Internal"), "the admin's index hid an app they can manage");
    let (boss_app, _) = hand_off(&config, &boss, "internal").await;
    let (status, ..) = send(&config, get_as_app("/p/internal/", "internal", &boss_app)).await;
    assert_eq!(status, StatusCode::OK, "the admin was kept out of a granted app");

    let (_, index, _) = send(&config, get_as("/", &reader)).await;
    assert!(!index.contains("Internal"), "a visitor without a grant saw the app");
    let (status, ..) = send(&config, get_as("/p/internal/", &reader)).await;
    assert_ne!(status, StatusCode::OK, "a visitor without a grant got in");
}

// --- a person's own account ----------------------------------------------
//
// Any signed-in account can see how it signs in and, if it has a password,
// change it. The reset path for a forgotten one is an admin's setup link.

fn account_form(token: &str, current: &str, new: &str, confirm: &str) -> String {
    format!(
        "token={token}&current={}&new={}&confirm={}",
        urlencoding::encode(current),
        urlencoding::encode(new),
        urlencoding::encode(confirm),
    )
}

#[tokio::test]
async fn the_account_page_is_for_the_signed_in_person_only() {
    let (_dir, config) = server();
    let (status, _, headers) = send(&config, get("/account")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location(&headers).starts_with("/auth/login?next="));

    account(&config, "me@example.com", "correct horse battery");
    let me = sign_in(&config, "me@example.com", "correct horse battery");
    let (status, page, _) = send(&config, get_as("/account", &me)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("me@example.com"));
    assert!(page.contains("Change password"), "a password account got no form");
    assert!(page.contains("A password"), "the page does not say how they sign in");
}

#[tokio::test]
async fn a_person_changes_their_own_password_and_the_old_one_stops_working() {
    let (_dir, config) = server();
    account(&config, "me@example.com", "correct horse battery");
    let me = sign_in(&config, "me@example.com", "correct horse battery");
    // A second session: a phone, say. It should not survive the change.
    let phone = sign_in(&config, "me@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/account", &me)).await;
    let token = form_token_from(&page);

    let body = account_form(&token, "correct horse battery", "a brand new secret", "a brand new secret");
    let (status, _, headers) = send(&config, post_form("/account/password", &me, body)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, page) = follow(&config, &me, &headers).await;
    assert_eq!(status, StatusCode::OK, "the session that changed the password was signed out");
    assert!(page.contains("The password is changed"), "no confirmation shown");

    assert!(
        toolsite::accounts::users::log_in(&config, "me@example.com", "correct horse battery").is_err(),
        "the old password still works"
    );
    assert!(toolsite::accounts::users::log_in(&config, "me@example.com", "a brand new secret").is_ok());
    let (status, ..) = send(&config, get_as("/account", &phone)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "another session survived the change");
}

#[tokio::test]
async fn a_wrong_current_password_or_a_mismatched_confirmation_changes_nothing() {
    let (_dir, config) = server();
    account(&config, "me@example.com", "correct horse battery");
    let me = sign_in(&config, "me@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/account", &me)).await;
    let token = form_token_from(&page);

    for (body, why) in [
        (account_form(&token, "not it", "a brand new secret", "a brand new secret"), "wrong current"),
        (account_form(&token, "correct horse battery", "a brand new secret", "a different secret"), "mismatch"),
        (account_form(&token, "correct horse battery", "short", "short"), "too short"),
    ] {
        let (status, _, headers) = send(&config, post_form("/account/password", &me, body)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{why}");
        assert!(
            headers.iter().any(|(k, v)| k == "set-cookie" && v.contains("ts_flash=error")),
            "{why}: no error was reported"
        );
        assert!(
            toolsite::accounts::users::log_in(&config, "me@example.com", "correct horse battery").is_ok(),
            "{why}: the password changed anyway"
        );
    }

    // Without the form token, nothing happens at all.
    let (status, ..) = send(
        &config,
        post_form("/account/password", &me, account_form("guess", "correct horse battery", "a brand new secret", "a brand new secret")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_provider_only_account_has_no_password_to_change() {
    let (_dir, config) = server();
    let user = toolsite::accounts::users::create_provider_account(&config, "sso@example.com").unwrap();
    toolsite::accounts::users::link_identity(&config, "google", "sub-123", &user.id).unwrap();
    let session = toolsite::accounts::users::start_session(&config, &user.id).unwrap();

    let (status, page, _) = send(&config, get_as("/account", &session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("Change password"), "a provider account was offered a password form");
    assert!(page.contains("google"), "the provider is not named");

    let token = toolsite::accounts::users::derive_form_token(&config, &user.id);
    let (status, _, headers) = send(
        &config,
        post_form("/account/password", &session, account_form(&token, "anything", "a brand new secret", "a brand new secret")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(headers.iter().any(|(k, v)| k == "set-cookie" && v.contains("ts_flash=error")));
}

#[tokio::test]
async fn the_sign_in_page_says_where_a_forgotten_password_goes() {
    let (_dir, config) = server();
    let (_, page, _) = send(&config, get("/auth/login")).await;
    assert!(page.contains("ask an admin for a setup link"));
}


// --- an app's source, from the admin -----------------------------------------

#[tokio::test]
async fn an_admin_downloads_the_stored_source_and_nobody_else_can() {
    let (dir, config) = server();
    write_page(&config, "reports/index", "<h1>r</h1>");
    let archive: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.path().join("reports.source"), &archive).unwrap();
    admin_account(&config, "boss@example.com", "correct horse battery");
    account(&config, "reader@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let reader = sign_in(&config, "reader@example.com", "correct horse battery");

    // The overview offers it.
    let (_, page, _) = send(&config, get_as("/admin/apps/reports", &boss)).await;
    assert!(page.contains("/admin/apps/reports/source"), "no download offered");

    let (status, body, headers) =
        send_bytes_with_headers(&config, get_as("/admin/apps/reports/source", &boss)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, archive, "the bytes differ from the stored archive");
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    assert_eq!(header("content-disposition"), Some("attachment; filename=\"reports-source.tar.gz\""));
    assert_eq!(header("cache-control"), Some("no-store"));

    // A visitor, a stranger, and the public site all get nothing.
    let (status, ..) = send(&config, get_as("/admin/apps/reports/source", &reader)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ..) = send(&config, get("/admin/apps/reports/source")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, ..) = send(&config, get("/p/reports.source")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the archive was served publicly");

    // An app with no source says so, with 404 on the download.
    write_page(&config, "bare/index", "<h1>b</h1>");
    let (status, ..) = send(&config, get_as("/admin/apps/bare/source", &boss)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, page, _) = send(&config, get_as("/admin/apps/bare", &boss)).await;
    assert!(page.contains("no source archive"));
}

// --- declared roles are a hint ---------------------------------------------------

#[tokio::test]
async fn roles_an_app_declares_are_offered_when_granting_but_never_required() {
    let (_dir, config) = server();
    write_page(&config, "board/index", "<h1>b</h1>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    account(&config, "reader@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");

    // Before the manifest says anything: a plain field, viewer by default.
    let (_, page, _) = send(&config, get_as("/admin/apps/board/access", &boss)).await;
    assert!(!page.contains("role-hints"));
    assert!(page.contains(r#"name="role" value="viewer""#));

    let changed = toolsite::platform::manifest::apply(&config, "board", "roles = [\"editor\", \"approver\"]\n")
        .await
        .unwrap();
    assert!(changed.iter().any(|c| c.contains("roles")), "{changed:?}");
    let meta = toolsite::content::store::read_meta(&config, "board").await;
    assert_eq!(meta.roles, ["editor", "approver"]);

    // Offered on the app's Access tab and on the account page.
    let (_, page, _) = send(&config, get_as("/admin/apps/board/access", &boss)).await;
    assert!(page.contains(r#"<option value="approver">"#), "declared roles are not offered");
    assert!(page.contains(r#"name="role" value="editor""#), "the first declared role is not the default");
    let (_, page, _) = send(&config, get_as("/admin/accounts/reader@example.com", &boss)).await;
    assert!(page.contains(r#"<option value="editor">"#));

    // Still only a hint: an undeclared role is granted without complaint.
    let (_, page, _) = send(&config, get_as("/admin/apps/board/access", &boss)).await;
    let token = form_token_from(&page);
    let (status, ..) = send(
        &config,
        post_form("/admin/access", &boss, format!("token={token}&app=board&email=reader@example.com&allow=1&role=auditor")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        toolsite::accounts::users::role_for(&config, &account_id(&config, "reader@example.com"), "board").as_deref(),
        Some("auditor")
    );
    let (_, page, _) = send(&config, get_as("/admin/apps/board/access", &boss)).await;
    assert!(page.contains("auditor"));
    assert!(!page.contains("badge warn"), "an undeclared role was flagged");

    // Withdrawing the hint leaves the grant alone.
    toolsite::platform::manifest::apply(&config, "board", "roles = []\n").await.unwrap();
    assert!(toolsite::content::store::read_meta(&config, "board").await.roles.is_empty());
    assert_eq!(
        toolsite::accounts::users::role_for(&config, &account_id(&config, "reader@example.com"), "board").as_deref(),
        Some("auditor")
    );
}

fn account_id(config: &Config, email: &str) -> String {
    toolsite::accounts::users::log_in(config, email, "correct horse battery").unwrap().0.id
}

#[tokio::test]
async fn a_new_export_token_comes_with_the_pull_ready_to_paste() {
    let (_dir, config) = server();
    seed_db(&config, "sales");
    write_page(&config, "sales/index", "<h1>sales</h1>");
    toolsite::accounts::users::sign_up_as(&config, "owner@example.com", "correct horse", true).unwrap();
    let session = sign_in(&config, "owner@example.com", "correct horse");
    let (_, page, _) = send(&config, get_as("/admin/apps/sales/exports", &session)).await;
    let form_token = form_token_from(&page);
    let body = format!("token={form_token}&action=create&app=sales&label=reporting");
    let (status, page, _) = send(&config, form_post("/admin/exports", &body, Some(&session))).await;
    assert_eq!(status, StatusCode::OK);
    let token = page
        .split("id=\"fresh-token\">tse_")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .map(|rest| format!("tse_{rest}"))
        .expect("the token");
    // maud escapes the quotes; what matters is that the token and the URL
    // sit in the one copyable line.
    let line = page
        .split(r#"id="fresh-token-curl">"#)
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("a ready-to-paste pull on the page");
    assert!(line.starts_with("curl -H "), "{line}");
    assert!(line.contains(&format!("Authorization: Bearer {token}")), "{line}");
    assert!(line.ends_with("-o sales.sqlite http://localhost:8080/export/sales.sqlite"), "{line}");
    assert!(page.contains(r#"data-copy="fresh-token-curl""#), "the pull has no copy button");
}

#[tokio::test]
async fn an_admin_creates_an_account_with_a_setup_link_the_owner_uses_once() {
    let (_dir, config) = server();
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/admin/accounts/new", &boss)).await;
    let token = form_token_from(&page);
    assert!(page.contains(r#"value="invite" checked"#), "the setup link is not the default");

    let (status, page, _) = send(
        &config,
        post_form("/admin/users", &boss, format!("token={token}&email=new@example.com&mode=invite")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let link = page
        .split("/auth/setup?token=")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("no setup link on the page");
    let setup = link.trim().to_string();
    assert!(toolsite::accounts::users::invited_account(&config, &setup).is_some(), "the link does not open the account");
    // No password yet: nobody can sign in as them until they choose one.
    assert!(toolsite::accounts::users::log_in(&config, "new@example.com", "anything-at-all").is_err());

    // The shared-login path still sets a password directly.
    let (status, ..) = send(
        &config,
        post_form("/admin/users", &boss, format!("token={token}&email=shared@example.com&mode=password&password=correct+horse+battery")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(toolsite::accounts::users::log_in(&config, "shared@example.com", "correct horse battery").is_ok());
    // A generated password is shown one time and works.
    let (status, page, _) = send(
        &config,
        post_form("/admin/users", &boss, format!("token={token}&email=kiosk@example.com&mode=generate")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let generated = page
        .split(r#"id="generated-password">"#)
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("no generated password shown")
        .to_string();
    assert_eq!(generated.len(), 20);
    assert!(generated.chars().all(|c| c.is_ascii_alphanumeric()));
    assert!(toolsite::accounts::users::log_in(&config, "kiosk@example.com", &generated).is_ok());
    // Asking for a password without giving one is refused, not silently invited.
    let (_, _, headers) = send(
        &config,
        post_form("/admin/users", &boss, format!("token={token}&email=third@example.com&mode=password")),
    )
    .await;
    assert!(headers.iter().any(|(k, v)| k == "set-cookie" && v.contains("ts_flash=error")));
}

// --- projects and scopes ---------------------------------------------------
//
// Apps sit in folders; a scope says what an account may do from a folder
// down; an editor's Claude publishes where the editor may and nowhere else.

/// One MCP session over the real router: the same router for every call,
/// since the session lives in it.
struct Mcp {
    router: axum::Router,
    token: String,
    session: String,
    next_id: u64,
}

fn mcp_data(body: &str) -> serde_json::Value {
    let data: Vec<&str> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect();
    let raw = data.last().copied().unwrap_or(body.trim());
    serde_json::from_str(raw).unwrap_or_else(|_| panic!("not JSON-RPC: {body}"))
}

impl Mcp {
    async fn open(config: &Arc<Config>, token: &str) -> Mcp {
        let router = build_router(config.clone(), Runtime::new().unwrap());
        let initialize = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("host", "localhost")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(initialize))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "initialize refused");
        let session = response
            .headers()
            .get("mcp-session-id")
            .expect("no session id")
            .to_str()
            .unwrap()
            .to_string();
        let mut mcp = Mcp { router, token: token.to_string(), session, next_id: 1 };
        mcp.post(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        mcp
    }

    async fn post(&mut self, body: &str) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("host", "localhost")
                    .header("authorization", format!("Bearer {}", self.token))
                    .header("mcp-session-id", &self.session)
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// Calls a tool and returns (is_error, the text of its first content block).
    async fn call(&mut self, name: &str, arguments: serde_json::Value) -> (bool, String) {
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
        let (status, text) = self.post(&body.to_string()).await;
        assert_eq!(status, StatusCode::OK, "{name}: {text}");
        let reply = mcp_data(&text);
        let result = &reply["result"];
        assert!(!result.is_null(), "{name} answered with no result: {reply}");
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
        (is_error, text)
    }
}

/// A site that knows its address (so sign-in works) and also takes a static
/// token (so a test can act as the platform itself).
fn scoped_site() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        data_dir: dir.path().to_path_buf(),
        base_url: Some(BASE.to_string()),
        local_base: "http://localhost:8080".to_string(),
        valid_tokens: vec![TOKEN.to_string()],
        uploads: std::sync::Mutex::new(std::collections::HashMap::new()),
        ..Config::local(dir.path().to_path_buf(), "unused")
    });
    (dir, config)
}

async fn folder(config: &Config, parent: &str, name: &str) {
    toolsite::content::store::create_folder(config, parent, name).await.unwrap();
}

/// An app that already exists in a folder, closed to strangers.
async fn app_in(config: &Config, app: &str, in_folder: &str) {
    write_page(config, &format!("{app}/index"), &format!("<title>{app}</title>"));
    let meta = toolsite::content::store::PageMeta {
        project: Some(in_folder.to_string()),
        gate: Some("granted".to_string()),
        ..Default::default()
    };
    toolsite::content::store::write_meta(config, app, &meta).await.unwrap();
}

fn scope(config: &Config, email: &str, prefix: &str, scope: &str) {
    let scope = toolsite::accounts::users::Scope::parse(scope).unwrap();
    toolsite::accounts::users::grant_scope(config, email, prefix, scope, None).unwrap();
}

/// Signs the account's Claude in: register, consent, exchange.
async fn mcp_token_for(config: &Arc<Config>, email: &str, password: &str) -> String {
    let session = sign_in(config, email, password);
    let client_id = register(config, CALLBACK).await;
    let code = consent(config, &session, &client_id, CALLBACK).await;
    let (status, tokens) = exchange(config, &exchange_body(&client_id, &code, CALLBACK, VERIFIER)).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    tokens["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn an_editor_publishes_in_its_folder_and_is_refused_everywhere_else() {
    let (_dir, config) = scoped_site();
    folder(&config, "", "ops").await;
    folder(&config, "ops", "warehouse").await;
    folder(&config, "", "finance").await;
    app_in(&config, "ledger", "finance").await;
    account(&config, "ed@example.com", "correct horse");
    scope(&config, "ed@example.com", "ops/warehouse", "editor");
    let token = mcp_token_for(&config, "ed@example.com", "correct horse").await;
    let mut mcp = Mcp::open(&config, &token).await;

    // A new app with no folder named lands in the one folder ed holds.
    let (is_error, text) = mcp.call("create_upload", serde_json::json!({ "slug": "wh-tool" })).await;
    assert!(!is_error, "{text}");
    let upload = text.lines().find_map(|line| line.split_whitespace().find(|w| w.contains("/upload/"))).expect("no upload url");
    let path = upload.trim_end_matches('\'').strip_prefix(BASE).unwrap().to_string();
    let (status, body, _) = send(&config, put_bytes(&path, "text/html", b"<title>WH</title>")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let meta = toolsite::content::store::read_meta(&config, "wh-tool").await;
    assert_eq!(meta.project.as_deref(), Some("ops/warehouse"), "the new app did not land in the editor's folder");
    assert!(meta.created_by.is_some(), "the creator was not recorded");

    // Naming another folder is refused with the folder and the scope.
    let (is_error, text) = mcp.call("create_upload", serde_json::json!({ "slug": "fin-tool", "project": "finance" })).await;
    assert!(is_error, "{text}");
    assert!(text.contains("finance") && text.contains("editor"), "{text}");

    // An app that exists elsewhere is out of reach, and the refusal says where.
    let (is_error, text) = mcp.call("run_sql", serde_json::json!({ "app": "ledger", "sql": "select 1" })).await;
    assert!(is_error, "{text}");
    assert!(text.contains("finance/ledger") && text.contains("editor"), "{text}");
    let (is_error, text) = mcp.call("app_notes", serde_json::json!({ "slug": "ledger" })).await;
    assert!(is_error, "{text}");

    // Setting access on its own app needs admin, which it is not.
    let (is_error, text) = mcp.call("set_visibility", serde_json::json!({ "slug": "wh-tool", "gate": "public" })).await;
    assert!(is_error, "{text}");
    assert!(text.contains("admin"), "{text}");
    // Hiding it is an editor's.
    let (is_error, text) = mcp.call("set_visibility", serde_json::json!({ "slug": "wh-tool", "listed": false })).await;
    assert!(!is_error, "{text}");

    // Site-wide tools are the site admin's.
    let (is_error, text) = mcp.call("create_user", serde_json::json!({ "email": "x@example.com" })).await;
    assert!(is_error, "{text}");
    assert!(text.contains("site admin"), "{text}");

    // The list is what ed may open: its app, not the ledger.
    let (_, text) = mcp.call("list_pages", serde_json::json!({ "include_all": true })).await;
    assert!(text.contains("wh-tool"), "{text}");
    assert!(!text.contains("ledger"), "ed saw an app it cannot open: {text}");
}

#[tokio::test]
async fn several_scopes_on_one_account_each_apply_in_their_own_folder() {
    let (_dir, config) = scoped_site();
    folder(&config, "", "ops").await;
    folder(&config, "ops", "warehouse").await;
    folder(&config, "", "finance").await;
    folder(&config, "finance", "reports").await;
    app_in(&config, "q3", "finance/reports").await;
    app_in(&config, "ledger", "finance").await;
    account(&config, "bo@example.com", "correct horse");
    scope(&config, "bo@example.com", "ops/warehouse", "editor");
    scope(&config, "bo@example.com", "finance/reports", "viewer");
    let token = mcp_token_for(&config, "bo@example.com", "correct horse").await;
    let mut mcp = Mcp::open(&config, &token).await;

    // Viewer lists the reports app and may open it, but cannot publish there.
    let (_, text) = mcp.call("list_pages", serde_json::json!({ "include_all": true })).await;
    assert!(text.contains("\"q3\""), "{text}");
    assert!(!text.contains("ledger"), "{text}");
    let (is_error, text) = mcp.call("create_upload", serde_json::json!({ "slug": "q4", "project": "finance/reports" })).await;
    assert!(is_error, "{text}");
    assert!(text.contains("viewer") && text.contains("finance/reports"), "{text}");
    let (is_error, text) = mcp.call("app_notes", serde_json::json!({ "slug": "q3", "notes": "x" })).await;
    assert!(is_error, "a viewer wrote notes: {text}");

    // Editor under the warehouse publishes there, with no folder named.
    let (is_error, text) = mcp.call("push_page", serde_json::json!({ "slug": "wh-page", "html": "<title>wh</title>" })).await;
    assert!(!is_error, "{text}");
    assert_eq!(
        toolsite::content::store::read_meta(&config, "wh-page").await.project.as_deref(),
        Some("ops/warehouse")
    );
    // And the viewer's app is open to them at the door.
    let session = sign_in(&config, "bo@example.com", "correct horse");
    let (q3_cookie, _) = hand_off(&config, &session, "q3").await;
    let (status, ..) = send(&config, get_as_app("/p/q3/", "q3", &q3_cookie)).await;
    assert_eq!(status, StatusCode::OK, "a viewer scope did not open the app");
}

#[tokio::test]
async fn an_editor_removes_only_the_apps_it_created() {
    let (_dir, config) = scoped_site();
    folder(&config, "", "ops").await;
    account(&config, "ed@example.com", "correct horse");
    scope(&config, "ed@example.com", "ops", "editor");
    // The platform itself puts an app in the folder first.
    let mut platform = Mcp::open(&config, TOKEN).await;
    let (is_error, text) = platform
        .call("push_app", serde_json::json!({ "app": "theirs", "project": "ops", "pages": { "index": "<title>theirs</title>" } }))
        .await;
    assert!(!is_error, "{text}");
    assert_eq!(toolsite::content::store::read_meta(&config, "theirs").await.project.as_deref(), Some("ops"));

    let token = mcp_token_for(&config, "ed@example.com", "correct horse").await;
    let mut mcp = Mcp::open(&config, &token).await;
    let (is_error, text) = mcp.call("push_page", serde_json::json!({ "slug": "mine", "html": "<title>mine</title>" })).await;
    assert!(!is_error, "{text}");

    let (is_error, text) = mcp.call("remove_page", serde_json::json!({ "slug": "theirs", "confirm": "theirs" })).await;
    assert!(is_error, "an editor removed an app it did not create: {text}");
    assert!(text.contains("admin"), "{text}");
    assert!(config.data_dir.join("theirs/index.html").exists());

    let (is_error, text) = mcp.call("remove_page", serde_json::json!({ "slug": "mine", "confirm": "mine" })).await;
    assert!(!is_error, "{text}");
    assert!(!config.data_dir.join("mine.html").exists());
}

#[tokio::test]
async fn a_folder_admin_sees_its_subtree_and_nothing_else() {
    let (_dir, config) = scoped_site();
    folder(&config, "", "ops").await;
    folder(&config, "ops", "yard").await;
    folder(&config, "", "finance").await;
    app_in(&config, "forklifts", "ops/yard").await;
    app_in(&config, "ledger", "finance").await;
    account(&config, "fa@example.com", "correct horse");
    scope(&config, "fa@example.com", "ops", "admin");
    let fa = sign_in(&config, "fa@example.com", "correct horse");

    let (status, page, _) = send(&config, get_as("/admin/apps", &fa)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("folder=ops"), "the folder they admin is not listed");
    assert!(!page.contains("folder=finance"), "a folder they hold nothing in was listed");
    assert!(!page.contains("ledger"));
    let (status, page, _) = send(&config, get_as("/admin/apps?folder=ops/yard", &fa)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("forklifts"));
    assert!(page.contains("Create folder"), "a folder admin cannot make folders in its subtree");
    let (status, ..) = send(&config, get_as("/admin/apps?folder=finance", &fa)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a direct URL to another folder opened");
    let (status, ..) = send(&config, get_as("/admin/apps/ledger", &fa)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ..) = send(&config, get_as("/admin/apps/forklifts/access", &fa)).await;
    assert_eq!(status, StatusCode::OK);
    for site_only in ["/admin/accounts", "/admin/exports", "/admin/github", "/admin/accounts/new"] {
        let (status, ..) = send(&config, get_as(site_only, &fa)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{site_only} opened for a folder admin");
    }
    // The sidebar shows the one entry they have.
    let (_, page, _) = send(&config, get_as("/admin/apps", &fa)).await;
    assert!(!page.contains("href=\"/admin/accounts\""), "the accounts link was shown to a folder admin");
}

#[tokio::test]
async fn a_folder_admin_grants_at_or_below_its_folder_and_never_above() {
    let (_dir, config) = scoped_site();
    folder(&config, "", "ops").await;
    folder(&config, "ops", "yard").await;
    account(&config, "fa@example.com", "correct horse");
    account(&config, "new@example.com", "correct horse");
    account(&config, "ed@example.com", "correct horse");
    scope(&config, "fa@example.com", "ops/yard", "admin");
    scope(&config, "ed@example.com", "ops/yard", "editor");
    let fa = sign_in(&config, "fa@example.com", "correct horse");
    let (_, page, _) = send(&config, get_as("/admin/apps?folder=ops/yard", &fa)).await;
    let token = form_token_from(&page);

    // Above its folder: refused.
    let (status, ..) = send(&config, post_form("/admin/scope", &fa, format!("token={token}&action=grant&prefix=ops&email=new@example.com&scope=viewer"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ..) = send(&config, post_form("/admin/scope", &fa, format!("token={token}&action=grant&prefix=&email=new@example.com&scope=viewer"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // At its folder, up to its own scope: fine.
    let (status, ..) = send(&config, post_form("/admin/scope", &fa, format!("token={token}&action=grant&prefix=ops/yard&email=new@example.com&scope=admin"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let held: Vec<_> = toolsite::accounts::users::list_scopes(&config).unwrap().into_iter().filter(|r| r.email == "new@example.com").collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].prefix, "ops/yard");
    // An editor cannot give access at all. Its folder page carries no such
    // form; the token it does hold, from an app page, buys nothing here.
    app_in(&config, "forklifts", "ops/yard").await;
    let ed = sign_in(&config, "ed@example.com", "correct horse");
    let (_, page, _) = send(&config, get_as("/admin/apps?folder=ops/yard", &ed)).await;
    assert!(!page.contains("Give access"), "an editor was offered the access form");
    let (_, page, _) = send(&config, get_as("/admin/apps/forklifts", &ed)).await;
    let ed_token = form_token_from(&page);
    let (status, ..) = send(&config, post_form("/admin/scope", &ed, format!("token={ed_token}&action=grant&prefix=ops/yard&email=new@example.com&scope=viewer"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_viewer_only_account_is_turned_away_from_the_publishing_endpoint_and_told_where_to_go() {
    let (_dir, config) = scoped_site();
    folder(&config, "", "finance").await;
    account(&config, "vi@example.com", "correct horse");
    scope(&config, "vi@example.com", "finance", "viewer");
    // The consent page admits a reader: their client belongs on /me/mcp.
    let token = mcp_token_for(&config, "vi@example.com", "correct horse").await;
    let (status, body, _) = send(&config, mcp_initialize(&token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a viewer-only account reached the publishing endpoint");
    assert!(body.contains("/me/mcp"), "the refusal does not say where the client belongs: {body}");
    // Give them editor somewhere and the same token opens /mcp.
    scope(&config, "vi@example.com", "finance", "editor");
    let (status, ..) = send(&config, mcp_initialize(&token)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_project_tree_is_never_served_and_a_moved_app_keeps_its_url() {
    let (dir, config) = scoped_site();
    folder(&config, "", "ops").await;
    assert!(dir.path().join(".site/projects.json").exists());
    let (status, ..) = send(&config, get("/p/.site/projects.json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&config, get("/p/.site/")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Moving an app changes who manages it, not where it is.
    write_page(&config, "tool/index", "<title>tool</title>");
    admin_account(&config, "boss@example.com", "correct horse battery");
    let boss = sign_in(&config, "boss@example.com", "correct horse battery");
    let (_, page, _) = send(&config, get_as("/admin/apps/tool", &boss)).await;
    let token = form_token_from(&page);
    let (status, ..) = send(&config, post_form("/admin/move", &boss, format!("token={token}&app=tool&folder=ops"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(toolsite::content::store::read_meta(&config, "tool").await.project.as_deref(), Some("ops"));
    let (status, ..) = send(&config, get("/p/tool/")).await;
    assert_eq!(status, StatusCode::OK, "the URL changed when the app moved");
    let (status, ..) = send(&config, post_form("/admin/move", &boss, format!("token={token}&app=tool&folder=nowhere"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "a bad folder should come back with a message, not a bare error");
    assert_eq!(toolsite::content::store::read_meta(&config, "tool").await.project.as_deref(), Some("ops"));
}

#[tokio::test]
async fn a_static_token_keeps_every_power() {
    let (_dir, config) = scoped_site();
    folder(&config, "", "ops").await;
    let mut platform = Mcp::open(&config, TOKEN).await;
    let (is_error, text) = platform.call("push_page", serde_json::json!({ "slug": "anywhere", "html": "<title>a</title>" })).await;
    assert!(!is_error, "{text}");
    assert_eq!(toolsite::content::store::read_meta(&config, "anywhere").await.project, None, "a static token's app went into a folder it did not name");
    let (is_error, text) = platform.call("create_user", serde_json::json!({ "email": "made@example.com" })).await;
    assert!(!is_error, "{text}");
    let (is_error, text) = platform.call("remove_page", serde_json::json!({ "slug": "anywhere", "confirm": "anywhere" })).await;
    assert!(!is_error, "{text}");
}


// --- row-level access ----------------------------------------------------------
//
// An app declares who may see which rows; the platform generates the views
// and triggers and holds the line for a handler, for a regular account over
// /me/mcp, and for an admin proving the policy as someone else.

/// One router for a whole MCP conversation: the session lives in it, so the
/// per-request router `send` builds would forget it between calls.
fn mcp_router(config: &Arc<Config>) -> axum::Router {
    build_router(config.clone(), Runtime::new().unwrap())
}

/// One JSON-RPC exchange with an MCP endpoint, through the router. Handles
/// rmcp's SSE framing for responses and the session header after initialize.
async fn mcp_post(router: &axum::Router, path: &str, token: &str, session: Option<&str>, body: serde_json::Value) -> (StatusCode, Option<String>, serde_json::Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
        .last()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    (status, session, json)
}

/// Initialises a session and returns its id.
async fn mcp_session(router: &axum::Router, path: &str, token: &str) -> String {
    let (status, session, json) = mcp_post(
        router,
        path,
        token,
        None,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let session = session.expect("no session id");
    let (status, ..) = mcp_post(
        router,
        path,
        token,
        Some(&session),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert!(status.is_success(), "initialized notification: {status}");
    session
}

/// Calls a tool and returns its text content, plus whether it was an error.
async fn mcp_tool(router: &axum::Router, path: &str, token: &str, session: &str, name: &str, arguments: serde_json::Value) -> (bool, String) {
    let (status, _, json) = mcp_post(
        router,
        path,
        token,
        Some(session),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let result = &json["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
    (is_error, text)
}

fn rows_of(text: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(text).map(|v| v["rows"].clone()).unwrap_or(serde_json::Value::Null)
}

/// An app with an orders table, a membership table, and two policies: own
/// rows of `orders`, and `records` by the person's location.
fn ledger(config: &Config) {
    write_page(config, "ledger/index", "<title>Ledger</title>");
    toolsite::runtime::migrate::store(
        config,
        "ledger",
        vec![(
            "001_initial.sql".to_string(),
            "create table orders (id integer primary key, owner_id text, total real);\n\
             create table members (user_id text, location text);\n\
             create table records (id integer primary key, location text, note text);"
                .to_string(),
        )],
    )
    .unwrap();
    toolsite::runtime::migrate::apply(config, "ledger").unwrap();
}

const LEDGER_MANIFEST: &str = r#"
[[access.table]]
table = "orders"
where = "owner_id = current_user()"
owner = "owner_id"
write = true

[[access.table]]
table = "records"
where = "location = (select location from members where user_id = current_user())"
write = true
"#;

fn person(config: &Config, email: &str) -> toolsite::accounts::users::User {
    toolsite::accounts::users::sign_up(config, email, "correct horse battery").unwrap()
}

fn bearer_for(config: &Config, user: &toolsite::accounts::users::User) -> String {
    let client = toolsite::platform::oauth_store::register_client(config, Some("t"), &["https://c.test/cb".into()]).unwrap();
    toolsite::platform::oauth_store::issue_tokens(config, &client.id, &user.id).unwrap().access_token
}

#[tokio::test]
async fn a_policy_in_the_manifest_becomes_a_view_and_triggers_and_leaves_with_it() {
    let (_dir, config) = server();
    ledger(&config);
    let changed = toolsite::platform::manifest::apply(&config, "ledger", LEDGER_MANIFEST).await.unwrap();
    assert!(changed.iter().any(|c| c.contains("access policies")), "{changed:?}");

    let objects = toolsite::runtime::db::run(
        &config,
        "ledger",
        "select type, name from sqlite_master where name like 'my_%' or name like 'ts_access_%' order by name",
        &[],
    )
    .unwrap();
    let names: Vec<String> = objects.rows.iter().map(|r| r[1].as_str().unwrap().to_string()).collect();
    assert_eq!(
        names,
        [
            "my_orders", "my_records",
            "ts_access_my_orders_delete", "ts_access_my_orders_insert", "ts_access_my_orders_update",
            "ts_access_my_records_delete", "ts_access_my_records_insert", "ts_access_my_records_update",
        ]
    );
    let meta = toolsite::content::store::read_meta(&config, "ledger").await;
    assert_eq!(meta.generated.len(), 8);

    // A column added later reaches the view after the migrations apply.
    toolsite::runtime::migrate::store(
        &config,
        "ledger",
        vec![
            ("001_initial.sql".to_string(), "create table orders (id integer primary key, owner_id text, total real);\ncreate table members (user_id text, location text);\ncreate table records (id integer primary key, location text, note text);".to_string()),
            ("002_note.sql".to_string(), "alter table orders add column note text;".to_string()),
        ],
    )
    .unwrap();
    toolsite::runtime::migrate::apply(&config, "ledger").unwrap();
    let columns = toolsite::runtime::db::describe_views(&config, "ledger", &["my_orders".to_string()]);
    assert!(columns[0].1.contains(&"note".to_string()), "{columns:?}");

    // Dropping a policy drops its objects and nothing else.
    toolsite::platform::manifest::apply(&config, "ledger", "[access]\nviews = []\n").await.unwrap();
    let left = toolsite::runtime::db::run(
        &config,
        "ledger",
        "select count(*) from sqlite_master where name like 'my_%' or name like 'ts_access_%'",
        &[],
    )
    .unwrap();
    assert_eq!(left.rows[0][0], serde_json::json!(0));
    let tables = toolsite::runtime::db::run(&config, "ledger", "select count(*) from sqlite_master where type = 'table'", &[]).unwrap();
    assert_eq!(tables.rows[0][0], serde_json::json!(3), "a base table went with the policy");
}

#[tokio::test]
async fn a_policy_that_cannot_hold_is_refused_at_apply() {
    let (_dir, config) = server();
    ledger(&config);
    toolsite::runtime::db::run(&config, "ledger", "create table pinned (k text primary key, v text) without rowid", &[]).unwrap();

    let no_rowid = "[[access.table]]\ntable = \"pinned\"\nwhere = \"1\"\nwrite = true\n";
    let error = toolsite::platform::manifest::apply(&config, "ledger", no_rowid).await.unwrap_err();
    assert!(error.contains("WITHOUT ROWID"), "{error}");

    let smuggled = "[[access.table]]\ntable = \"orders\"\nwhere = \"1 = 1; drop table orders\"\n";
    let error = toolsite::platform::manifest::apply(&config, "ledger", smuggled).await.unwrap_err();
    assert!(error.contains("';'"), "{error}");

    let broken = "[[access.table]]\ntable = \"orders\"\nwhere = \"nonsense_column = 1\"\n";
    let error = toolsite::platform::manifest::apply(&config, "ledger", broken).await.unwrap_err();
    assert!(error.contains("does not parse"), "{error}");

    // Nothing of the above landed.
    let meta = toolsite::content::store::read_meta(&config, "ledger").await;
    assert!(meta.policies.is_empty() && meta.generated.is_empty());
    let tables = toolsite::runtime::db::run(&config, "ledger", "select count(*) from sqlite_master where name = 'orders'", &[]).unwrap();
    assert_eq!(tables.rows[0][0], serde_json::json!(1));
}

#[tokio::test]
async fn two_people_over_me_mcp_each_see_and_change_only_their_own_rows() {
    let (_dir, config) = public_server();
    ledger(&config);
    toolsite::platform::manifest::apply(&config, "ledger", LEDGER_MANIFEST).await.unwrap();
    let alice = person(&config, "alice@example.com");
    let bob = person(&config, "bob@example.com");
    toolsite::runtime::db::run(
        &config,
        "ledger",
        &format!("insert into members values ('{}', 'north'), ('{}', 'south')", alice.id, bob.id),
        &[],
    )
    .unwrap();
    let alice_token = bearer_for(&config, &alice);
    let bob_token = bearer_for(&config, &bob);
    let router = mcp_router(&config);
    let a = mcp_session(&router, "/me/mcp", &alice_token).await;
    let b = mcp_session(&router, "/me/mcp", &bob_token).await;

    // Each sees the app and its views, with the writable ones marked.
    let (err, apps) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "my_apps", serde_json::json!({})).await;
    assert!(!err, "{apps}");
    assert!(apps.contains("ledger") && apps.contains("my_orders (read, write)"), "{apps}");

    // Each inserts; the owner is filled in from the account.
    let (err, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"insert into my_orders (total) values (?)","params":[10]})).await;
    assert!(!err, "{out}");
    assert!(out.contains("\"rows_affected\":1"), "{out}");
    let (err, out) = mcp_tool(&router, "/me/mcp", &bob_token, &b, "query", serde_json::json!({"app":"ledger","sql":"insert into my_orders (total) values (20)"})).await;
    assert!(!err, "{out}");

    // Each reads only their own.
    let (_, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"select total from my_orders"})).await;
    assert_eq!(rows_of(&out), serde_json::json!([[10.0]]), "{out}");
    let (_, out) = mcp_tool(&router, "/me/mcp", &bob_token, &b, "query", serde_json::json!({"app":"ledger","sql":"select total from my_orders"})).await;
    assert_eq!(rows_of(&out), serde_json::json!([[20.0]]), "{out}");

    // Reaching past the view is refused, and says so.
    let (err, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"select * from orders"})).await;
    assert!(err && out.contains("refused"), "{out}");

    // An insert for someone else is aborted; a change to someone else's row
    // changes nothing.
    let (err, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"insert into my_orders (owner_id, total) values (?, 1)","params":[bob.id]})).await;
    assert!(err && out.contains("not visible"), "{out}");
    let (err, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"update my_orders set total = 0 where total = 20"})).await;
    assert!(!err && out.contains("\"rows_affected\":0"), "{out}");
    let (_, out) = mcp_tool(&router, "/me/mcp", &bob_token, &b, "query", serde_json::json!({"app":"ledger","sql":"select total from my_orders"})).await;
    assert_eq!(rows_of(&out), serde_json::json!([[20.0]]));

    // Location scoping through the membership table: each writes and reads
    // their own location's records, and an insert for the other is aborted.
    let (err, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"insert into my_records (location, note) values ('north', 'hello')"})).await;
    assert!(!err, "{out}");
    let (err, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"insert into my_records (location, note) values ('south', 'sneaky')"})).await;
    assert!(err && out.contains("not visible"), "{out}");
    let (_, out) = mcp_tool(&router, "/me/mcp", &bob_token, &b, "query", serde_json::json!({"app":"ledger","sql":"select note from my_records"})).await;
    assert_eq!(rows_of(&out), serde_json::json!([]), "{out}");
    let (_, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"ledger","sql":"select note from my_records"})).await;
    assert_eq!(rows_of(&out), serde_json::json!([["hello"]]), "{out}");

    // An app the account may not open is refused before any SQL runs.
    write_page(&config, "secret/index", "<h1>s</h1>");
    toolsite::platform::manifest::apply(&config, "secret", "gate = \"granted\"\n").await.unwrap();
    let (err, out) = mcp_tool(&router, "/me/mcp", &alice_token, &a, "query", serde_json::json!({"app":"secret","sql":"select 1"})).await;
    assert!(err && out.contains("may not open"), "{out}");
    assert!(!apps.contains("secret"));
}

#[tokio::test]
async fn an_admin_proves_a_policy_by_running_sql_as_each_account() {
    let (_dir, config) = server();
    ledger(&config);
    toolsite::platform::manifest::apply(&config, "ledger", LEDGER_MANIFEST).await.unwrap();
    let alice = person(&config, "alice@example.com");
    let bob = person(&config, "bob@example.com");
    toolsite::runtime::db::run(
        &config,
        "ledger",
        &format!("insert into orders (owner_id, total) values ('{}', 1), ('{}', 2), ('{}', 3)", alice.id, bob.id, alice.id),
        &[],
    )
    .unwrap();
    let router = mcp_router(&config);
    let session = mcp_session(&router, "/mcp", TOKEN).await;

    let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", serde_json::json!({"app":"ledger","sql":"select total from my_orders order by total","as_user":"alice@example.com"})).await;
    assert!(!err, "{out}");
    assert_eq!(rows_of(&out), serde_json::json!([[1.0],[3.0]]));
    let (_, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", serde_json::json!({"app":"ledger","sql":"select total from my_orders","as_user":"bob@example.com"})).await;
    assert_eq!(rows_of(&out), serde_json::json!([[2.0]]));
    // As an account, the base table is out of reach.
    let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", serde_json::json!({"app":"ledger","sql":"select count(*) from orders","as_user":"alice@example.com"})).await;
    assert!(err && out.contains("not authorized"), "{out}");
    // Without as_user: the admin path, everything.
    let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", serde_json::json!({"app":"ledger","sql":"select count(*) from orders"})).await;
    assert!(!err, "{out}");
    assert_eq!(rows_of(&out), serde_json::json!([[3]]));
    // An account that does not exist is named, not guessed at.
    let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", serde_json::json!({"app":"ledger","sql":"select 1","as_user":"nobody@example.com"})).await;
    assert!(err && out.contains("no account"), "{out}");
}

#[tokio::test]
async fn a_handler_sees_the_visitor_in_sql_and_can_query_inside_the_policy() {
    let (_dir, config) = server();
    ledger(&config);
    publish_handler(&config, "ledger");
    toolsite::platform::manifest::apply(&config, "ledger", LEDGER_MANIFEST).await.unwrap();
    let alice = person(&config, "alice@example.com");
    let bob = person(&config, "bob@example.com");
    toolsite::runtime::db::run(
        &config,
        "ledger",
        &format!("insert into orders (owner_id, total) values ('{}', 1), ('{}', 2)", alice.id, bob.id),
        &[],
    )
    .unwrap();
    let site = sign_in(&config, "alice@example.com", "correct horse battery");
    let (app_token, _) = hand_off(&config, &site, "ledger").await;

    // The handler's own SQL sees who is calling, and the whole table.
    let (status, body, _) = send(&config, get_as_app("/p/ledger/api/sql?q=select+current_user(),+count(*)+from+orders", "ledger", &app_token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, format!("0:{}|2", alice.id));
    // Scoped: the visitor's rows, and nothing past the view.
    let (status, body, _) = send(&config, get_as_app("/p/ledger/api/scoped?q=select+total+from+my_orders", "ledger", &app_token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "0:1");
    let (status, body, _) = send(&config, get_as_app("/p/ledger/api/scoped?q=select+total+from+orders", "ledger", &app_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Nobody signed in: the identity functions are NULL and the scoped view
    // is empty rather than everyone's.
    let (status, body, _) = send(&config, get("/p/ledger/api/sql?q=select+current_user()+is+null")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "0:1");
    let (_, body, _) = send(&config, get("/p/ledger/api/scoped?q=select+count(*)+from+my_orders")).await;
    assert_eq!(body, "0:0");
}
