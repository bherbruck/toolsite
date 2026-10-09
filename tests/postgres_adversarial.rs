//! Two routers on one Postgres database, attacked the way two runners
//! behind a balancer would be: every route that reaches accounts, OAuth or
//! tickets driven on Postgres (a store call that waits on an async worker
//! panics, which would be a denial of service), revocations made on one
//! router and tested on the other, wrong two-step codes sent to both at
//! once, and emails that differ only by case or by Unicode. Every test is
//! ignored unless asked for: `scripts/test-postgres.sh` starts a database
//! and sets TOOLSITE_TEST_DATABASE_URL.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Once,
};
use toolsite::{
    accounts::users,
    build_router,
    runtime::wasm::Runtime,
    state::{pg, Backend, Stores},
    Config,
};
use tower::ServiceExt;

const NEEDS: &str = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one";
const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const TOKEN: &str = "test-token";
const BASE: &str = "https://site.test";
const PASSWORD: &str = "correct horse battery";
const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");
const CALLBACK: &str = "https://client.test/callback";
// RFC 7636's own test vector.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// Panics anywhere in this binary, spawned tasks and blocking threads
/// included: a handler that panics is not always a failed request.
static PANICS: AtomicUsize = AtomicUsize::new(0);

fn prepare() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Sealing reads the key from the environment, as on a real
        // Postgres site. Set before any thread of this binary reads it.
        unsafe { std::env::set_var("TOOLSITE_SECRET_KEY", KEY) };
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICS.fetch_add(1, Ordering::SeqCst);
            previous(info);
        }));
    });
}

/// Two runners of one site: separate pools and processes' worth of state,
/// one database and one volume.
struct Site {
    a: Arc<Config>,
    b: Arc<Config>,
    name: String,
    _volume: tempfile::TempDir,
}

impl Site {
    async fn new() -> Site {
        prepare();
        let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
        let name = format!("t_{}", toolsite::content::slug::random_token(12));
        let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client.batch_execute(&format!("create database {name}")).await.unwrap();
        let mut url = url::Url::parse(&server).unwrap();
        url.set_path(&name);
        let volume = tempfile::tempdir().unwrap();
        let runner = |url: String, volume: std::path::PathBuf| async move {
            let postgres = pg::connect(&url, 8).await.unwrap();
            pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap();
            let stores = Stores::new(Backend::Postgres(Arc::new(postgres)), None, Some(KEY)).unwrap();
            Arc::new(Config {
                base_url: Some(BASE.to_string()),
                stores,
                ..Config::local(volume, TOKEN)
            })
        };
        let a = runner(url.to_string(), volume.path().to_path_buf()).await;
        let b = runner(url.to_string(), volume.path().to_path_buf()).await;
        Site { a, b, name, _volume: volume }
    }

    /// Runner A for even `i`, B for odd: a balancer's coin.
    fn runner(&self, i: usize) -> &Arc<Config> {
        if i.is_multiple_of(2) { &self.a } else { &self.b }
    }

    async fn finish(self) {
        for config in [&self.a, &self.b] {
            if let Backend::Postgres(postgres) = &config.stores.backend {
                postgres.pool.close();
            }
        }
        let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").unwrap();
        let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client.batch_execute(&format!("drop database if exists {} with (force)", self.name)).await.unwrap();
    }
}

/// An account function, on a blocking thread as the server runs them.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work).await.unwrap()
}

struct Reply {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .find_map(|(_, c)| c.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap_or("").to_string()))
            .filter(|value| !value.is_empty())
    }
}

async fn send(config: &Arc<Config>, method: &str, uri: &str, body: Option<(&str, String)>, headers: &[(&str, String)]) -> Reply {
    let mut request = Request::builder().method(method).uri(uri).header("host", "localhost");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let body = match body {
        Some((kind, body)) => {
            request = request.header("content-type", kind);
            Body::from(body)
        }
        None => Body::empty(),
    };
    let response = build_router(config.clone(), Runtime::new().unwrap())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    Reply { status, headers, body: String::from_utf8_lossy(&bytes).to_string() }
}

fn session(token: &str) -> Vec<(&'static str, String)> {
    vec![("cookie", format!("ts_session={token}"))]
}

fn form(body: String) -> Option<(&'static str, String)> {
    Some(("application/x-www-form-urlencoded", body))
}

fn json(body: serde_json::Value) -> Option<(&'static str, String)> {
    Some(("application/json", body.to_string()))
}

