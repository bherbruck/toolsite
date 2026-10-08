//! Subdomain mode: every app on a host of its own, `<label>.apps.test`, and
//! the main host `site.test` serving toolsite and no app content. Requests
//! go through the real router with the `Host` header a browser would send.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::Arc;
use tempfile::TempDir;
use toolsite::{build_router, content::origins::AppsDomain, runtime::wasm::Runtime, Config};
use tower::ServiceExt;

const BASE: &str = "https://site.test";
const MAIN: &str = "site.test";
const TOKEN: &str = "test-token";
const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

fn site() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        base_url: Some(BASE.to_string()),
        apps: Some(AppsDomain::parse("apps.test", Some(BASE), None).unwrap()),
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    (dir, config)
}

/// The same site in path mode, for what must not change there.
fn path_mode() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config { base_url: Some(BASE.to_string()), ..Config::local(dir.path().to_path_buf(), TOKEN) });
    (dir, config)
}

struct Reply {
    status: StatusCode,
    body: String,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    fn location(&self) -> &str {
        self.header("location").unwrap_or_else(|| panic!("no Location: {} {}", self.status, self.body))
    }
    fn cookies(&self) -> Vec<&str> {
        self.headers.iter().filter(|(k, _)| k == "set-cookie").map(|(_, v)| v.as_str()).collect()
    }
    /// The value a Set-Cookie of this name carries.
    fn cookie(&self, name: &str) -> Option<String> {
        self.cookies()
            .into_iter()
            .find_map(|c| c.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap_or("").to_string()))
    }
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> Reply {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    Reply { status, body: String::from_utf8_lossy(&bytes).to_string(), headers }
}

fn on(host: &str, method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(uri).header("host", host)
}

async fn get(config: &Arc<Config>, host: &str, uri: &str, cookie: Option<&str>) -> Reply {
    let mut request = on(host, "GET", uri);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    send(config, request.body(Body::empty()).unwrap()).await
}

fn app(config: &Config, name: &str, gate: &str) {
    let dir = config.data_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), format!("<title>{name}</title><h1>{name} home</h1>")).unwrap();
    std::fs::write(dir.join("handler.wasm"), HANDLER).unwrap();
    let mut meta = toolsite::content::store::read_meta_blocking(config, name);
    meta.gate = Some(gate.to_string());
    toolsite::content::store::write_meta_blocking(config, name, &meta).unwrap();
}

fn host_of(config: &Config, app: &str) -> String {
    format!("{}.apps.test", toolsite::content::origins::label_for(config, app))
}

fn person(config: &Config, email: &str) -> String {
    toolsite::accounts::users::sign_up(config, email, "correct horse battery").unwrap();
    let (_, token) = toolsite::accounts::users::log_in(config, email, "correct horse battery").unwrap();
    token
}

/// Walks a browser through the whole sign-in: the app host sends it to the
/// main host, which sends a code back, which the app host trades for its
/// cookie. Returns the app host's session token.
async fn sign_in_on_app_host(config: &Arc<Config>, app: &str, site_token: &str) -> String {
    let host = host_of(config, app);
    let start = get(config, &host, &format!("/p/{app}/"), None).await;
    assert_eq!(start.status, StatusCode::SEE_OTHER, "{}", start.body);
    let state = start.cookie("__Host-ts_handoff").expect("no handoff cookie");
    let handoff = start.location().strip_prefix(BASE).expect("the handoff is on the main host").to_string();

    let back = get(config, MAIN, &handoff, Some(&format!("__Host-ts_session={site_token}"))).await;
    assert_eq!(back.status, StatusCode::SEE_OTHER, "{}", back.body);
    let landing = back
        .location()
        .strip_prefix(&format!("https://{host}"))
        .unwrap_or_else(|| panic!("the code went somewhere else: {}", back.location()))
        .to_string();

    let landed = get(config, &host, &landing, Some(&format!("__Host-ts_handoff={state}"))).await;
    assert_eq!(landed.status, StatusCode::SEE_OTHER, "{}", landed.body);
    landed.cookie("__Host-ts_app").expect("no app cookie")
}

// --- the two kinds of host ------------------------------------------------

#[tokio::test]
async fn the_main_host_sends_an_app_page_to_the_apps_own_host_with_path_and_query() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    let reply = get(&config, MAIN, "/p/orders/reports/2026?q=1&x=%2F", None).await;
    assert_eq!(reply.status, StatusCode::FOUND);
    assert_eq!(reply.location(), "https://orders.apps.test/p/orders/reports/2026?q=1&x=%2F");
    assert!(!reply.body.contains("orders home"));

    // A HEAD goes the same way; the bare app root too.
    let head = send(&config, on(MAIN, "HEAD", "/p/orders").body(Body::empty()).unwrap()).await;
    assert_eq!(head.status, StatusCode::FOUND);
    assert_eq!(head.location(), "https://orders.apps.test/p/orders");
}

