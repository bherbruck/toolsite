//! The catalog through the whole router: publishing, hiding, gating, notes,
//! concurrent changes and removal, the way an agent drives them over MCP;
//! projects created, moved, renamed and removed, two moves at once, a move
//! interrupted and finished by the next runner; and host labels in
//! subdomain mode.
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

mod common;
use common::blocking;

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
    bucket: Option<toolsite::runtime::blobs::S3>,
    panics_before: usize,
}

impl Site {
    fn on_postgres(&self) -> bool {
        self.database.is_some()
    }

    /// Another runner on the same site: its own `Config`, the same data
    /// directory, and on Postgres the same database through its own stores.
    fn second_runner(&self) -> Arc<Config> {
        let local = Config::local(self.config.data_dir.clone(), TOKEN);
        Arc::new(match &self.database {
            Some((postgres, _)) => Config {
                stores: Stores::new(Backend::Postgres(postgres.clone()), None, Some(KEY)).unwrap(),
                blobs: common::blobs_in(self.bucket.as_ref().expect("a Postgres site has a bucket")),
                ..local
            },
            None => local,
        })
    }

    async fn rows(&self, sql: &str) -> Vec<tokio_postgres::Row> {
        let (postgres, _) = self.database.as_ref().expect("a Postgres site");
        postgres.pool.get().await.unwrap().query(sql, &[]).await.unwrap()
    }

    /// The project tree as the backend stores it: the rows, or the file.
    async fn stored_tree(&self) -> Vec<String> {
        let mut paths: Vec<String> = if self.on_postgres() {
            assert!(!self.config.data_dir.join(".site/projects.json").exists(), "a tree file on a Postgres site");
            self.rows("select path from platform.projects").await.iter().map(|row| row.get(0)).collect()
        } else {
            let text = std::fs::read_to_string(self.config.data_dir.join(".site/projects.json")).unwrap_or_else(|_| "[]".into());
            serde_json::from_str::<Vec<serde_json::Value>>(&text)
                .unwrap()
                .iter()
                .map(|folder| folder["path"].as_str().unwrap().to_string())
                .collect()
        };
        paths.sort();
        paths
    }

    /// Whether a move's record is kept, as the backend keeps it.
    async fn move_recorded(&self) -> bool {
        if self.on_postgres() {
            assert!(!self.config.data_dir.join(".site/relocating.json").exists());
            !self.rows("select 1 from platform.relocations").await.is_empty()
        } else {
            self.config.data_dir.join(".site/relocating.json").exists()
        }
    }