/// The JSON of an MCP reply, through rmcp's SSE framing.
fn rpc(reply: &Reply) -> serde_json::Value {
    reply
        .body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&reply.body).ok())
        .unwrap_or(serde_json::Value::Null)
}

async fn mcp(config: &Arc<Config>, path: &str, bearer: &str, tool: &str, arguments: serde_json::Value) -> Reply {
    let headers = [
        ("authorization", format!("Bearer {bearer}")),
        ("accept", "application/json, text/event-stream".to_string()),
    ];
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
    send(config, "POST", path, json(body), &headers).await
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| urlencoding::decode(v).unwrap().into_owned())
}

/// A gated app with a handler, on the shared volume.
fn app(config: &Config, name: &str) {
    let dir = config.data_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), format!("<title>{name}</title><h1>{name} home</h1>")).unwrap();
    std::fs::write(dir.join("handler.wasm"), HANDLER).unwrap();
    let mut meta = toolsite::content::catalog::meta_blocking(config, name);
    meta.gate = Some("authenticated".to_string());
    toolsite::content::catalog::update_meta_blocking(config, name, { let meta = meta.clone(); move |stored| { *stored = meta; Ok(()) } }).unwrap();
}

/// Registers an MCP client on one runner, has `who` consent on the other,
/// and exchanges the code on the first. Returns the client and its tokens.
async fn connect_client(site: &Site, who: &str, resource: &str) -> (String, String, String) {
    let registered = send(
        &site.a,
        "POST",
        "/register",
        json(serde_json::json!({"client_name":"t","redirect_uris":[CALLBACK],"token_endpoint_auth_method":"none"})),
        &[],
    )
    .await;
    assert_eq!(registered.status, StatusCode::CREATED, "{}", registered.body);
    let client: serde_json::Value = serde_json::from_str(&registered.body).unwrap();
    let client = client["client_id"].as_str().unwrap().to_string();
    let ask = format!(
        "response_type=code&client_id={client}&redirect_uri={}&state=xyz&code_challenge={CHALLENGE}&code_challenge_method=S256&resource={}",
        urlencoding::encode(CALLBACK),
        urlencoding::encode(resource),
    );
    let page = send(&site.b, "GET", &format!("/authorize?{ask}"), None, &session(who)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let token = page.body.split("name=\"token\" value=\"").nth(1).and_then(|rest| rest.split('"').next()).unwrap();
    let decided = send(&site.a, "POST", "/authorize", form(format!("token={token}&{ask}&decision=allow")), &session(who)).await;
    assert_eq!(decided.status, StatusCode::SEE_OTHER, "{}", decided.body);
    let code = query_param(decided.header("location").unwrap(), "code").expect("no code");
    let exchanged = send(
        &site.b,
        "POST",
        "/token",
        form(format!(
            "grant_type=authorization_code&client_id={client}&code={code}&redirect_uri={}&code_verifier={VERIFIER}",
            urlencoding::encode(CALLBACK)
        )),
        &[],
    )
    .await;
    assert_eq!(exchanged.status, StatusCode::OK, "{}", exchanged.body);
    let tokens: serde_json::Value = serde_json::from_str(&exchanged.body).unwrap();
    (
        client,
        tokens["access_token"].as_str().unwrap().to_string(),
        tokens["refresh_token"].as_str().unwrap().to_string(),
    )
}

async fn refresh(config: &Arc<Config>, client: &str, refresh_token: &str) -> Reply {
    send(
        config,
        "POST",
        "/token",
        form(format!("grant_type=refresh_token&client_id={client}&refresh_token={refresh_token}")),
        &[],
    )
    .await
}

/// Every route that reaches accounts, OAuth or tickets, on Postgres, half
/// on each runner. A store call made on an async worker panics in
/// `state::wait`; none may, and none may answer 500.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn every_account_oauth_and_ticket_route_answers_without_panicking_on_postgres() {
    let site = Site::new().await;
    let before = PANICS.load(Ordering::SeqCst);
    app(&site.a, "ledger");
    let config = site.a.clone();
    let (boss, boss_session, reader, reader_session, invite) = blocking(move || {
        let boss = users::sign_up_as(&config, "boss@example.com", PASSWORD, true).unwrap();
        let reader = users::sign_up(&config, "reader@example.com", PASSWORD).unwrap();
        users::grant(&config, "reader@example.com", "ledger", "viewer").unwrap();
        let (_, boss_session) = users::log_in(&config, "boss@example.com", PASSWORD).unwrap();
        let (_, reader_session) = users::log_in(&config, "reader@example.com", PASSWORD).unwrap();
        let (_, invite) = users::invite(&config, "invited@example.com", false).unwrap();
        (boss, boss_session, reader, reader_session, invite)
    })
    .await;
    let token = users::derive_form_token(&site.a, &boss.id);
    let reader_token = users::derive_form_token(&site.a, &reader.id);
    let (client, access, refresh_token) = connect_client(&site, &boss_session, &format!("{BASE}/mcp")).await;

    type Body = Option<(&'static str, String)>;
    type Route = (&'static str, String, Body, Vec<(&'static str, String)>);
    let boss_cookie = session(&boss_session);
    let reader_cookie = session(&reader_session);
    let bearer = vec![
        ("authorization", format!("Bearer {TOKEN}")),
        ("accept", "application/json, text/event-stream".to_string()),
    ];
    let oauth_bearer = vec![
        ("authorization", format!("Bearer {access}")),
        ("accept", "application/json, text/event-stream".to_string()),
    ];
    let tool = |name: &str, arguments: serde_json::Value| -> Body {
        json(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
    };
    let initialize = json(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}));
    let routes: Vec<Route> = vec![
        ("GET", "/".into(), None, boss_cookie.clone()),
        ("GET", "/browse/ledger".into(), None, boss_cookie.clone()),
        ("GET", "/p/ledger/".into(), None, reader_cookie.clone()),
        ("GET", "/p/ledger/api/whoami".into(), None, reader_cookie.clone()),
        ("GET", "/icon/ledger".into(), None, reader_cookie.clone()),
        ("GET", "/auth/login".into(), None, boss_cookie.clone()),
        ("POST", "/auth/login".into(), form(format!("email=reader@example.com&password={}", urlencoding::encode(PASSWORD))), vec![]),
        ("POST", "/auth/login".into(), form("email=reader@example.com&password=wrong".into()), vec![]),
        ("GET", "/auth/me".into(), None, reader_cookie.clone()),
        ("GET", "/auth/setup?token=".to_string() + &invite, None, vec![]),
        ("GET", "/auth/handoff?app=ledger".into(), None, reader_cookie.clone()),
        ("GET", "/auth/landing?code=nothing".into(), None, vec![]),
        ("GET", "/auth/mfa".into(), None, vec![("cookie", "ts_mfa=nothing".to_string())]),
        ("POST", "/auth/mfa".into(), form("code=123456".into()), vec![("cookie", "ts_mfa=nothing".to_string())]),
        ("GET", "/auth/mfa/setup".into(), None, vec![("cookie", "ts_mfa=nothing".to_string())]),
        ("GET", "/auth/login/github".into(), None, vec![]),
        ("GET", "/auth/callback/github?state=nothing&code=x".into(), None, vec![]),
        ("GET", "/account".into(), None, reader_cookie.clone()),
        ("POST", "/account/mfa/start".into(), form(format!("token={reader_token}")), reader_cookie.clone()),
        ("POST", "/account/mfa/confirm".into(), form(format!("token={reader_token}&code=000000&password=x")), reader_cookie.clone()),
        ("POST", "/account/mfa/cancel".into(), form(format!("token={reader_token}")), reader_cookie.clone()),
        ("POST", "/account/mfa/recovery".into(), form(format!("token={reader_token}&code=000000")), reader_cookie.clone()),
        ("POST", "/account/mfa/off".into(), form(format!("token={reader_token}&code=000000")), reader_cookie.clone()),
        ("GET", "/admin".into(), None, boss_cookie.clone()),
        ("GET", "/admin/apps".into(), None, boss_cookie.clone()),
        ("GET", "/admin/apps/ledger".into(), None, boss_cookie.clone()),
        ("GET", "/admin/apps/ledger/access".into(), None, boss_cookie.clone()),
        ("GET", "/admin/apps/ledger/settings".into(), None, boss_cookie.clone()),
        ("GET", "/admin/apps/ledger/exports".into(), None, boss_cookie.clone()),
        ("GET", "/admin/apps/search?q=led".into(), None, boss_cookie.clone()),
        ("GET", "/admin/accounts".into(), None, boss_cookie.clone()),
        ("GET", "/admin/accounts/new".into(), None, boss_cookie.clone()),
        ("GET", "/admin/accounts/search?q=read".into(), None, boss_cookie.clone()),
        ("GET", "/admin/accounts/reader@example.com".into(), None, boss_cookie.clone()),
        ("GET", "/admin/permissions/candidates?q=re&path=ledger".into(), None, boss_cookie.clone()),
        ("GET", "/admin/projects/search?q=x".into(), None, boss_cookie.clone()),
        ("GET", "/admin/exports".into(), None, boss_cookie.clone()),
        ("POST", "/admin/users".into(), form(format!("token={token}&email=New@Example.com&mode=invite")), boss_cookie.clone()),
        ("POST", "/admin/users".into(), form(format!("token={token}&email=pw@example.com&mode=password&password={}", urlencoding::encode(PASSWORD))), boss_cookie.clone()),
        ("POST", "/admin/reinvite".into(), form(format!("token={token}&email=invited@example.com")), boss_cookie.clone()),
        ("POST", "/admin/access".into(), form(format!("token={token}&app=ledger&email=pw@example.com&allow=1&role=editor")), boss_cookie.clone()),
        ("POST", "/admin/access".into(), form(format!("token={token}&app=ledger&email=pw@example.com&allow=0")), boss_cookie.clone()),
        ("POST", "/admin/active".into(), form(format!("token={token}&email=pw@example.com&active=0")), boss_cookie.clone()),
        ("POST", "/admin/active".into(), form(format!("token={token}&email=pw@example.com&active=1")), boss_cookie.clone()),
        ("POST", "/admin/mfa-reset".into(), form(format!("token={token}&email=reader@example.com")), boss_cookie.clone()),
        ("POST", "/admin/gate".into(), form(format!("token={token}&app=ledger&gate=granted")), boss_cookie.clone()),
        ("POST", "/admin/rule".into(), form(format!("token={token}&app=ledger&action=add&prefix=api&gate=public")), boss_cookie.clone()),
        ("POST", "/admin/visibility".into(), form(format!("token={token}&app=ledger&listed=1")), boss_cookie.clone()),
        ("POST", "/admin/notes".into(), form(format!("token={token}&app=ledger&notes=hello")), boss_cookie.clone()),
        ("POST", "/admin/settings-link".into(), form(format!("token={token}&app=ledger")), boss_cookie.clone()),
        ("POST", "/admin/scope".into(), form(format!("token={token}&action=grant&prefix=ops&email=reader@example.com&scope=editor")), boss_cookie.clone()),
        ("POST", "/admin/scope".into(), form(format!("token={token}&action=revoke&prefix=ops&email=reader@example.com")), boss_cookie.clone()),
        ("POST", "/admin/permissions/cell".into(), form(format!("token={token}&path=ledger&app=ledger&email=reader@example.com&level=edit")), boss_cookie.clone()),
        ("POST", "/admin/permissions/add".into(), form(format!("token={token}&path=ledger&app=ledger&email=pw@example.com&level=view")), boss_cookie.clone()),
        ("POST", "/admin/permissions/lock".into(), form(format!("token={token}&path=ledger&locked=1")), boss_cookie.clone()),
        ("POST", "/admin/folder".into(), form(format!("token={token}&parent=&name=ops")), boss_cookie.clone()),
        ("POST", "/admin/project".into(), form(format!("token={token}&action=rename&path=ops&name=labs")), boss_cookie.clone()),
        ("POST", "/admin/move".into(), form(format!("token={token}&app=ledger&folder=labs")), boss_cookie.clone()),
        ("POST", "/admin/pin".into(), form(format!("token={token}&app=ledger&pinned=1")), boss_cookie.clone()),
        ("POST", "/admin/exports".into(), form(format!("token={token}&action=create&app=ledger&label=r")), boss_cookie.clone()),
        ("POST", "/admin/devices".into(), form(format!("token={token}&action=create&app=ledger&label=d")), boss_cookie.clone()),
        ("GET", "/settings/nothing".into(), None, vec![]),
        ("GET", "/preview/nothing".into(), None, vec![]),
        ("PUT", "/upload/nothing".into(), Some(("text/html", "<h1>x</h1>".into())), vec![]),
        ("PUT", "/blob/nothing".into(), Some(("text/plain", "x".into())), vec![]),
        ("GET", "/export/ledger.sqlite".into(), None, vec![("authorization", "Bearer nothing".to_string())]),
        ("PUT", "/deploy/ledger".into(), Some(("text/html", "<h1>x</h1>".into())), vec![("authorization", "Bearer nothing".to_string())]),
        ("GET", "/.well-known/oauth-authorization-server".into(), None, vec![]),
        ("POST", "/token".into(), form(format!("grant_type=refresh_token&client_id={client}&refresh_token=nothing")), vec![]),
        ("POST", "/mcp".into(), initialize.clone(), bearer.clone()),
        ("POST", "/mcp".into(), initialize.clone(), oauth_bearer.clone()),
        ("POST", "/mcp".into(), tool("create_upload", serde_json::json!({"slug":"page-one","kind":"page"})), bearer.clone()),
        ("POST", "/mcp".into(), tool("upload_begin", serde_json::json!({"slug":"page-two","kind":"page"})), bearer.clone()),
        ("POST", "/mcp".into(), tool("create_user", serde_json::json!({"email":"Mcp@Example.com"})), bearer.clone()),
        ("POST", "/mcp".into(), tool("set_user_active", serde_json::json!({"email":"mcp@example.com","active":false})), bearer.clone()),
        ("POST", "/mcp".into(), tool("set_access", serde_json::json!({"app":"ledger","email":"reader@example.com","allow":true})), bearer.clone()),
        ("POST", "/mcp".into(), tool("app_settings", serde_json::json!({"app":"ledger","action":"link"})), bearer.clone()),
        ("POST", "/mcp".into(), tool("app_exports", serde_json::json!({"app":"ledger","action":"list"})), bearer.clone()),
        ("POST", "/mcp".into(), tool("app_deploy_tokens", serde_json::json!({"app":"ledger","action":"list"})), bearer.clone()),
        ("POST", "/mcp".into(), tool("projects", serde_json::json!({"action":"tree"})), bearer.clone()),
        ("POST", "/mcp".into(), tool("list_pages", serde_json::json!({})), oauth_bearer.clone()),
        ("POST", "/mcp".into(), tool("pin_app", serde_json::json!({"app":"ledger","pinned":true})), oauth_bearer.clone()),
        ("POST", "/mcp".into(), tool("screenshot", serde_json::json!({"slug":"ledger"})), oauth_bearer.clone()),
        ("POST", "/me/mcp".into(), initialize.clone(), oauth_bearer.clone()),
        ("POST", "/p/ledger/mcp".into(), initialize.clone(), oauth_bearer.clone()),
        ("POST", "/auth/logout".into(), None, reader_cookie.clone()),
    ];

    let mut failures = Vec::new();
    for (i, (method, uri, body, headers)) in routes.into_iter().enumerate() {
        let panics = PANICS.load(Ordering::SeqCst);
        let reply = send(site.runner(i), method, &uri, body, &headers).await;
        let rpc_error = rpc(&reply)["error"]["message"].as_str().unwrap_or("").contains("panic");
        if reply.status == StatusCode::INTERNAL_SERVER_ERROR || PANICS.load(Ordering::SeqCst) != panics || rpc_error {
            failures.push(format!("{method} {uri} -> {}: {:.300}", reply.status, reply.body));
        }
    }
    // The flows those routes begin, each leg on the other runner from the
    // one before: an app session through the handoff, an upload URL, and an
    // inline upload in chunks.
    let (_, reader_session) = blocking({
        let config = site.a.clone();
        move || users::log_in(&config, "reader@example.com", PASSWORD).unwrap()
    })
    .await;
    let handoff = send(&site.a, "GET", "/auth/handoff?app=ledger&next=%2Fp%2Fledger%2F", None, &session(&reader_session)).await;
    let landed = handoff.header("location").unwrap_or_default().to_string();
    let landing = send(&site.b, "GET", &landed, None, &session(&reader_session)).await;
    let app_cookie = landing.cookie("ts_app_ledger").or_else(|| handoff.cookie("ts_app_ledger"));
    match app_cookie {
        Some(app) => {
            let cookie = [("cookie", format!("ts_app_ledger={app}"))];
            let page = send(&site.a, "GET", "/p/ledger/", None, &cookie).await;
            if page.status != StatusCode::OK || !page.body.contains("ledger home") {
                failures.push(format!("app session from B refused on A -> {}: {:.200}", page.status, page.body));
            }
            // The handler asks who is calling and with which role, through
            // host calls that wait on the account store.
            let role = send(&site.b, "GET", "/p/ledger/api/myrole", None, &cookie).await;
            if role.status != StatusCode::OK || role.body != "viewer" {
                failures.push(format!("the handler's role on B -> {}: {:.200}", role.status, role.body));
            }
            let minted = send(&site.a, "GET", "/p/ledger/api/blob-upload-url?key=notes.txt&max=1024", None, &cookie).await;
            let path = minted.body.split_once("/blob/").map(|(_, rest)| format!("/blob/{rest}")).unwrap_or_default();
            let put = send(&site.b, "PUT", &path, Some(("text/plain", "hello".into())), &[]).await;
            let again = send(&site.a, "PUT", &path, Some(("text/plain", "again".into())), &[]).await;
            if minted.status != StatusCode::OK || !put.status.is_success() || again.status.is_success() {
                failures.push(format!(
                    "a browser upload URL minted on A: {} {:.120}, used on B: {} {:.120}, again on A: {}",
                    minted.status, minted.body, put.status, put.body, again.status
                ));
            }
        }
        None => failures.push(format!("no app session: {} {:?} then {} {:.200}", handoff.status, handoff.header("location"), landing.status, landing.body)),
    }
    let upload = rpc(&mcp(&site.a, "/mcp", TOKEN, "create_upload", serde_json::json!({"slug":"page-three","kind":"page"})).await);
    let text = upload["result"]["content"][0]["text"].as_str().unwrap_or_default();
    let url = text.split_whitespace().find(|word| word.starts_with(&format!("{BASE}/upload/"))).unwrap_or_default();
    let put = send(&site.b, "PUT", url.trim_start_matches(BASE), Some(("text/html", "<h1>three</h1>".into())), &[]).await;
    if !put.status.is_success() {
        failures.push(format!("an upload URL from A refused on B -> {}: {}", put.status, put.body));
    }
    let begun = rpc(&mcp(&site.b, "/mcp", TOKEN, "upload_begin", serde_json::json!({"slug":"page-four","kind":"page"})).await);
    let text = begun["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    let id = text.split_whitespace().nth(1).unwrap_or_default();
    let data = data_encoding::BASE64.encode(b"<h1>four</h1>");
    let chunk = rpc(&mcp(&site.a, "/mcp", TOKEN, "upload_chunk", serde_json::json!({"id":id,"index":0,"data":data})).await);
    let done = rpc(&mcp(&site.b, "/mcp", TOKEN, "upload_finish", serde_json::json!({"id":id,"chunks":1})).await);
    if chunk["result"]["isError"] != false || done["result"]["isError"] != false {
        failures.push(format!("an inline upload across runners failed: {chunk} then {done}"));
    }
    let four = send(&site.a, "GET", "/p/page-four", None, &[]).await;
    if !four.body.contains("four") {
        failures.push(format!("the inline upload did not publish -> {}: {:.200}", four.status, four.body));
    }

    // A refresh on the runner that did not issue the token, as a balancer
    // would send it.
    let refreshed = refresh(&site.b, &client, &refresh_token).await;
    if refreshed.status != StatusCode::OK {
        failures.push(format!("refresh on B -> {}: {}", refreshed.status, refreshed.body));
    }
    assert!(failures.is_empty(), "routes that failed on Postgres:\n{}", failures.join("\n"));
    assert_eq!(PANICS.load(Ordering::SeqCst), before, "something panicked on Postgres");
    site.finish().await;
}