#[tokio::test]
async fn the_main_host_serves_no_app_content_to_anything_but_a_navigation() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    for (method, uri) in [("POST", "/p/orders/api/echo"), ("PUT", "/p/orders/x"), ("DELETE", "/p/orders/api/x"), ("POST", "/p/orders/mcp")] {
        let reply = send(&config, on(MAIN, method, uri).header("content-type", "application/json").body(Body::from("{}")).unwrap()).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {uri} on the main host: {}", reply.body);
        assert!(reply.header("location").is_none());
    }
    let upgrade = send(
        &config,
        on(MAIN, "GET", "/p/orders/ws")
            .header("upgrade", "websocket")
            .header("connection", "upgrade")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(upgrade.status, StatusCode::NOT_FOUND, "a socket opened on the main host");
    let metadata = get(&config, MAIN, "/.well-known/oauth-protected-resource/p/orders/mcp", None).await;
    assert_eq!(metadata.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_app_host_serves_its_own_app_and_nothing_of_toolsites_or_another_apps() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    app(&config, "billing", "public");
    let host = host_of(&config, "orders");

    let page = get(&config, &host, "/p/orders/", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("orders home"));
    let api = get(&config, &host, "/p/orders/api/echo", None).await;
    assert_eq!(api.body, "GET /api/echo?");
    assert_eq!(get(&config, &host, "/", None).await.location(), "/p/orders/");

    // Another app's path on this host is not that app.
    for uri in ["/p/billing/", "/p/billing/api/echo", "/p/ordersx/"] {
        let reply = get(&config, &host, uri, None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}: {}", reply.body);
        assert!(!reply.body.contains("billing home"));
    }
    // Machine endpoints are the main host's alone.
    for (method, uri) in [
        ("POST", "/mcp"),
        ("POST", "/me/mcp"),
        ("POST", "/register"),
        ("POST", "/token"),
        ("GET", "/.well-known/oauth-authorization-server"),
        ("GET", "/upload/anything"),
        ("GET", "/export/orders.sqlite"),
        ("PUT", "/deploy/orders"),
        ("GET", "/icon/orders"),
        ("POST", "/admin/gate"),
        ("POST", "/auth/login"),
    ] {
        let reply = send(&config, on(&host, method, uri).header("authorization", format!("Bearer {TOKEN}")).body(Body::empty()).unwrap()).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {uri} on an app host: {}", reply.body);
    }
    // A person who opens one of toolsite's pages on an app host is sent to
    // the main host, and nothing of the page is served here.
    for uri in ["/admin", "/admin/apps", "/account", "/browse/ops", "/auth/login?next=/p/orders/", "/authorize?x=1"] {
        let reply = get(&config, &host, uri, None).await;
        assert_eq!(reply.status, StatusCode::FOUND, "{uri}: {}", reply.body);
        assert_eq!(reply.location(), format!("{BASE}{uri}"));
    }
}

#[tokio::test]
async fn an_unknown_or_forged_host_is_404_and_never_a_redirect_target() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    for host in [
        "evil.com",
        "orders.apps.test.evil.com",
        "orders.apps.test.",
        "orders.apps.test:8443",
        "x.orders.apps.test",
        "nobody.apps.test",
        "apps.test",
        "site.test.evil.com",
        "site.test:1",
    ] {
        for uri in ["/p/orders/", "/admin", "/", "/auth/handoff?app=orders&state=aaaaaaaaaaaaaaaaaaaaaaaa"] {
            let reply = get(&config, host, uri, None).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{host}{uri}: {}", reply.body);
            assert!(reply.header("location").is_none(), "{host}{uri} redirected");
        }
    }
    // Upper case is the same name, as DNS has it.
    assert_eq!(get(&config, "ORDERS.APPS.TEST", "/p/orders/", None).await.status, StatusCode::OK);

    // A redirect is built from the stored label, whatever else the request
    // claims about where it was sent.
    let reply = send(
        &config,
        on(MAIN, "GET", "/p/orders/").header("x-forwarded-host", "evil.com").header("forwarded", "host=evil.com").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(reply.location(), "https://orders.apps.test/p/orders/");
}

#[tokio::test]
async fn a_label_stays_with_its_app_through_project_moves_and_renames() {
    let (_dir, config) = site();
    app(&config, "Orders", "public");
    let label = toolsite::content::origins::label_for(&config, "Orders");
    assert!(label.starts_with("orders-") && label.len() <= 63, "{label}");

    let projects = |args: serde_json::Value| mcp_tool(&config, MAIN, "/mcp", TOKEN, "projects", args);
    projects(serde_json::json!({"action": "create", "path": "", "name": "ops"})).await;
    projects(serde_json::json!({"action": "move", "app": "Orders", "path": "ops"})).await;
    assert_eq!(toolsite::content::origins::label_for(&config, "Orders"), label);
    projects(serde_json::json!({"action": "rename", "path": "ops", "name": "yard"})).await;
    assert_eq!(toolsite::content::store::read_meta(&config, "Orders").await.project.as_deref(), Some("yard"));
    assert_eq!(toolsite::content::origins::label_for(&config, "Orders"), label);

    // An app published later under exactly that name does not take it.
    app(&config, &label, "public");
    assert_eq!(toolsite::content::origins::label_for(&config, "Orders"), label);
    let page = get(&config, &format!("{label}.apps.test"), "/p/Orders/", None).await;
    assert!(page.body.contains("Orders home"), "{}", page.body);
}

// --- signing in on an app host -------------------------------------------

#[tokio::test]
async fn the_handoff_sets_a_host_only_cookie_on_the_app_host() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    let site_token = person(&config, "someone@example.com");
    let host = host_of(&config, "members");

    // No app session: off to the main host, with a nonce in a cookie here.
    let start = get(&config, &host, "/p/members/inbox", None).await;
    assert_eq!(start.status, StatusCode::SEE_OTHER);
    assert!(start.location().starts_with(&format!("{BASE}/auth/handoff?app=members&next=%2Fp%2Fmembers%2Finbox&state=")), "{}", start.location());
    let state_cookie = start.cookies().into_iter().find(|c| c.starts_with("__Host-ts_handoff=")).unwrap().to_string();
    assert!(state_cookie.contains("Path=/;") && state_cookie.contains("Secure") && !state_cookie.contains("Domain"), "{state_cookie}");

    let token = sign_in_on_app_host(&config, "members", &site_token).await;
    let page = get(&config, &host, "/p/members/", Some(&format!("__Host-ts_app={token}"))).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let whoami = get(&config, &host, "/p/members/api/whoami", Some(&format!("__Host-ts_app={token}"))).await;
    assert!(whoami.body.ends_with(":someone@example.com"), "{}", whoami.body);
}

