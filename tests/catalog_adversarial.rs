//! Attacks on the catalog: how access settings, project trees and host
//! labels are written, by a hostile publisher, an editor without Manage, a
//! viewer, a stranger, and two runners on one Postgres.
//!
//! Every test is an attempt by someone who should not get through, named by
//! the property that holds. Each scenario runs on the backend
//! `TOOLSITE_TEST_BACKEND` names, and has an `_on_postgres` twin that
//! `scripts/test-postgres.sh` runs.
//!
//! The cast, set up by `world`:
//! - `boss`: site admin.
//! - `mgr`: Manage on `ops`.
//! - `ed`: Edit on `ops` (no Manage).
//! - `out`: Manage on `side`, nothing in `ops`.
//!
//! Apps: `yard` in `ops`, `vault` in `ops/locked` (a locked project), both
//! restricted.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::Arc;
use tempfile::TempDir;
use toolsite::{
    accounts::users::{self, Scope},
    build_router,
    content::{catalog, store},
    runtime::wasm::Runtime,
    Config,
};
use tower::ServiceExt;

mod common;
use common::blocking;

const TOKEN: &str = "test-token";
const PW: &str = "correct horse battery";

// --- plumbing ---------------------------------------------------------------

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String) {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()
}

fn put_bytes(uri: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder().method("PUT").uri(uri).body(Body::from(body)).unwrap()
}

fn write_page(config: &Config, slug: &str, html: &str) {
    common::publish(config, &format!("{slug}.html"), html.to_string());
}

async fn place_app(config: &Config, app: &str, project: &str, gate: &str) {
    write_page(config, &format!("{app}/index"), &format!("<title>{app}</title><p>{app} body</p>"));
    let (project, gate) = ((!project.is_empty()).then(|| project.to_string()), gate.to_string());
    catalog::update_meta(config, app, move |meta| {
        meta.project = project;
        meta.gate = Some(gate);
        Ok(())
    })
    .await
    .unwrap();
}

fn user(config: &Config, email: &str) -> users::User {
    blocking(|| users::user_by_email(config, email).unwrap())
}

fn token_for(config: &Config, email: &str) -> String {
    let user = user(config, email);
    blocking(|| {
        let client = toolsite::platform::oauth_store::register_client(config, Some("t"), &["https://c.test/cb".into()]).unwrap();
        toolsite::platform::oauth_store::issue_tokens(config, &client.id, &user.id, None).unwrap().access_token
    })
}

fn held_on(config: &Config, email: &str, app: &str) -> Option<Scope> {
    let user = user(config, email);
    blocking(|| {
        let folder = catalog::meta_blocking(config, app).project.unwrap_or_default();
        users::app_scope(config, &user, &folder, app, &store::locked_prefixes_blocking(config))
    })
}

async fn mcp_post(config: &Arc<Config>, token: &str, body: serde_json::Value) -> serde_json::Value {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text) = send(config, request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Calls one tool as `token`. Returns (is_error, text).
async fn tool(config: &Arc<Config>, token: &str, name: &str, arguments: serde_json::Value) -> (bool, String) {
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    mcp_post(config, token, init).await;
    let call = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}});
    let json = mcp_post(config, token, call).await;
    let result = &json["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false) || json.get("error").is_some();
    let text = result["content"][0]["text"].as_str().map(str::to_string).unwrap_or_else(|| json.to_string());
    (is_error, text)
}

fn upload_path(text: &str) -> String {
    let start = text.find("/upload/").expect("no upload URL in the reply");
    text[start..].split(|c: char| c.is_whitespace() || c == '\'' || c == '"' || c == '?').next().unwrap().to_string()
}

/// An upload URL for `slug`, minted by `token`'s account.
async fn upload_for(config: &Arc<Config>, token: &str, slug: &str) -> String {
    let (failed, text) = tool(config, token, "create_upload", serde_json::json!({ "slug": slug })).await;
    assert!(!failed, "{text}");
    upload_path(&text)
}

/// A gzipped tar of `files`.
fn tgz_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, &body[..]).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut encoder, &tar).unwrap();
    encoder.finish().unwrap()
}

struct World {
    _dir: TempDir,
    config: Arc<Config>,
    database: Option<common::Database>,
}

impl World {
    async fn finish(self) {
        if let Some(database) = self.database {
            drop(self.config);
            database.drop().await;
        }
    }
}

/// The world on files, or on Postgres: accounts, OAuth, tickets and the
/// catalog in a database of its own.
async fn world_on(postgres: bool) -> World {
    world_shaped(postgres, |config| config).await
}

/// Subdomain mode: each app on a host of its own under `apps.test`.
fn subdomains(config: Config) -> Config {
    let apps = toolsite::content::origins::AppsDomain::parse("apps.test", config.base_url.as_deref(), None).unwrap();
    Config { apps: Some(apps), ..config }
}