/// What one runner revokes, the other stops honouring at its next request:
/// a password changed on B ends A's other sessions, its app sessions and
/// the MCP clients the account connected; an account disabled on A is
/// refused on B. Nothing about a session is remembered in a process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_revocation_on_one_runner_holds_on_the_other_on_postgres() {
    let site = Site::new().await;
    app(&site.a, "ledger");
    let config = site.a.clone();
    let (ann, keep, other, app_session) = blocking(move || {
        let ann = users::sign_up_as(&config, "ann@example.com", PASSWORD, true).unwrap();
        users::sign_up(&config, "bo@example.com", PASSWORD).unwrap();
        let (_, keep) = users::log_in(&config, "ann@example.com", PASSWORD).unwrap();
        let (_, other) = users::log_in(&config, "ann@example.com", PASSWORD).unwrap();
        let (_, app_session, _) = users::create_app_session(&config, &other, "ledger").unwrap();
        (ann, keep, other, app_session)
    })
    .await;
    let (client, access, refresh_token) = connect_client(&site, &other, &format!("{BASE}/mcp")).await;
    let app_cookie = [("cookie", format!("ts_app_ledger={app_session}"))];
    // Warm both runners: each has seen every credential work.
    for config in [&site.a, &site.b] {
        assert_eq!(send(config, "GET", "/auth/me", None, &session(&other)).await.status, StatusCode::OK);
        assert_eq!(send(config, "GET", "/p/ledger/", None, &app_cookie).await.status, StatusCode::OK);
        assert_eq!(mcp(config, "/mcp", &access, "list_pages", serde_json::json!({})).await.status, StatusCode::OK);
    }

    let token = users::derive_form_token(&site.b, &ann.id);
    let new = "a different long password";
    let changed = send(
        &site.b,
        "POST",
        "/account/password",
        form(format!("token={token}&current={}&new={}&confirm={}", urlencoding::encode(PASSWORD), urlencoding::encode(new), urlencoding::encode(new))),
        &session(&keep),
    )
    .await;
    assert_eq!(changed.status, StatusCode::SEE_OTHER, "{}", changed.body);
    for config in [&site.a, &site.b] {
        assert_ne!(send(config, "GET", "/auth/me", None, &session(&other)).await.status, StatusCode::OK, "an old session outlived a password change");
        assert_ne!(send(config, "GET", "/p/ledger/", None, &app_cookie).await.status, StatusCode::OK, "an app session outlived a password change");
        assert_eq!(mcp(config, "/mcp", &access, "list_pages", serde_json::json!({})).await.status, StatusCode::UNAUTHORIZED, "a client outlived a password change");
        assert_eq!(send(config, "GET", "/auth/me", None, &session(&keep)).await.status, StatusCode::OK, "the session that changed it ended");
    }
    assert_ne!(refresh(&site.a, &client, &refresh_token).await.status, StatusCode::OK, "a refresh token outlived a password change");

    let (_, bo) = blocking({
        let config = site.b.clone();
        move || users::log_in(&config, "bo@example.com", PASSWORD).unwrap()
    })
    .await;
    assert_eq!(send(&site.b, "GET", "/auth/me", None, &session(&bo)).await.status, StatusCode::OK);
    let token = users::derive_form_token(&site.a, &ann.id);
    let disabled = send(&site.a, "POST", "/admin/active", form(format!("token={token}&email=bo@example.com&active=0")), &session(&keep)).await;
    assert_eq!(disabled.status, StatusCode::SEE_OTHER, "{}", disabled.body);
    assert_ne!(send(&site.b, "GET", "/auth/me", None, &session(&bo)).await.status, StatusCode::OK, "a disabled account's session worked on the other runner");
    let again = send(&site.b, "POST", "/auth/login", form(format!("email=bo@example.com&password={}", urlencoding::encode(PASSWORD))), &[]).await;
    assert!(again.cookie("ts_session").is_none(), "a disabled account signed in on the other runner");
    site.finish().await;
}