#[tokio::test]
async fn the_app_cookie_is_host_only_secure_and_out_of_reach_of_script() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    let site_token = person(&config, "someone@example.com");
    let host = host_of(&config, "members");
    let start = get(&config, &host, "/p/members/", None).await;
    let state = start.cookie("__Host-ts_handoff").unwrap();
    let back = get(&config, MAIN, start.location().strip_prefix(BASE).unwrap(), Some(&format!("__Host-ts_session={site_token}"))).await;
    let landing = back.location().strip_prefix(&format!("https://{host}")).unwrap().to_string();
    let landed = get(&config, &host, &landing, Some(&format!("__Host-ts_handoff={state}"))).await;
    let cookie = landed.cookies().into_iter().find(|c| c.starts_with("__Host-ts_app=")).unwrap().to_string();
    for part in ["Path=/;", "HttpOnly", "SameSite=Lax", "Secure", "Max-Age="] {
        assert!(cookie.contains(part), "{part} missing: {cookie}");
    }
    assert!(!cookie.to_ascii_lowercase().contains("domain"), "the cookie would reach other hosts: {cookie}");
    assert!(landed.cookies().iter().any(|c| c.starts_with("__Host-ts_handoff=;") && c.contains("Max-Age=0")), "the nonce outlived its use");
    assert_eq!(landed.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(landed.location(), "/p/members/");

    // The site session on the main host never names a parent domain either.
    let login = send(
        &config,
        on(MAIN, "POST", "/auth/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("email=someone%40example.com&password=correct+horse+battery"))
            .unwrap(),
    )
    .await;
    let site_cookie = login.cookies().into_iter().find(|c| c.starts_with("__Host-ts_session=")).expect("no site cookie").to_string();
    assert!(site_cookie.contains("Path=/;") && site_cookie.contains("Secure") && !site_cookie.to_ascii_lowercase().contains("domain"), "{site_cookie}");
}

#[tokio::test]
async fn a_code_goes_only_to_its_own_app_and_only_with_the_browsers_nonce() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    app(&config, "billing", "authenticated");
    let site_token = person(&config, "someone@example.com");
    let members = host_of(&config, "members");
    let billing = host_of(&config, "billing");
    let session = format!("__Host-ts_session={site_token}");
    let state = "abcdefghijklmnopqrstuvwxyz012345";
    let nonce = format!("__Host-ts_handoff={state}");

    // A next for another app, another host or a scheme is not followed: the
    // code goes to members' host and lands on members' own root.
    for next in ["/p/billing/", "https://evil.com/p/members/", "//evil.com/p/members/", "/admin", "/p/membersx/"] {
        let uri = format!("/auth/handoff?app=members&next={}&state={state}", urlencoding::encode(next));
        let back = get(&config, MAIN, &uri, Some(&session)).await;
        assert!(back.location().starts_with(&format!("https://{members}/auth/landing?code=")), "{next}: {}", back.location());
        let landing = back.location().strip_prefix(&format!("https://{members}")).unwrap().to_string();
        let landed = get(&config, &members, &landing, Some(&nonce)).await;
        assert_eq!(landed.location(), "/p/members/", "{next}");
    }

    // A code minted for members is refused on billing's host, and is spent.
    let landing = code_for(&config, &session, state).await;
    let elsewhere = get(&config, &billing, &format!("/auth/landing{landing}"), Some(&nonce)).await;
    assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
    assert!(elsewhere.cookies().is_empty());
    let again = get(&config, &members, &format!("/auth/landing{landing}"), Some(&nonce)).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST, "a code was used twice");

    // Without the nonce cookie, or with another, the code is not taken: a
    // code collected for one's own account cannot be walked into a victim's
    // browser.
    let landing = code_for(&config, &session, state).await;
    let bare = get(&config, &members, &format!("/auth/landing{landing}"), None).await;
    assert_eq!(bare.status, StatusCode::FORBIDDEN);
    let landing = code_for(&config, &session, state).await;
    let other = get(&config, &members, &format!("/auth/landing{landing}"), Some("__Host-ts_handoff=zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")).await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
    assert!(other.cookies().is_empty());

    // The main host never hands out a code without the app host's nonce, and
    // the landing is not on the main host at all.
    let unbound = get(&config, MAIN, "/auth/handoff?app=members&next=%2Fp%2Fmembers%2F", Some(&session)).await;
    assert_eq!(unbound.status, StatusCode::BAD_REQUEST);
    let landing = code_for(&config, &session, state).await;
    let on_main = get(&config, MAIN, &format!("/auth/landing{landing}"), Some(&nonce)).await;
    assert_eq!(on_main.status, StatusCode::NOT_FOUND);
}

/// A fresh code for members, as the query string of its landing.
async fn code_for(config: &Arc<Config>, session: &str, state: &str) -> String {
    let back = get(config, MAIN, &format!("/auth/handoff?app=members&next=%2Fp%2Fmembers%2F&state={state}"), Some(session)).await;
    back.location().split("/auth/landing").nth(1).unwrap().to_string()
}

#[tokio::test]
async fn a_person_the_gate_refuses_gets_no_session_for_the_app() {
    let (_dir, config) = site();
    app(&config, "vault", "restricted");
    let site_token = person(&config, "someone@example.com");
    let back = get(
        &config,
        MAIN,
        "/auth/handoff?app=vault&next=%2Fp%2Fvault%2F&state=abcdefghijklmnopqrstuvwxyz012345",
        Some(&format!("__Host-ts_session={site_token}")),
    )
    .await;
    assert_eq!(back.status, StatusCode::FORBIDDEN);
    assert!(back.header("location").is_none());
    assert!(config.stores.tickets.live(toolsite::state::tickets::Kind::Handoff).await.unwrap() == 0, "a code was minted for a refused visitor");

    // Signed out, the handoff asks for a sign-in and comes back to itself.
    let signed_out = get(&config, MAIN, "/auth/handoff?app=vault&next=%2Fp%2Fvault%2F&state=abcdefghijklmnopqrstuvwxyz012345", None).await;
    assert!(signed_out.location().starts_with("/auth/login?next=%2Fauth%2Fhandoff%3Fapp%3Dvault"), "{}", signed_out.location());
}

#[tokio::test]
async fn each_hosts_cookie_is_worthless_on_every_other_host() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    app(&config, "billing", "authenticated");
    let site_token = person(&config, "someone@example.com");
    let members_token = sign_in_on_app_host(&config, "members", &site_token).await;
    let billing = host_of(&config, "billing");
    let members = host_of(&config, "members");

    // members' cookie, presented to billing's host, opens nothing there.
    let reply = get(&config, &billing, "/p/billing/", Some(&format!("__Host-ts_app={members_token}"))).await;
    assert_ne!(reply.status, StatusCode::OK);
    assert!(!reply.body.contains("billing home"));
    let api = get(&config, &billing, "/p/billing/api/whoami", Some(&format!("__Host-ts_app={members_token}"))).await;
    assert_eq!(api.status, StatusCode::UNAUTHORIZED);

    // Nor is it a site session on the main host.
    let me = get(&config, MAIN, "/auth/me", Some(&format!("__Host-ts_session={members_token}"))).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);

    // And the site session is not an app session on an app host, under
    // either name.
    for jar in [format!("__Host-ts_app={site_token}"), format!("__Host-ts_session={site_token}"), format!("ts_session={site_token}"), format!("ts_app_members={site_token}")] {
        let reply = get(&config, &members, "/p/members/api/whoami", Some(&jar)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{jar}");
    }
}