    /// Every host label issued, as the backend keeps them: label to app.
    async fn issued_labels(&self) -> std::collections::BTreeMap<String, String> {
        if self.on_postgres() {
            assert!(!self.config.data_dir.join(".site/labels.json").exists(), "a labels file on a Postgres site");
            self.rows("select label, app from platform.host_labels").await.iter().map(|row| (row.get(0), row.get(1))).collect()
        } else {
            let text = std::fs::read_to_string(self.config.data_dir.join(".site/labels.json")).unwrap_or_else(|_| "{}".into());
            serde_json::from_str(&text).unwrap()
        }
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
    site_with(postgres, |config| config).await
}

/// Subdomain mode: each app on a host of its own under `apps.test`.
fn subdomains(config: Config) -> Config {
    let base = "https://site.test";
    Config {
        base_url: Some(base.to_string()),
        apps: Some(toolsite::content::origins::AppsDomain::parse("apps.test", Some(base), None).unwrap()),
        ..config
    }
}

async fn site_with(postgres: bool, shape: fn(Config) -> Config) -> Site {
    count_panics();
    let panics_before = PANICS.load(Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    if !postgres {
        let config = Arc::new(shape(Config::local(dir.path().to_path_buf(), TOKEN)));
        return Site { _dir: dir, config, database: None, bucket: None, panics_before };
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
    let bucket = common::bucket().await;
    let config = Arc::new(Config { stores, blobs: common::blobs_in(&bucket), ..shape(Config::local(dir.path().to_path_buf(), TOKEN)) });
    Site { _dir: dir, config, database: Some((postgres, name)), bucket: Some(bucket), panics_before }
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
    get_on(config, "localhost", uri).await.0
}

async fn get_on(config: &Arc<Config>, host: &str, uri: &str) -> (StatusCode, String) {
    let request = Request::builder().uri(uri).header("host", host).body(Body::empty()).unwrap();
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn projects(config: &Arc<Config>, arguments: serde_json::Value) -> (bool, String) {
    call(config, "projects", arguments).await
}

fn project_of(config: &Config, app: &str) -> Option<String> {
    catalog::meta_blocking(config, app).project
}

fn label_of(config: &Config, app: &str) -> String {
    blocking(|| toolsite::content::origins::label_for(config, app))
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

    let kept = [common::trash_files(config, "slug.meta"), common::trash_files(config, "app/index.meta")].concat();
    assert!(kept.iter().any(|meta| meta.contains("\"hidden\":true")), "the removed meta was not kept: {kept:?}");
    if site.on_postgres() {
        assert_eq!(common::trash_files(config, "slug.notes"), vec!["the old app's notes"]);
    }
    site.finish().await;
}

/// Projects through the tool: created, an app moved in, renamed, moved
/// under another, given general access and removed, with the tree, the
/// apps' metas and the old name's alias where the backend keeps them.
async fn projects_through_the_tool(site: Site) {
    let config = &site.config;
    for (path, name) in [("", "ops"), ("ops", "yard"), ("", "finance")] {
        let (failed, said) = projects(config, serde_json::json!({ "action": "create", "path": path, "name": name })).await;
        assert!(!failed, "{said}");
    }
    assert_eq!(site.stored_tree().await, vec!["finance", "ops", "ops/yard"]);
    call(config, "push_app", serde_json::json!({ "app": "board", "pages": { "index": "<title>Board</title>" } })).await;
    let (failed, said) = projects(config, serde_json::json!({ "action": "move", "app": "board", "path": "ops/yard" })).await;
    assert!(!failed, "{said}");
    assert_eq!(project_of(config, "board").as_deref(), Some("ops/yard"));

    let (failed, said) = projects(config, serde_json::json!({ "action": "rename", "path": "ops", "name": "site" })).await;
    assert!(!failed, "{said}");
    assert_eq!(site.stored_tree().await, vec!["finance", "site", "site/yard"]);
    assert_eq!(project_of(config, "board").as_deref(), Some("site/yard"));
    let renamed = catalog::folders(config).await.into_iter().find(|f| f.path == "site").unwrap();
    assert_eq!(renamed.renamed_from, vec!["ops".to_string()]);
    assert_eq!(toolsite::content::store::renamed_path(config, "ops/yard").await.as_deref(), Some("site/yard"));

    let (failed, said) = projects(config, serde_json::json!({ "action": "move_project", "path": "site/yard", "parent": "finance" })).await;
    assert!(!failed, "{said}");
    assert_eq!(site.stored_tree().await, vec!["finance", "finance/yard", "site"]);
    assert_eq!(project_of(config, "board").as_deref(), Some("finance/yard"));

    assert_eq!(get(config, "/p/board/").await, StatusCode::OK);
    let (failed, said) = projects(config, serde_json::json!({ "action": "access", "path": "finance", "gate": "restricted" })).await;
    assert!(!failed, "{said}");
    assert_ne!(get(config, "/p/board/").await, StatusCode::OK, "the project's access did not reach its app");

    let (failed, said) = projects(config, serde_json::json!({ "action": "remove", "path": "finance" })).await;
    assert!(failed && said.contains("not empty"), "{said}");
    let (failed, said) = projects(config, serde_json::json!({ "action": "remove", "path": "site" })).await;
    assert!(!failed, "{said}");
    assert_eq!(site.stored_tree().await, vec!["finance", "finance/yard"]);
    assert!(!site.move_recorded().await, "a finished move left its record");
    site.finish().await;
}

/// Moves at once: eight renames of eight projects all land; two renames of
/// one project, one wins and the other is told the project is gone; and a
/// rename racing a move into the renamed project never leaves a project
/// whose parent is missing.
async fn two_moves_at_once(site: Site) {
    let config = site.config.clone();
    for n in 0..8 {
        projects(&config, serde_json::json!({ "action": "create", "path": "", "name": format!("p{n}") })).await;
        call(&config, "push_app", serde_json::json!({ "app": format!("a{n}"), "pages": { "index": "<title>A</title>" } })).await;
        projects(&config, serde_json::json!({ "action": "move", "app": format!("a{n}"), "path": format!("p{n}") })).await;
    }
    let mut renames = Vec::new();
    for n in 0..8 {
        let config = config.clone();
        renames.push(tokio::spawn(async move {
            projects(&config, serde_json::json!({ "action": "rename", "path": format!("p{n}"), "name": format!("q{n}") })).await
        }));
    }
    for rename in renames {
        let (failed, said) = rename.await.unwrap();
        assert!(!failed, "{said}");
    }
    let wanted: Vec<String> = (0..8).map(|n| format!("q{n}")).collect();
    assert_eq!(site.stored_tree().await, wanted);
    for n in 0..8 {
        assert_eq!(project_of(&config, &format!("a{n}")), Some(format!("q{n}")));
    }

    projects(&config, serde_json::json!({ "action": "create", "path": "", "name": "solo" })).await;
    call(&config, "push_app", serde_json::json!({ "app": "lone", "pages": { "index": "<title>Lone</title>" } })).await;
    projects(&config, serde_json::json!({ "action": "move", "app": "lone", "path": "solo" })).await;
    let both: Vec<_> = ["left", "right"]
        .into_iter()
        .map(|name| {
            let config = config.clone();
            tokio::spawn(async move { (name, projects(&config, serde_json::json!({ "action": "rename", "path": "solo", "name": name })).await) })
        })
        .collect();
    let mut won = Vec::new();
    for done in both {
        let (name, (failed, said)) = done.await.unwrap();
        if failed {
            assert!(said.contains("no project 'solo'"), "{said}");
        } else {
            won.push(name);
        }
    }
    assert_eq!(won.len(), 1, "both renames of one project went through: {won:?}");
    let tree = site.stored_tree().await;
    assert!(tree.contains(&won[0].to_string()) && !tree.contains(&"solo".to_string()), "{tree:?}");
    assert!(!tree.contains(&(if won[0] == "left" { "right" } else { "left" }).to_string()), "{tree:?}");
    assert_eq!(project_of(&config, "lone").as_deref(), Some(won[0]));

    projects(&config, serde_json::json!({ "action": "create", "path": "", "name": "dock" })).await;
    projects(&config, serde_json::json!({ "action": "create", "path": "", "name": "crate" })).await;
    let rename = {
        let config = config.clone();
        tokio::spawn(async move { projects(&config, serde_json::json!({ "action": "rename", "path": "dock", "name": "pier" })).await })
    };
    let into = {
        let config = config.clone();
        tokio::spawn(async move { projects(&config, serde_json::json!({ "action": "move_project", "path": "crate", "parent": "dock" })).await })
    };
    let (rename_refused, _) = rename.await.unwrap();
    let (refused, said) = into.await.unwrap();
    assert!(!rename_refused, "the rename was refused");
    let tree = site.stored_tree().await;
    // Either order is right: the move first, and the crate went along with
    // the rename; the rename first, and the move found no dock.
    if refused {
        assert!(said.contains("no project") || said.contains("dock"), "{said}");
        assert!(tree.contains(&"crate".to_string()), "{tree:?}");
    } else {
        assert!(tree.contains(&"pier/crate".to_string()), "{tree:?}");
    }
    for path in &tree {
        if let Some((parent, _)) = path.rsplit_once('/') {
            assert!(tree.contains(&parent.to_string()), "{path} is in the tree without {parent}: {tree:?}");
        }
    }
    assert!(!site.move_recorded().await);
    site.finish().await;
}

/// A move that stopped after its first step, as a process that died would
/// leave it, is finished by the next runner to start, and two runners
/// starting together finish it once between them: the tree, the apps, the
/// access rows and the lock all arrive, and the record is cleared.
async fn an_interrupted_move_is_finished_by_the_next_runner(site: Site) {
    use toolsite::accounts::users::{self, Scope};
    let config = site.config.clone();
    projects(&config, serde_json::json!({ "action": "create", "path": "", "name": "ops" })).await;
    projects(&config, serde_json::json!({ "action": "create", "path": "ops", "name": "warehouse" })).await;
    call(&config, "push_app", serde_json::json!({ "app": "yard", "pages": { "index": "<title>Yard</title>" } })).await;
    projects(&config, serde_json::json!({ "action": "move", "app": "yard", "path": "ops/warehouse" })).await;
    toolsite::content::store::set_locked(&config, "ops", true).await.unwrap();
    blocking(|| {
        users::sign_up(&config, "ed@x.test", "correct horse battery").unwrap();
        users::grant_scope(&config, "ed@x.test", "ops/warehouse", Scope::Editor, None).unwrap();
    });

    // The first step only, then the process is gone.
    toolsite::platform::projects::begin_relocation(&config, "ops", "ops2").await.unwrap();
    catalog::update_meta(&config, "yard", |meta| {
        meta.project = Some("ops2/warehouse".into());
        Ok(())
    })
    .await
    .unwrap();
    assert!(site.move_recorded().await);
    // In between, the lock still covers the old rows.
    let locks = blocking(|| toolsite::content::store::locked_prefixes_blocking(&config));
    assert!(locks.contains(&"ops".to_string()), "{locks:?}");

    let (one, two) = (config.clone(), site.second_runner());
    let (first, second) = tokio::join!(
        tokio::spawn(async move { toolsite::platform::projects::resume_pending(&one).await }),
        tokio::spawn(async move { toolsite::platform::projects::resume_pending(&two).await }),
    );
    first.unwrap().unwrap();
    second.unwrap().unwrap();

    assert_eq!(site.stored_tree().await, vec!["ops2", "ops2/warehouse"]);
    assert!(toolsite::content::store::folder_locked(&config, "ops2").await, "the lock did not move");
    assert_eq!(project_of(&config, "yard").as_deref(), Some("ops2/warehouse"));
    let rows = blocking(|| users::list_scopes(&config)).unwrap();
    assert!(rows.iter().any(|r| r.email == "ed@x.test" && r.prefix == "ops2/warehouse"), "{rows:?}");
    assert!(!rows.iter().any(|r| r.prefix.starts_with("ops/") || r.prefix == "ops"), "{rows:?}");
    assert!(!site.move_recorded().await, "the record outlived the move");
    site.finish().await;
}

/// Subdomain mode: apps published at once each get a label of their own,
/// every one issued where the backend keeps them; each host serves its
/// app; a label stays through a project move and a rename; and once its
/// app is removed it goes to nobody else, and comes back with the app.
async fn labels_in_subdomain_mode(site: Site) {
    let config = site.config.clone();
    let names = ["Orders", "orders", "or_ders", "ORDERS", "Shop", "shop"];
    let publishes: Vec<_> = names
        .iter()
        .map(|name| {
            let config = config.clone();
            let name = name.to_string();
            tokio::spawn(async move {
                call(&config, "push_app", serde_json::json!({ "app": name, "pages": { "index": format!("<title>{name}</title><h1>{name} home</h1>") } })).await
            })
        })
        .collect();
    for publish in publishes {
        let (failed, said) = publish.await.unwrap();
        assert!(!failed, "{said}");
    }
    let labels: Vec<String> = names.iter().map(|name| label_of(&config, name)).collect();
    let distinct: std::collections::HashSet<&String> = labels.iter().collect();
    assert_eq!(distinct.len(), names.len(), "two apps share a host: {labels:?}");
    let issued = site.issued_labels().await;
    for (name, label) in names.iter().zip(&labels) {
        assert_eq!(issued.get(label).map(String::as_str), Some(*name), "{label} is not issued to {name}: {issued:?}");
        assert_eq!(catalog::meta(&config, name).await.label.as_ref(), Some(label));
        let (status, body) = get_on(&config, &format!("{label}.apps.test"), &format!("/p/{name}/")).await;
        assert_eq!(status, StatusCode::OK, "{label}");
        assert!(body.contains(&format!("{name} home")), "{label} served another app: {body}");
    }
    assert_eq!(labels[1], "orders");

    projects(&config, serde_json::json!({ "action": "create", "path": "", "name": "ops" })).await;
    projects(&config, serde_json::json!({ "action": "move", "app": "Orders", "path": "ops" })).await;
    projects(&config, serde_json::json!({ "action": "rename", "path": "ops", "name": "yard" })).await;
    assert_eq!(project_of(&config, "Orders").as_deref(), Some("yard"));
    assert_eq!(label_of(&config, "Orders"), labels[0], "a move changed the app's host");

    let shop = labels[4].clone();
    let (failed, said) = call(&config, "remove_page", serde_json::json!({ "slug": "Shop", "confirm": "Shop" })).await;
    assert!(!failed, "{said}");
    let (status, _) = get_on(&config, &format!("{shop}.apps.test"), "/p/Shop/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    call(&config, "push_app", serde_json::json!({ "app": shop.clone(), "pages": { "index": "<title>Squat</title>" } })).await;
    assert_ne!(label_of(&config, &shop), shop, "a removed app's host went to an app named like it");
    call(&config, "push_app", serde_json::json!({ "app": "Shop", "pages": { "index": "<title>Shop</title><h1>Shop again</h1>" } })).await;
    assert_eq!(label_of(&config, "Shop"), shop, "the app came back on another host");
    let (status, body) = get_on(&config, &format!("{shop}.apps.test"), "/p/Shop/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Shop again"), "{body}");
    assert_eq!(site.issued_labels().await.get(&shop).map(String::as_str), Some("Shop"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn projects_are_created_moved_renamed_and_removed() {
    projects_through_the_tool(site(wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moves_at_once_take_turns_and_never_leave_half_a_tree() {
    two_moves_at_once(site(wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_move_is_finished_once_by_the_next_runner() {
    an_interrupted_move_is_finished_by_the_next_runner(site(wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_labels_are_issued_once_each_and_kept() {
    labels_in_subdomain_mode(site_with(wants_postgres(), subdomains).await).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn projects_are_created_moved_renamed_and_removed_on_postgres() {
    projects_through_the_tool(site(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn moves_at_once_take_turns_and_never_leave_half_a_tree_on_postgres() {
    two_moves_at_once(site(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_interrupted_move_is_finished_once_by_the_next_runner_on_postgres() {
    an_interrupted_move_is_finished_by_the_next_runner(site(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn host_labels_are_issued_once_each_and_kept_on_postgres() {
    labels_in_subdomain_mode(site_with(true, subdomains).await).await;
}