/// Refresh tokens and codes presented to both runners at once: one wins,
/// whichever runner it reaches.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_refresh_token_sent_to_both_runners_at_once_rotates_once_on_postgres() {
    let site = Arc::new(Site::new().await);
    let config = site.a.clone();
    let boss = blocking(move || {
        users::sign_up_as(&config, "boss@example.com", PASSWORD, true).unwrap();
        users::log_in(&config, "boss@example.com", PASSWORD).unwrap().1
    })
    .await;
    let (client, _, mut refresh_token) = connect_client(&site, &boss, &format!("{BASE}/mcp")).await;
    for round in 0..5 {
        let start = Arc::new(tokio::sync::Barrier::new(8));
        let racers: Vec<_> = (0..8)
            .map(|i| {
                let (site, start, client, token) = (site.clone(), start.clone(), client.clone(), refresh_token.clone());
                tokio::spawn(async move {
                    start.wait().await;
                    let reply = refresh(site.runner(i), &client, &token).await;
                    (reply.status == StatusCode::OK).then(|| serde_json::from_str::<serde_json::Value>(&reply.body).unwrap())
                })
            })
            .collect();
        let mut won = Vec::new();
        for racer in racers {
            won.extend(racer.await.unwrap());
        }
        assert_eq!(won.len(), 1, "round {round}: one refresh token rotated {} times across runners", won.len());
        refresh_token = won[0]["refresh_token"].as_str().unwrap().to_string();
    }
    Arc::try_unwrap(site).ok().unwrap().finish().await;
}