async fn world_shaped(postgres: bool, shape: fn(Config) -> Config) -> World {
    let dir = tempfile::tempdir().unwrap();
    let database = match postgres {
        true => Some(common::Database::new().await),
        false => None,
    };
    let base = shape(Config { base_url: Some("https://site.test".to_string()), ..Config::local(dir.path().to_path_buf(), TOKEN) });
    let config = Arc::new(match &database {
        Some(database) => Config { stores: database.stores(), blobs: database.blobs(), ..base },
        None => base,
    });
    blocking(|| {
        users::sign_up_as(&config, "boss@x.test", PW, true).unwrap();
        for who in ["mgr", "ed", "out"] {
            users::sign_up(&config, &format!("{who}@x.test"), PW).unwrap();
        }
    });
    store::create_folder(&config, "", "ops").await.unwrap();
    store::create_folder(&config, "ops", "locked").await.unwrap();
    store::create_folder(&config, "", "side").await.unwrap();
    store::set_locked(&config, "ops/locked", true).await.unwrap();
    store::set_folder_gate(&config, "ops/locked", Some("restricted")).await.unwrap();
    blocking(|| {
        users::grant_scope(&config, "mgr@x.test", "ops", Scope::Admin, None).unwrap();
        users::grant_scope(&config, "ed@x.test", "ops", Scope::Editor, None).unwrap();
        users::grant_scope(&config, "out@x.test", "side", Scope::Admin, None).unwrap();
    });
    place_app(&config, "yard", "ops", "restricted").await;
    place_app(&config, "vault", "ops/locked", "restricted").await;
    World { _dir: dir, config, database }
}

async fn world() -> World {
    world_on(common::wants_postgres()).await
}

// --- a bundle carrying the platform's own files -----------------------------------