#[tokio::test]
async fn signing_out_on_the_main_host_ends_every_app_hosts_session() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    let site_token = person(&config, "someone@example.com");
    let token = sign_in_on_app_host(&config, "members", &site_token).await;
    let host = host_of(&config, "members");
    let jar = format!("__Host-ts_app={token}");
    assert_eq!(get(&config, &host, "/p/members/api/whoami", Some(&jar)).await.status, StatusCode::OK);

    let out = send(&config, on(MAIN, "POST", "/auth/logout").header("cookie", format!("__Host-ts_session={site_token}")).body(Body::empty()).unwrap()).await;
    assert!(out.cookies().iter().any(|c| c.starts_with("__Host-ts_session=;") && c.contains("Max-Age=0")));
    assert_eq!(get(&config, &host, "/p/members/api/whoami", Some(&jar)).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_app_host_refuses_a_write_sent_by_another_origins_page() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    app(&config, "billing", "public");
    let host = host_of(&config, "orders");
    let post = |origin: Option<&str>| {
        let mut request = on(&host, "POST", "/p/orders/api/echo");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        request.body(Body::from("x")).unwrap()
    };
    // A sibling app is one site with this one, so its script's POST would
    // carry this host's cookie. The Origin says who sent it.
    for origin in ["https://billing.apps.test", "https://site.test", "https://evil.com", "null", "http://orders.apps.test", "https://orders.apps.test:8443"] {
        let reply = send(&config, post(Some(origin))).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "a POST from {origin} ran");
    }
    assert_eq!(send(&config, post(Some("https://orders.apps.test"))).await.body, "POST /api/echo?");
    // No Origin is not a browser page, and carries no cookie of the visitor's.
    assert_eq!(send(&config, post(None)).await.body, "POST /api/echo?");
    // Reads are not writes.
    let read = send(&config, on(&host, "GET", "/p/orders/api/echo").header("origin", "https://billing.apps.test").body(Body::empty()).unwrap()).await;
    assert_eq!(read.status, StatusCode::OK);
}

// --- connectors and MCP ---------------------------------------------------

/// A tool call through an MCP endpoint on a given host.
async fn mcp_call(config: &Arc<Config>, host: &str, path: &str, token: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let request = on(host, "POST", path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let reply = send(config, request).await;
    let json = reply
        .body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&reply.body).ok())
        .unwrap_or(serde_json::Value::Null);
    (reply.status, json)
}

