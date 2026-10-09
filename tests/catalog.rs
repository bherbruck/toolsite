//! The catalog through the whole router: publishing, hiding, gating, notes,
//! concurrent changes and removal, the way an agent drives them over MCP.
//!
//! Each scenario runs on the backend `TOOLSITE_TEST_BACKEND` names: files
//! by default, Postgres when it is `postgres` (`scripts/test-postgres.sh
//! --full` sets it). Each also has an `_on_postgres` twin, ignored unless
//! asked for, so `scripts/test-postgres.sh` alone runs them on Postgres.
//! On Postgres the site is a real Postgres site: accounts, OAuth, tickets
//! and the catalog all live in the database, and a panic anywhere, as a
//! store call waiting on an async worker would be, fails the scenario.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Once,
};
use toolsite::{
    build_router,
    content::catalog,
    runtime::wasm::Runtime,
    state::{pg, Backend, Stores},
    Config,
};
use tower::ServiceExt;

const NEEDS: &str = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one";
const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const TOKEN: &str = "test-token";

static PANICS: AtomicUsize = AtomicUsize::new(0);

fn count_panics() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICS.fetch_add(1, Ordering::SeqCst);
            previous(info);
        }));
    });
}

/// One site on one backend, and what to drop when it is done.
struct Site {
    _dir: tempfile::TempDir,
    config: Arc<Config>,
    database: Option<(Arc<pg::Postgres>, String)>,
    panics_before: usize,
}

impl Site {
    fn on_postgres(&self) -> bool {
        self.database.is_some()
    }

    async fn finish(self) {
        let panics = PANICS.load(Ordering::SeqCst) - self.panics_before;
        if let Some((postgres, name)) = self.database {
            postgres.pool.close();
            drop(self.config);
            drop(postgres);
            let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
            let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
            tokio::spawn(connection);
            client.batch_execute(&format!("drop database if exists {name} with (force)")).await.unwrap();
        }
        assert_eq!(panics, 0, "something panicked while the scenario ran");
    }
}

fn wants_postgres() -> bool {
    std::env::var("TOOLSITE_TEST_BACKEND").is_ok_and(|backend| backend == "postgres")
}

