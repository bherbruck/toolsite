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

    let (status, ..) = send(
        &config,
        post_form(
            "/admin/active",
            &boss,
            format!("token={token}&email=boss@example.com&active=0"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(send(&config, get_as("/admin", &boss)).await.0, StatusCode::OK);
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