async fn mcp_tool(config: &Arc<Config>, host: &str, path: &str, token: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    let (status, json) = mcp_call(
        config,
        host,
        path,
        token,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_ne!(json["result"]["isError"], true, "{json}");
    json["result"].clone()
}

const FARM_TOOLS: &str = r#"
[[tool]]
name = "whoami"
description = "Who is calling."
path = "/api/whoami"
read_only = true
"#;

fn bearer(config: &Config, email: &str, resource: Option<&str>) -> String {
    let user = toolsite::accounts::users::user_by_email(config, email).unwrap();
    let client = toolsite::platform::oauth_store::register_client(config, Some("t"), &["https://c.test/cb".into()]).unwrap();
    toolsite::platform::oauth_store::issue_tokens(config, &client.id, &user.id, resource).unwrap().access_token
}

#[tokio::test]
async fn an_apps_connector_answers_on_its_host_for_a_token_bound_to_that_host() {
    let (_dir, config) = site();
    app(&config, "farm", "restricted");
    toolsite::platform::manifest::apply(&config, "farm", FARM_TOOLS).await.unwrap();
    person(&config, "alice@example.com");
    toolsite::accounts::users::grant(&config, "alice@example.com", "farm", "viewer").unwrap();
    let host = host_of(&config, "farm");
    let resource = format!("https://{host}/p/farm/mcp");

    let metadata = get(&config, &host, "/.well-known/oauth-protected-resource/p/farm/mcp", None).await;
    let metadata: serde_json::Value = serde_json::from_str(&metadata.body).unwrap();
    assert_eq!(metadata["resource"], resource);
    assert_eq!(metadata["authorization_servers"][0], BASE);

    let token = bearer(&config, "alice@example.com", Some(&resource));
    let result = mcp_tool(&config, &host, "/p/farm/mcp", &token, "whoami", serde_json::json!({})).await;
    assert!(result["content"][0]["text"].as_str().unwrap().ends_with(":alice@example.com"), "{result}");

    // A token bound to the path-mode address is for a resource this site no
    // longer serves; a token for another app's host is for another app.
    for other in [format!("{BASE}/p/farm/mcp"), "https://other.apps.test/p/farm/mcp".to_string()] {
        let token = bearer(&config, "alice@example.com", Some(&other));
        let (status, _) = mcp_call(&config, &host, "/p/farm/mcp", &token, serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a token for {other} was taken");
    }
    // The challenge names the metadata on the app's host.
    let reply = send(&config, on(&host, "POST", "/p/farm/mcp").header("content-type", "application/json").body(Body::from("{}")).unwrap()).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.header("www-authenticate").unwrap().contains(&format!("https://{host}/.well-known/oauth-protected-resource/p/farm/mcp")));

    // The consent flow takes the app host's address as a resource and
    // refuses the old one.
    let admin_token = {
        toolsite::accounts::users::sign_up_as(&config, "root@example.com", "correct horse battery", true).unwrap();
        toolsite::accounts::users::log_in(&config, "root@example.com", "correct horse battery").unwrap().1
    };
    let client = toolsite::platform::oauth_store::register_client(&config, Some("t"), &["https://client.test/cb".into()]).unwrap();
    for (asked, allowed) in [(resource.clone(), true), (format!("{BASE}/p/farm/mcp"), false), (format!("https://{host}/p/other/mcp"), false)] {
        let uri = format!(
            "/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256&state=s&resource={}",
            client.id,
            urlencoding::encode("https://client.test/cb"),
            urlencoding::encode(&asked)
        );
        let reply = get(&config, MAIN, &uri, Some(&format!("__Host-ts_session={admin_token}"))).await;
        let refused = reply.header("location").is_some_and(|l| l.contains("invalid_target"));
        assert_eq!(!refused, allowed, "{asked}: {} {}", reply.status, reply.body);
    }
}

#[tokio::test]
async fn links_point_at_app_hosts_in_subdomain_mode_and_stay_as_they_were_in_path_mode() {
    for (config, expected_href, expected_url) in [
        (site().1, "href=\"https://orders.apps.test/p/orders\"", "https://orders.apps.test/p/orders"),
        (path_mode().1, "href=\"/p/orders\"", "https://site.test/p/orders"),
    ] {
        app(&config, "orders", "public");
        let index = get(&config, MAIN, "/", None).await;
        assert!(index.body.contains(expected_href), "{expected_href} not in the app browser");

        let listing = mcp_tool(&config, MAIN, "/mcp", TOKEN, "list_pages", serde_json::json!({})).await;
        assert!(listing.to_string().contains(expected_url), "{listing}");
        let upload = mcp_tool(&config, MAIN, "/mcp", TOKEN, "create_upload", serde_json::json!({"slug": "orders"})).await;
        let text = upload["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(expected_url), "{text}");
        // The base path a build needs is the same in both modes.
        assert!(text.contains("/p/orders/"), "{text}");
        assert_eq!(
            toolsite::platform::app_tools::connector_url(&config, "orders"),
            format!("{expected_url}/mcp")
        );
    }
}

#[tokio::test]
async fn a_preview_signs_the_renderer_in_on_the_apps_own_host() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    person(&config, "reader@example.com");
    let user = toolsite::accounts::users::user_by_email(&config, "reader@example.com").unwrap();
    let host = host_of(&config, "members");

    let token = toolsite::platform::preview::issue(&config, "members", "/", Some(&user.id)).await.unwrap();
    assert_eq!(
        toolsite::platform::screenshot::preview_url(&config, "members", &token),
        format!("https://{host}/preview/{token}")
    );
    let reply = get(&config, &host, &format!("/preview/{token}"), None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/p/members/");
    let session = reply.cookie("__Host-ts_app").expect("no app cookie");
    let page = get(&config, &host, "/p/members/", Some(&format!("__Host-ts_app={session}"))).await;
    assert_eq!(page.status, StatusCode::OK);

    // Not on the main host, and not on another app's host.
    app(&config, "billing", "public");
    for elsewhere in [MAIN.to_string(), host_of(&config, "billing")] {
        let token = toolsite::platform::preview::issue(&config, "members", "/", Some(&user.id)).await.unwrap();
        let reply = get(&config, &elsewhere, &format!("/preview/{token}"), None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{elsewhere}");
        assert!(reply.cookies().is_empty());
    }
}

#[tokio::test]
async fn a_browser_upload_url_is_on_the_apps_host_and_only_that_host_takes_it() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    app(&config, "billing", "public");
    let url = toolsite::runtime::blobs::issue_upload(&config, "orders", "files/a.txt", 0).await.unwrap();
    let host = host_of(&config, "orders");
    assert!(url.starts_with(&format!("https://{host}/blob/")), "{url}");
    let path = url.strip_prefix(&format!("https://{host}")).unwrap().to_string();
    let wrong = send(&config, on(&host_of(&config, "billing"), "PUT", &path).body(Body::from("x")).unwrap()).await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND);

    let url = toolsite::runtime::blobs::issue_upload(&config, "orders", "files/a.txt", 0).await.unwrap();
    let path = url.strip_prefix(&format!("https://{host}")).unwrap().to_string();
    let right = send(&config, on(&host, "PUT", &path).header("origin", format!("https://{host}")).body(Body::from("x")).unwrap()).await;
    assert!(right.status.is_success(), "{} {}", right.status, right.body);
}

// --- sockets, over a real connection ---------------------------------------

#[tokio::test]
async fn a_socket_opens_only_from_its_own_app_hosts_pages() {
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
    let (_dir, config) = site();
    app(&config, "team", "public");
    let mut meta = toolsite::content::store::read_meta_blocking(&config, "team");
    meta.sockets = vec!["/ws".into()];
    toolsite::content::store::write_meta_blocking(&config, "team", &meta).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(config.clone(), Runtime::new().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let host = host_of(&config, "team");

    let connect = |host: String, origin: String| async move {
        let mut request = format!("ws://{addr}/p/team/ws").into_client_request().unwrap();
        request.headers_mut().insert("host", host.parse().unwrap());
        request.headers_mut().insert("origin", origin.parse().unwrap());
        match tokio_tungstenite::connect_async(request).await {
            Ok(_) => Ok(()),
            Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
            Err(other) => panic!("connect failed: {other}"),
        }
    };
    for origin in ["https://billing.apps.test", BASE, "https://evil.com", "http://team.apps.test"] {
        assert_eq!(connect(host.clone(), origin.to_string()).await, Err(403), "an upgrade from {origin} was taken");
    }
    assert_eq!(connect(MAIN.to_string(), format!("https://{host}")).await, Err(404), "a socket opened on the main host");
    assert_eq!(connect(host.clone(), format!("https://{host}")).await, Ok(()));
}

// --- adversarial: hosts ----------------------------------------------------

#[tokio::test]
async fn a_health_check_is_answered_on_any_host_in_either_mode_and_says_nothing_else() {
    for config in [site().1, path_mode().1] {
        app(&config, "orders", "public");
        let host = host_of(&config, "orders");
        for asked in ["healthcheck.railway.app", MAIN, host.as_str(), "evil.com", "orders.apps.test:1"] {
            let reply = get(&config, asked, "/healthz", None).await;
            assert_eq!(reply.status, StatusCode::OK, "{asked}");
            assert_eq!(reply.body, "ok");
            let head = send(&config, on(asked, "HEAD", "/healthz").body(Body::empty()).unwrap()).await;
            assert_eq!(head.status, StatusCode::OK, "HEAD on {asked}");
        }
        let bare = send(&config, Request::builder().uri("/healthz").body(Body::empty()).unwrap()).await;
        assert_eq!(bare.status, StatusCode::OK, "no Host at all");
    }
    // Nothing but the health check is answered on an unknown host.
    let (_dir, config) = site();
    app(&config, "orders", "public");
    for uri in ["/healthz/", "/healthz?x=/p/orders/", "/p/orders/", "/", "/guide", "/healthzx"] {
        let reply = get(&config, "healthcheck.railway.app", uri, None).await;
        if uri.starts_with("/healthz?") {
            assert_eq!(reply.body, "ok");
            continue;
        }
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}: {}", reply.body);
        assert!(reply.header("location").is_none(), "{uri}");
    }
}

#[tokio::test]
async fn a_request_that_names_two_hosts_is_refused_rather_than_routed_by_either() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    app(&config, "billing", "public");
    let orders = host_of(&config, "orders");
    let billing = host_of(&config, "billing");

    // Two Host headers: a proxy may route by one, the server by the other.
    let twice = Request::builder().uri("/p/billing/api/echo").header("host", &orders).header("host", &billing).body(Body::empty()).unwrap();
    assert_eq!(send(&config, twice).await.status, StatusCode::BAD_REQUEST);
    let twice = Request::builder().uri("/p/orders/").header("host", MAIN).header("host", &orders).body(Body::empty()).unwrap();
    let reply = send(&config, twice).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.header("location").is_none());

    // An absolute-form request line, or an HTTP/2 :authority, that disagrees
    // with Host.
    for (uri, host) in [
        (format!("https://{billing}/p/billing/api/echo"), orders.clone()),
        ("http://evil.com/p/orders/".to_string(), orders.clone()),
        (format!("https://{orders}/p/orders/"), MAIN.to_string()),
    ] {
        let reply = send(&config, on(&host, "GET", &uri).body(Body::empty()).unwrap()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri} with Host {host}: {}", reply.body);
        assert!(!reply.body.contains("echo") && reply.header("location").is_none());
    }
    // Agreeing, or the authority alone as HTTP/2 sends it, is one host.
    let agreed = send(&config, on(&orders, "GET", &format!("https://{}/p/orders/api/echo", orders.to_uppercase())).body(Body::empty()).unwrap()).await;
    assert_eq!(agreed.body, "GET /api/echo?");
    let h2 = send(&config, Request::builder().uri(format!("https://{orders}/p/orders/api/echo")).body(Body::empty()).unwrap()).await;
    assert_eq!(h2.body, "GET /api/echo?");
    let h2_evil = send(&config, Request::builder().uri("https://evil.com/p/orders/").body(Body::empty()).unwrap()).await;
    assert_eq!(h2_evil.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn malformed_and_lookalike_hosts_select_no_app() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    for host in [
        ".apps.test",
        "orders..apps.test",
        "orders.apps.test:",
        "orders.apps.test:443",
        "orders.apps.test.:443",
        "-orders.apps.test",
        "orders-.apps.test",
        "or_ders.apps.test",
        "orders%2eapps.test",
        "orders.apps.test/evil",
        "user@orders.apps.test",
        "orders.apps.test@evil.com",
        "[::1].apps.test",
        "xn--rders-3ve.apps.test",
        "orders.APPS.test.evil.com",
    ] {
        let Ok(value) = axum::http::HeaderValue::from_str(host) else { continue };
        let request = Request::builder().uri("/p/orders/").header("host", value).body(Body::empty()).unwrap();
        let reply = send(&config, request).await;
        assert!(
            reply.status == StatusCode::NOT_FOUND || reply.status == StatusCode::BAD_REQUEST,
            "{host}: {} {}",
            reply.status,
            reply.body
        );
        assert!(!reply.body.contains("orders home") && reply.header("location").is_none(), "{host}");
    }
}

