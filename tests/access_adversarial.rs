//! Attacks on projects, scopes, the permissions grid, the app browser, the
//! MCP tools' scope checks, uploads that outlive the scope that minted them,
//! preview sign-in and the pictures of restricted apps.
//!
//! Every test is an attempt by someone who should not get through. Each is
//! named by the property that holds, so a failure says what broke.
//!
//! The cast, set up by `world`:
//! - `boss`: site admin.
//! - `mgr`: Manage on `ops`.
//! - `sub`: Manage on `ops/warehouse`.
//! - `ed`: Edit on `ops/warehouse`.
//! - `fin`: View on `finance`.
//! - `nobody`: an account with nothing.
//!
//! Apps: `yard` in `ops/warehouse`, `dock` in `ops`, `ledger` in `finance`,
//! `rootapp` at the top. All restricted.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::Arc;
use tempfile::TempDir;
use toolsite::{
    accounts::users::{self, Scope},
    build_router,
    content::store,
    runtime::wasm::Runtime,
    Config,
};
use tower::ServiceExt;

const TOKEN: &str = "test-token";
const PW: &str = "correct horse battery";

// --- plumbing ---------------------------------------------------------------

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String, Vec<(String, String)>) {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string(), headers)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn get_as(uri: &str, session: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", format!("ts_session={session}"))
        .body(Body::empty())
        .unwrap()
}

