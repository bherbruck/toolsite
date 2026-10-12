//! Moving a site between files and Postgres, end to end: a file-mode site
//! built the way people build one (examples published through the router,
//! an admin who turned on two-step sign-in, grants, scopes, projects moved
//! and renamed, labels, settings, every kind of token, jobs, OAuth clients,
//! a removal in the trash, apps' files on the volume), migrated, served,
//! and brought back.
//!
//! Every test needs Postgres and MinIO and is ignored unless asked for:
//! `scripts/test-postgres.sh` starts both. Each works in a database and a
//! bucket of its own.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use toolsite::{
    accounts::{
        mfa::{self, Clock, Policy, Settings},
        users,
    },
    build_router,
    content::catalog,
    platform::{
        tokens::{self, Kind},
        transfer::{self, Options},
        upload::UploadTicket,
    },
    runtime::wasm::Runtime,
    state::{self, Backend},
    Config,
};
use tower::ServiceExt;

const TOKEN: &str = "test-token";
const BASE: &str = "https://site.test";
const PASSWORD: &str = "correct horse battery";
/// Mid-step, so one step either side is plainly 30 seconds away.
const T0: u64 = 1_800_000_015;
const EXAMPLES: [&str; 4] = ["static-report", "kitchen-sink", "orders", "blob-gallery"];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// --- talking to a site ---------------------------------------------------------

struct Reply {
    status: StatusCode,
    body: String,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .find_map(|(_, v)| v.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap_or("").to_string()))
            .filter(|value| !value.is_empty())
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

async fn get(config: &Arc<Config>, uri: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    send(config, request.body(Body::empty()).unwrap()).await
}

async fn post(config: &Arc<Config>, uri: &str, body: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::builder().method("POST").uri(uri).header("content-type", "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    send(config, request.body(Body::from(body.to_string())).unwrap()).await
}

async fn put(config: &Arc<Config>, uri: &str, bearer: Option<&str>, body: Vec<u8>) -> Reply {
    let mut request = Request::builder().method("PUT").uri(uri);
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    send(config, request.body(Body::from(body)).unwrap()).await
}

async fn tool(config: &Arc<Config>, name: &str, arguments: serde_json::Value) -> String {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let reply = send(config, request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(!reply.body.contains("\"isError\":true"), "{name} {arguments}: {}", reply.body);
    reply.body
}

// --- publishing an example as the CLI does ---------------------------------------

fn migrations_archive(dir: &Path) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let body = std::fs::read(dir.join(&name)).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name.to_string_lossy().to_string(), body.as_slice()).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

async fn publish(config: &Arc<Config>, name: &str) {
    let source = root().join("examples").join(name);
    let fixtures = root().join("tests/fixtures/examples");
    let ticket = UploadTicket { slug: name.to_string(), user: None, project: None };
    let ticket = toolsite::platform::upload::issue_ticket(config, &ticket, Duration::from_secs(600)).await.unwrap();
    let upload = |flag: &'static str, body: Vec<u8>| {
        let (config, ticket) = (config.clone(), ticket.clone());
        async move {
            let reply = put(&config, &format!("/upload/{ticket}?{flag}"), None, body).await;
            assert!(reply.status.is_success(), "upload ?{flag}: {} {}", reply.status, reply.body);
        }
    };
    if source.join("migrations").is_dir() {
        upload("migrations", migrations_archive(&source.join("migrations"))).await;
    }
    upload("manifest", std::fs::read(source.join("toolsite.toml")).unwrap()).await;
    if let Ok(wasm) = std::fs::read(fixtures.join(format!("{name}.wasm"))) {
        upload("handler", wasm).await;
    }
    upload("bundle", std::fs::read(fixtures.join(format!("{name}-dist.tar.gz"))).unwrap()).await;
}

// --- two-step sign-in, as a phone would answer ------------------------------------

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(&text[from..to])
}

/// The code a phone shows one step later than the site's clock, which moves
/// on to it, so each sign-in spends a step of its own.
fn next_code(config: &Config, secret: &str) -> String {
    let now = config.mfa.clock.now() + mfa::STEP;
    config.mfa.clock.set(now);
    mfa::code_at(secret, now).unwrap()
}

