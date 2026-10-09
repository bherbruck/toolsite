//! Per-app records and tokens through the whole router: settings set over
//! MCP and read by nothing but the app, export, deploy and device tokens
//! minted, used and revoked the way an owner and their tools do, two runners
//! sharing one site, and a removal that takes an app's records and tokens
//! with it so the next app at its name starts with none.
//!
//! Each scenario runs on the backend `TOOLSITE_TEST_BACKEND` names: files
//! by default, Postgres when it is `postgres` (`scripts/test-postgres.sh
//! --full` sets it). Each also has an `_on_postgres` twin, ignored unless
//! asked for, so `scripts/test-postgres.sh` alone runs them on Postgres.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::Arc;
use toolsite::{build_router, runtime::wasm::Runtime, Config};
use tower::ServiceExt;

mod common;
use common::blocking;

const TOKEN: &str = "test-token";

/// One site on one backend, and the database to drop when it is done.
struct Site {
    _dir: tempfile::TempDir,
    config: Arc<Config>,
    database: Option<common::Database>,
}

impl Site {
    async fn on(postgres: bool) -> Site {
        let dir = tempfile::tempdir().unwrap();
        let local = Config::local(dir.path().to_path_buf(), TOKEN);
        if !postgres {
            return Site { _dir: dir, config: Arc::new(local), database: None };
        }
        let database = common::Database::new().await;
        let config = Arc::new(Config { stores: database.stores(), ..local });
        Site { _dir: dir, config, database: Some(database) }
    }

    fn on_postgres(&self) -> bool {
        self.database.is_some()
    }

    /// Another runner on the same site: its own `Config` and stores, the
    /// same data directory and, on Postgres, the same database.
    fn second_runner(&self) -> Arc<Config> {
        let local = Config::local(self.config.data_dir.clone(), TOKEN);
        Arc::new(match &self.database {
            Some(database) => Config { stores: database.stores(), ..local },
            None => local,
        })
    }

    /// Every row of a platform table as text, the way a dump would hold it.
    async fn dump(&self, table: &str) -> Vec<String> {
        let database = self.database.as_ref().expect("a Postgres site");
        let client = database.postgres.pool.get().await.unwrap();
        // A table name from this file's own literals, never from input.
        assert!(table.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == '.'));
        client
            .query(&format!("select t::text from {table} t"), &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    async fn finish(self) {
        drop(self.config);
        if let Some(database) = self.database {
            database.drop().await;
        }
    }
}

async fn call(config: &Arc<Config>, name: &str, arguments: serde_json::Value) -> (bool, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})
                .to_string(),
        ))
        .unwrap();
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json: serde_json::Value = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let result = &json["result"];
    let said = result["content"][0]["text"].as_str().unwrap_or_default().to_string();
    (result["isError"] == true, said)
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, bytes.to_vec())
}

async fn get(config: &Arc<Config>, uri: &str) -> StatusCode {
    send(config, Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()).await.0
}