async fn bundle_cannot_plant(w: World) {
    let config = &w.config;
    let ed = token_for(config, "ed@x.test");
    let path = upload_for(config, &ed, "yard").await;
    let forged_meta = br#"{"hidden":false,"gate":"public","project":null,"created_by":"someone","label":"victim","allow_http":["evil.test"]}"#;
    let bundle = tgz_of(&[
        ("index.html", b"<title>yard</title>new"),
        ("index.meta", forged_meta),
        ("about.meta", forged_meta),
        ("index.notes", b"planted notes"),
        ("handler.wasm", b"\0asm not really"),
        ("data.db", b"SQLite format 3\0planted"),
        ("data.db-wal", b"planted"),
    ]);
    let (status, said) = send(config, put_bytes(&format!("{path}?bundle"), bundle)).await;
    // Refused outright, or published without the platform's files.
    let meta = catalog::meta(config, "yard").await;
    assert_eq!(meta.gate.as_deref(), Some("restricted"), "a bundle set the gate ({status}: {said})");
    assert_eq!(meta.project.as_deref(), Some("ops"), "a bundle moved the app out of its project");
    assert!(meta.created_by.as_deref() != Some("someone") && meta.label.is_none() && meta.allow_http.is_empty(), "{meta:?}");
    assert_eq!(catalog::meta(config, "yard/about").await.gate, None, "a bundle wrote a page's meta");
    assert_ne!(catalog::notes(config, "yard").await.as_deref(), Some("planted notes"));
    for planted in ["handler.wasm", "data.db", "data.db-wal"] {
        assert!(!config.data_dir.join("yard").join(planted).exists(), "a bundle wrote the platform's {planted}");
    }
    assert_ne!(send(config, get("/p/yard")).await.0, StatusCode::OK, "the app opened to a stranger");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bundle_cannot_write_the_metas_notes_handler_or_database_the_platform_keeps() {
    bundle_cannot_plant(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_bundle_cannot_write_the_metas_notes_handler_or_database_the_platform_keeps_on_postgres() {
    bundle_cannot_plant(world_on(true).await).await;
}

async fn platform_files_not_served(w: World) {
    let config = &w.config;
    place_app(config, "open", "", "public").await;
    common::publish(config, "open/handler.wasm", &b"\0asm"[..]);
    // The database is a file on the volume, whichever store keeps the rest.
    let dir = config.data_dir.join("open");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data.db"), b"SQLite format 3\0secret rows").unwrap();
    std::fs::write(dir.join("data.db-wal"), b"secret wal").unwrap();
    for file in ["handler.wasm", "data.db", "data.db-wal"] {
        let (status, body) = send(config, get(&format!("/p/open/{file}"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{file} was served: {body}");
    }
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apps_database_and_handler_are_never_served_as_files() {
    platform_files_not_served(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_apps_database_and_handler_are_never_served_as_files_on_postgres() {
    platform_files_not_served(world_on(true).await).await;
}

// --- the manifest owns what it declares, and nothing else ------------------------

async fn manifest_owns_its_fields(w: World) {
    let config = &w.config;
    let ed = token_for(config, "ed@x.test");
    let path = upload_for(config, &ed, "yard").await;
    let put = |body: &str| put_bytes(&format!("{path}?manifest"), body.as_bytes().to_vec());
    // Fields a toolsite.toml may not name: refused, and nothing applied.
    for forged in ["hidden = false\n", "label = \"victim\"\n", "project = \"side\"\n", "created_by = \"x\"\n", "listed = true\n"] {
        let (status, said) = send(config, put(&format!("gate = \"public\"\n{forged}"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{forged} was taken: {said}");
    }
    assert_eq!(catalog::meta(config, "yard").await.gate.as_deref(), Some("restricted"));

    // A hide made by someone else is kept by the next deploy.
    let (failed, said) = tool(config, TOKEN, "set_visibility", serde_json::json!({ "slug": "yard", "hidden": true })).await;
    assert!(!failed, "{said}");
    let (status, said) = send(config, put("gate = \"authenticated\"\n")).await;
    assert_eq!(status, StatusCode::OK, "{said}");
    let meta = catalog::meta(config, "yard").await;
    assert!(meta.hidden, "a deploy un-hid the app");
    assert_eq!(meta.project.as_deref(), Some("ops"), "a deploy moved the app");
    assert_eq!(send(config, get("/p/yard")).await.0, StatusCode::NOT_FOUND);

    // Under a locked project an app's own gate does not count, however it
    // was set.
    let path = upload_for(config, TOKEN, "vault").await;
    let (status, said) = send(config, put_bytes(&format!("{path}?manifest"), b"gate = \"public\"\n[[route]]\npath = \"/\"\ngate = \"public\"\n".to_vec())).await;
    assert_eq!(status, StatusCode::OK, "{said}");
    let effective = store::effective_gate(config, "vault", "/").await;
    assert_eq!(effective.gate, "restricted", "a manifest opened an app in a locked project: {effective:?}");
    assert_eq!(effective.locked_by.as_deref(), Some("ops/locked"));
    assert_ne!(send(config, get("/p/vault")).await.0, StatusCode::OK);
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manifest_sets_only_its_own_fields_and_never_unhides_moves_or_opens_a_locked_app() {
    manifest_owns_its_fields(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_manifest_sets_only_its_own_fields_and_never_unhides_moves_or_opens_a_locked_app_on_postgres() {
    manifest_owns_its_fields(world_on(true).await).await;
}

// --- one change, all or nothing ----------------------------------------------------

async fn failed_edit_leaves_nothing(w: World) {
    let config = &w.config;
    // Hiding and removing a rule that is not there, in one call: the call
    // fails, and the hide must not land with it.
    let (failed, said) =
        tool(config, TOKEN, "set_visibility", serde_json::json!({ "slug": "yard", "hidden": true, "listed": false, "path": "/nope" })).await;
    assert!(failed, "removing a missing rule succeeded: {said}");
    let meta = catalog::meta(config, "yard").await;
    assert!(!meta.hidden && meta.listed, "half of a failed change was written: {meta:?}");

    // An editor without Manage changes no gate, no rule.
    let ed = token_for(config, "ed@x.test");
    for arguments in [
        serde_json::json!({ "slug": "yard", "gate": "public" }),
        serde_json::json!({ "slug": "yard", "gate": "public", "path": "/" }),
        serde_json::json!({ "slug": "yard", "hidden": true, "gate": "public" }),
    ] {
        let (failed, said) = tool(config, &ed, "set_visibility", arguments.clone()).await;
        assert!(failed, "{arguments}: {said}");
    }
    let meta = catalog::meta(config, "yard").await;
    assert!(meta.gate.as_deref() == Some("restricted") && meta.rules.is_empty() && !meta.hidden, "{meta:?}");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_that_fails_halfway_writes_nothing_and_an_editor_changes_no_gate() {
    failed_edit_leaves_nothing(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_change_that_fails_halfway_writes_nothing_and_an_editor_changes_no_gate_on_postgres() {
    failed_edit_leaves_nothing(world_on(true).await).await;
}

// --- hiding and removing reach what the app holds open ------------------------------

async fn events_close(w: World) {
    let config = &w.config;
    place_app(config, "live", "", "public").await;
    place_app(config, "doomed", "", "public").await;
    let (_live, mut live_rx) = config.connections.register("live", None).unwrap();
    let (_doomed, mut doomed_rx) = config.connections.register("doomed", None).unwrap();
    config.connections.accept("live", &_live.id);
    config.connections.accept("doomed", &_doomed.id);
    let closed = |rx: &mut tokio::sync::mpsc::Receiver<toolsite::runtime::connections::Outgoing>| {
        std::iter::from_fn(|| rx.try_recv().ok()).any(|out| matches!(out, toolsite::runtime::connections::Outgoing::Close))
    };
    let (failed, said) = tool(config, TOKEN, "set_visibility", serde_json::json!({ "slug": "live", "hidden": true })).await;
    assert!(!failed, "{said}");
    assert!(closed(&mut live_rx), "hiding left the socket open");

    // A removal whose catalog step fails still closes what the app held:
    // its files are gone already.
    if let Some(database) = &w.database {
        database.postgres.pool.get().await.unwrap().batch_execute("alter table platform.removed_pages rename to removed_pages_away").await.unwrap();
    }
    let (_, said) = tool(config, TOKEN, "remove_page", serde_json::json!({ "slug": "doomed", "confirm": "doomed" })).await;
    eprintln!("remove: {said}");
    assert!(closed(&mut doomed_rx), "removing left the socket open");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hiding_or_removing_closes_the_apps_connections_even_when_the_catalog_step_fails() {
    events_close(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn hiding_or_removing_closes_the_apps_connections_even_when_the_catalog_step_fails_on_postgres() {
    events_close(world_on(true).await).await;
}

/// A second runner on the same site: its own `Config` and registries, the
/// same data directory, and on Postgres the same database.
fn second_runner(w: &World) -> Arc<Config> {
    let base = Config {
        base_url: w.config.base_url.clone(),
        apps: w.config.apps.clone(),
        ..Config::local(w.config.data_dir.clone(), TOKEN)
    };
    Arc::new(match &w.database {
        Some(database) => Config { stores: database.stores(), blobs: database.blobs(), ..base },
        None => base,
    })
}

async fn hide_seen_by_other_runner(w: World) {
    let config = &w.config;
    place_app(config, "shared", "", "public").await;
    let other = second_runner(&w);
    assert_eq!(send(&other, get("/p/shared/")).await.0, StatusCode::OK);
    let (failed, said) = tool(config, TOKEN, "set_visibility", serde_json::json!({ "slug": "shared", "hidden": true })).await;
    assert!(!failed, "{said}");
    // No cache of metas on the other runner: the next request sees it.
    assert_eq!(send(&other, get("/p/shared/")).await.0, StatusCode::NOT_FOUND, "the other runner still served a hidden app");
    let (failed, said) = tool(config, TOKEN, "set_visibility", serde_json::json!({ "slug": "shared", "hidden": false, "gate": "restricted" })).await;
    assert!(!failed, "{said}");
    assert_ne!(send(&other, get("/p/shared/")).await.0, StatusCode::OK, "the other runner served a gated app openly");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hide_or_a_gate_on_one_runner_holds_on_the_next_request_to_another() {
    hide_seen_by_other_runner(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_hide_or_a_gate_on_one_runner_holds_on_the_next_request_to_another_on_postgres() {
    hide_seen_by_other_runner(world_on(true).await).await;
}

/// Fails the catalog's project tree, as an outage would: the file becomes
/// unreadable, or the table goes away.
async fn break_the_tree(w: &World) {
    match &w.database {
        Some(database) => database
            .postgres
            .pool
            .get()
            .await
            .unwrap()
            .batch_execute("alter table platform.projects rename to projects_away")
            .await
            .unwrap(),
        None => std::fs::write(w.config.data_dir.join(".site/projects.json"), b"{ not json").unwrap(),
    }
}

// --- projects ------------------------------------------------------------------

async fn tree_outage_closes(w: World) {
    let config = &w.config;
    blocking(|| {
        users::sign_up(config, "inner@x.test", PW).unwrap();
        // Set inside the lock (before it, say): ignored while it is locked.
        users::grant_scope(config, "inner@x.test", "ops/locked/vault", Scope::Admin, None).unwrap();
    });
    assert_eq!(held_on(config, "inner@x.test", "vault"), None, "a row inside a lock counted");
    assert_eq!(held_on(config, "ed@x.test", "yard"), Some(Scope::Editor));
    break_the_tree(&w).await;
    assert_eq!(held_on(config, "inner@x.test", "vault"), None, "an unreadable tree forgot the lock");
    assert_eq!(held_on(config, "ed@x.test", "yard"), None, "an unreadable tree left rows inside projects counting");
    assert_eq!(held_on(config, "boss@x.test", "vault"), Some(Scope::Admin), "a site admin is still one");
    assert_ne!(send(config, get("/p/vault/")).await.0, StatusCode::OK);
    // A change on an unreadable tree is refused and writes nothing over it.
    assert!(store::create_folder(config, "", "fresh").await.is_err(), "a project was made on an unreadable tree");
    if w.database.is_none() {
        assert_eq!(std::fs::read(config.data_dir.join(".site/projects.json")).unwrap(), b"{ not json", "the tree file was overwritten");
    }
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_project_tree_locks_everything_and_is_never_written_over() {
    tree_outage_closes(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_unreadable_project_tree_locks_everything_and_is_never_written_over_on_postgres() {
    tree_outage_closes(world_on(true).await).await;
}

async fn lock_cannot_be_escaped(w: World) {
    let config = &w.config;
    blocking(|| {
        users::sign_up(config, "inner@x.test", PW).unwrap();
        users::grant_scope(config, "inner@x.test", "ops/locked/vault", Scope::Admin, None).unwrap();
        users::grant_scope(config, "inner@x.test", "side", Scope::Admin, None).unwrap();
    });
    // A row the lock ignores does not move the app out of the lock.
    let inner = token_for(config, "inner@x.test");
    let (failed, said) = tool(config, &inner, "projects", serde_json::json!({ "action": "move", "app": "vault", "path": "side" })).await;
    assert!(failed, "a row inside a lock moved the app out: {said}");
    assert_eq!(catalog::meta(config, "vault").await.project.as_deref(), Some("ops/locked"));
    // Nor does Manage elsewhere move an app in, or out of, ops.
    let out = token_for(config, "out@x.test");
    place_app(config, "mine", "side", "restricted").await;
    for (app, path) in [("mine", "ops/locked"), ("mine", "ops"), ("yard", "side"), ("vault", "side")] {
        let (failed, said) = tool(config, &out, "projects", serde_json::json!({ "action": "move", "app": app, "path": path })).await;
        assert!(failed, "{app} to {path}: {said}");
    }
    // An editor without Manage moves nothing.
    let ed = token_for(config, "ed@x.test");
    let (failed, said) = tool(config, &ed, "projects", serde_json::json!({ "action": "move", "app": "yard", "path": "ops/locked" })).await;
    assert!(failed, "{said}");
    // Moved in by someone who may: the lock's setting is what applies now,
    // and the app's own does not.
    let (failed, said) = tool(config, TOKEN, "set_visibility", serde_json::json!({ "slug": "yard", "gate": "public" })).await;
    assert!(!failed, "{said}");
    let mgr = token_for(config, "mgr@x.test");
    let (failed, said) = tool(config, &mgr, "projects", serde_json::json!({ "action": "move", "app": "yard", "path": "ops/locked" })).await;
    assert!(!failed, "{said}");
    assert_eq!(store::effective_gate(config, "yard", "/").await.gate, "restricted");
    assert_ne!(send(config, get("/p/yard/")).await.0, StatusCode::OK, "an app moved into a lock kept its own public gate");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_leaves_or_enters_a_locked_project_only_by_someone_holding_manage_on_both_ends() {
    lock_cannot_be_escaped(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_app_leaves_or_enters_a_locked_project_only_by_someone_holding_manage_on_both_ends_on_postgres() {
    lock_cannot_be_escaped(world_on(true).await).await;
}

async fn old_name_inherits_nothing(w: World) {
    let config = &w.config;
    blocking(|| {
        users::sign_up(config, "a@x.test", PW).unwrap();
        // A row left at a path no project or app holds.
        users::grant_scope(config, "a@x.test", "fresh", Scope::Admin, None).unwrap();
    });
    // `ops` becomes `site`: its rows go with it, and `ops` is free.
    let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "rename", "path": "ops", "name": "site" })).await;
    assert!(!failed, "{said}");
    assert_eq!(held_on(config, "mgr@x.test", "yard"), Some(Scope::Admin), "the rows did not follow the rename");
    // `side` takes the old name: nothing of the old `ops` comes with it.
    let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "rename", "path": "side", "name": "ops" })).await;
    assert!(!failed, "{said}");
    place_app(config, "newcomer", "ops", "restricted").await;
    for who in ["mgr@x.test", "ed@x.test"] {
        assert_eq!(held_on(config, who, "newcomer"), None, "{who} inherited the old ops");
    }
    assert_eq!(held_on(config, "out@x.test", "newcomer"), Some(Scope::Admin), "side's own rows went missing");
    // Nor does a project renamed onto a path with a stale row pick it up.
    let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "rename", "path": "ops", "name": "fresh" })).await;
    assert!(!failed, "{said}");
    assert_eq!(held_on(config, "a@x.test", "newcomer"), None, "a stale row at the new name counted");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_renamed_to_an_old_name_inherits_nothing_left_there() {
    old_name_inherits_nothing(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_project_renamed_to_an_old_name_inherits_nothing_left_there_on_postgres() {
    old_name_inherits_nothing(world_on(true).await).await;
}

/// Every app names a project the tree holds, or none.
async fn no_app_outside_the_tree(config: &Config) {
    let tree: Vec<String> = store::list_folders(config).await.into_iter().map(|f| f.path).collect();
    for (app, project) in store::apps_with_folders(config).await {
        assert!(project.is_empty() || tree.contains(&project), "{app} names {project}, which the tree does not hold: {tree:?}");
    }
}

async fn move_races_remove(w: World) {
    let config = &w.config;
    let other = second_runner(&w);
    for round in 0..12 {
        let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "create", "path": "", "name": "temp" })).await;
        assert!(!failed, "round {round}: {said}");
        let mover = if round % 2 == 0 { config.clone() } else { other.clone() };
        let (moved, removed, renamed) = tokio::join!(
            tool(&mover, TOKEN, "projects", serde_json::json!({ "action": "move", "app": "yard", "path": "temp" })),
            tool(config, TOKEN, "projects", serde_json::json!({ "action": "remove", "path": "temp" })),
            tool(&other, TOKEN, "projects", serde_json::json!({ "action": "rename", "path": "temp", "name": "temp2" })),
        );
        no_app_outside_the_tree(config).await;
        // Put things back for the next round, whichever won.
        let _ = (moved, removed, renamed);
        let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "move", "app": "yard", "path": "ops" })).await;
        assert!(!failed, "round {round}: {said}");
        for leftover in ["temp", "temp2"] {
            if store::folder_exists(config, leftover).await {
                let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "remove", "path": leftover })).await;
                assert!(!failed, "round {round}: {said}");
            }
        }
    }
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_moved_while_its_project_is_removed_or_renamed_never_names_a_missing_project() {
    move_races_remove(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_app_moved_while_its_project_is_removed_or_renamed_never_names_a_missing_project_on_postgres() {
    move_races_remove(world_on(true).await).await;
}

async fn crafted_journal(w: World) {
    let config = &w.config;
    let other = second_runner(&w);
    for (from, to) in [("", "side"), ("ops", ""), ("ops", "ops/inner"), ("ops/locked", "ops"), ("../ops", "x"), ("ops", "a%b"), ("ops", "side/../x")] {
        toolsite::platform::projects::begin_relocation(config, from, to).await.unwrap();
        let resumed = toolsite::platform::projects::resume_pending(&other).await;
        assert!(resumed.is_err(), "{from:?} -> {to:?} was finished");
        assert_eq!(catalog::meta(config, "yard").await.project.as_deref(), Some("ops"), "{from:?} -> {to:?} moved an app");
        assert_eq!(held_on(config, "mgr@x.test", "yard"), Some(Scope::Admin), "{from:?} -> {to:?} moved access rows");
        assert!(store::folder_exists(config, "ops/locked").await && store::folder_locked(config, "ops/locked").await);
        // Every move waits on it: noticed, not skipped.
        let (failed, _) = tool(&other, TOKEN, "projects", serde_json::json!({ "action": "rename", "path": "side", "name": "aside" })).await;
        assert!(failed, "a move went ahead past {from:?} -> {to:?}");
    }
    // A record a move could have written is finished by the other runner.
    toolsite::platform::projects::begin_relocation(config, "side", "aside").await.unwrap();
    toolsite::platform::projects::resume_pending(&other).await.unwrap();
    assert!(store::folder_exists(config, "aside").await && !store::folder_exists(config, "side").await);
    assert_eq!(held_on(config, "out@x.test", "yard"), None);
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crafted_move_record_is_left_in_place_and_moves_nothing() {
    crafted_journal(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_crafted_move_record_is_left_in_place_and_moves_nothing_on_postgres() {
    crafted_journal(world_on(true).await).await;
}

async fn project_names(w: World) {
    let config = &w.config;
    for bad in ["a%b", "..", ".", "a.b", "é", "ops/../side", "a b", "a\u{0}b", "Ops%", "_%", "a'b", "-%-"] {
        let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "create", "path": "", "name": bad })).await;
        assert!(failed, "{bad:?} became a project: {said}");
        let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "rename", "path": "side", "name": bad })).await;
        assert!(failed, "side was renamed to {bad:?}: {said}");
    }
    // `_` is a name character, never a wildcard; case is part of the name.
    for name in ["o_s", "Ops", "OPS"] {
        let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "create", "path": "", "name": name })).await;
        assert!(!failed, "{name}: {said}");
        place_app(config, &format!("in-{}", name.to_lowercase().replace('_', "-")), name, "restricted").await;
    }
    for app in ["in-o-s", "in-ops"] {
        assert_eq!(held_on(config, "mgr@x.test", app), None, "Manage on ops reached {app}");
        assert_eq!(held_on(config, "ed@x.test", app), None, "Edit on ops reached {app}");
    }
    // Removing `o_s` takes only its own rows, not `ops`'s.
    let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "move", "app": "in-o-s", "path": "" })).await;
    assert!(!failed, "{said}");
    let (failed, said) = tool(config, TOKEN, "projects", serde_json::json!({ "action": "remove", "path": "o_s" })).await;
    assert!(!failed, "{said}");
    assert_eq!(held_on(config, "mgr@x.test", "yard"), Some(Scope::Admin));
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_names_are_slugs_and_no_spelling_reaches_another_project() {
    project_names(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn project_names_are_slugs_and_no_spelling_reaches_another_project_on_postgres() {
    project_names(world_on(true).await).await;
}

async fn bounded_queue(w: World) {
    let config = &w.config;
    let hold = catalog::of(config).hold_relocations().await.unwrap();
    let waiting: Vec<_> = (0..catalog::MAX_WAITING_MOVES + 8)
        .map(|_| {
            let config = config.clone();
            tokio::spawn(async move { toolsite::platform::projects::resume_pending(&config).await })
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let refused = waiting.iter().filter(|task| task.is_finished()).count();
    assert!(refused >= 8, "only {refused} moves were turned away; the queue has no end");
    hold.release().await;
    let mut done = 0;
    for task in waiting {
        if tokio::time::timeout(std::time::Duration::from_secs(30), task).await.unwrap().unwrap().is_ok() {
            done += 1;
        }
    }
    assert!((1..=catalog::MAX_WAITING_MOVES).contains(&done), "{done} moves ran");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moves_waiting_for_their_turn_are_bounded_and_the_rest_are_refused() {
    bounded_queue(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn moves_waiting_for_their_turn_are_bounded_and_the_rest_are_refused_on_postgres() {
    bounded_queue(world_on(true).await).await;
}

async fn wait_for_a_move(w: World) {
    let config = &w.config;
    let other = second_runner(&w);
    // A move under way, on this runner or another: an app move, a new
    // project and a removal each wait for it rather than check a tree it is
    // halfway through changing.
    let hold = catalog::of(config).hold_relocations().await.unwrap();
    let calls = [
        serde_json::json!({ "action": "move", "app": "yard", "path": "side" }),
        serde_json::json!({ "action": "create", "path": "", "name": "later" }),
        serde_json::json!({ "action": "remove", "path": "side" }),
    ];
    for arguments in calls {
        let (runner, arguments2) = (other.clone(), arguments.clone());
        let task = tokio::spawn(async move { tool(&runner, TOKEN, "projects", arguments2).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!task.is_finished(), "{arguments} did not wait for the move under way");
        task.abort();
    }
    hold.release().await;
    let (failed, said) = tool(&other, TOKEN, "projects", serde_json::json!({ "action": "move", "app": "yard", "path": "side" })).await;
    assert!(!failed, "{said}");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_move_a_new_project_and_a_removal_wait_for_a_move_under_way() {
    wait_for_a_move(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_app_move_a_new_project_and_a_removal_wait_for_a_move_under_way_on_postgres() {
    wait_for_a_move(world_on(true).await).await;
}

// --- host labels ------------------------------------------------------------------

fn label_of(config: &Config, app: &str) -> String {
    blocking(|| toolsite::content::origins::label_for(config, app))
}

async fn get_on(config: &Arc<Config>, host: &str, uri: &str) -> StatusCode {
    let request = Request::builder().uri(uri).header("host", host).body(Body::empty()).unwrap();
    send(config, request).await.0
}

async fn issued(w: &World) -> std::collections::BTreeMap<String, String> {
    match &w.database {
        Some(database) => database
            .postgres
            .pool
            .get()
            .await
            .unwrap()
            .query("select label, app from platform.host_labels", &[])
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect(),
        None => serde_json::from_str(&std::fs::read_to_string(w.config.data_dir.join(".site/labels.json")).unwrap_or_else(|_| "{}".into())).unwrap(),
    }
}

async fn labels_reserved(w: World) {
    let config = &w.config;
    let other = second_runner(&w);
    place_app(config, "Shop", "", "public").await;
    let shop = label_of(config, "Shop");
    assert!(shop.starts_with("shop-") && shop != "shop", "{shop}");
    assert_eq!(get_on(config, &format!("{shop}.apps.test"), "/p/Shop/").await, StatusCode::OK);
    let (failed, said) = tool(config, TOKEN, "remove_page", serde_json::json!({ "slug": "Shop", "confirm": "Shop" })).await;
    assert!(!failed, "{said}");

    // An app named exactly the removed app's label, on either runner,
    // never gets it, and its host serves nothing.
    place_app(config, &shop, "", "public").await;
    assert_ne!(label_of(&other, &shop), shop, "a removed app's label was issued again");
    assert_ne!(label_of(config, &shop), shop);
    for host in [format!("{shop}.apps.test"), format!("{}.APPS.TEST", shop.to_uppercase())] {
        assert_eq!(get_on(&other, &host, &format!("/p/{shop}/")).await, StatusCode::NOT_FOUND, "{host} served the squatter");
    }

    // A name spelled as punycode is not its own label, in any case.
    place_app(config, "xn--pple-43d", "", "public").await;
    let label = label_of(config, "xn--pple-43d");
    assert!(!label.starts_with("xn--") && label.get(2..4) != Some("--"), "{label}");
    for host in ["xn--pple-43d.apps.test", "XN--PPLE-43D.apps.test"] {
        assert_eq!(get_on(config, host, "/p/xn--pple-43d/").await, StatusCode::NOT_FOUND, "{host}");
    }

    // Two runners labelling one app at once issue it one label.
    place_app(config, "Race", "", "public").await;
    let mut tasks = Vec::new();
    for n in 0..8 {
        let runner = if n % 2 == 0 { config.clone() } else { other.clone() };
        tasks.push(tokio::task::spawn_blocking(move || toolsite::content::origins::label_for(&runner, "Race")));
    }
    let mut labels = Vec::new();
    for task in tasks {
        labels.push(task.await.unwrap());
    }
    labels.dedup();
    assert_eq!(labels.len(), 1, "{labels:?}");
    let issued = issued(&w).await;
    assert_eq!(issued.values().filter(|app| *app == "Race").count(), 1, "{issued:?}");
    assert_eq!(issued.get(&shop).map(String::as_str), Some("Shop"), "the removed app's label was forgotten: {issued:?}");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_apps_host_label_is_never_issued_again_by_any_runner_or_spelling() {
    labels_reserved(world_shaped(common::wants_postgres(), subdomains).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_removed_apps_host_label_is_never_issued_again_by_any_runner_or_spelling_on_postgres() {
    labels_reserved(world_shaped(true, subdomains).await).await;
}

/// Fails the list of issued labels, as an outage would.
async fn break_the_labels(w: &World) {
    match &w.database {
        Some(database) => database
            .postgres
            .pool
            .get()
            .await
            .unwrap()
            .batch_execute("alter table platform.host_labels rename to host_labels_away")
            .await
            .unwrap(),
        None => std::fs::write(w.config.data_dir.join(".site/labels.json"), b"{ not json").unwrap(),
    }
}

async fn label_outage(w: World) {
    let config = &w.config;
    place_app(config, "Shop", "", "public").await;
    let shop = label_of(config, "Shop");
    let (failed, said) = tool(config, TOKEN, "remove_page", serde_json::json!({ "slug": "Shop", "confirm": "Shop" })).await;
    assert!(!failed, "{said}");
    break_the_labels(&w).await;
    // While the list cannot be read, the removed app's label is nobody's.
    place_app(config, &shop, "", "public").await;
    assert_eq!(get_on(config, &format!("{shop}.apps.test"), &format!("/p/{shop}/")).await, StatusCode::NOT_FOUND, "a label went to a squatter while the list was unreadable");
    assert!(catalog::meta(config, &shop).await.label.is_none(), "a label was stored while the list was unreadable");
    if w.database.is_none() {
        assert_eq!(std::fs::read(config.data_dir.join(".site/labels.json")).unwrap(), b"{ not json", "the list was written over");
    }
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_label_list_issues_no_label_and_is_never_written_over() {
    label_outage(world_shaped(common::wants_postgres(), subdomains).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_unreadable_label_list_issues_no_label_and_is_never_written_over_on_postgres() {
    label_outage(world_shaped(true, subdomains).await).await;
}

// --- names and what is left at them -----------------------------------------------

async fn name_taken_by_first_publish(w: World) {
    let config = &w.config;
    // `out` publishes only a manifest at a new name, in `side`.
    let out = token_for(config, "out@x.test");
    let path = upload_for(config, &out, "shared").await;
    let (status, said) = send(config, put_bytes(&format!("{path}?manifest"), b"gate = \"public\"\n".to_vec())).await;
    assert_eq!(status, StatusCode::OK, "{said}");
    // An editor in `ops` publishing at that name is refused: the name is
    // taken, and taking it would put the editor's app in `side` under
    // `out`, with `out`'s gate.
    let ed = token_for(config, "ed@x.test");
    let (failed, text) = tool(config, &ed, "create_upload", serde_json::json!({ "slug": "shared" })).await;
    if !failed {
        let (status, said) = send(config, put_bytes(&format!("{}?bundle", upload_path(&text)), tgz_of(&[("index.html", b"ed's app")]))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "an editor published into a name another project holds: {said}");
    }
    assert_eq!(held_on(config, "out@x.test", "shared"), Some(Scope::Admin));
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_is_taken_by_whatever_is_published_there_first_on_either_backend() {
    name_taken_by_first_publish(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_name_is_taken_by_whatever_is_published_there_first_on_either_backend_on_postgres() {
    name_taken_by_first_publish(world_on(true).await).await;
}

async fn stale_meta_not_inherited(w: World) {
    let config = &w.config;
    // A meta left at a name with no files: a removal whose catalog step
    // failed, or a page's sidecar left behind.
    let stale = r#"{"gate":"public","project":"side","created_by":"mallory","allow_http":["evil.test"],"label":"victim"}"#;
    match &w.database {
        Some(database) => {
            database
                .postgres
                .pool
                .get()
                .await
                .unwrap()
                .execute(
                    "insert into platform.pages (slug, meta, created_at, updated_at) values ('fresh', $1::text::json, 0, 0)",
                    &[&stale],
                )
                .await
                .unwrap();
        }
        None => std::fs::write(config.data_dir.join("fresh.meta"), stale).unwrap(),
    }
    let ed = token_for(config, "ed@x.test");
    let path = upload_for(config, &ed, "fresh").await;
    let (status, said) = send(config, put_bytes(&format!("{path}?bundle"), tgz_of(&[("index.html", b"<title>fresh</title>")]))).await;
    assert_eq!(status, StatusCode::OK, "{said}");
    let meta = catalog::meta(config, "fresh").await;
    assert_eq!(meta.project.as_deref(), Some("ops"), "the new app landed in the stale meta's project: {meta:?}");
    assert_eq!(meta.created_by, Some(user(config, "ed@x.test").id), "{meta:?}");
    assert!(meta.gate.is_none() && meta.allow_http.is_empty() && meta.label.is_none(), "{meta:?}");
    assert_eq!(held_on(config, "out@x.test", "fresh"), None);
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_app_never_inherits_a_meta_left_at_its_name() {
    stale_meta_not_inherited(world().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_new_app_never_inherits_a_meta_left_at_its_name_on_postgres() {
    stale_meta_not_inherited(world_on(true).await).await;
}

// --- files: what a crash leaves -------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_temporary_meta_left_by_a_crash_is_never_read_served_or_listed() {
    let w = world_on(false).await;
    let config = &w.config;
    // A crash between writing the temporary file and renaming it leaves
    // the old meta in place and the temporary file beside it.
    let open = br#"{"gate":"public","hidden":false}"#;
    std::fs::write(config.data_dir.join("yard/.index.meta.AbCd1234.part"), open).unwrap();
    std::fs::write(config.data_dir.join(".yard.meta.AbCd1234.part"), open).unwrap();
    std::fs::write(config.data_dir.join(".site/.projects.json.AbCd1234.part"), b"[]").unwrap();
    assert_eq!(catalog::meta(config, "yard").await.gate.as_deref(), Some("restricted"));
    assert!(store::folder_locked(config, "ops/locked").await);
    for uri in ["/p/yard/.index.meta.AbCd1234.part", "/p/.yard.meta.AbCd1234.part", "/p/yard/index.meta", "/p/.site/projects.json"] {
        assert_ne!(send(config, get(uri)).await.0, StatusCode::OK, "{uri}");
    }
    let listed = catalog::slugs(config).await;
    assert!(listed.iter().all(|slug| !slug.contains('.')), "{listed:?}");
    // A change made now replaces the meta whole and leaves no new litter.
    let (failed, said) = tool(config, TOKEN, "set_visibility", serde_json::json!({ "slug": "yard", "listed": false })).await;
    assert!(!failed, "{said}");
    let litter = std::fs::read_dir(config.data_dir.join("yard")).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".part")).count();
    assert_eq!(litter, 1, "a change left a temporary file behind");
    assert_eq!(catalog::meta(config, "yard").await.gate.as_deref(), Some("restricted"));
    w.finish().await;
}