#[tokio::test]
async fn a_path_on_an_app_host_never_reaches_another_app_however_it_is_spelled() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    app(&config, "billing", "public");
    let host = host_of(&config, "orders");
    for uri in [
        "/p/orders/../billing/api/echo",
        "/p/orders/%2E%2E/billing/api/echo",
        "/p/orders/%2e%2e%2fbilling/api/echo",
        "/p/orders%2F..%2Fbilling/api/echo",
        "/p//billing/api/echo",
        "/P/billing/api/echo",
        "/p/%62illing/api/echo",
        "/p/orders/..%5Cbilling/api/echo",
        "/p/billing%00/api/echo",
    ] {
        let reply = get(&config, &host, uri, None).await;
        assert!(!reply.body.contains("billing"), "{uri}: {} {}", reply.status, reply.body);
        if let Some(location) = reply.header("location") {
            assert!(!location.contains("billing"), "{uri} -> {location}");
        }
    }
}

#[tokio::test]
async fn an_idna_spelled_name_is_not_issued_as_a_host_that_browsers_would_show_in_unicode() {
    let (_dir, config) = site();
    // `xn--pple-43d` is how DNS spells "аpple" with a Cyrillic а.
    for name in ["xn--pple-43d", "ab--cd"] {
        app(&config, name, "public");
        let label = toolsite::content::origins::label_for(&config, name);
        assert_ne!(label, name);
        assert!(label.get(2..4) != Some("--"), "{label}");
        let page = get(&config, &format!("{label}.apps.test"), &format!("/p/{name}/"), None).await;
        assert_eq!(page.status, StatusCode::OK, "{label}");
        assert_eq!(get(&config, &format!("{name}.apps.test"), &format!("/p/{name}/"), None).await.status, StatusCode::NOT_FOUND);
    }
}

// --- adversarial: labels ---------------------------------------------------

#[tokio::test]
async fn a_removed_apps_host_is_never_issued_to_another_app_and_comes_back_with_it() {
    let (_dir, config) = site();
    app(&config, "Shop", "authenticated");
    let label = toolsite::content::origins::label_for(&config, "Shop");
    let old_host = format!("{label}.apps.test");
    let site_token = person(&config, "someone@example.com");
    let shop_cookie = sign_in_on_app_host(&config, "Shop", &site_token).await;

    toolsite::platform::trash::remove(&config, "Shop", 1000).unwrap();
    assert_eq!(get(&config, &old_host, "/p/Shop/", None).await.status, StatusCode::NOT_FOUND);

    // Someone publishes an app named exactly the old label, to inherit the
    // host: its bookmarks, its storage in the browser, its cookies.
    app(&config, &label, "public");
    let squatter = toolsite::content::origins::label_for(&config, &label);
    assert_ne!(squatter, label, "a removed app's host went to another app");
    let reply = get(&config, &old_host, &format!("/p/{label}/"), Some(&format!("__Host-ts_app={shop_cookie}"))).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    // Nor does the old cookie mean anything on the squatter's own host.
    let squatter_host = format!("{squatter}.apps.test");
    let whoami = get(&config, &squatter_host, &format!("/p/{label}/api/whoami"), Some(&format!("__Host-ts_app={shop_cookie}"))).await;
    assert!(!whoami.body.contains("someone@example.com"), "{}", whoami.body);

    // Put back from the bin, Shop has its own host again.
    let kept = config.data_dir.join(".trash/1000-Shop");
    std::fs::rename(kept.join("app"), config.data_dir.join("Shop")).unwrap();
    std::fs::rename(kept.join("slug.meta"), config.data_dir.join("Shop.meta")).unwrap();
    assert_eq!(toolsite::content::origins::label_for(&config, "Shop"), label);
    assert!(get(&config, &old_host, "/p/Shop/", Some(&format!("__Host-ts_app={shop_cookie}"))).await.body.contains("Shop home"));
    assert!(get(&config, &squatter_host, &format!("/p/{label}/"), None).await.body.contains(&format!("{label} home")));
}

#[tokio::test]
async fn an_app_published_again_under_its_name_gets_its_host_back_even_without_its_meta() {
    let (_dir, config) = site();
    app(&config, "Shop", "public");
    let label = toolsite::content::origins::label_for(&config, "Shop");
    toolsite::platform::trash::remove(&config, "Shop", 1000).unwrap();
    app(&config, "Shop", "public");
    assert_eq!(toolsite::content::origins::label_for(&config, "Shop"), label);

    // A meta rewritten by a writer that read it before the label was stored
    // loses the label; the registry still knows it.
    let mut meta = toolsite::content::store::read_meta_blocking(&config, "Shop");
    meta.label = None;
    toolsite::content::store::write_meta_blocking(&config, "Shop", &meta).unwrap();
    app(&config, "shop-squat", "public");
    assert_eq!(toolsite::content::origins::label_for(&config, "Shop"), label);
}

// --- adversarial: the handoff -----------------------------------------------