fn bearer(method: &str, uri: &str, token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The id and the plain token from a token tool's answer.
fn minted(said: &str) -> (String, String) {
    let id = said.strip_prefix("Token ").and_then(|rest| rest.split(' ').next()).unwrap_or_else(|| panic!("{said}"));
    let token = said.split("Shown once:\n\n").nth(1).and_then(|rest| rest.lines().next()).unwrap_or_else(|| panic!("{said}"));
    (id.to_string(), token.to_string())
}

async fn mint(config: &Arc<Config>, tool: &str, app: &str, label: &str) -> (String, String) {
    let (failed, said) = call(config, tool, serde_json::json!({ "app": app, "action": "create", "label": label })).await;
    assert!(!failed, "{said}");
    minted(&said)
}

fn seed(config: &Config, app: &str) {
    blocking(|| {
        toolsite::runtime::db::run(config, app, "create table orders (id integer)", &[]).unwrap();
        toolsite::runtime::db::run(config, app, "insert into orders values (1), (2)", &[]).unwrap();
    });
}

// --- scenarios ------------------------------------------------------------------

/// A setting goes in over MCP, comes back only as a name, is read by the
/// app's own code alone, and is never at rest in the clear: not in the
/// sidecar, not in any row.
async fn settings_sealed_and_apart(site: Site) {
    let config = &site.config;
    let secret = "hunter2-the-plain-value";
    let (failed, said) = call(config, "app_settings", serde_json::json!({ "app": "crm", "name": "API_KEY", "value": secret })).await;
    assert!(!failed, "{said}");
    call(config, "app_settings", serde_json::json!({ "app": "crm", "name": "ENDPOINT", "value": "https://example.com" })).await;
    let (_, listed) = call(config, "app_settings", serde_json::json!({ "app": "crm" })).await;
    assert_eq!(listed, "crm: API_KEY, ENDPOINT");
    assert!(!listed.contains(secret));

    assert_eq!(toolsite::platform::secrets::get(config, "crm", "API_KEY").await.as_deref(), Some(secret));
    let blocking_read = blocking(|| toolsite::platform::secrets::get_blocking(config, "crm", "API_KEY"));
    assert_eq!(blocking_read.as_deref(), Some(secret));
    assert!(toolsite::platform::secrets::names(config, "crm2").await.is_empty(), "another app saw the settings");
    assert_eq!(toolsite::platform::secrets::get(config, "cr", "API_KEY").await, None);
    // Another runner opens what this one sealed.
    let other = site.second_runner();
    assert_eq!(toolsite::platform::secrets::get(&other, "crm", "API_KEY").await.as_deref(), Some(secret));

    let sidecar = config.data_dir.join("crm.secrets");
    if site.on_postgres() {
        assert!(!sidecar.exists(), "a settings sidecar on a Postgres site");
        let rows = site.dump("platform.app_settings").await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().all(|row| !row.contains(secret) && !row.contains("example.com")), "a setting at rest in the clear: {rows:?}");
        assert!(rows.iter().any(|row| row.contains("API_KEY")), "names are not secret: {rows:?}");
    } else {
        let stored = std::fs::read_to_string(&sidecar).unwrap();
        assert!(stored.contains("API_KEY") && !stored.contains(secret));
    }

    let (failed, said) = call(config, "app_settings", serde_json::json!({ "app": "crm", "name": "API_KEY" })).await;
    assert!(!failed, "{said}");
    assert_eq!(toolsite::platform::secrets::get(config, "crm", "API_KEY").await, None);
    let (failed, said) = call(config, "app_settings", serde_json::json!({ "app": "crm", "name": "API_KEY" })).await;
    assert!(failed && said.contains("no setting called API_KEY"), "{said}");

    // Never served, by any spelling.
    for uri in ["/crm.secrets", "/p/crm.secrets", "/p/crm/index.secrets"] {
        assert_eq!(get(config, uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
    site.finish().await;
}

/// An export token minted on one runner opens its app's database on the
/// other, opens no other app's, and stops at once on both when revoked.
async fn export_tokens_across_runners(site: Site) {
    let a = site.config.clone();
    let b = site.second_runner();
    seed(&a, "sales");
    seed(&a, "hr");
    let (id, token) = mint(&a, "app_exports", "sales", "reporting").await;
    assert!(token.starts_with("tse_"));

    let (status, body) = send(&b, bearer("GET", "/export/sales.sqlite", &token, "")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with(b"SQLite format 3"), "not a database");
    assert_eq!(send(&b, bearer("GET", "/export/hr.sqlite", &token, "")).await.0, StatusCode::UNAUTHORIZED, "a token for sales opened hr");
    assert_eq!(send(&a, bearer("GET", "/export/sales.sqlite", TOKEN, "")).await.0, StatusCode::UNAUTHORIZED, "the publish token was taken");
    let listed = toolsite::platform::export::list(&b, "sales").await;
    assert!(listed[0].last_used.is_some(), "use was not recorded");
    let (_, said) = call(&a, "app_exports", serde_json::json!({ "app": "sales", "action": "list" })).await;
    assert!(said.contains(&id) && said.contains("reporting"), "{said}");
    assert!(!said.contains(&token), "a listing showed the token");

    if site.on_postgres() {
        assert!(!a.data_dir.join("sales.exports").exists(), "a token sidecar on a Postgres site");
        let rows = site.dump("platform.app_tokens").await;
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].contains(&token[4..]), "a token at rest in the clear");
    } else {
        assert!(a.data_dir.join("sales.exports").is_file());
    }
    for uri in ["/sales.exports", "/p/sales.exports"] {
        assert_eq!(get(&a, uri).await, StatusCode::NOT_FOUND, "{uri}");
    }

    let (failed, said) = call(&a, "app_exports", serde_json::json!({ "app": "sales", "action": "revoke", "id": id })).await;
    assert!(!failed, "{said}");
    assert_eq!(send(&b, bearer("GET", "/export/sales.sqlite", &token, "")).await.0, StatusCode::UNAUTHORIZED, "revoked, and still open on the other runner");
    assert_eq!(send(&a, bearer("GET", "/export/sales.sqlite", &token, "")).await.0, StatusCode::UNAUTHORIZED);
    let (failed, _) = call(&b, "app_exports", serde_json::json!({ "app": "sales", "action": "revoke", "id": id })).await;
    assert!(failed, "a second revocation said it revoked something");
    site.finish().await;
}

/// A deploy token publishes its own app and no other, and stops at once.
async fn deploy_token_for_one_app(site: Site) {
    let a = site.config.clone();
    let b = site.second_runner();
    let (id, token) = mint(&a, "app_deploy_tokens", "shop", "ci").await;
    let (status, body) = send(&b, bearer("PUT", "/deploy/shop", &token, "<title>Shop</title>v1")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(get(&a, "/p/shop").await, StatusCode::OK);
    assert_eq!(send(&b, bearer("PUT", "/deploy/other", &token, "<title>x</title>")).await.0, StatusCode::UNAUTHORIZED, "a token for shop published other");
    assert_eq!(get(&a, "/p/other").await, StatusCode::NOT_FOUND);
    let (_, export) = mint(&a, "app_exports", "shop", "reporting").await;
    assert_eq!(send(&b, bearer("PUT", "/deploy/shop", &export, "<title>x</title>")).await.0, StatusCode::UNAUTHORIZED, "an export token deployed");

    let (failed, said) = call(&a, "app_deploy_tokens", serde_json::json!({ "app": "shop", "action": "revoke", "id": id })).await;
    assert!(!failed, "{said}");
    assert_eq!(send(&b, bearer("PUT", "/deploy/shop", &token, "<title>Shop</title>v2")).await.0, StatusCode::UNAUTHORIZED, "revoked, and still deploying");
    site.finish().await;
}

/// A device token names its device to its own app's handler alone, and a
/// device checking on every message records its use once a minute at most.
async fn device_tokens_and_their_use(site: Site) {
    let config = site.config.clone();
    let (id, token) = mint(&config, "app_device_tokens", "broker", "boiler").await;
    let check = |app: &'static str, presented: String| {
        let config = config.clone();
        blocking(move || toolsite::platform::devices::check(&config, app, &presented))
    };
    assert_eq!(check("broker", token.clone()).as_deref(), Some("boiler"));
    assert_eq!(check("syslog", token.clone()), None, "another app's handler accepted the token");
    let first = toolsite::platform::devices::list(&config, "broker").await[0].last_used.expect("use was not recorded");
    for _ in 0..20 {
        assert_eq!(check("broker", format!(" {token} ")).as_deref(), Some("boiler"));
    }
    assert_eq!(toolsite::platform::devices::list(&config, "broker").await[0].last_used, Some(first), "use was written on every check");

    let (failed, said) = call(&config, "app_device_tokens", serde_json::json!({ "app": "broker", "action": "revoke", "id": id })).await;
    assert!(!failed, "{said}");
    let other = site.second_runner();
    assert_eq!(blocking(|| toolsite::platform::devices::check(&other, "broker", &token)), None, "revoked, and still checked in");
    site.finish().await;
}

/// The bug a row per token fixes: a use recorded by rewriting the token
/// list lost every token minted meanwhile. Two runners, one minting, the
/// other using a token over and over.
async fn minted_while_used_is_never_lost(site: Site) {
    let a = site.config.clone();
    let b = site.second_runner();
    let (_, first) = toolsite::platform::export::create(&a, "sales", "first").await.unwrap();
    let minting: Vec<_> = (0..4)
        .map(|n| {
            let a = a.clone();
            tokio::spawn(async move {
                let mut minted = Vec::new();
                for m in 0..10 {
                    minted.push(toolsite::platform::export::create(&a, "sales", &format!("t{n}-{m}")).await.unwrap().1);
                }
                minted
            })
        })
        .collect();
    let using: Vec<_> = (0..4)
        .map(|_| {
            let (b, first) = (b.clone(), first.clone());
            tokio::spawn(async move {
                for _ in 0..50 {
                    assert!(toolsite::platform::export::authorize(&b, "sales", &first).await, "a use missed a live token");
                }
            })
        })
        .collect();
    let mut minted = Vec::new();
    for task in minting {
        minted.extend(task.await.unwrap());
    }
    for task in using {
        task.await.unwrap();
    }
    let lost = count_lost(&a, &minted).await;
    assert_eq!(lost, 0, "{lost} of {} tokens minted during use were lost", minted.len());
    assert_eq!(toolsite::platform::export::list(&a, "sales").await.len(), minted.len() + 1);
    site.finish().await;
}

async fn count_lost(config: &Arc<Config>, tokens: &[String]) -> usize {
    let mut lost = 0;
    for token in tokens {
        if !toolsite::platform::export::authorize(config, "sales", token).await {
            lost += 1;
        }
    }
    lost
}

/// Tools, the migration ladder and their listing, kept where the backend
/// keeps them.
async fn tools_and_migrations(site: Site) {
    let config = site.config.clone();
    let tools: Vec<toolsite::platform::app_tools::AppTool> = serde_json::from_value(serde_json::json!([
        { "name": "look_up", "description": "Finds an order. 'quoted' \u{0430} \u{1F512}", "path": "/api/look-up", "input": { "type": "object" } }
    ]))
    .unwrap();
    toolsite::platform::app_tools::write(&config, "farm", &tools).await.unwrap();
    assert_eq!(toolsite::platform::app_tools::read(&config, "farm").await, tools);
    assert!(toolsite::platform::app_tools::read(&config, "farm2").await.is_empty());
    assert_eq!(toolsite::platform::app_tools::apps_with_tools(&config).await, ["farm"]);
    toolsite::platform::app_tools::write(&config, "farm", &[]).await.unwrap();
    assert!(toolsite::platform::app_tools::apps_with_tools(&config).await.is_empty());

    let ladder = vec![
        ("002_more.sql".to_string(), "alter table t add column note text".to_string()),
        ("001_initial.sql".to_string(), "create table t (n integer)".to_string()),
    ];
    let (version, ran, _) = blocking(|| {
        toolsite::runtime::migrate::store(&config, "farm", ladder).unwrap();
        toolsite::runtime::migrate::apply(&config, "farm").unwrap()
    });
    assert_eq!((version, ran), (2, 2));
    let stored = blocking(|| toolsite::runtime::migrate::stored(&config, "farm"));
    assert_eq!(stored[0].0, "001_initial.sql", "the ladder was not kept in order");
    assert!(blocking(|| toolsite::runtime::migrate::stored(&config, "other")).is_empty());
    assert_eq!(config.data_dir.join("farm.migrations").exists(), !site.on_postgres());
    for uri in ["/farm.migrations", "/p/farm.migrations", "/p/farm.tools"] {
        assert_eq!(get(&config, uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
    site.finish().await;
}

/// A removal takes the app's settings, tools, ladder and every token with
/// it, keeps them in the trash, and the next app at the name starts with
/// none of them.
async fn removal_takes_records_and_tokens(site: Site) {
    let config = site.config.clone();
    call(&config, "push_app", serde_json::json!({ "app": "gone", "pages": { "index": "<title>Gone</title>" } })).await;
    seed(&config, "gone");
    toolsite::platform::secrets::set(&config, "gone", "API_KEY", Some("old-secret")).await.unwrap();
    let tools: Vec<toolsite::platform::app_tools::AppTool> =
        serde_json::from_value(serde_json::json!([{ "name": "t", "description": "d", "path": "/api/t", "input": { "type": "object" } }])).unwrap();
    toolsite::platform::app_tools::write(&config, "gone", &tools).await.unwrap();
    let (_, export) = mint(&config, "app_exports", "gone", "reporting").await;
    let (_, deploy) = mint(&config, "app_deploy_tokens", "gone", "ci").await;
    let (_, device) = mint(&config, "app_device_tokens", "gone", "pump").await;
    // A neighbour whose name only starts the same keeps everything.
    let (_, neighbour) = mint(&config, "app_exports", "gone_x", "reporting").await;
    seed(&config, "gone_x");

    let (failed, said) = call(&config, "remove_page", serde_json::json!({ "slug": "gone", "confirm": "gone" })).await;
    assert!(!failed, "{said}");
    call(&config, "push_app", serde_json::json!({ "app": "gone", "pages": { "index": "<title>New</title>" } })).await;
    seed(&config, "gone");

    assert_eq!(send(&config, bearer("GET", "/export/gone.sqlite", &export, "")).await.0, StatusCode::UNAUTHORIZED, "an old export token opened the new app");
    assert_eq!(send(&config, bearer("PUT", "/deploy/gone", &deploy, "<title>x</title>")).await.0, StatusCode::UNAUTHORIZED, "an old deploy token published the new app");
    assert_eq!(blocking(|| toolsite::platform::devices::check(&config, "gone", &device)), None, "an old device token checked in");
    assert!(toolsite::platform::secrets::names(&config, "gone").await.is_empty(), "the old settings outlived their app");
    assert!(toolsite::platform::app_tools::read(&config, "gone").await.is_empty(), "the old tools outlived their app");
    assert_eq!(send(&config, bearer("GET", "/export/gone_x.sqlite", &neighbour, "")).await.0, StatusCode::OK, "the neighbour lost its token");

    let trash = std::fs::read_dir(config.data_dir.join(".trash")).unwrap().next().unwrap().unwrap().path();
    for kept in ["slug.secrets", "slug.tools", "slug.exports", "slug.deploys", "slug.devices"] {
        assert!(trash.join(kept).is_file(), "{kept} was not kept in the trash");
    }
    let kept = std::fs::read_to_string(trash.join("slug.secrets")).unwrap();
    assert!(kept.contains("API_KEY") && !kept.contains("old-secret"));
    assert!(!std::fs::read_to_string(trash.join("slug.exports")).unwrap().contains(&export[4..]));
    if site.on_postgres() {
        let removed = site.dump("platform.removed_records").await;
        assert_eq!(removed.len(), 5);
        assert!(removed.iter().all(|row| !row.contains("old-secret") && !row.contains(&export[4..])), "{removed:?}");
    }
    site.finish().await;
}

// --- on the backend TOOLSITE_TEST_BACKEND names ------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_are_sealed_at_rest_and_read_by_their_app_alone() {
    settings_sealed_and_apart(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_token_opens_one_app_on_every_runner_until_revoked() {
    export_tokens_across_runners(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_token_publishes_one_app_until_revoked() {
    deploy_token_for_one_app(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_token_is_checked_by_its_app_and_its_use_kept_coarse() {
    device_tokens_and_their_use(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_minted_while_another_is_used_is_never_lost() {
    minted_while_used_is_never_lost(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_and_the_migration_ladder_are_kept_per_app() {
    tools_and_migrations(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_an_app_takes_its_records_and_tokens_with_it() {
    removal_takes_records_and_tokens(Site::on(common::wants_postgres()).await).await;
}

// --- on Postgres, when asked for -------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn settings_are_sealed_at_rest_and_read_by_their_app_alone_on_postgres() {
    settings_sealed_and_apart(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_export_token_opens_one_app_on_every_runner_until_revoked_on_postgres() {
    export_tokens_across_runners(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_deploy_token_publishes_one_app_until_revoked_on_postgres() {
    deploy_token_for_one_app(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_device_token_is_checked_by_its_app_and_its_use_kept_coarse_on_postgres() {
    device_tokens_and_their_use(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_token_minted_while_another_is_used_is_never_lost_on_postgres() {
    minted_while_used_is_never_lost(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn tools_and_the_migration_ladder_are_kept_per_app_on_postgres() {
    tools_and_migrations(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn removing_an_app_takes_its_records_and_tokens_with_it_on_postgres() {
    removal_takes_records_and_tokens(Site::on(true).await).await;
}