/// Wrong two-step codes sent to both runners at once are checked no more
/// often than one runner checking them in turn would allow: the limits
/// live in the database and are counted before a code is checked.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn wrong_codes_sent_to_both_runners_at_once_are_checked_within_the_limits_on_postgres() {
    let site = Arc::new(Site::new().await);
    let config = site.a.clone();
    let (cy, secret) = blocking(move || {
        let cy = users::sign_up(&config, "cy@example.com", PASSWORD).unwrap();
        let (_, session) = users::log_in(&config, "cy@example.com", PASSWORD).unwrap();
        let now = config.mfa.clock.now();
        let secret = toolsite::accounts::mfa::begin_setup(&config, &cy.id, &session).unwrap();
        toolsite::accounts::mfa::confirm_setup(&config, &cy, &toolsite::accounts::mfa::code_at(&secret, now).unwrap(), &session).unwrap();
        (cy, secret)
    })
    .await;
    let now = site.a.mfa.clock.now();
    let accepted: Vec<String> = [now - 30, now, now + 30].iter().map(|t| toolsite::accounts::mfa::code_at(&secret, *t).unwrap()).collect();
    let wrong = (0..).map(|n: u32| format!("{:06}", (n * 7919) % 1_000_000)).find(|c| !accepted.contains(c)).unwrap();
    let password = || form(format!("email=cy@example.com&password={}", urlencoding::encode(PASSWORD)));
    let pending = |reply: &Reply| reply.cookie("ts_mfa").expect("no code asked");
    let burst = |pending: String| {
        let (site, wrong) = (site.clone(), wrong.clone());
        async move {
            let sent: Vec<_> = (0..40)
                .map(|i| {
                    let (site, pending, wrong) = (site.clone(), pending.clone(), wrong.clone());
                    tokio::spawn(async move {
                        let cookie = [("cookie", format!("ts_mfa={pending}"))];
                        send(site.runner(i), "POST", "/auth/mfa", form(format!("code={wrong}")), &cookie).await.status
                    })
                })
                .collect();
            for request in sent {
                assert_ne!(request.await.unwrap(), StatusCode::SEE_OTHER);
            }
        }
    };
    let checked = || {
        let (config, id) = (site.a.clone(), cy.id.clone());
        blocking(move || toolsite::accounts::store::of(&config).failures_since(&id, 0).unwrap())
    };

    burst(pending(&send(&site.a, "POST", "/auth/login", password(), &[]).await)).await;
    let one = checked().await;
    assert!(one <= 5, "one sign-in had {one} codes checked across two runners");

    let mut pendings = Vec::new();
    for i in 0..3 {
        pendings.push(pending(&send(site.runner(i), "POST", "/auth/login", password(), &[]).await));
    }
    let bursts: Vec<_> = pendings.into_iter().map(|pending| tokio::spawn(burst(pending))).collect();
    for b in bursts {
        b.await.unwrap();
    }
    let all = checked().await;
    assert!(all <= 10, "the account had {all} codes checked across two runners");
    let last = pending(&send(&site.b, "POST", "/auth/login", password(), &[]).await);
    let right = toolsite::accounts::mfa::code_at(&secret, now + 30).unwrap();
    let refused = send(&site.a, "POST", "/auth/mfa", form(format!("code={right}")), &[("cookie", format!("ts_mfa={last}"))]).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS, "{}", refused.body);
    Arc::try_unwrap(site).ok().unwrap().finish().await;
}