#[tokio::test]
async fn a_handoff_next_with_control_characters_or_backslashes_lands_on_the_apps_root() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    let site_token = person(&config, "someone@example.com");
    let members = host_of(&config, "members");
    let session = format!("__Host-ts_session={site_token}");
    let state = "abcdefghijklmnopqrstuvwxyz012345";
    for next in [
        "/p/members/\r\nSet-Cookie: ts_app=x",
        "/p/members/\u{0}",
        "/p/members/\\\\evil.com",
        "/p/members\\..\\admin",
        "/p/members/ x",
        "/p/members/\u{e9}",
        "/p/members/\t//evil.com",
        "/p/members/%2F%2Fevil.com",
    ] {
        let uri = format!("/auth/handoff?app=members&next={}&state={state}", urlencoding::encode(next));
        let back = get(&config, MAIN, &uri, Some(&session)).await;
        assert!(back.location().starts_with(&format!("https://{members}/auth/landing?code=")), "{next:?}: {}", back.location());
        let landing = back.location().strip_prefix(&format!("https://{members}")).unwrap().to_string();
        let landed = get(&config, &members, &landing, Some(&format!("__Host-ts_handoff={state}"))).await;
        assert_eq!(landed.status, StatusCode::SEE_OTHER, "{next:?}: {}", landed.body);
        let location = landed.location();
        assert!(location == "/p/members/" || location == "/p/members/%2F%2Fevil.com", "{next:?} -> {location}");
    }
}

#[tokio::test]
async fn a_handoff_a_sibling_triggers_with_a_subresource_mints_nothing() {
    let (_dir, config) = site();
    app(&config, "members", "authenticated");
    let site_token = person(&config, "someone@example.com");
    for (mode, dest) in [("no-cors", "image"), ("cors", "empty"), ("navigate", "iframe"), ("no-cors", "script")] {
        let request = on(MAIN, "GET", "/auth/handoff?app=members&next=%2Fp%2Fmembers%2F&state=abcdefghijklmnopqrstuvwxyz012345")
            .header("cookie", format!("__Host-ts_session={site_token}"))
            .header("sec-fetch-mode", mode)
            .header("sec-fetch-dest", dest)
            .header("sec-fetch-site", "same-site")
            .body(Body::empty())
            .unwrap();
        let reply = send(&config, request).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{mode}/{dest}");
        assert!(reply.header("location").is_none());
    }
    assert!(config.stores.tickets.live(toolsite::state::tickets::Kind::Handoff).await.unwrap() == 0, "a code was minted for a request the visitor did not make");
}

// --- adversarial: cookies --------------------------------------------------

#[tokio::test]
async fn an_app_handler_cannot_toss_a_platform_cookie_under_any_spelling() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    let host = host_of(&config, "orders");
    for set in [
        "=ts_app=forged; Domain=apps.test; Path=/",
        "=__Host-ts_app=forged; Path=/; Secure",
        " =ts_handoff=abcdefghijklmnopqrstuvwxyz012345; Domain=apps.test",
        "__HOST-TS_APP=forged; Path=/; Secure",
        "__Secure-ts_session=forged; Domain=site.test; Secure",
        "Ts_Session=forged; Domain=test",
        "ts_app",
    ] {
        let reply = get(&config, &host, &format!("/p/orders/api/set-cookie?{}", urlencoding::encode(set)), None).await;
        assert_eq!(reply.body, "set");
        assert!(reply.cookies().is_empty(), "{set:?} reached the browser: {:?}", reply.cookies());
    }
    // An app's own cookie is the app's to set.
    let reply = get(&config, &host, &format!("/p/orders/api/set-cookie?{}", urlencoding::encode("theme=dark; Path=/p/orders/")), None).await;
    assert_eq!(reply.cookies(), vec!["theme=dark; Path=/p/orders/"]);
}