async fn site(postgres: bool) -> Site {
    count_panics();
    let panics_before = PANICS.load(Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    if !postgres {
        let config = Arc::new(Config::local(dir.path().to_path_buf(), TOKEN));
        return Site { _dir: dir, config, database: None, panics_before };
    }
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
    let name = format!("t_{}", toolsite::content::slug::random_token(12).to_lowercase());
    let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("create database {name}")).await.unwrap();
    let mut url = url::Url::parse(&server).unwrap();
    url.set_path(&name);
    let postgres = Arc::new(pg::connect(url.as_str(), 8).await.unwrap());
    pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap();
    let stores = Stores::new(Backend::Postgres(postgres.clone()), None, Some(KEY)).unwrap();
    let config = Arc::new(Config { stores, ..Config::local(dir.path().to_path_buf(), TOKEN) });
    Site { _dir: dir, config, database: Some((postgres, name)), panics_before }
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

async fn get(config: &Arc<Config>, uri: &str) -> StatusCode {
    let request = Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap();
    build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap().status()
}

/// Where the meta is kept: a row on Postgres and no sidecar, or a sidecar
/// on files and no row. Proves the backend asked for is the one in use.
async fn kept_in_the_backend(site: &Site, slug: &str) {
    let sidecars = [format!("{slug}.meta"), format!("{slug}/index.meta")];
    let on_disk = sidecars.iter().any(|name| site.config.data_dir.join(name).exists());
    match &site.database {
        Some((postgres, _)) => {
            let rows: i64 = postgres
                .pool
                .get()
                .await
                .unwrap()
                .query_one("select count(*) from platform.pages where slug = $1", &[&slug])
                .await
                .unwrap()
                .get(0);
            assert_eq!(rows, 1, "{slug} has no catalog row");
            assert!(!on_disk, "{slug} has a sidecar on a Postgres site");
        }
        None => assert!(on_disk, "{slug} has no sidecar"),
    }
}

// --- scenarios ------------------------------------------------------------------

async fn publishing_hiding_and_gating(site: Site) {
    let config = &site.config;
    let (failed, said) = call(config, "push_page", serde_json::json!({ "slug": "note", "html": "<title>Note</title>hi" })).await;
    assert!(!failed, "{said}");
    let (failed, said) =
        call(config, "push_app", serde_json::json!({ "app": "board", "pages": { "index": "<title>Board</title>", "about": "a" } })).await;
    assert!(!failed, "{said}");
    assert_eq!(get(config, "/p/note").await, StatusCode::OK);
    assert_eq!(get(config, "/p/board/about").await, StatusCode::OK);

    let (failed, said) = call(config, "set_visibility", serde_json::json!({ "slug": "note", "hidden": true })).await;
    assert!(!failed, "{said}");
    assert_eq!(get(config, "/p/note").await, StatusCode::NOT_FOUND);
    assert!(catalog::meta(config, "note").await.hidden);
    kept_in_the_backend(&site, "note").await;
    let (_, listed) = call(config, "list_pages", serde_json::json!({})).await;
    assert!(!listed.contains("/p/note"), "a hidden page was listed: {listed}");

    call(config, "set_visibility", serde_json::json!({ "slug": "note", "hidden": false, "listed": false })).await;
    assert_eq!(get(config, "/p/note").await, StatusCode::OK);
    let meta = catalog::meta(config, "note").await;
    assert!(!meta.hidden && !meta.listed);

    // Restricted, and a public corner inside it.
    let (failed, said) = call(config, "set_visibility", serde_json::json!({ "slug": "board", "gate": "granted" })).await;
    assert!(!failed, "{said}");
    let (failed, said) =
        call(config, "set_visibility", serde_json::json!({ "slug": "board", "path": "/about", "gate": "public" })).await;
    assert!(!failed, "{said}");
    assert_ne!(get(config, "/p/board/").await, StatusCode::OK, "a restricted app opened for nobody");
    assert_eq!(get(config, "/p/board/about").await, StatusCode::OK, "its public corner stayed closed");
    let meta = catalog::meta(config, "board").await;
    assert_eq!(meta.gate.as_deref(), Some("restricted"), "the old word was stored as it came");
    assert_eq!(meta.rules.len(), 1);
    kept_in_the_backend(&site, "board").await;

    // Removing a rule that is not there changes nothing and says so.
    let (failed, said) = call(config, "set_visibility", serde_json::json!({ "slug": "board", "path": "/nope" })).await;
    assert!(failed && said.contains("no rule for /nope"), "{said}");
    assert_eq!(catalog::meta(config, "board").await.rules.len(), 1);
    site.finish().await;
}

async fn notes_through_the_tool(site: Site) {
    let config = &site.config;
    call(config, "push_page", serde_json::json!({ "slug": "ledger", "html": "<title>Ledger</title>" })).await;
    let (_, said) = call(config, "app_notes", serde_json::json!({ "slug": "ledger" })).await;
    assert!(said.contains("no notes"), "{said}");
    let (failed, said) = call(config, "app_notes", serde_json::json!({ "slug": "ledger", "notes": "schema: entries(id, amount)" })).await;
    assert!(!failed, "{said}");
    let (_, said) = call(config, "app_notes", serde_json::json!({ "slug": "ledger" })).await;
    assert_eq!(said, "schema: entries(id, amount)");
    assert_eq!(catalog::notes(config, "ledger").await.as_deref(), Some("schema: entries(id, amount)"));
    // Notes are the platform's, never served.
    assert_eq!(get(config, "/p/ledger.notes").await, StatusCode::NOT_FOUND);
    site.finish().await;
}

/// Agents changing one app's access at the same moment, each adding its own
/// rule: every rule is there after. Read, change and write as three steps
/// would keep only some of them.
async fn concurrent_changes_through_the_router_all_land(site: Site) {
    let config = site.config.clone();
    call(&config, "push_app", serde_json::json!({ "app": "busy", "pages": { "index": "<title>Busy</title>" } })).await;
    let mut calls = Vec::new();
    for n in 0..16 {
        let config = config.clone();
        calls.push(tokio::spawn(async move {
            call(&config, "set_visibility", serde_json::json!({ "slug": "busy", "path": format!("/area-{n}"), "gate": "public" })).await
        }));
    }
    for done in calls {
        let (failed, said) = done.await.unwrap();
        assert!(!failed, "{said}");
    }
    let mut prefixes: Vec<String> = catalog::meta(&config, "busy").await.rules.into_iter().map(|rule| rule.prefix).collect();
    prefixes.sort();
    let mut wanted: Vec<String> = (0..16).map(|n| format!("/area-{n}")).collect();
    wanted.sort();
    assert_eq!(prefixes, wanted, "concurrent rule changes were lost");
    site.finish().await;
}

/// A removed app takes its meta with it: an app published again at the
/// slug starts with nothing of the old one, not its creator, not its gate,
/// not its being hidden. What was taken is kept beside the files.
async fn a_removed_app_leaves_nothing_for_the_next(site: Site) {
    let config = &site.config;
    call(config, "push_app", serde_json::json!({ "app": "gone", "pages": { "index": "<title>Gone</title>" } })).await;
    call(config, "set_visibility", serde_json::json!({ "slug": "gone", "gate": "restricted", "hidden": true })).await;
    call(config, "app_notes", serde_json::json!({ "slug": "gone", "notes": "the old app's notes" })).await;
    let (failed, said) = call(config, "remove_page", serde_json::json!({ "slug": "gone", "confirm": "gone" })).await;
    assert!(!failed, "{said}");

    let fresh = catalog::meta(config, "gone").await;
    assert!(!fresh.hidden && fresh.gate.is_none(), "the old meta outlived its app");
    assert_eq!(catalog::notes(config, "gone").await, None);
    call(config, "push_app", serde_json::json!({ "app": "gone", "pages": { "index": "<title>New</title>" } })).await;
    assert_eq!(get(config, "/p/gone/").await, StatusCode::OK);

    let trash = std::fs::read_dir(config.data_dir.join(".trash")).unwrap().next().unwrap().unwrap().path();
    let kept = std::fs::read_to_string(trash.join("slug.meta"))
        .or_else(|_| std::fs::read_to_string(trash.join("app/index.meta")))
        .expect("the removed meta was not kept");
    assert!(kept.contains("\"hidden\":true"), "{kept}");
    if site.on_postgres() {
        assert_eq!(std::fs::read_to_string(trash.join("slug.notes")).unwrap(), "the old app's notes");
    }
    site.finish().await;
}

// --- on the backend TOOLSITE_TEST_BACKEND names ------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publishing_hiding_and_gating_keep_their_meta() {
    publishing_hiding_and_gating(site(wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notes_are_kept_and_never_served() {
    notes_through_the_tool(site(wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_access_changes_all_land() {
    concurrent_changes_through_the_router_all_land(site(wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_an_app_leaves_nothing_for_an_app_published_after() {
    a_removed_app_leaves_nothing_for_the_next(site(wants_postgres()).await).await;
}

// --- on Postgres, when asked for -------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn publishing_hiding_and_gating_keep_their_meta_on_postgres() {
    publishing_hiding_and_gating(site(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn notes_are_kept_and_never_served_on_postgres() {
    notes_through_the_tool(site(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn concurrent_access_changes_all_land_on_postgres() {
    concurrent_changes_through_the_router_all_land(site(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn removing_an_app_leaves_nothing_for_an_app_published_after_on_postgres() {
    a_removed_app_leaves_nothing_for_the_next(site(true).await).await;
}