/// An email that differs only by case, surrounding space or a Unicode
/// letter whose lower case is ASCII names the same account on Postgres, as
/// it does on SQLite: never a second account beside the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_email_differing_only_by_case_is_one_account_on_postgres() {
    let site = Site::new().await;
    let (a, b) = (site.a.clone(), site.b.clone());
    blocking(move || {
        let first = users::sign_up(&a, "Kay@Example.COM", PASSWORD).unwrap();
        for twin in ["kay@example.com", " KAY@EXAMPLE.COM ", "\u{212A}ay@example.com"] {
            assert!(users::sign_up(&b, twin, PASSWORD).is_err(), "{twin:?} became a second account");
            assert!(users::invite(&b, twin, false).is_err(), "{twin:?} was invited as a second account");
            assert_eq!(users::log_in(&a, twin, PASSWORD).map(|(user, _)| user.id).ok(), Some(first.id.clone()), "{twin:?}");
        }
        let listed = users::list_accounts(&b).unwrap();
        assert_eq!(listed.iter().filter(|row| row.email.eq_ignore_ascii_case("kay@example.com")).count(), 1, "the list showed a twin");
        // A NUL is refused, never cut short into a lookalike.
        assert!(users::sign_up(&a, "kay@example.com\0x", PASSWORD).is_err());
        assert!(users::log_in(&a, "kay@example.com\0", PASSWORD).is_err());
    })
    .await;
    site.finish().await;
}