/// Turns two-step sign-in on from the account page. The secret, and the
/// recovery codes.
async fn enable_mfa(config: &Arc<Config>, user: &users::User) -> (String, Vec<String>) {
    let session = users::log_in(config, &user.email, PASSWORD).unwrap().1;
    let cookie = format!("ts_session={session}");
    let token = users::derive_form_token(config, &user.id);
    let started = post(config, "/account/mfa/start", &format!("token={token}"), Some(&cookie)).await;
    assert_eq!(started.status, StatusCode::SEE_OTHER, "{}", started.body);
    let page = get(config, "/account", Some(&cookie)).await;
    let secret = between(&page.body, "<code id=\"mfa-key\">", "</code>").unwrap().replace(' ', "");
    let code = next_code(config, &secret);
    let done = post(
        config,
        "/account/mfa/confirm",
        &format!("token={token}&code={code}&password={}", urlencoding::encode(PASSWORD)),
        Some(&cookie),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    let list = between(&done.body, "id=\"recovery-codes\">", "</ul>").unwrap();
    let codes = list.split("<li>").skip(1).map(|item| item.split("</li>").next().unwrap().to_string()).collect();
    (secret, codes)
}

/// A password, then a code: the site session it ends with.
async fn sign_in(config: &Arc<Config>, email: &str, code: &str) -> Result<String, String> {
    let body = format!("email={}&password={}&next=%2F", urlencoding::encode(email), urlencoding::encode(PASSWORD));
    let first = post(config, "/auth/login", &body, None).await;
    let pending = first.cookie("ts_mfa").ok_or_else(|| format!("no code was asked for: {} {}", first.status, first.body))?;
    let second = post(config, "/auth/mfa", &format!("code={}", urlencoding::encode(code)), Some(&format!("ts_mfa={pending}"))).await;
    second.cookie("ts_session").ok_or_else(|| format!("the code was refused: {} {}", second.status, second.body))
}

// --- the site to move -------------------------------------------------------------

/// What the fixture hands the tests: the secrets a person would hold.
struct Built {
    config: Arc<Config>,
    mfa_secret: String,
    recovery: Vec<String>,
    deploy_token: String,
    export_token: String,
    device_token: String,
    /// A site session made in file mode.
    viewer_session: String,
}

/// A data directory inside a temporary one, so the fixture's links have
/// somewhere outside it to point that goes when the test does.
struct SiteDir {
    _root: tempfile::TempDir,
    path: PathBuf,
}

impl SiteDir {
    fn new() -> SiteDir {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("site");
        std::fs::create_dir(&path).unwrap();
        SiteDir { _root: root, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn files_site(dir: &Path) -> Arc<Config> {
    Arc::new(Config {
        base_url: Some(BASE.to_string()),
        mfa: Settings { policy: Policy::Off, for_providers: false, clock: Clock::fixed(T0) },
        ..Config::local(dir.to_path_buf(), TOKEN)
    })
}

/// A file-mode site with one of everything, built through the router and
/// the functions the routes call.
async fn build(dir: &Path) -> Built {
    common::secret_key();
    let config = files_site(dir);
    for name in EXAMPLES {
        publish(&config, name).await;
    }
    let page = UploadTicket { slug: "gone".into(), user: None, project: None };
    let ticket = toolsite::platform::upload::issue_ticket(&config, &page, Duration::from_secs(600)).await.unwrap();
    assert!(put(&config, &format!("/upload/{ticket}"), None, b"<h1>gone soon</h1>".to_vec()).await.status.is_success());

    let (mfa_secret, recovery) = {
        let admin = users::sign_up_as(&config, "admin@example.com", PASSWORD, true).unwrap();
        enable_mfa(&config, &admin).await
    };
    let viewer = users::sign_up(&config, "viewer@example.com", PASSWORD).unwrap();
    let viewer_session = users::log_in(&config, "viewer@example.com", PASSWORD).unwrap().1;
    users::grant(&config, "viewer@example.com", "kitchen-sink", "manager").unwrap();
    users::set_pin(&config, &viewer.id, "orders", true).unwrap();

    tool(&config, "projects", serde_json::json!({"action": "create", "path": "", "name": "ops"})).await;
    tool(&config, "projects", serde_json::json!({"action": "create", "path": "ops", "name": "yard"})).await;
    tool(&config, "projects", serde_json::json!({"action": "move", "app": "orders", "path": "ops/yard"})).await;
    tool(&config, "projects", serde_json::json!({"action": "rename", "path": "ops", "name": "operations"})).await;
    tool(&config, "projects", serde_json::json!({"action": "grant", "path": "operations", "email": "viewer@example.com", "scope": "editor"})).await;

    let catalog = catalog::of(&config);
    common::blocking(|| catalog.assign_label_blocking("orders", true, Box::new(|_| "orders-7k".into()))).unwrap();
    catalog::set_notes(&config, "kitchen-sink", "the heartbeat job is what keeps it warm").await.unwrap();
    toolsite::platform::secrets::set(&config, "kitchen-sink", "GITHUB_TOKEN", Some("ghp_not_a_real_one")).await.unwrap();
    toolsite::platform::records::of(&config).set_installations(r#"[{"id": 7, "account": "herbrucks"}]"#).await.unwrap();
    let (_, deploy_token) = tokens::create(&config, "static-report", Kind::Deploy, "ci", "ci").await.unwrap();
    let (_, export_token) = tokens::create(&config, "orders", Kind::Export, "warehouse", "warehouse").await.unwrap();
    let (_, device_token) = tokens::create(&config, "kitchen-sink", Kind::Device, "scanner 1", "scanner 1").await.unwrap();
    let client = toolsite::platform::oauth_store::register_client(&config, Some("Claude"), &["https://claude.ai/cb".into()]).unwrap();
    let admin = users::user_by_email(&config, "admin@example.com").unwrap();
    toolsite::platform::oauth_store::issue_tokens(&config, &client.id, &admin.id, None).unwrap();
    toolsite::runtime::blobs::put(&config, "blob-gallery", "photos/one.png", "image/png", b"\x89PNG not really").unwrap();
    toolsite::runtime::blobs::put(&config, "blob-gallery", "notes.txt", "text/plain", b"a file of the app's own").unwrap();
    common::blocking(|| toolsite::platform::trash::remove(&config, "gone", 1_800_000_000)).unwrap();

    // Links out of the directory: never followed, never copied.
    let outside = dir.parent().unwrap().join(format!("outside-{}", toolsite::content::slug::random_token(6)));
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("index.html"), b"<h1>not the site's</h1>").unwrap();
    std::fs::write(outside.join("settings.json"), br#"{"STOLEN": "x"}"#).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("evil")).unwrap();
    std::os::unix::fs::symlink(outside.join("index.html"), dir.join("static-report/leak.html")).unwrap();
    std::os::unix::fs::symlink(outside.join("settings.json"), dir.join("linked.secrets")).unwrap();

    Built { config, mfa_secret, recovery, deploy_token, export_token, device_token, viewer_session }
}

// --- the database and bucket it moves to ----------------------------------------

fn postgres_site(dir: &Path, database: &common::Database) -> Arc<Config> {
    Arc::new(Config {
        base_url: Some(BASE.to_string()),
        mfa: Settings { policy: Policy::Off, for_providers: false, clock: Clock::fixed(T0 + 3600) },
        stores: database.stores(),
        blobs: database.blobs(),
        ..Config::local(dir.to_path_buf(), TOKEN)
    })
}

fn key() -> [u8; 32] {
    toolsite::seal::parse_key(&common::secret_key()).unwrap()
}

async fn migrate(from: &Arc<Config>, to: &Arc<Config>, options: Options) -> Result<transfer::Report, String> {
    let (from, to) = (from.clone(), to.clone());
    tokio::task::spawn_blocking(move || {
        let prepared = transfer::prepare(&from, &key(), &options)?;
        transfer::migrate(prepared, &from, &to, &options)
    })
    .await
    .unwrap()
}

async fn export(from: &Arc<Config>, to: &Arc<Config>, options: Options) -> Result<transfer::Report, String> {
    let (from, to) = (from.clone(), to.clone());
    tokio::task::spawn_blocking(move || transfer::export(&from, &to, &options)).await.unwrap()
}

/// Every row of the platform's schemas and every object in the bucket with
/// its size and time: what "nothing changed" is checked against.
async fn everything(database: &common::Database) -> String {
    let client = database.postgres.pool.get().await.unwrap();
    let tables = client
        .query(
            "select table_schema, table_name from information_schema.tables
              where table_schema in ('accounts', 'oauth', 'platform') order by 1, 2",
            &[],
        )
        .await
        .unwrap();
    let mut all = String::new();
    for table in tables {
        let (schema, name): (String, String) = (table.get(0), table.get(1));
        let mut rows: Vec<String> = client
            .query(&format!("select t::text from {schema}.{name} t"), &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        rows.sort();
        all.push_str(&format!("{schema}.{name}\n{}\n", rows.join("\n")));
    }
    let listing = database.bucket.object_list("", false).await.unwrap();
    for (key, object) in listing.objects {
        all.push_str(&format!("{key} {} {:?}\n", object.size, object.modified));
    }
    all
}

fn differences(report: &transfer::Report) -> Vec<String> {
    report.lines.iter().flat_map(|line| line.differences.iter().map(move |d| format!("{}: {d}", line.name))).collect()
}

// --- the tests --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_site_goes_to_postgres_and_back_unchanged() {
    let (dir, back) = (SiteDir::new(), tempfile::tempdir().unwrap());
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);

    let report = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!(differences(&report), Vec::<String>::new(), "{}", report.table());
    let marker = std::fs::read_to_string(dir.path().join(".site").join(state::MARKER)).unwrap();
    assert!(marker.contains("migrate-to-postgres") && marker.contains("published files"), "{marker}");
    for linked in ["evil", "static-report/leak.html", "linked.secrets"] {
        assert!(report.notes.iter().any(|note| note.contains(linked)), "no word of {linked}: {:?}", report.notes);
    }
    let objects: Vec<String> = database.bucket.object_list("", false).await.unwrap().objects.into_iter().map(|(key, _)| key).collect();
    assert!(objects.contains(&"blob-gallery/photos/one.png".to_string()), "apps' files did not reach the bucket: {objects:?}");
    assert!(objects.contains(&".toolsite/content/kitchen-sink/handler.wasm".to_string()), "{objects:?}");
    assert!(
        !objects.iter().any(|key| key.contains("evil") || key.contains("leak.html") || key.contains("data.db")),
        "a link was followed or a database copied: {objects:?}"
    );
    let settings = toolsite::platform::records::of(&target).settings("linked").await.unwrap();
    assert!(settings.is_empty(), "a linked sidecar was read: {settings:?}");

    // Not over the directory it left: what it still holds would bring back
    // whatever is removed on Postgres from now on.
    let volume = Arc::new(transfer::side(dir.path().to_path_buf(), Backend::Files, None, None).unwrap());
    let refused = export(&target, &volume, Options::default()).await.expect_err("exported over the old state");
    assert!(refused.contains("--to"), "{refused}");

    // Back into a fresh directory, beside the volume the databases are on.
    let returned = Arc::new(transfer::side(back.path().to_path_buf(), Backend::Files, None, None).unwrap());
    let report = export(&target, &returned, Options::default()).await.unwrap();
    assert_eq!(differences(&report), Vec::<String>::new(), "{}", report.table());
    assert!(!back.path().join(".site").join(state::MARKER).exists());

    // The directory that came back holds what the one that left held.
    let original = Arc::new(transfer::side(dir.path().to_path_buf(), Backend::Files, None, None).unwrap());
    std::fs::remove_file(dir.path().join(".site").join(state::MARKER)).unwrap();
    let compared = export(&original, &returned, Options { verify_only: true, ..Options::default() }).await.unwrap();
    assert_eq!(differences(&compared), Vec::<String>::new(), "{}", compared.table());
    let published = compared.lines.iter().find(|line| line.name == "published files").unwrap().here;
    assert!(published > 10, "only {published} published files: {}", compared.table());
    for app in ["kitchen-sink", "orders"] {
        let database = |root: &Path| std::fs::read(root.join(app).join("data.db")).unwrap();
        assert_eq!(database(dir.path()), database(back.path()), "{app}'s database did not come across whole");
    }
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn an_interrupted_migration_resumes_and_finishes_on_postgres() {
    let dir = SiteDir::new();
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    let mut written = 0;
    for most in [3, 9, 4] {
        let stopped = migrate(&built.config, &target, Options { fail_after: Some(most), ..Options::default() }).await;
        let why = stopped.expect_err("the run was not stopped");
        assert!(why.contains("stopped after"), "{why}");
        assert!(!dir.path().join(".site").join(state::MARKER).exists(), "a stopped run wrote the marker");
        written += most;
    }
    let finished = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!(differences(&finished), Vec::<String>::new(), "{}", finished.table());
    assert!(finished.written > 0, "the stops wrote {written} and left nothing");
    assert!(dir.path().join(".site").join(state::MARKER).exists());
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn migrating_twice_changes_nothing_the_second_time_on_postgres() {
    let dir = SiteDir::new();
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    migrate(&built.config, &target, Options::default()).await.unwrap();
    let before = everything(&database).await;

    // On a migrated directory a run only compares.
    let again = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!((again.written, differences(&again).len()), (0, 0), "{}", again.table());
    assert!(again.notes.iter().any(|note| note.contains("migrated already")), "{:?}", again.notes);
    // And with the marker taken away, a copy finds nothing to write.
    std::fs::remove_file(dir.path().join(".site").join(state::MARKER)).unwrap();
    let copied = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!((copied.written, differences(&copied).len()), (0, 0), "{}", copied.table());
    assert_eq!(everything(&database).await, before, "a second run changed the database or the bucket");
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn verify_only_finds_a_planted_difference_and_writes_nothing_on_postgres() {
    let dir = SiteDir::new();
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    migrate(&built.config, &target, Options::default()).await.unwrap();
    std::fs::remove_file(dir.path().join(".site").join(state::MARKER)).unwrap();

    database.bucket.object_delete(".toolsite/content/orders/index.html").await.unwrap();
    toolsite::platform::records::of(&target).set_setting("kitchen-sink", "GITHUB_TOKEN", Some("planted")).await.unwrap();
    let before = everything(&database).await;
    let checked = migrate(&built.config, &target, Options { verify_only: true, ..Options::default() }).await.unwrap();
    let found = differences(&checked);
    assert!(found.contains(&"published files: missing orders/index.html".to_string()), "{found:?}");
    assert!(found.contains(&"settings: different kitchen-sink GITHUB_TOKEN".to_string()), "{found:?}");
    assert_eq!(found.len(), 2, "{}", checked.table());
    assert_eq!(checked.written, 0);
    assert_eq!(everything(&database).await, before, "a verify wrote");
    assert!(!dir.path().join(".site").join(state::MARKER).exists(), "a verify wrote the marker");
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_migrated_site_serves_signs_in_and_deploys_on_postgres() {
    let dir = SiteDir::new();
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    migrate(&built.config, &target, Options::default()).await.unwrap();

    // File mode refuses the directory now; Postgres mode starts on it.
    let files = state::Settings { database_url: None, secret_key: None, bucket: false, pool_size: 4 };
    let why = state::check(&files, dir.path()).unwrap_err();
    assert!(why.contains("export-to-files"), "{why}");
    let on_postgres =
        state::Settings { database_url: Some("postgres://unused".into()), secret_key: Some(common::secret_key()), bucket: true, pool_size: 4 };
    state::check(&on_postgres, dir.path()).unwrap();

    // Every migrated app serves.
    let report = get(&target, "/p/static-report/", None).await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);
    assert!(report.body.contains("<title>Q3 shipping report</title>"), "{}", report.body);
    for app in ["kitchen-sink", "orders", "blob-gallery"] {
        let reply = get(&target, &format!("/p/{app}/"), None).await;
        assert!(reply.status != StatusCode::NOT_FOUND, "{app} is gone: {} {}", reply.status, reply.body);
    }
    assert_eq!(get(&target, "/p/gone/", None).await.status, StatusCode::NOT_FOUND, "a removed page came back");
    assert_eq!(get(&target, "/p/evil/", None).await.status, StatusCode::NOT_FOUND, "a link was copied");

    // The admin signs in with password and code, and with a recovery code.
    let code = next_code(&target, &built.mfa_secret);
    let session = sign_in(&target, "admin@example.com", &code).await.unwrap();
    assert!(common::blocking(|| users::site_session_user(&target, &session)).is_some_and(|user| user.is_admin));
    assert!(sign_in(&target, "admin@example.com", &built.recovery[0]).await.is_ok(), "a recovery code was refused");
    assert!(sign_in(&target, "admin@example.com", &built.recovery[0]).await.is_err(), "a recovery code worked twice");
    // A session from before the move still works, with its grant.
    let viewer = common::blocking(|| users::site_session_user(&target, &built.viewer_session)).expect("a file-mode session was lost");
    assert_eq!(common::blocking(|| users::role_for(&target, &viewer.id, "kitchen-sink")).as_deref(), Some("manager"));
    assert_eq!(toolsite::platform::secrets::get(&target, "kitchen-sink", "GITHUB_TOKEN").await.as_deref(), Some("ghp_not_a_real_one"));

    // Every kind of token carried over and still opens only its own app.
    assert!(tokens::check(&target, "orders", Kind::Export, &built.export_token).await.is_some());
    assert!(tokens::check(&target, "kitchen-sink", Kind::Device, &built.device_token).await.is_some());
    assert!(tokens::check(&target, "orders", Kind::Deploy, &built.deploy_token).await.is_none());
    let bundle = std::fs::read(root().join("tests/fixtures/examples/static-report-dist.tar.gz")).unwrap();
    let deployed = put(&target, "/deploy/static-report?bundle", Some(&built.deploy_token), bundle).await;
    assert!(deployed.status.is_success(), "{} {}", deployed.status, deployed.body);
    let page = put(&target, "/deploy/static-report/about", Some(&built.deploy_token), b"<h1>after the move</h1>".to_vec()).await;
    assert!(page.status.is_success(), "{} {}", page.status, page.body);
    assert_eq!(get(&target, "/p/static-report/about", None).await.body, "<h1>after the move</h1>");
    assert!(!put(&target, "/deploy/orders?bundle", Some(&built.deploy_token), vec![]).await.status.is_success());
    database.drop().await;
}

/// Through the command itself: a wrong key is refused before the database
/// has a schema or the bucket an object; the right one migrates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_command_refuses_a_wrong_key_before_writing_anything_on_postgres() {
    let dir = SiteDir::new();
    build(dir.path()).await;
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(common::NEEDS);
    let name = format!("t_{}", toolsite::content::slug::random_token(12).to_lowercase());
    let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("create database {name}")).await.unwrap();
    let mut url = url::Url::parse(&server).unwrap();
    url.set_path(&name);
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} unset; scripts/test-postgres.sh sets it"));
    let bucket_name = format!("t-{}", toolsite::content::slug::random_token(12).to_lowercase());
    let bucket = toolsite::runtime::blobs::S3::new(
        &var("TOOLSITE_TEST_S3_ENDPOINT"),
        &bucket_name,
        "us-east-1",
        &var("TOOLSITE_TEST_S3_ACCESS_KEY_ID"),
        &var("TOOLSITE_TEST_S3_SECRET_ACCESS_KEY"),
        true,
    )
    .unwrap();
    bucket.ensure_bucket().await.unwrap();
    let bucket_env = |command: &mut std::process::Command| {
        let var = |name: &str| std::env::var(name).unwrap();
        command
            .env("TOOLSITE_BLOB_S3_ENDPOINT", var("TOOLSITE_TEST_S3_ENDPOINT"))
            .env("TOOLSITE_BLOB_S3_ACCESS_KEY_ID", var("TOOLSITE_TEST_S3_ACCESS_KEY_ID"))
            .env("TOOLSITE_BLOB_S3_SECRET_ACCESS_KEY", var("TOOLSITE_TEST_S3_SECRET_ACCESS_KEY"))
            .env("TOOLSITE_BLOB_S3_REGION", "us-east-1")
            .env("TOOLSITE_BLOB_S3_PATH_STYLE", "1");
    };
    let run = |key: &str| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_toolsite"));
        command
            .arg("migrate-to-postgres")
            .env("DATABASE_URL", url.as_str())
            .env_remove("TOOLSITE_DATABASE_URL")
            .env("TOOLSITE_SECRET_KEY", key)
            .env("TOOLSITE_DATA_DIR", dir.path())
            .env("TOOLSITE_BLOB_S3_BUCKET", &bucket_name);
        bucket_env(&mut command);
        let out = command.output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };

    let wrong = {
        use base64::Engine as _;
        let bytes: Vec<u8> = (0..32u8).map(|n| n.wrapping_mul(37).wrapping_add(11)).collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    };
    let (ok, text) = run(&wrong);
    assert!(!ok && text.contains("does not open"), "{text}");
    assert!(!text.contains("ghp_not_a_real_one"), "a sealed value was printed");
    let schemas: Vec<String> = {
        let mut url = url.clone();
        url.set_query(Some("sslmode=disable"));
        let (client, connection) = tokio_postgres::connect(url.as_str(), tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client
            .query("select schema_name from information_schema.schemata where schema_name in ('state', 'accounts', 'oauth', 'platform')", &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    assert!(schemas.is_empty(), "a refused run made {schemas:?}");
    assert!(bucket.object_list("", false).await.unwrap().objects.is_empty(), "a refused run wrote to the bucket");
    assert!(!dir.path().join(".site").join(state::MARKER).exists());

    let (ok, text) = run(&common::secret_key());
    assert!(ok, "{text}");
    assert!(text.contains("migrated:") && text.contains("published files"), "{text}");
    let (ok, text) = run(&common::secret_key());
    assert!(ok && text.contains("migrated already") && text.contains("writes: 0") && text.contains("both sides match"), "{text}");
    client.batch_execute(&format!("drop database if exists {name} with (force)")).await.unwrap();
}

/// `toolsite key show` prints the key a file-mode site made, for
/// TOOLSITE_SECRET_KEY, and nothing else on stdout.
#[test]
fn key_show_prints_the_sites_key() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".site")).unwrap();
    let key: Vec<u8> = (0..32u8).map(|n| n.wrapping_mul(29).wrapping_add(3)).collect();
    std::fs::write(dir.path().join(".site/secret.key"), &key).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_toolsite"))
        .args(["key", "show"])
        .env("TOOLSITE_DATA_DIR", dir.path())
        .env("RUST_LOG", "off")
        .env_remove("TOOLSITE_SECRET_KEY")
        .env_remove("DATABASE_URL")
        .env_remove("TOOLSITE_DATABASE_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(printed, base64::engine::general_purpose::STANDARD.encode(&key));
    assert_eq!(toolsite::seal::parse_key(&printed).unwrap().to_vec(), key);

    let empty = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_toolsite"))
        .args(["key", "show"])
        .env("TOOLSITE_DATA_DIR", empty.path())
        .env("RUST_LOG", "off")
        .env_remove("TOOLSITE_SECRET_KEY")
        .env_remove("DATABASE_URL")
        .env_remove("TOOLSITE_DATABASE_URL")
        .output()
        .unwrap();
    assert!(!out.status.success() && out.stdout.is_empty(), "a key was printed for a site with none");
}

// --- revocations, and what may run beside a copy ---------------------------------

/// What the revocations made on the file-mode site answer on another site:
/// each must still be refused.
struct Refusals {
    disabled: String,
    revoked_deploy: String,
    revoked_oauth: String,
    retired: String,
}

async fn revoke_on_files(built: &Built) -> Refusals {
    let config = &built.config;
    users::sign_up(config, "left@example.com", PASSWORD).unwrap();
    users::set_active(config, "left@example.com", false).unwrap();
    let (revoked, revoked_deploy) = tokens::create(config, "orders", Kind::Deploy, "old ci", "ci").await.unwrap();
    tokens::revoke(config, "orders", Kind::Deploy, &revoked.id).await.unwrap();
    let (_, retired) = tokens::create(config, "not-yet", Kind::Deploy, "early", "someone").await.unwrap();
    common::blocking(|| toolsite::platform::trash::retire_tokens(config, "not-yet", 1_800_000_100)).unwrap();
    let viewer = users::user_by_email(config, "viewer@example.com").unwrap();
    let client = toolsite::platform::oauth_store::register_client(config, Some("Old"), &["https://o.test/cb".into()]).unwrap();
    let issued = toolsite::platform::oauth_store::issue_tokens(config, &client.id, &viewer.id, None).unwrap();
    toolsite::platform::oauth_store::revoke_for_user(config, &viewer.id).unwrap();
    // A recovery code spent before the move.
    assert!(sign_in(config, "admin@example.com", &built.recovery[1]).await.is_ok());
    Refusals { disabled: "left@example.com".into(), revoked_deploy, revoked_oauth: issued.access_token, retired }
}

async fn assert_refused(config: &Arc<Config>, built: &Built, refusals: &Refusals, side: &str) {
    assert!(common::blocking(|| users::check_password(config, &refusals.disabled, PASSWORD)).is_err(), "{side}: a disabled account signs in");
    assert!(tokens::check(config, "orders", Kind::Deploy, &refusals.revoked_deploy).await.is_none(), "{side}: a revoked token opens");
    assert!(tokens::check(config, "not-yet", Kind::Deploy, &refusals.retired).await.is_none(), "{side}: a retired token opens");
    assert!(
        common::blocking(|| toolsite::platform::oauth_store::access_token_holder(config, &refusals.revoked_oauth)).is_none(),
        "{side}: a revoked OAuth token opens"
    );
    assert!(sign_in(config, "admin@example.com", &built.recovery[1]).await.is_err(), "{side}: a spent recovery code worked");
    assert_eq!(get(config, "/p/gone/", None).await.status, StatusCode::NOT_FOUND, "{side}: a removed app came back");
    assert!(tokens::check(config, "orders", Kind::Export, &built.export_token).await.is_some(), "{side}: a live token was lost");
}

/// Every revocation made on files is one on Postgres after the move, and
/// on files again after the way back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn revocations_stay_revoked_both_ways_on_postgres() {
    let (dir, back) = (SiteDir::new(), tempfile::tempdir().unwrap());
    let built = build(dir.path()).await;
    let refusals = revoke_on_files(&built).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    let report = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!(differences(&report), Vec::<String>::new(), "{}", report.table());
    assert_refused(&target, &built, &refusals, "postgres").await;
    let returned = files_site(back.path());
    let report = export(&target, &returned, Options::default()).await.unwrap();
    assert_eq!(differences(&report), Vec::<String>::new(), "{}", report.table());
    assert_refused(&returned, &built, &refusals, "files again").await;
    database.drop().await;
}

/// Once a database was migrated into, a second copy never runs over it,
/// whatever the directory says: what changed on Postgres since (an account
/// turned off, a token revoked) stays changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_database_migrated_into_is_never_copied_over_on_postgres() {
    let dir = SiteDir::new();
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    migrate(&built.config, &target, Options::default()).await.unwrap();
    common::blocking(|| users::set_active(&target, "viewer@example.com", false)).unwrap();
    tokens::revoke_all(&target, "orders", Kind::Export).await.unwrap();

    // The marker gone, or a copy of the directory from before: either way
    // the directory alone no longer says it moved.
    std::fs::remove_file(dir.path().join(".site").join(state::MARKER)).unwrap();
    let again = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert!(again.compared_only && again.written == 0, "{}", again.table());
    assert!(again.notes.iter().any(|note| note.contains("migrated into already")), "{:?}", again.notes);
    assert!(!differences(&again).is_empty(), "the changes on Postgres were not seen");
    assert!(common::blocking(|| users::check_password(&target, "viewer@example.com", PASSWORD)).is_err(), "the account came back");
    assert!(tokens::check(&target, "orders", Kind::Export, &built.export_token).await.is_none(), "the token came back");
    database.drop().await;
}

/// A database holding a site of its own is not copied into: the two would
/// be merged, and what that site revoked overwritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_database_holding_a_site_is_not_copied_into_on_postgres() {
    let dir = SiteDir::new();
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    // Another site's admin, turned off there; the files hold the same email.
    common::blocking(|| users::sign_up_as(&target, "viewer@example.com", PASSWORD, false)).unwrap();
    common::blocking(|| users::set_active(&target, "viewer@example.com", false)).unwrap();
    let before = everything(&database).await;
    let why = migrate(&built.config, &target, Options::default()).await.expect_err("copied into a site");
    assert!(why.contains("already holds a site") && why.contains("accounts"), "{why}");
    assert_eq!(everything(&database).await, before, "a refused run wrote");
    assert!(!dir.path().join(".site").join(state::MARKER).exists());
    database.drop().await;
}

/// From the first write until both sides match, no runner starts on the
/// database, from any directory, and it is not exported: it holds an
/// account without its two-step secret, or grants without the marker that
/// they were adopted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn no_runner_starts_on_a_half_migrated_database_on_postgres() {
    let (dir, elsewhere, back) = (SiteDir::new(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    let settings = state::Settings { database_url: Some(database.url()), secret_key: Some(common::secret_key()), bucket: true, pool_size: 2 };
    state::open(&settings, elsewhere.path()).await.map(|_| ()).expect("a fresh database was refused");
    std::fs::remove_file(elsewhere.path().join(".site").join(state::MARKER)).unwrap();

    let stopped = migrate(&built.config, &target, Options { fail_after: Some(5), ..Options::default() }).await;
    assert!(stopped.is_err());
    let why = state::open(&settings, elsewhere.path()).await.map(|_| ()).expect_err("a runner started on half a site");
    assert!(why.contains("has not finished"), "{why}");
    assert!(!elsewhere.path().join(".site").join(state::MARKER).exists(), "a refused boot claimed the directory");
    let returned = Arc::new(transfer::side(back.path().to_path_buf(), Backend::Files, None, None).unwrap());
    let why = export(&target, &returned, Options::default()).await.expect_err("half a site was exported");
    assert!(why.contains("has not finished"), "{why}");

    let finished = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!(differences(&finished), Vec::<String>::new(), "{}", finished.table());
    state::open(&settings, elsewhere.path()).await.map(|_| ()).expect("a migrated database was refused");
    database.drop().await;
}

/// A runner whose heartbeat stalled past the live window is not live by
/// the clock, and gets past the check at the start. If it beats again
/// while the copy runs, the run does not say it finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_runner_beating_during_the_copy_stops_it_finishing_on_postgres() {
    let dir = SiteDir::new();
    let built = build(dir.path()).await;
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    let client = database.postgres.pool.get().await.unwrap();
    client
        .execute(
            "insert into state.runners (id, roles, pool, started_at, heartbeat_at) values ('stalled', '{control,worker}', 'default', 1, 1)",
            &[],
        )
        .await
        .unwrap();
    assert!(state::runners::Runner::live(&database.postgres.pool).await.unwrap().is_empty(), "the stalled runner counts as live");
    let beating = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let beat = {
        let (pool, beating) = (database.postgres.pool.clone(), beating.clone());
        tokio::spawn(async move {
            let mut at = 2i64;
            while beating.load(std::sync::atomic::Ordering::SeqCst) {
                let client = pool.get().await.unwrap();
                client.execute("update state.runners set heartbeat_at = $1 where id = 'stalled'", &[&at]).await.unwrap();
                at += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let why = migrate(&built.config, &target, Options::default()).await.expect_err("finished beside a running server");
    assert!(why.contains("heartbeat") && why.contains("stalled"), "{why}");
    assert!(!dir.path().join(".site").join(state::MARKER).exists(), "marked beside a running server");
    beating.store(false, std::sync::atomic::Ordering::SeqCst);
    beat.await.unwrap();
    let finished = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!(differences(&finished), Vec::<String>::new(), "{}", finished.table());
    database.drop().await;
}

/// A value Postgres will not hold (a NUL in a text column, which SQLite
/// keeps) stops the copy part way. What was written is never served: the
/// database stays marked as being copied into until a run finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_value_postgres_refuses_leaves_no_half_site_to_serve_on_postgres() {
    let (dir, elsewhere) = (SiteDir::new(), tempfile::tempdir().unwrap());
    let built = build(dir.path()).await;
    let admin = users::user_by_email(&built.config, "admin@example.com").unwrap();
    let auth = rusqlite::Connection::open(dir.path().join(".site/auth.db")).unwrap();
    auth.execute(
        "insert into mfa_pending (token_hash, user_id, stage, next, expires_at, failures) values (?1, ?2, 'code', '/', 9999999999, 0)",
        rusqlite::params!["nul\u{0}hash", admin.id],
    )
    .unwrap();
    let database = common::Database::new().await;
    let target = postgres_site(dir.path(), &database);
    let why = migrate(&built.config, &target, Options::default()).await.expect_err("Postgres took a NUL");
    assert!(why.to_lowercase().contains("0x00") || why.contains("invalid"), "{why}");
    let settings = state::Settings { database_url: Some(database.url()), secret_key: Some(common::secret_key()), bucket: true, pool_size: 2 };
    let refused = state::open(&settings, elsewhere.path()).await.map(|_| ()).expect_err("a runner started on part of a site");
    assert!(refused.contains("has not finished"), "{refused}");
    assert!(!dir.path().join(".site").join(state::MARKER).exists());

    auth.execute("delete from mfa_pending where token_hash = ?1", ["nul\u{0}hash"]).unwrap();
    let finished = migrate(&built.config, &target, Options::default()).await.unwrap();
    assert_eq!(differences(&finished), Vec::<String>::new(), "{}", finished.table());
    state::open(&settings, elsewhere.path()).await.map(|_| ()).expect("a migrated database was refused");
    database.drop().await;
}