fn post_as(uri: &str, session: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("cookie", format!("ts_session={session}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

fn put(uri: &str, body: &str) -> Request<Body> {
    Request::builder().method("PUT").uri(uri).body(Body::from(body.to_string())).unwrap()
}

fn write_page(config: &Config, slug: &str, html: &str) {
    let path = config.data_dir.join(format!("{slug}.html"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, html).unwrap();
}

async fn place_app(config: &Config, app: &str, project: &str, gate: &str) {
    write_page(config, &format!("{app}/index"), &format!("<title>{app} title</title><p>{app} body</p>"));
    let mut meta = store::read_meta(config, app).await;
    meta.project = (!project.is_empty()).then(|| project.to_string());
    meta.gate = Some(gate.to_string());
    store::write_meta(config, app, &meta).await.unwrap();
}

fn session(config: &Arc<Config>, email: &str) -> String {
    users::log_in(config, email, PW).unwrap().1
}

fn user(config: &Config, email: &str) -> users::User {
    users::user_by_email(config, email).unwrap()
}

fn token_for(config: &Config, email: &str) -> String {
    let client = toolsite::platform::oauth_store::register_client(config, Some("t"), &["https://c.test/cb".into()]).unwrap();
    toolsite::platform::oauth_store::issue_tokens(config, &client.id, &user(config, email).id)
        .unwrap()
        .access_token
}

fn form_token(config: &Config, email: &str) -> String {
    users::derive_form_token(config, &user(config, email).id)
}

struct World {
    _dir: TempDir,
    config: Arc<Config>,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    // OAuth tokens are only honoured where the site knows its own address.
    let config = Arc::new(Config {
        base_url: Some("https://site.test".to_string()),
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    users::sign_up_as(&config, "boss@x.test", PW, true).unwrap();
    for who in ["mgr", "sub", "ed", "fin", "nobody"] {
        users::sign_up(&config, &format!("{who}@x.test"), PW).unwrap();
    }
    store::create_folder(&config, "", "ops").await.unwrap();
    store::create_folder(&config, "ops", "warehouse").await.unwrap();
    store::create_folder(&config, "", "finance").await.unwrap();
    users::grant_scope(&config, "mgr@x.test", "ops", Scope::Admin, None).unwrap();
    users::grant_scope(&config, "sub@x.test", "ops/warehouse", Scope::Admin, None).unwrap();
    users::grant_scope(&config, "ed@x.test", "ops/warehouse", Scope::Editor, None).unwrap();
    users::grant_scope(&config, "fin@x.test", "finance", Scope::Viewer, None).unwrap();
    place_app(&config, "yard", "ops/warehouse", "restricted").await;
    place_app(&config, "dock", "ops", "restricted").await;
    place_app(&config, "ledger", "finance", "restricted").await;
    place_app(&config, "rootapp", "", "restricted").await;
    World { _dir: dir, config }
}

fn held(config: &Config, email: &str, path: &str) -> Option<Scope> {
    let locks = store::locked_prefixes_blocking(config);
    users::effective_scope(config, &user(config, email), path, &locks)
}

async fn mcp_post(config: &Arc<Config>, path: &str, token: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Calls one tool, initialising first. Returns (is_error, text).
async fn tool(config: &Arc<Config>, path: &str, token: &str, name: &str, arguments: serde_json::Value) -> (bool, String) {
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    let (status, json) = mcp_post(config, path, token, init).await;
    assert_eq!(status, StatusCode::OK, "initialize: {json}");
    let call = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}});
    let (status, json) = mcp_post(config, path, token, call).await;
    assert_eq!(status, StatusCode::OK, "{name}: {json}");
    let result = &json["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false) || json.get("error").is_some();
    let text = result["content"][0]["text"].as_str().map(str::to_string).unwrap_or_else(|| json.to_string());
    (is_error, text)
}

fn cell(token: &str, path: &str, email: &str, level: &str) -> String {
    format!(
        "token={token}&path={}&email={}&level={level}&confirm=1",
        urlencoding::encode(path),
        urlencoding::encode(email)
    )
}

// --- the grid's actions -----------------------------------------------------------

#[tokio::test]
async fn a_manager_cannot_change_permissions_outside_its_project_by_any_spelling() {
    let w = world().await;
    let s = session(&w.config, "mgr@x.test");
    let t = form_token(&w.config, "mgr@x.test");
    for path in ["finance", "/finance/", "finance/", "", "/", "ops/../finance", "../finance", "%2e%2e/finance", "Finance", "ops/..", "."] {
        let (status, body, _) = send(&w.config, post_as("/admin/permissions/cell", &s, cell(&t, path, "nobody@x.test", "admin"))).await;
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::BAD_REQUEST,
            "{path:?} answered {status}: {body}"
        );
    }
    assert_eq!(held(&w.config, "nobody@x.test", "finance"), None);
    assert_eq!(held(&w.config, "nobody@x.test", ""), None, "someone was given the whole site");
}

#[tokio::test]
async fn a_forged_or_borrowed_form_token_changes_nothing() {
    let w = world().await;
    let mgr = session(&w.config, "mgr@x.test");
    // The site admin's own token, presented with the manager's session.
    let boss_token = form_token(&w.config, "boss@x.test");
    for token in ["", "guess", boss_token.as_str()] {
        let (status, ..) = send(&w.config, post_as("/admin/permissions/cell", &mgr, cell(token, "ops", "nobody@x.test", "viewer"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "token {token:?} was accepted");
    }
    assert_eq!(held(&w.config, "nobody@x.test", "ops"), None);
}

#[tokio::test]
async fn an_editor_cannot_raise_itself_or_anyone() {
    let w = world().await;
    let s = session(&w.config, "ed@x.test");
    let t = form_token(&w.config, "ed@x.test");
    for (path, email) in [("ops/warehouse", "ed@x.test"), ("ops/warehouse", "nobody@x.test"), ("ops/warehouse/yard", "ed@x.test")] {
        let (status, ..) = send(&w.config, post_as("/admin/permissions/cell", &s, cell(&t, path, email, "admin"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "an editor changed {email} at {path}");
    }
    let add = format!("token={t}&path=ops/warehouse&scope=admin&email=ed%40x.test");
    let (status, ..) = send(&w.config, post_as("/admin/permissions/add", &s, add)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(held(&w.config, "ed@x.test", "ops/warehouse"), Some(Scope::Editor));
}

#[tokio::test]
async fn a_sub_manager_cannot_touch_the_row_that_comes_from_above() {
    let w = world().await;
    let s = session(&w.config, "sub@x.test");
    let t = form_token(&w.config, "sub@x.test");
    // Straight at the parent: refused.
    let (status, ..) = send(&w.config, post_as("/admin/permissions/cell", &s, cell(&t, "ops", "mgr@x.test", "none"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // At its own level, removing the inherited holder removes nothing above.
    let _ = send(&w.config, post_as("/admin/permissions/cell", &s, cell(&t, "ops/warehouse", "mgr@x.test", "none"))).await;
    assert_eq!(held(&w.config, "mgr@x.test", "ops"), Some(Scope::Admin), "the row above was removed from below");
    assert_eq!(held(&w.config, "mgr@x.test", "ops/warehouse"), Some(Scope::Admin));
}

#[tokio::test]
async fn nobody_below_the_top_can_take_access_from_a_site_admin() {
    let w = world().await;
    let s = session(&w.config, "mgr@x.test");
    let t = form_token(&w.config, "mgr@x.test");
    let _ = send(&w.config, post_as("/admin/permissions/cell", &s, cell(&t, "ops", "boss@x.test", "viewer"))).await;
    let _ = send(&w.config, post_as("/admin/permissions/cell", &s, cell(&t, "ops", "boss@x.test", "none"))).await;
    assert_eq!(held(&w.config, "boss@x.test", "ops"), Some(Scope::Admin));
    assert_eq!(held(&w.config, "boss@x.test", ""), Some(Scope::Admin));
}

#[tokio::test]
async fn removing_a_grant_through_the_grid_cannot_reach_an_app_outside_the_managers_project() {
    let w = world().await;
    // fin holds the old per-app access to ledger, with a role the app reads.
    users::grant(&w.config, "fin@x.test", "ledger", "auditor").unwrap();
    let s = session(&w.config, "mgr@x.test");
    let t = form_token(&w.config, "mgr@x.test");
    // The manager manages ops, names its own project as the path, and
    // smuggles a foreign app in the form.
    let body = format!("{}&app=ledger", cell(&t, "ops", "fin@x.test", "none"));
    let _ = send(&w.config, post_as("/admin/permissions/cell", &s, body)).await;
    assert_eq!(
        users::role_for(&w.config, &user(&w.config, "fin@x.test").id, "ledger").as_deref(),
        Some("auditor"),
        "a manager of ops revoked access on finance's app"
    );
}

#[tokio::test]
async fn only_a_manager_at_the_project_may_lock_it_and_rows_inside_a_lock_stop_counting() {
    let w = world().await;
    for who in ["ed@x.test", "sub@x.test", "fin@x.test"] {
        let s = session(&w.config, who);
        let t = form_token(&w.config, who);
        let (status, ..) = send(&w.config, post_as("/admin/permissions/lock", &s, format!("token={t}&path=ops&locked=1"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who} locked ops");
    }
    assert!(!store::folder_locked(&w.config, "ops").await);

    let s = session(&w.config, "mgr@x.test");
    let t = form_token(&w.config, "mgr@x.test");
    let (status, ..) = send(&w.config, post_as("/admin/permissions/lock", &s, format!("token={t}&path=ops&locked=1"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    // Under the lock, rows set inside ops are ignored, so sub manages nothing.
    assert_eq!(held(&w.config, "sub@x.test", "ops/warehouse"), None);
    let s2 = session(&w.config, "sub@x.test");
    let t2 = form_token(&w.config, "sub@x.test");
    let (status, ..) = send(&w.config, post_as("/admin/permissions/cell", &s2, cell(&t2, "ops/warehouse", "nobody@x.test", "admin"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a manager whose row is ignored still granted");
    let (status, ..) = send(&w.config, post_as("/admin/permissions/lock", &s2, format!("token={t2}&path=ops/warehouse&locked=0"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // And the manager of ops cannot write rows inside its own lock either:
    // they would be ignored, so the server says so instead of storing them.
    let _ = send(&w.config, post_as("/admin/permissions/cell", &s, cell(&t, "ops/warehouse", "nobody@x.test", "viewer"))).await;
    let rows = users::list_scopes(&w.config).unwrap();
    assert!(!rows.iter().any(|r| r.email == "nobody@x.test"), "a row was stored inside a lock");
}

#[tokio::test]
async fn the_people_search_is_for_managers_of_that_place_and_never_lists_more_than_ten() {
    let w = world().await;
    for n in 0..15 {
        users::sign_up(&w.config, &format!("extra{n:02}@x.test"), PW).unwrap();
    }
    let (status, ..) = send(&w.config, get("/admin/permissions/candidates?path=ops&q=x")).await;
    assert!(status == StatusCode::SEE_OTHER || status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN, "{status}");
    for who in ["ed@x.test", "fin@x.test", "nobody@x.test"] {
        let (status, body, _) = send(&w.config, get_as("/admin/permissions/candidates?path=ops&q=x", &session(&w.config, who))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who} listed accounts: {body}");
    }
    let mgr = session(&w.config, "mgr@x.test");
    let (status, ..) = send(&w.config, get_as("/admin/permissions/candidates?path=finance&q=x", &mgr)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a manager of ops searched people for finance");
    for q in ["", "x", "extra", "%25", "_"] {
        let (status, body, _) = send(&w.config, get_as(&format!("/admin/permissions/candidates?path=ops&q={q}"), &mgr)).await;
        assert_eq!(status, StatusCode::OK);
        let found: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert!(found.len() <= 10, "q={q:?} returned {}", found.len());
    }
}

// --- the app browser ------------------------------------------------------------

#[tokio::test]
async fn a_project_the_viewer_cannot_see_reads_exactly_like_one_that_does_not_exist() {
    let w = world().await;
    let (s1, b1, _) = send(&w.config, get("/browse/finance")).await;
    let (s2, b2, _) = send(&w.config, get("/browse/no-such-project")).await;
    assert_eq!((s1, &b1), (s2, &b2), "a stranger can tell finance exists");
    let nobody = session(&w.config, "nobody@x.test");
    let (s1, b1, _) = send(&w.config, get_as("/browse/finance", &nobody)).await;
    let (s2, b2, _) = send(&w.config, get_as("/browse/no-such-project", &nobody)).await;
    assert_eq!((s1, &b1), (s2, &b2));
    // fin sees finance and not ops.
    let fin = session(&w.config, "fin@x.test");
    assert_eq!(send(&w.config, get_as("/browse/finance", &fin)).await.0, StatusCode::OK);
    assert_eq!(send(&w.config, get_as("/browse/ops", &fin)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_top_level_names_no_project_or_app_the_viewer_cannot_open_even_when_asked_to_open_it() {
    let w = world().await;
    for (who, forbidden) in [(None, vec!["ops", "finance", "yard", "ledger", "dock", "rootapp"]), (Some("fin@x.test"), vec!["ops", "yard", "dock", "rootapp"])] {
        let uri = "/?open=ops,finance,ops/warehouse&q=";
        let page = match who {
            None => send(&w.config, get(uri)).await.1,
            Some(email) => send(&w.config, get_as(uri, &session(&w.config, email))).await.1,
        };
        for name in &forbidden {
            assert!(!page.contains(&format!("{name} title")), "{who:?} saw the title of {name}");
            assert!(!page.contains(&format!("/browse/{name}\"")) && !page.contains(&format!("/p/{name}/")), "{who:?} saw a link to {name}");
        }
        for q in ["ledger", "yard", "title"] {
            let page = match who {
                None => send(&w.config, get(&format!("/?q={q}"))).await.1,
                Some(email) => send(&w.config, get_as(&format!("/?q={q}"), &session(&w.config, email))).await.1,
            };
            for name in &forbidden {
                assert!(!page.contains(&format!("{name} title")), "{who:?} found {name} by searching {q}");
            }
        }
    }
}

#[tokio::test]
async fn a_project_row_counts_only_the_apps_the_viewer_may_open() {
    let w = world().await;
    // Someone given View on one app inside ops/warehouse only.
    users::sign_up(&w.config, "one@x.test", PW).unwrap();
    users::grant_scope(&w.config, "one@x.test", "ops/warehouse/yard", Scope::Viewer, None).unwrap();
    place_app(&w.config, "crane", "ops/warehouse", "restricted").await;
    let page = send(&w.config, get_as("/", &session(&w.config, "one@x.test"))).await.1;
    assert!(page.contains("ops"), "the path to the one app is hidden");
    assert!(!page.contains("crane title") && !page.contains("dock title"));
    assert!(!page.contains("2 apps") && !page.contains("3 apps"), "the count gives away apps the viewer cannot open");
}

#[tokio::test]
async fn a_page_inside_a_restricted_app_is_not_listed_or_fetched_through_its_own_missing_meta() {
    let w = world().await;
    // A site whose default is public, and an app with loose pages and no
    // index: the app's own meta restricts it, the pages have none.
    let config = Arc::new(Config {
        default_gate: "public".to_string(),
        base_url: Some("https://site.test".to_string()),
        ..Config::local(w.config.data_dir.clone(), TOKEN)
    });
    std::fs::create_dir_all(config.data_dir.join("vault")).unwrap();
    write_page(&config, "vault/plans", "<title>vault plans title</title><p>secret plans</p>");
    let mut meta = store::read_meta(&config, "vault").await;
    meta.gate = Some("restricted".to_string());
    store::write_meta(&config, "vault", &meta).await.unwrap();

    assert_ne!(send(&config, get("/p/vault/plans")).await.0, StatusCode::OK, "the page itself is closed");
    let page = send(&config, get("/")).await.1;
    assert!(!page.contains("vault plans title"), "the index lists a page of a restricted app");
    let page = send(&config, get("/?q=plans")).await.1;
    assert!(!page.contains("vault plans title"), "search lists a page of a restricted app");

    users::sign_up(&config, "visitor@x.test", PW).unwrap();
    let visitor = token_for(&config, "visitor@x.test");
    let (_, found) = tool(&config, "/me/mcp", &visitor, "search", serde_json::json!({"query":"plans"})).await;
    assert!(!found.contains("vault"), "search over MCP found it: {found}");
    let (err, text) = tool(&config, "/me/mcp", &visitor, "fetch", serde_json::json!({"id":"vault/plans"})).await;
    assert!(err || !text.contains("secret plans"), "fetch read it: {text}");
}

// --- the MCP tools ---------------------------------------------------------------

#[tokio::test]
async fn an_editor_over_mcp_is_refused_every_tool_on_an_app_outside_its_project() {
    let w = world().await;
    let ed = token_for(&w.config, "ed@x.test");
    let calls: Vec<(&str, serde_json::Value)> = vec![
        ("create_upload", serde_json::json!({"slug":"ledger"})),
        ("push_page", serde_json::json!({"slug":"ledger","html":"<p>x</p>"})),
        ("push_app", serde_json::json!({"app":"ledger","pages":{"index":"<p>x</p>"}})),
        ("upload_begin", serde_json::json!({"slug":"ledger","kind":"page"})),
        ("run_sql", serde_json::json!({"app":"ledger","sql":"select 1"})),
        ("run_sql", serde_json::json!({"app":"ledger","sql":"select 1","as_user":"fin@x.test"})),
        ("app_settings", serde_json::json!({"app":"ledger"})),
        ("app_jobs", serde_json::json!({"app":"ledger"})),
        ("app_migrations", serde_json::json!({"app":"ledger"})),
        ("app_notes", serde_json::json!({"slug":"ledger"})),
        ("set_icon", serde_json::json!({"slug":"ledger","icon":"x"})),
        ("set_visibility", serde_json::json!({"slug":"ledger","hidden":true})),
        ("set_visibility", serde_json::json!({"slug":"ledger","gate":"public"})),
        ("set_access", serde_json::json!({"app":"ledger","email":"ed@x.test","allow":true})),
        ("app_exports", serde_json::json!({"app":"ledger","action":"list"})),
        ("app_repo", serde_json::json!({"app":"ledger","action":"status"})),
        ("app_deploy_tokens", serde_json::json!({"app":"ledger","action":"list"})),
        ("remove_page", serde_json::json!({"slug":"ledger","confirm":"ledger"})),
        ("screenshot", serde_json::json!({"slug":"ledger"})),
        ("pull_page", serde_json::json!({"slug":"ledger"})),
        ("pull_app", serde_json::json!({"app":"ledger"})),
        ("projects", serde_json::json!({"action":"create","path":"finance","name":"x"})),
        ("projects", serde_json::json!({"action":"move","app":"ledger","path":"ops/warehouse"})),
        ("projects", serde_json::json!({"action":"move","app":"yard","path":"finance"})),
        ("projects", serde_json::json!({"action":"permissions","path":"finance"})),
        ("projects", serde_json::json!({"action":"grant","path":"finance","email":"ed@x.test","scope":"viewer"})),
        ("projects", serde_json::json!({"action":"grant","path":"ops/warehouse","email":"ed@x.test","scope":"admin"})),
        ("projects", serde_json::json!({"action":"revoke","path":"finance","email":"fin@x.test"})),
        ("create_user", serde_json::json!({"email":"new@x.test"})),
        ("set_user_active", serde_json::json!({"email":"fin@x.test","active":false})),
        // A new app landing in a project the editor does not hold.
        ("create_upload", serde_json::json!({"slug":"newthing","project":"finance"})),
        ("push_page", serde_json::json!({"slug":"newthing","project":"finance","html":"<p>x</p>"})),
        ("upload_begin", serde_json::json!({"slug":"newthing","kind":"page","project":"finance"})),
    ];
    for (name, args) in calls {
        let (err, text) = tool(&w.config, "/mcp", &ed, name, args.clone()).await;
        assert!(err, "{name} {args} went through for an editor of ops/warehouse: {text}");
    }
    // Nothing changed.
    assert_eq!(held(&w.config, "ed@x.test", "finance"), None);
    assert_eq!(held(&w.config, "ed@x.test", "ops/warehouse"), Some(Scope::Editor));
    assert!(users::user_by_email(&w.config, "fin@x.test").is_some());
    assert_eq!(store::app_folder(&w.config, "ledger").await, "finance");
    assert_eq!(store::app_folder(&w.config, "yard").await, "ops/warehouse");
    assert!(!store::read_meta(&w.config, "ledger").await.hidden);
}

#[tokio::test]
async fn an_editor_may_not_impersonate_an_account_in_its_own_apps_sql() {
    let w = world().await;
    let ed = token_for(&w.config, "ed@x.test");
    let (err, text) = tool(&w.config, "/mcp", &ed, "run_sql", serde_json::json!({"app":"yard","sql":"select current_user()","as_user":"fin@x.test"})).await;
    assert!(err && text.contains("needs admin"), "an editor ran SQL as another account: {text}");
    // A manager of the app gets past the access check (the app declares no
    // views, so the query itself is then refused for that reason).
    let sub = token_for(&w.config, "sub@x.test");
    let (_, text) = tool(&w.config, "/mcp", &sub, "run_sql", serde_json::json!({"app":"yard","sql":"select 1","as_user":"fin@x.test"})).await;
    assert!(!text.contains("needs admin") && !text.contains("holds no access"), "a manager was refused: {text}");
}

#[tokio::test]
async fn a_viewer_cannot_reach_the_publishing_endpoint_and_a_static_token_cannot_reach_the_personal_one() {
    let w = world().await;
    let fin = token_for(&w.config, "fin@x.test");
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    let (status, _) = mcp_post(&w.config, "/mcp", &fin, init.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a viewer reached /mcp");
    let (status, _) = mcp_post(&w.config, "/me/mcp", TOKEN, init.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a static token reached /me/mcp");
    let (status, _) = mcp_post(&w.config, "/me/mcp", &fin, init).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn search_and_fetch_never_return_what_the_caller_cannot_open() {
    let w = world().await;
    for (endpoint, who) in [("/mcp", "ed@x.test"), ("/me/mcp", "fin@x.test"), ("/me/mcp", "nobody@x.test")] {
        let token = token_for(&w.config, who);
        let (_, found) = tool(&w.config, endpoint, &token, "search", serde_json::json!({"query":"title"})).await;
        let forbidden: &[&str] = match who {
            "ed@x.test" => &["ledger", "dock", "rootapp"],
            "fin@x.test" => &["yard", "dock", "rootapp"],
            _ => &["yard", "dock", "rootapp", "ledger"],
        };
        for app in forbidden {
            assert!(!found.contains(&format!("\"{app}\"")), "{who} found {app} on {endpoint}: {found}");
            let (err, text) = tool(&w.config, endpoint, &token, "fetch", serde_json::json!({"id": app})).await;
            assert!(err && !text.contains("body"), "{who} fetched {app} on {endpoint}: {text}");
        }
        let (_, listed) = tool(&w.config, endpoint, &token, if endpoint == "/mcp" { "list_pages" } else { "my_apps" }, serde_json::json!({})).await;
        for app in forbidden {
            assert!(!listed.contains(&format!("\"{app}\"")) && !listed.contains(&format!("/p/{app}")), "{who} listed {app}: {listed}");
        }
    }
}

// --- uploads that outlive their scope ----------------------------------------------

fn upload_path(text: &str) -> String {
    let start = text.find("/upload/").expect("no upload URL in the reply");
    text[start..].split(|c: char| c.is_whitespace() || c == '\'' || c == '"' || c == '?').next().unwrap().to_string()
}

#[tokio::test]
async fn an_upload_ticket_dies_with_the_scope_that_minted_it() {
    let w = world().await;
    let ed = token_for(&w.config, "ed@x.test");
    let (err, text) = tool(&w.config, "/mcp", &ed, "create_upload", serde_json::json!({"slug":"yard"})).await;
    assert!(!err, "{text}");
    let path = upload_path(&text);
    users::revoke_scope(&w.config, "ed@x.test", "ops/warehouse").unwrap();
    let (status, body, _) = send(&w.config, put(&path, "<p>after revocation</p>")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a revoked editor's ticket still wrote: {body}");
    assert!(!std::fs::read_to_string(w.config.data_dir.join("yard.html")).unwrap_or_default().contains("after revocation"));
}

#[tokio::test]
async fn an_upload_ticket_stops_working_when_the_app_moves_out_of_the_editors_project() {
    let w = world().await;
    let ed = token_for(&w.config, "ed@x.test");
    let (_, text) = tool(&w.config, "/mcp", &ed, "create_upload", serde_json::json!({"slug":"yard"})).await;
    let path = upload_path(&text);
    let boss = token_for(&w.config, "boss@x.test");
    let (err, moved) = tool(&w.config, "/mcp", &boss, "projects", serde_json::json!({"action":"move","app":"yard","path":"finance"})).await;
    assert!(!err, "{moved}");
    let (status, ..) = send(&w.config, put(&path, "<p>planted</p>")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a ticket for an app now in finance still wrote");
}

#[tokio::test]
async fn an_inline_upload_is_judged_again_when_it_finishes() {
    let w = world().await;
    let ed = token_for(&w.config, "ed@x.test");
    let (err, text) = tool(&w.config, "/mcp", &ed, "upload_begin", serde_json::json!({"slug":"yard","kind":"page"})).await;
    assert!(!err, "{text}");
    let id = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|word| word.len() >= 24)
        .expect("no upload id")
        .to_string();
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode("<p>late</p>");
    let (err, text) = tool(&w.config, "/mcp", &ed, "upload_chunk", serde_json::json!({"id":id,"index":0,"data":data})).await;
    assert!(!err, "{text}");
    // Keep ed on /mcp with another folder, and take ops/warehouse away.
    store::create_folder(&w.config, "", "side").await.unwrap();
    users::grant_scope(&w.config, "ed@x.test", "side", Scope::Editor, None).unwrap();
    users::revoke_scope(&w.config, "ed@x.test", "ops/warehouse").unwrap();
    let (err, text) = tool(&w.config, "/mcp", &ed, "upload_finish", serde_json::json!({"id":id,"chunks":1})).await;
    assert!(err, "finished after revocation: {text}");
    // Someone else cannot finish it either.
    let (err, text) = tool(&w.config, "/mcp", &token_for(&w.config, "mgr@x.test"), "upload_finish", serde_json::json!({"id":id,"chunks":1})).await;
    assert!(err, "another caller finished it: {text}");
}

// --- preview sign-in ----------------------------------------------------------------

#[tokio::test]
async fn a_preview_link_works_once_for_one_app_and_never_for_a_disabled_account() {
    let w = world().await;
    let fin = user(&w.config, "fin@x.test");
    let token = toolsite::platform::preview::issue(&w.config, "ledger", "/", Some(&fin.id)).unwrap();
    let (status, _, headers) = send(&w.config, get(&format!("/preview/{token}"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = headers.iter().find(|(k, _)| k == "set-cookie").map(|(_, v)| v.clone()).unwrap_or_default();
    assert!(cookie.starts_with("ts_app_ledger=") && cookie.contains("Path=/p/ledger/"), "{cookie}");
    assert!(!cookie.starts_with("ts_session="), "a preview handed out a site session");
    assert_eq!(send(&w.config, get(&format!("/preview/{token}"))).await.0, StatusCode::NOT_FOUND, "replayed");

    let token = toolsite::platform::preview::issue(&w.config, "ledger", "/", Some(&fin.id)).unwrap();
    users::set_active(&w.config, "fin@x.test", false).unwrap();
    let (status, _, headers) = send(&w.config, get(&format!("/preview/{token}"))).await;
    assert_ne!(status, StatusCode::SEE_OTHER, "a disabled account was signed in by a preview");
    assert!(!headers.iter().any(|(k, _)| k == "set-cookie"));

    for bad in ["", "x", "../ledger", "ledger/../yard"] {
        assert_ne!(send(&w.config, get(&format!("/preview/{bad}"))).await.0, StatusCode::SEE_OTHER);
    }
    assert!(toolsite::platform::preview::issue(&w.config, "ledger", "//evil.test/", None).is_err());
    assert!(toolsite::platform::preview::issue(&w.config, "../ledger", "/", None).is_err());
}

#[tokio::test]
async fn only_a_site_admin_may_take_a_screenshot_as_someone_else() {
    let w = world().await;
    for who in ["ed@x.test", "sub@x.test", "mgr@x.test"] {
        let token = token_for(&w.config, who);
        let (err, text) = tool(&w.config, "/mcp", &token, "screenshot", serde_json::json!({"slug":"yard","as_user":"fin@x.test"})).await;
        assert!(err && (text.contains("site admin") || text.contains("no access") || text.contains("holds")), "{who}: {text}");
    }
}

// --- pictures of restricted apps -------------------------------------------------------

#[tokio::test]
async fn a_restricted_apps_favicon_and_icon_are_closed_to_strangers() {
    let w = world().await;
    let nobody = session(&w.config, "nobody@x.test");
    for path in ["/p/ledger/favicon.svg", "/p/ledger/favicon.ico", "/p/ledger/apple-touch-icon.png", "/icon/ledger"] {
        assert_ne!(send(&w.config, get(path)).await.0, StatusCode::OK, "a stranger got {path}");
        assert_ne!(send(&w.config, get_as(path, &nobody)).await.0, StatusCode::OK, "an account without access got {path}");
    }
}

// --- the one-time adoption of old grants ------------------------------------------------

#[tokio::test]
async fn adopting_old_grants_runs_once_and_a_disabled_account_gains_nothing_from_it() {
    let w = world().await;
    users::grant(&w.config, "nobody@x.test", "ledger", "viewer").unwrap();
    users::set_active(&w.config, "nobody@x.test", false).unwrap();
    toolsite::platform::permissions::adopt_grants(&w.config).await;
    let first = users::list_scopes(&w.config).unwrap().len();
    toolsite::platform::permissions::adopt_grants(&w.config).await;
    assert_eq!(users::list_scopes(&w.config).unwrap().len(), first, "adoption ran twice");
    // The disabled account cannot sign in, so its row opens nothing.
    assert!(users::log_in(&w.config, "nobody@x.test", PW).is_err());
}

// --- where project paths and app paths meet ------------------------------------------------

#[tokio::test]
async fn a_project_cannot_take_the_path_of_an_app_so_access_on_one_never_spills_onto_the_other() {
    let w = world().await;
    // rootapp's path is "rootapp". A project of that name would share every
    // permission row with it.
    assert!(store::create_folder(&w.config, "", "rootapp").await.is_err(), "a project took an app's path");
    // dock sits in ops, so its path is ops/dock.
    assert!(store::create_folder(&w.config, "ops", "dock").await.is_err(), "a project took ops/dock");
}

#[tokio::test]
async fn an_app_cannot_be_moved_or_published_onto_the_path_of_a_project() {
    let w = world().await;
    // An app named like the project ops/warehouse, at the top.
    place_app(&w.config, "warehouse", "", "restricted").await;
    users::sign_up(&w.config, "appmgr@x.test", PW).unwrap();
    users::grant_scope(&w.config, "appmgr@x.test", "warehouse", Scope::Admin, None).unwrap();
    let boss = token_for(&w.config, "boss@x.test");
    let (err, text) = tool(&w.config, "/mcp", &boss, "projects", serde_json::json!({"action":"move","app":"warehouse","path":"ops"})).await;
    assert!(err, "the app landed on the project's path: {text}");
    assert_eq!(held(&w.config, "appmgr@x.test", "ops/warehouse"), None, "managing one app became managing a project");

    // A new app published into ops under the name of a subproject.
    let mgr = token_for(&w.config, "mgr@x.test");
    let (err, text) = tool(&w.config, "/mcp", &mgr, "push_page", serde_json::json!({"slug":"warehouse2","html":"<p>x</p>","project":"ops"})).await;
    assert!(!err, "{text}");
    store::create_folder(&w.config, "", "spare").await.unwrap();
    users::grant_scope(&w.config, "mgr@x.test", "spare", Scope::Admin, None).unwrap();
    store::create_folder(&w.config, "spare", "inner").await.unwrap();
    let (err, text) = tool(&w.config, "/mcp", &mgr, "push_page", serde_json::json!({"slug":"inner","html":"<p>x</p>","project":"spare"})).await;
    assert!(err, "a new app took the path of the project spare/inner: {text}");
}