#[tokio::test]
async fn cookies_from_the_other_mode_open_nothing_after_a_switch() {
    // Path mode to subdomain mode: the bare site cookie and path-scoped app
    // cookies are no longer read anywhere.
    let (dir, config) = path_mode();
    app(&config, "members", "authenticated");
    let site_token = person(&config, "someone@example.com");
    let (_, path_app_token, _) = toolsite::accounts::users::create_app_session(&config, &site_token, "members").unwrap();
    assert_eq!(get(&config, MAIN, "/auth/me", Some(&format!("ts_session={site_token}"))).await.status, StatusCode::OK);

    let subdomain = Arc::new(Config {
        base_url: Some(BASE.to_string()),
        apps: Some(AppsDomain::parse("apps.test", Some(BASE), None).unwrap()),
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    let host = host_of(&subdomain, "members");
    assert_eq!(get(&subdomain, MAIN, "/auth/me", Some(&format!("ts_session={site_token}"))).await.status, StatusCode::UNAUTHORIZED);
    for jar in [format!("ts_app_members={path_app_token}"), format!("ts_app={path_app_token}"), format!("ts_session={site_token}")] {
        let reply = get(&subdomain, &host, "/p/members/api/whoami", Some(&jar)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{jar}");
        // On the main host the old app cookie is not read at all: the path
        // only redirects.
        let main = get(&subdomain, MAIN, "/p/members/api/whoami", Some(&jar)).await;
        assert_eq!(main.status, StatusCode::FOUND);
        assert!(!main.body.contains("someone@example.com"));
    }

    // And back: the prefixed cookies of subdomain mode mean nothing in path
    // mode, and a connector token bound to an app's host is not taken at
    // the path-mode address.
    let app_token = sign_in_on_app_host(&subdomain, "members", &site_token).await;
    let path_again = path_mode_on(dir.path());
    assert_eq!(get(&path_again, MAIN, "/auth/me", Some(&format!("__Host-ts_session={site_token}"))).await.status, StatusCode::UNAUTHORIZED);
    let reply = get(&path_again, MAIN, "/p/members/api/whoami", Some(&format!("__Host-ts_app={app_token}"))).await;
    assert_ne!(reply.status, StatusCode::OK);
    assert!(!reply.body.contains("someone@example.com"));
    toolsite::platform::manifest::apply(&path_again, "members", FARM_TOOLS).await.unwrap();
    let token = bearer(&path_again, "someone@example.com", Some(&format!("https://{host}/p/members/mcp")));
    let (status, _) = mcp_call(&path_again, MAIN, "/p/members/mcp", &token, serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

fn path_mode_on(dir: &std::path::Path) -> Arc<Config> {
    Arc::new(Config { base_url: Some(BASE.to_string()), ..Config::local(dir.to_path_buf(), TOKEN) })
}

// --- adversarial: the origin rule --------------------------------------------

#[tokio::test]
async fn a_write_without_origin_that_says_it_crossed_sites_is_refused() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    let host = host_of(&config, "orders");
    let post = |site: Option<&str>| {
        let mut request = on(&host, "POST", "/p/orders/api/echo");
        if let Some(site) = site {
            request = request.header("sec-fetch-site", site);
        }
        request.body(Body::from("x")).unwrap()
    };
    for site in ["same-site", "cross-site", "Cross-Site"] {
        assert_eq!(send(&config, post(Some(site))).await.status, StatusCode::FORBIDDEN, "{site}");
    }
    for site in [Some("same-origin"), Some("none"), None] {
        assert_eq!(send(&config, post(site)).await.body, "POST /api/echo?", "{site:?}");
    }
    // Origin decides when present, whatever fetch metadata says.
    let forged = on(&host, "POST", "/p/orders/api/echo")
        .header("origin", "https://billing.apps.test")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(send(&config, forged).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn toolsite_adds_no_cors_permission_that_would_let_a_sibling_read_an_app() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    let host = host_of(&config, "orders");
    let preflight = send(
        &config,
        on(&host, "OPTIONS", "/p/orders/api/echo")
            .header("origin", "https://billing.apps.test")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let read = send(&config, on(&host, "GET", "/p/orders/api/echo").header("origin", "https://billing.apps.test").body(Body::empty()).unwrap()).await;
    for reply in [&preflight, &read] {
        assert!(
            reply.headers.iter().all(|(name, _)| !name.starts_with("access-control-")),
            "{:?}",
            reply.headers
        );
    }
}

#[tokio::test]
async fn a_socket_without_origin_that_says_it_crossed_sites_is_refused() {
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
    let (_dir, config) = site();
    app(&config, "team", "public");
    let mut meta = toolsite::content::store::read_meta_blocking(&config, "team");
    meta.sockets = vec!["/ws".into()];
    toolsite::content::store::write_meta_blocking(&config, "team", &meta).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(config.clone(), Runtime::new().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let host = host_of(&config, "team");

    let connect = |headers: Vec<(&'static str, String)>| {
        let host = host.clone();
        async move {
            let mut request = format!("ws://{addr}/p/team/ws").into_client_request().unwrap();
            request.headers_mut().insert("host", host.parse().unwrap());
            for (name, value) in headers {
                request.headers_mut().insert(name, value.parse().unwrap());
            }
            match tokio_tungstenite::connect_async(request).await {
                Ok(_) => Ok(()),
                Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
                Err(other) => panic!("connect failed: {other}"),
            }
        }
    };
    assert_eq!(connect(vec![("sec-fetch-site", "same-site".into()), ("cookie", "__Host-ts_app=x".into())]).await, Err(403));
    assert_eq!(connect(vec![("sec-fetch-site", "cross-site".into())]).await, Err(403));
    assert_eq!(connect(vec![("origin", "null".into())]).await, Err(403));
    assert_eq!(connect(vec![]).await, Ok(()), "a client that is not a browser page");
}

// --- adversarial: connectors, tickets, the main host ---------------------------

#[tokio::test]
async fn a_token_for_one_apps_connector_is_refused_at_anothers() {
    let (_dir, config) = site();
    for name in ["farm", "barn"] {
        app(&config, name, "authenticated");
        toolsite::platform::manifest::apply(&config, name, FARM_TOOLS).await.unwrap();
    }
    person(&config, "alice@example.com");
    let farm = host_of(&config, "farm");
    let barn = host_of(&config, "barn");
    let token = bearer(&config, "alice@example.com", Some(&format!("https://{farm}/p/farm/mcp")));
    let list = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    assert_eq!(mcp_call(&config, &farm, "/p/farm/mcp", &token, list.clone()).await.0, StatusCode::OK);
    assert_eq!(mcp_call(&config, &barn, "/p/barn/mcp", &token, list.clone()).await.0, StatusCode::UNAUTHORIZED);
    // Nor at farm's path on barn's host, nor at the main host's endpoints.
    assert_eq!(mcp_call(&config, &barn, "/p/farm/mcp", &token, list.clone()).await.0, StatusCode::NOT_FOUND);
    assert_ne!(mcp_call(&config, MAIN, "/mcp", &token, list.clone()).await.0, StatusCode::OK);
    assert_ne!(mcp_call(&config, MAIN, "/me/mcp", &token, list).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_browser_upload_ticket_is_refused_on_the_main_host() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    let url = toolsite::runtime::blobs::issue_upload(&config, "orders", "files/a.txt", 0).await.unwrap();
    let path = url.split_once("/blob/").map(|(_, t)| format!("/blob/{t}")).unwrap();
    let reply = send(&config, on(MAIN, "PUT", &path).body(Body::from("x")).unwrap()).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert!(!config.data_dir.join("orders/.blobs/files/a.txt").exists());
}

#[tokio::test]
async fn an_apps_icon_on_the_main_host_cannot_run_as_the_site() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    std::fs::write(
        config.data_dir.join("orders.icon"),
        r#"<svg xmlns="http://www.w3.org/2000/svg"><script>fetch('/admin')</script></svg>"#,
    )
    .unwrap();
    let reply = get(&config, MAIN, "/icon/orders", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("image/svg+xml"));
    let policy = reply.header("content-security-policy").expect("no policy on an app's SVG");
    assert!(policy.contains("sandbox") && policy.contains("default-src 'none'"), "{policy}");
    assert_eq!(reply.header("x-content-type-options"), Some("nosniff"));
}

#[tokio::test]
async fn the_main_host_runs_no_app_code_by_any_method_or_spelling() {
    let (_dir, config) = site();
    app(&config, "orders", "public");
    for (method, uri) in [
        ("GET", "/p/orders/api/echo"),
        ("HEAD", "/p/orders/api/echo"),
        ("GET", "/p/orders.icon"),
        ("GET", "/p/orders/favicon.svg"),
        ("OPTIONS", "/p/orders/api/echo"),
        ("PATCH", "/p/orders/api/echo"),
        ("GET", "/p/%6Frders/api/echo"),
        ("GET", "/p/orders%2Fapi%2Fecho"),
        ("GET", "/p/./orders/api/echo"),
    ] {
        let reply = send(&config, on(MAIN, method, uri).body(Body::empty()).unwrap()).await;
        assert!(!reply.body.contains("/api/echo") && !reply.body.contains("orders home"), "{method} {uri}: {}", reply.body);
        assert!(reply.status == StatusCode::FOUND || reply.status == StatusCode::NOT_FOUND, "{method} {uri}: {}", reply.status);
        if let Some(location) = reply.header("location") {
            assert!(location.starts_with("https://orders.apps.test/p/orders"), "{method} {uri} -> {location}");
        }
    }
}
