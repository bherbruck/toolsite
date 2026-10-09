//! The example apps in examples/, published through the real router and
//! driven end to end. They are the documentation agents copy from, so each
//! feature they claim to show is checked here, as the people who would use
//! it.
//!
//! The handlers and front ends are committed builds in
//! tests/fixtures/examples/, made by scripts/build-examples.sh, so this suite
//! needs no wasm toolchain and no npm. Beside each build is a digest of the
//! source it came from; the first test fails when the source moved on.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use toolsite::{build_router, platform::upload::UploadTicket, runtime::wasm::Runtime, Config};
use tower::ServiceExt;

const TOKEN: &str = "test-token";
const BASE: &str = "https://site.test";
const EXAMPLES: [&str; 10] = [
    "kitchen-sink",
    "orders",
    "static-report",
    "blob-gallery",
    "inventory-policies",
    "live-board",
    "mqtt-broker",
    "tcp-chat",
    "syslog",
    "duckdb-report",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A site that knows its address (for upload URLs and the OAuth tokens app
/// tools are called with) and takes the static token on /mcp.
fn site() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        data_dir: dir.path().to_path_buf(),
        base_url: Some(BASE.to_string()),
        local_base: "http://localhost:8080".to_string(),
        valid_tokens: vec![TOKEN.to_string()],
        ..Config::local(dir.path().to_path_buf(), "unused")
    });
    (dir, config)
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, Vec<u8>, Vec<(String, String)>) {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, bytes.to_vec(), headers)
}

async fn send_text(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String) {
    let (status, body, _) = send(config, request).await;
    (status, String::from_utf8_lossy(&body).to_string())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

// --- the fixtures are the source ---------------------------------------------

/// The digest scripts/build-examples.sh writes: a "<sha256>  <path>" line per
/// file, sorted by path, hashed. node_modules, target and dist are skipped;
/// symlinks are followed.
fn source_hash(dir: &Path) -> String {
    fn walk(base: &Path, relative: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(base.join(relative)) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if ["node_modules", "target", "dist"].contains(&name.as_str()) {
                continue;
            }
            let path = if relative.is_empty() { name } else { format!("{relative}/{name}") };
            let Ok(meta) = std::fs::metadata(base.join(&path)) else { continue };
            if meta.is_dir() {
                walk(base, &path, out);
            } else if meta.is_file() {
                out.push(path);
            }
        }
    }
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut files = Vec::new();
    walk(dir, "", &mut files);
    files.sort();
    let mut lines = String::new();
    for file in &files {
        let body = std::fs::read(dir.join(file)).unwrap();
        lines.push_str(&format!("{}  {file}\n", hex(&Sha256::digest(&body))));
    }
    hex(&Sha256::digest(lines.as_bytes()))
}

#[test]
fn every_fixture_was_built_from_the_example_source_as_it_is_now() {
    let mut stale = Vec::new();
    for name in EXAMPLES {
        let recorded = std::fs::read_to_string(root().join(format!("tests/fixtures/examples/{name}.hash")))
            .unwrap_or_else(|_| panic!("no fixture for {name}: run scripts/build-examples.sh"));
        if recorded.trim() != source_hash(&root().join("examples").join(name)) {
            stale.push(name);
        }
    }
    assert!(
        stale.is_empty(),
        "examples changed after their fixtures were built: {stale:?}. Run scripts/build-examples.sh and commit tests/fixtures/examples/."
    );
}

// --- publishing an example as the CLI does --------------------------------------

async fn ticket(config: &Config, slug: &str) -> String {
    let ticket = UploadTicket { slug: slug.to_string(), user: None, project: None };
    toolsite::platform::upload::issue_ticket(config, &ticket, Duration::from_secs(600)).await.unwrap()
}

async fn upload(config: &Arc<Config>, ticket: &str, flag: &str, body: Vec<u8>) {
    let uri = if flag.is_empty() { format!("/upload/{ticket}") } else { format!("/upload/{ticket}?{flag}") };
    let request = Request::builder().method("PUT").uri(uri).body(Body::from(body)).unwrap();
    let (status, text) = send_text(config, request).await;
    assert!(status.is_success(), "upload ?{flag}: {status} {text}");
}

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

/// Schema, then manifest, then handler, then the page: the order `toolsite
/// deploy` uses, so nothing is live before what it needs.
async fn publish(config: &Arc<Config>, name: &str) {
    let manifest = std::fs::read_to_string(root().join("examples").join(name).join("toolsite.toml")).unwrap();
    publish_with(config, name, &manifest).await;
}

/// `publish` with the manifest given, for a test that must change one
/// value in it, such as a port.
async fn publish_with(config: &Arc<Config>, name: &str, manifest: &str) {
    let source = root().join("examples").join(name);
    let fixtures = root().join("tests/fixtures/examples");
    let ticket = ticket(config, name).await;
    if source.join("migrations").is_dir() {
        upload(config, &ticket, "migrations", migrations_archive(&source.join("migrations"))).await;
    }
    upload(config, &ticket, "manifest", manifest.as_bytes().to_vec()).await;
    if let Ok(wasm) = std::fs::read(fixtures.join(format!("{name}.wasm"))) {
        upload(config, &ticket, "handler", wasm).await;
    }
    upload(config, &ticket, "bundle", std::fs::read(fixtures.join(format!("{name}-dist.tar.gz"))).unwrap()).await;
}

// --- people --------------------------------------------------------------------

struct Person {
    email: String,
    site: String,
    bearer: String,
}

fn person(config: &Config, email: &str) -> Person {
    let user = toolsite::accounts::users::sign_up(config, email, "correct horse battery").unwrap();
    let (_, site) = toolsite::accounts::users::log_in(config, email, "correct horse battery").unwrap();
    let client = toolsite::platform::oauth_store::register_client(config, Some("t"), &["https://c.test/cb".into()]).unwrap();
    let bearer = toolsite::platform::oauth_store::issue_tokens(config, &client.id, &user.id, None).unwrap().access_token;
    Person { email: email.to_string(), site, bearer }
}

/// The app session a browser holds after the hand-off, as a Cookie value.
async fn app_cookie(config: &Arc<Config>, who: &Person, app: &str) -> String {
    let request = Request::builder()
        .uri(format!("/auth/handoff?app={app}&next=/p/{app}/"))
        .header("cookie", format!("ts_session={}", who.site))
        .body(Body::empty())
        .unwrap();
    let (status, body, headers) = send(config, request).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{} was not handed off to {app}: {}", who.email, String::from_utf8_lossy(&body));
    let cookie = headers.iter().find(|(k, _)| k == "set-cookie").expect("hand-off set no cookie").1.clone();
    cookie.split(';').next().unwrap().to_string()
}

async fn call(config: &Arc<Config>, cookie: &str, method: &str, uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri).header("cookie", cookie);
    let body = if body.is_null() {
        Body::empty()
    } else {
        builder = builder.header("content-type", "application/json");
        Body::from(body.to_string())
    };
    let (status, bytes, _) = send(config, builder.body(body).unwrap()).await;
    let json = serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({ "text": String::from_utf8_lossy(&bytes) }));
    (status, json)
}

// --- MCP -----------------------------------------------------------------------

async fn mcp(config: &Arc<Config>, path: &str, token: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string()))
        .unwrap();
    let (status, bytes, _) = send(config, request).await;
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert_eq!(status, StatusCode::OK, "{text}");
    let json = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    json["result"].clone()
}

async fn tool(config: &Arc<Config>, path: &str, token: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    mcp(config, path, token, "tools/call", serde_json::json!({ "name": name, "arguments": arguments })).await
}

/// run_sql as the admin, or as a person with `as_user`, through /mcp.
async fn run_sql(config: &Arc<Config>, app: &str, sql: &str, as_user: Option<&str>) -> (bool, String) {
    let mut args = serde_json::json!({ "app": app, "sql": sql });
    if let Some(email) = as_user {
        args["as_user"] = email.into();
    }
    let result = tool(config, "/mcp", TOKEN, "run_sql", args).await;
    (result["isError"] == true, result["content"][0]["text"].as_str().unwrap_or("").to_string())
}

// --- /examples ------------------------------------------------------------------

#[tokio::test]
async fn the_site_lists_every_example_and_hands_one_over_renamed() {
    let (_dir, config) = site();
    let (status, list) = send_text(&config, get("/examples")).await;
    assert_eq!(status, StatusCode::OK);
    for name in EXAMPLES {
        assert!(list.contains(name), "{name} is not listed:\n{list}");
    }
    assert!(list.contains("toolsite init <name> --example <example>"), "{list}");

    let (status, gz, headers) = send(&config, get("/examples/kitchen-sink.tar.gz?slug=my-sink")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "application/gzip"));
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut raw).unwrap();
    let mut archive = tar::Archive::new(raw.as_slice());
    let mut vite = String::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().to_string_lossy() == "my-sink/vite.config.ts" {
            entry.read_to_string(&mut vite).unwrap();
        }
    }
    assert!(vite.contains("base: '/p/my-sink/'"), "{vite}");

    let (status, _) = send_text(&config, get("/examples/kitchen-sink.tar.gz?slug=../etc")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, text) = send_text(&config, get("/examples/nothing.tar.gz")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(text.contains("kitchen-sink"), "a miss names what exists: {text}");
}

#[tokio::test]
async fn every_example_publishes_as_it_ships() {
    let (_dir, config) = site();
    for name in EXAMPLES {
        publish(&config, name).await;
        assert!(config.data_dir.join(name).join("index.html").is_file(), "{name} has no page");
    }
    // The static one is public: no account, and the page is the report.
    let (status, page) = send_text(&config, get("/p/static-report/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Q3 shipping report"));
    // The rest are not.
    for name in ["kitchen-sink", "orders", "blob-gallery", "inventory-policies", "live-board", "mqtt-broker", "tcp-chat", "syslog", "duckdb-report"] {
        let (status, _) = send_text(&config, get(&format!("/p/{name}/"))).await;
        assert_ne!(status, StatusCode::OK, "{name} opened with no account");
    }
}

// --- kitchen-sink -----------------------------------------------------------------

struct Sink {
    alice: Person,
    bob: Person,
    carol: Person,
    alice_app: String,
    bob_app: String,
    carol_app: String,
}

/// Alice works at north and bob at south, placed there by carol, who has
/// the manager role. Alice and bob have no grant: the app admits any
/// account.
async fn kitchen_sink(config: &Arc<Config>) -> Sink {
    publish(config, "kitchen-sink").await;
    let alice = person(config, "alice@example.com");
    let bob = person(config, "bob@example.com");
    let carol = person(config, "carol@example.com");
    toolsite::accounts::users::grant(config, "carol@example.com", "kitchen-sink", "manager").unwrap();
    let alice_app = app_cookie(config, &alice, "kitchen-sink").await;
    let bob_app = app_cookie(config, &bob, "kitchen-sink").await;
    let carol_app = app_cookie(config, &carol, "kitchen-sink").await;
    for (email, location) in [("alice@example.com", "north"), ("bob@example.com", "south")] {
        let (status, body) = call(
            config,
            &carol_app,
            "POST",
            "/p/kitchen-sink/api/admin/members",
            serde_json::json!({ "email": email, "location": location }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    Sink { alice, bob, carol, alice_app, bob_app, carol_app }
}

#[tokio::test]
async fn kitchen_sink_two_people_see_their_own_locations_orders_from_the_same_sql() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;

    // Alice through the page, bob through his AI assistant.
    let (status, body) = call(
        &config,
        &s.alice_app,
        "POST",
        "/p/kitchen-sink/api/orders",
        serde_json::json!({ "customer": "Acme", "item": "pallets", "quantity": 4 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["order"]["location"], "north");
    let made = tool(&config, "/p/kitchen-sink/mcp", &s.bob.bearer, "create_order", serde_json::json!({ "customer": "Globex", "item": "wrap", "quantity": 2 })).await;
    assert_ne!(made["isError"], true, "{made}");
    assert_eq!(made["structuredContent"]["order"]["location"], "south", "{made}");

    let (_, alice_sees) = call(&config, &s.alice_app, "GET", "/p/kitchen-sink/api/orders", serde_json::Value::Null).await;
    let customers: Vec<_> = alice_sees["orders"].as_array().unwrap().iter().map(|o| o["customer"].clone()).collect();
    assert_eq!(customers, ["Acme"], "{alice_sees}");
    let bob_sees = tool(&config, "/p/kitchen-sink/mcp", &s.bob.bearer, "list_my_orders", serde_json::json!({})).await;
    let customers: Vec<_> = bob_sees["structuredContent"]["orders"].as_array().unwrap().iter().map(|o| o["customer"].clone()).collect();
    assert_eq!(customers, ["Globex"], "{bob_sees}");

    // The policy refuses a write to a location the person is not in.
    let (status, body) = call(
        &config,
        &s.alice_app,
        "POST",
        "/p/kitchen-sink/api/orders",
        serde_json::json!({ "customer": "Initech", "item": "tape", "quantity": 1, "location": "south" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A person's own SQL stays inside the declared views.
    let (status, body) = call(&config, &s.alice_app, "POST", "/p/kitchen-sink/api/sql", serde_json::json!({ "sql": "select * from orders" })).await;
    assert!(status.is_client_error(), "the base table answered: {body}");
    let (status, body) = call(&config, &s.alice_app, "POST", "/p/kitchen-sink/api/sql", serde_json::json!({ "sql": "select code from my_locations" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], serde_json::json!([["north"]]));

    // And the platform agrees, asked from outside the app.
    let (_, out) = run_sql(&config, "kitchen-sink", "select customer from my_orders", Some("bob@example.com")).await;
    assert!(out.contains("Globex") && !out.contains("Acme"), "{out}");
    let _ = (&s.carol, &s.bob_app);
}

#[tokio::test]
async fn kitchen_sink_route_rules_open_status_to_anyone_and_close_admin_to_the_ungranted() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;

    let (status, body) = send_text(&config, get("/p/kitchen-sink/status")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"ok\":true"), "{body}");
    let (status, _) = send_text(&config, get("/p/kitchen-sink/api/me")).await;
    assert_ne!(status, StatusCode::OK, "the rest of the app opened with no account");

    // Alice may open the app but holds no grant, so the restricted paths
    // refuse her before the handler runs.
    let (status, _) = call(&config, &s.alice_app, "GET", "/p/kitchen-sink/admin", serde_json::Value::Null).await;
    assert_ne!(status, StatusCode::OK);
    let (status, _) = call(&config, &s.alice_app, "GET", "/p/kitchen-sink/api/admin/members", serde_json::Value::Null).await;
    assert_ne!(status, StatusCode::OK);
    let (status, body) = call(&config, &s.carol_app, "GET", "/p/kitchen-sink/admin", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["text"].as_str().unwrap().contains("alice@example.com"), "{body}");

    // Identity, as the handler and SQL see it.
    let (_, me) = call(&config, &s.carol_app, "GET", "/p/kitchen-sink/api/me", serde_json::Value::Null).await;
    assert_eq!(me["user"]["email"], "carol@example.com");
    assert_eq!(me["role"], "manager");
    assert_eq!(me["sql"]["current_email"], "carol@example.com");
    assert_eq!(me["roles"], serde_json::json!(["viewer", "manager"]));
}

#[tokio::test]
async fn kitchen_sink_heartbeat_runs_on_the_schedule_and_never_for_a_visitor() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;
    let jobs = toolsite::platform::schedule::read_jobs(&config, "kitchen-sink");
    assert!(jobs.contains_key("heartbeat"), "{:?}", jobs.keys());

    // The scheduler is driven at a time of the test's choosing, not the
    // clock's: a second past a five-minute mark, when the heartbeat is due.
    let state = toolsite::AppState { config: config.clone(), runtime: Runtime::new().unwrap() };
    let scheduler = toolsite::platform::schedule::Scheduler::new(state);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let at = now / 300 * 300 + 1;
    let runs = scheduler.tick(at).await;
    assert_eq!(runs.len(), 1, "the heartbeat was not due a second past its mark");
    for run in runs {
        run.await.unwrap();
    }
    let jobs = toolsite::platform::schedule::read_jobs(&config, "kitchen-sink");
    assert_eq!(jobs["heartbeat"].last_status.as_deref(), Some("200"));
    // Having run, it is not due again until the next mark.
    assert!(scheduler.tick(at).await.is_empty(), "the heartbeat ran twice for one mark");
    let (_, beats) = call(&config, &s.alice_app, "GET", "/p/kitchen-sink/api/heartbeats", serde_json::Value::Null).await;
    assert_eq!(beats["beats"].as_array().unwrap().len(), 1, "{beats}");

    // A visitor who forges the header is still a visitor.
    let request = Request::builder()
        .uri("/p/kitchen-sink/api/heartbeat")
        .header("cookie", &s.alice_app)
        .header("x-toolsite-scheduled", "heartbeat")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_text(&config, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn kitchen_sink_fetch_is_refused_for_a_host_the_manifest_does_not_allow() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;
    let (status, body) = call(&config, &s.alice_app, "POST", "/p/kitchen-sink/api/fetch", serde_json::json!({ "url": "https://example.org/" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], false, "{body}");
    assert!(body["error"].as_str().unwrap().contains("example.org"), "the refusal names the host: {body}");
}

#[tokio::test]
#[ignore = "reaches api.github.com over the network"]
async fn kitchen_sink_fetch_reaches_the_allowed_host() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;
    let (_, body) = call(&config, &s.alice_app, "POST", "/p/kitchen-sink/api/fetch", serde_json::json!({ "url": "https://api.github.com/zen" })).await;
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["status"], 200, "{body}");
}

#[tokio::test]
async fn kitchen_sink_files_go_to_storage_by_upload_url_and_come_back_by_header() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;

    let (status, ticket) = call(&config, &s.alice_app, "POST", "/p/kitchen-sink/api/files", serde_json::json!({ "name": "notes.txt" })).await;
    assert_eq!(status, StatusCode::OK, "{ticket}");
    let url = ticket["url"].as_str().unwrap();
    let path = url.strip_prefix(BASE).expect("an upload URL on this site");
    let put = Request::builder().method("PUT").uri(path).header("content-type", "text/plain").body(Body::from("hello from alice")).unwrap();
    let (status, text) = send_text(&config, put).await;
    assert!(status.is_success(), "{status} {text}");

    let (_, listed) = call(&config, &s.bob_app, "GET", "/p/kitchen-sink/api/files", serde_json::Value::Null).await;
    assert_eq!(listed["files"][0]["name"], "notes.txt", "{listed}");
    assert_eq!(listed["files"][0]["uploaded_by"], "alice@example.com");
    let (status, body) = call(&config, &s.bob_app, "GET", "/p/kitchen-sink/api/files/notes.txt", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "hello from alice", "the platform streamed the stored file in place of the empty body");

    // The URL was for one file, once.
    let again = Request::builder().method("PUT").uri(path).body(Body::from("overwrite")).unwrap();
    let (status, _) = send_text(&config, again).await;
    assert!(status.is_client_error(), "an upload URL worked twice");

    let (status, _) = call(&config, &s.alice_app, "DELETE", "/p/kitchen-sink/api/files/notes.txt", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&config, &s.alice_app, "GET", "/p/kitchen-sink/api/files/notes.txt", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn kitchen_sink_says_whether_a_setting_is_set_and_never_what_it_is() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;
    let (_, before) = call(&config, &s.alice_app, "GET", "/p/kitchen-sink/api/settings", serde_json::Value::Null).await;
    assert_eq!(before["greeting_set"], false, "{before}");

    toolsite::platform::secrets::set(&config, "kitchen-sink", "GREETING", Some("s3cret-hello-value")).await.unwrap();
    let (_, after) = call(&config, &s.alice_app, "GET", "/p/kitchen-sink/api/settings", serde_json::Value::Null).await;
    assert_eq!(after["greeting_set"], true, "{after}");
    assert_eq!(after["names"], serde_json::json!(["GREETING"]));
    assert!(!after.to_string().contains("s3cret-hello-value"), "the value reached a browser: {after}");
}

#[tokio::test]
async fn kitchen_sink_offers_one_reading_tool_and_one_writing_tool() {
    let (_dir, config) = site();
    let s = kitchen_sink(&config).await;
    let listed = mcp(&config, "/p/kitchen-sink/mcp", &s.alice.bearer, "tools/list", serde_json::json!({})).await;
    let tools = listed["tools"].as_array().unwrap();
    let hint = |name: &str| tools.iter().find(|t| t["name"] == name).map(|t| t["annotations"]["readOnlyHint"].clone());
    assert_eq!(hint("create_order"), Some(serde_json::json!(false)), "{listed}");
    assert_eq!(hint("list_my_orders"), Some(serde_json::json!(true)), "{listed}");
}

// --- orders -----------------------------------------------------------------------

#[tokio::test]
async fn orders_totals_are_the_servers_and_the_status_rules_hold_through_tools() {
    let (_dir, config) = site();
    publish(&config, "orders").await;
    let alice = person(&config, "alice@example.com");
    let bob = person(&config, "bob@example.com");
    let path = "/p/orders/mcp";

    let made = tool(&config, path, &alice.bearer, "create_order", serde_json::json!({
        "customer": "Acme",
        "lines": [{ "sku": "PAL-STD", "quantity": 10 }, { "sku": "WRAP-18", "quantity": 2 }],
    }))
    .await;
    assert_ne!(made["isError"], true, "{made}");
    let order = &made["structuredContent"]["order"];
    let id = order["id"].as_i64().unwrap();
    // 10 x 18.50 + 2 x 24.75, in cents.
    assert_eq!(order["total_cents"], 10 * 1850 + 2 * 2475, "{order}");
    assert_eq!(order["status"], "draft");

    let added = tool(&config, path, &alice.bearer, "add_line", serde_json::json!({ "order_id": id, "sku": "PAL-STD", "quantity": 5 })).await;
    assert_eq!(added["structuredContent"]["order"]["lines"].as_array().unwrap().len(), 2, "the same product joins its line: {added}");
    assert_eq!(added["structuredContent"]["order"]["total_cents"], 15 * 1850 + 2 * 2475);

    // Bob cannot touch alice's order: the policy hides it from him.
    let theirs = tool(&config, path, &bob.bearer, "add_line", serde_json::json!({ "order_id": id, "sku": "PAL-STD", "quantity": 1 })).await;
    assert_eq!(theirs["isError"], true, "{theirs}");
    let bobs = tool(&config, path, &bob.bearer, "my_orders", serde_json::json!({})).await;
    assert_eq!(bobs["structuredContent"]["orders"], serde_json::json!([]), "{bobs}");

    // An empty draft cannot be submitted; a full one can, once, and then it is fixed.
    let empty = tool(&config, path, &alice.bearer, "create_order", serde_json::json!({ "customer": "Globex" })).await;
    let empty_id = empty["structuredContent"]["order"]["id"].as_i64().unwrap();
    let refused = tool(&config, path, &alice.bearer, "submit_order", serde_json::json!({ "order_id": empty_id })).await;
    assert_eq!(refused["isError"], true);
    assert!(refused["content"][0]["text"].as_str().unwrap().contains("no lines"), "{refused}");

    let submitted = tool(&config, path, &alice.bearer, "submit_order", serde_json::json!({ "order_id": id })).await;
    assert_eq!(submitted["structuredContent"]["order"]["status"], "submitted", "{submitted}");
    let twice = tool(&config, path, &alice.bearer, "submit_order", serde_json::json!({ "order_id": id })).await;
    assert_ne!(twice["isError"], true, "a retried submit is not an error: {twice}");
    let late = tool(&config, path, &alice.bearer, "add_line", serde_json::json!({ "order_id": id, "sku": "TAPE-48", "quantity": 1 })).await;
    assert_eq!(late["isError"], true);
    assert!(late["content"][0]["text"].as_str().unwrap().contains("submitted"), "{late}");

    // Only an approver sees the queue and decides.
    let alice_app = app_cookie(&config, &alice, "orders").await;
    let (status, _) = call(&config, &alice_app, "GET", "/p/orders/api/queue", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let dana = person(&config, "dana@example.com");
    toolsite::accounts::users::grant(&config, "dana@example.com", "orders", "approver").unwrap();
    let dana_app = app_cookie(&config, &dana, "orders").await;
    let (_, queue) = call(&config, &dana_app, "GET", "/p/orders/api/queue", serde_json::Value::Null).await;
    assert_eq!(queue["orders"][0]["id"], id, "{queue}");
    let (status, _) = call(&config, &dana_app, "POST", &format!("/p/orders/api/orders/{id}/decide"), serde_json::json!({ "decision": "approve" })).await;
    assert_eq!(status, StatusCode::OK);
    let mine = tool(&config, path, &alice.bearer, "my_orders", serde_json::json!({ "status": "approved" })).await;
    assert_eq!(mine["structuredContent"]["orders"][0]["id"], id, "{mine}");
}

// --- blob-gallery -------------------------------------------------------------------

async fn put_to(config: &Arc<Config>, url: &str, content_type: &str, body: &'static [u8]) {
    let path = url.strip_prefix(BASE).expect("an upload URL on this site");
    let request = Request::builder().method("PUT").uri(path).header("content-type", content_type).body(Body::from(body)).unwrap();
    let (status, text) = send_text(config, request).await;
    assert!(status.is_success(), "{status} {text}");
}

#[tokio::test]
async fn blob_gallery_lists_a_photo_only_once_both_images_arrived_and_serves_them() {
    let (_dir, config) = site();
    publish(&config, "blob-gallery").await;
    let alice = person(&config, "alice@example.com");
    let bob = person(&config, "bob@example.com");
    let alice_app = app_cookie(&config, &alice, "blob-gallery").await;
    let bob_app = app_cookie(&config, &bob, "blob-gallery").await;
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really, but labelled as one";

    let (_, ticket) = call(&config, &alice_app, "POST", "/p/blob-gallery/api/photos", serde_json::json!({ "caption": "Dock 4" })).await;
    let id = ticket["id"].as_i64().unwrap();
    put_to(&config, ticket["full"].as_str().unwrap(), "image/png", PNG).await;
    // Only one of two files: not ready, not listed.
    let (status, _) = call(&config, &alice_app, "POST", &format!("/p/blob-gallery/api/photos/{id}/ready"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, listed) = call(&config, &bob_app, "GET", "/p/blob-gallery/api/photos", serde_json::Value::Null).await;
    assert_eq!(listed["photos"], serde_json::json!([]));

    put_to(&config, ticket["thumb"].as_str().unwrap(), "image/jpeg", b"thumb bytes").await;
    let (status, body) = call(&config, &alice_app, "POST", &format!("/p/blob-gallery/api/photos/{id}/ready"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, listed) = call(&config, &bob_app, "GET", "/p/blob-gallery/api/photos", serde_json::Value::Null).await;
    assert_eq!(listed["photos"][0]["caption"], "Dock 4", "{listed}");

    let (status, full, headers) = send(&config, Request::builder().uri(format!("/p/blob-gallery/api/photos/{id}/full")).header("cookie", &bob_app).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(full, PNG);
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "image/png"), "{headers:?}");

    // Not his to remove.
    let (status, _) = call(&config, &bob_app, "DELETE", &format!("/p/blob-gallery/api/photos/{id}"), serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Something that is not an image is taken back out at the ready step.
    let (_, other) = call(&config, &bob_app, "POST", "/p/blob-gallery/api/photos", serde_json::json!({ "caption": "script" })).await;
    let other_id = other["id"].as_i64().unwrap();
    put_to(&config, other["full"].as_str().unwrap(), "text/html", b"<script>alert(1)</script>").await;
    put_to(&config, other["thumb"].as_str().unwrap(), "image/jpeg", b"thumb").await;
    let (status, _) = call(&config, &bob_app, "POST", &format!("/p/blob-gallery/api/photos/{other_id}/ready"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (status, _) = call(&config, &bob_app, "GET", &format!("/p/blob-gallery/api/photos/{other_id}/full"), serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A curator may remove anyone's.
    let erin = person(&config, "erin@example.com");
    toolsite::accounts::users::grant(&config, "erin@example.com", "blob-gallery", "curator").unwrap();
    let erin_app = app_cookie(&config, &erin, "blob-gallery").await;
    let (status, _) = call(&config, &erin_app, "DELETE", &format!("/p/blob-gallery/api/photos/{id}"), serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&config, &alice_app, "GET", &format!("/p/blob-gallery/api/photos/{id}/thumb"), serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- inventory-policies -------------------------------------------------------------

/// The README's proof steps, run as written.
#[tokio::test]
async fn inventory_policies_hold_as_the_readme_says() {
    let (_dir, config) = site();
    publish(&config, "inventory-policies").await;
    let alice = person(&config, "alice@example.com");
    person(&config, "bob@example.com");
    let app = "inventory-policies";

    let (err, out) = run_sql(&config, app, "select location, sku, quantity from my_stock order by sku", Some("alice@example.com")).await;
    assert!(!err, "{out}");
    assert!(out.contains("north") && !out.contains("south"), "alice: {out}");
    let (_, out) = run_sql(&config, app, "select location, sku, quantity from my_stock order by sku", Some("bob@example.com")).await;
    assert!(out.contains("south") && !out.contains("north"), "bob: {out}");

    for sql in [
        "select * from stock",
        "update my_stock set location = 'south' where sku = 'PAL-STD'",
        "update members set location = 'south'",
        "update my_members set location = 'south'",
    ] {
        let (err, out) = run_sql(&config, app, sql, Some("alice@example.com")).await;
        assert!(err, "alice reached past the policy with {sql:?}: {out}");
    }
    let (_, out) = run_sql(&config, app, "select location from stock where sku = 'PAL-STD' order by location", None).await;
    assert!(out.contains("north") && out.contains("south"), "the refused move changed nothing: {out}");

    let (err, out) = run_sql(
        &config,
        app,
        "insert into my_movements (location, sku, delta, reason) values ('north', 'PAL-STD', -10, 'shipped')",
        Some("alice@example.com"),
    )
    .await;
    assert!(!err, "{out}");
    let alice_id = toolsite::accounts::users::user_by_email(&config, &alice.email).map(|u| u.id).unwrap();
    let (_, out) = run_sql(&config, app, "select by_user from movements", None).await;
    assert!(out.contains(&alice_id), "by_user is alice's id: {out}");

    let (_, out) = run_sql(&config, app, "select quantity from stock_totals where sku = 'PAL-STD'", Some("bob@example.com")).await;
    assert!(out.contains("225"), "totals add both warehouses: {out}");
}

// --- live-board ----------------------------------------------------------------------

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The same router on a real port, since a WebSocket needs a connection the
/// server keeps. Requests through `send` reach the same sockets, because
/// the connections live in the shared Config.
async fn listen(config: &Arc<Config>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(config.clone(), Runtime::new().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

/// Connects to `path` inside the board with an app cookie, or answers the
/// HTTP status the upgrade was refused with.
async fn board_socket(addr: std::net::SocketAddr, cookie: Option<&str>, path: &str) -> Result<Socket, u16> {
    app_socket(addr, "live-board", cookie, path).await
}

/// Connects to `path` inside `app`, or answers the HTTP status the upgrade
/// was refused with.
async fn app_socket(addr: std::net::SocketAddr, app: &str, cookie: Option<&str>, path: &str) -> Result<Socket, u16> {
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
    let mut request = format!("ws://{addr}/p/{app}{path}").into_client_request().unwrap();
    if let Some(cookie) = cookie {
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("connect failed: {other}"),
    }
}

/// The next JSON message of one type, skipping the others.
async fn next_of(socket: &mut Socket, kind: &str) -> Option<serde_json::Value> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let frame = tokio::time::timeout_at(deadline, socket.next()).await.ok()??.ok()?;
        if let Message::Text(text) = frame {
            let msg: serde_json::Value = serde_json::from_str(&text).ok()?;
            if msg["type"] == kind {
                return Some(msg);
            }
        }
    }
}

/// No message of this type arrives for a short while.
async fn none_of(socket: &mut Socket, kind: &str) -> bool {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return true,
            Ok(Some(Ok(Message::Text(text)))) if text.contains(&format!("\"type\":\"{kind}\"")) => return false,
            Ok(Some(Ok(_))) => continue,
            Ok(_) => return true,
        }
    }
}

async fn say(socket: &mut Socket, msg: serde_json::Value) {
    use futures_util::SinkExt;
    socket.send(tokio_tungstenite::tungstenite::Message::Text(msg.to_string().into())).await.unwrap();
}

struct Board {
    config: Arc<Config>,
    addr: std::net::SocketAddr,
    _dir: TempDir,
}

/// The board published, with each of `granted` given access and handed an
/// app cookie.
async fn live_board(granted: &[&str]) -> (Board, Vec<(Person, String)>) {
    let (dir, config) = site();
    publish(&config, "live-board").await;
    let addr = listen(&config).await;
    let mut people = Vec::new();
    for email in granted {
        let who = person(&config, email);
        toolsite::accounts::users::grant(&config, email, "live-board", "viewer").unwrap();
        let cookie = app_cookie(&config, &who, "live-board").await;
        people.push((who, cookie));
    }
    (Board { config, addr, _dir: dir }, people)
}

/// Connects and reads the board, which is always the first message.
async fn join(board: &Board, cookie: &str, query: &str) -> (Socket, serde_json::Value) {
    use futures_util::StreamExt;
    let mut socket = board_socket(board.addr, Some(cookie), &format!("/live/ws{query}"))
        .await
        .unwrap_or_else(|status| panic!("refused with {status}"));
    let first = tokio::time::timeout(Duration::from_secs(3), socket.next()).await.unwrap().unwrap().unwrap();
    let first: serde_json::Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(first["type"], "board", "the first message is the board: {first}");
    (socket, first)
}

#[tokio::test]
async fn live_board_a_card_one_person_adds_reaches_everyone_with_the_board_open() {
    let (board, people) = live_board(&["alice@example.com", "bob@example.com"]).await;
    let [(alice, alice_app), (_, bob_app)] = &people[..] else { unreachable!() };
    let (mut a, first) = join(&board, alice_app, "").await;
    assert_eq!(first["me"]["email"], "alice@example.com");
    assert_eq!(first["cards"], serde_json::json!([]));
    let (mut b, _) = join(&board, bob_app, "").await;

    let (status, card) =
        call(&board.config, alice_app, "POST", "/p/live-board/api/cards", serde_json::json!({ "title": "Order wrap" })).await;
    assert_eq!(status, StatusCode::OK, "{card}");
    for socket in [&mut a, &mut b] {
        let event = next_of(socket, "card").await.expect("the new card was not pushed");
        assert_eq!(event["card"]["title"], "Order wrap");
        assert_eq!(event["card"]["lane"], "todo");
        assert_eq!(event["card"]["author_email"], "alice@example.com");
    }

    // A move and a delete reach the others the same way.
    let id = card["id"].as_i64().unwrap();
    let uri = format!("/p/live-board/api/cards/{id}/move");
    let (status, _) = call(&board.config, alice_app, "POST", &uri, serde_json::json!({ "lane": "done" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next_of(&mut b, "card").await.unwrap()["card"]["lane"], "done");
    let uri = format!("/p/live-board/api/cards/{id}");
    let (status, _) = call(&board.config, alice_app, "DELETE", &uri, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next_of(&mut b, "deleted").await.unwrap()["id"], id);

    // A card an assistant adds through the app tool arrives the same way.
    let args = serde_json::json!({ "title": "From the assistant", "lane": "doing" });
    let result = tool(&board.config, "/p/live-board/mcp", &alice.bearer, "add_card", args).await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(next_of(&mut b, "card").await.unwrap()["card"]["title"], "From the assistant");

    // A browser that reconnects gets the board as it is now, first.
    drop(b);
    let (_, first) = join(&board, bob_app, "").await;
    let titles: Vec<_> = first["cards"].as_array().unwrap().iter().map(|c| c["title"].clone()).collect();
    assert_eq!(titles, vec![serde_json::json!("From the assistant")]);
}

#[tokio::test]
async fn live_board_presence_lists_everyone_here_and_drops_a_person_who_leaves() {
    let (board, people) = live_board(&["alice@example.com", "bob@example.com"]).await;
    let [(_, alice_app), (_, bob_app)] = &people[..] else { unreachable!() };
    let (mut a, _) = join(&board, alice_app, "?name=Alice").await;
    let who = next_of(&mut a, "who").await.unwrap();
    assert_eq!(who["people"].as_array().unwrap().len(), 1, "{who}");

    let (mut b, first) = join(&board, bob_app, "?name=Bob%20B").await;
    assert_eq!(first["me"]["name"], "Bob B", "the name from the query is kept in the connection's state");
    let who = next_of(&mut a, "who").await.unwrap();
    let names: Vec<_> = who["people"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["Alice", "Bob B"]);

    // A rename over the socket shows on everyone's list.
    say(&mut b, serde_json::json!({ "type": "name", "name": "Robert" })).await;
    let who = next_of(&mut a, "who").await.unwrap();
    assert!(who.to_string().contains("Robert"), "{who}");

    drop(b);
    let who = next_of(&mut a, "who").await.expect("no presence update after bob left");
    let names: Vec<_> = who["people"].as_array().unwrap().iter().map(|p| p["name"].clone()).collect();
    assert_eq!(names, vec![serde_json::json!("Alice")]);
}

#[tokio::test]
async fn live_board_a_nudge_reaches_only_the_person_it_names() {
    let (board, people) = live_board(&["alice@example.com", "bob@example.com", "carol@example.com"]).await;
    let [(_, alice_app), (_, bob_app), (_, carol_app)] = &people[..] else { unreachable!() };
    let (mut a, _) = join(&board, alice_app, "?name=Alice").await;
    let (mut b, bob) = join(&board, bob_app, "").await;
    let (mut b2, _) = join(&board, bob_app, "").await;
    let (mut c, _) = join(&board, carol_app, "").await;

    say(&mut a, serde_json::json!({ "type": "nudge", "to": bob["me"]["id"], "text": "standup" })).await;
    let sent = next_of(&mut a, "nudged").await.unwrap();
    assert_eq!(sent["reached"], 2, "both of bob's tabs: {sent}");
    for socket in [&mut b, &mut b2] {
        let nudge = next_of(socket, "nudge").await.expect("bob was not nudged");
        assert_eq!(nudge["from"], "Alice");
        assert_eq!(nudge["text"], "standup");
    }
    assert!(none_of(&mut c, "nudge").await, "carol saw bob's nudge");
    assert!(none_of(&mut a, "nudge").await, "the sender saw their own nudge");

    // Nobody by that id has the board open, so the sender is told.
    say(&mut a, serde_json::json!({ "type": "nudge", "to": "no-such-person" })).await;
    let refused = next_of(&mut a, "error").await.unwrap();
    assert!(refused["error"].as_str().unwrap().contains("does not have the board open"), "{refused}");
}

#[tokio::test]
async fn live_board_refuses_the_socket_to_a_stranger_and_to_no_account() {
    let (board, people) = live_board(&["alice@example.com"]).await;
    let alice_app = &people[0].1;
    let (mut a, _) = join(&board, alice_app, "").await;
    let who = next_of(&mut a, "who").await.unwrap();
    assert_eq!(who["people"].as_array().unwrap().len(), 1, "{who}");

    // Dave has an account but no grant. The gate in front of the socket is
    // the gate in front of the app, so he never reaches the handler. He gets
    // no app session from the hand-off, so try his site session too.
    let dave = person(&board.config, "dave@example.com");
    let request = Request::builder()
        .uri("/auth/handoff?app=live-board&next=/p/live-board/")
        .header("cookie", format!("ts_session={}", dave.site))
        .body(Body::empty())
        .unwrap();
    let (_, _, headers) = send(&board.config, request).await;
    let handed = headers.iter().find(|(k, _)| k == "set-cookie").map(|(_, v)| v.split(';').next().unwrap().to_string());
    let site_session = format!("ts_session={}", dave.site);
    for cookie in handed.iter().map(String::as_str).chain([site_session.as_str()]) {
        let status = board_socket(board.addr, Some(cookie), "/live/ws").await.expect_err("a stranger connected");
        assert!(status == 401 || status == 403, "stranger with {cookie}: {status}");
    }
    let status = board_socket(board.addr, None, "/live/ws").await.expect_err("no account connected");
    assert!([401, 403, 303].contains(&status), "no account: {status}");

    // Neither reached the handler: nobody joined the presence list.
    assert!(none_of(&mut a, "who").await, "a refused socket changed the presence list");

    // A path the manifest does not declare takes no socket at all.
    let status = board_socket(board.addr, Some(alice_app), "/api/board").await.expect_err("an undeclared path upgraded");
    assert_eq!(status, 404);
}

// --- mqtt-broker ---------------------------------------------------------------------

use bytes::BytesMut;
use rumqttc::{AsyncClient, ConnectReturnCode, ConnectionError, Event as MqttEvent, Incoming, LastWill, MqttOptions, Packet, QoS};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Mqtt {
    config: Arc<Config>,
    addr: std::net::SocketAddr,
    /// The TCP port the site's owner mapped to the broker.
    port: u16,
    /// A device token of the broker, labelled "sensor-1".
    token: String,
    _dir: TempDir,
}

/// A port nothing listens on now, for the site to take. Handed out below the
/// kernel's ephemeral range and never twice in one process: a port bound to 0
/// and let go can come back as the source port of another test's client
/// before the site binds it.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 40) as u16 * 300;
    loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let port = base + n % 300;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
            && std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok()
        {
            return port;
        }
    }
}

/// The broker published on a site whose owner gave it a free TCP port, as
/// `TOOLSITE_PORTS=<port>=mqtt-broker` would, with the HTTP side on a real
/// port for the WebSocket. The manifest declares 1883; the test declares
/// the port it was given instead, since 1883 may be taken on this machine.
async fn mqtt_broker() -> Mqtt {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        data_dir: dir.path().to_path_buf(),
        base_url: Some(BASE.to_string()),
        valid_tokens: vec![TOKEN.to_string()],
        ports: toolsite::platform::ports::PortMap {
            bind: "127.0.0.1".parse().unwrap(),
            mappings: toolsite::platform::ports::parse(&format!("{port}=mqtt-broker")).unwrap(),
        },
        ..Config::local(dir.path().to_path_buf(), "unused")
    });
    let manifest = std::fs::read_to_string(root().join("examples/mqtt-broker/toolsite.toml")).unwrap();
    assert!(manifest.contains("port = 1883"), "the example no longer declares 1883");
    publish_with(&config, "mqtt-broker", &manifest.replace("port = 1883", &format!("port = {port}"))).await;
    let addr = listen(&config).await;
    toolsite::platform::ports::listen(config.clone(), Runtime::new().unwrap()).await.unwrap();
    let (_, token) = toolsite::platform::devices::create(&config, "mqtt-broker", "sensor-1").await.unwrap();
    // The first connection starts the resident instance, which compiles
    // the handler. Done here, unhurried, so the tests' own timings measure
    // the broker and not a busy machine compiling seven handlers at once.
    let warm = device_within(port, "warm-up", Some(&token), true, Duration::from_secs(120)).await;
    drop(warm.expect("the broker did not start"));
    Mqtt { config, addr, port, token, _dir: dir }
}

/// A real MQTT client on the TCP port, past its CONNACK.
struct Client {
    client: AsyncClient,
    events: tokio::sync::mpsc::UnboundedReceiver<Incoming>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    /// Drops the network connection without DISCONNECT, as a device that
    /// loses power does.
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn device(port: u16, id: &str, password: Option<&str>, clean: bool) -> Result<Client, Box<ConnectionError>> {
    device_within(port, id, password, clean, Duration::from_secs(10)).await
}

async fn device_within(
    port: u16,
    id: &str,
    password: Option<&str>,
    clean: bool,
    within: Duration,
) -> Result<Client, Box<ConnectionError>> {
    let mut options = MqttOptions::new(id, "127.0.0.1", port);
    options.set_keep_alive(Duration::from_secs(30)).set_clean_session(clean);
    if let Some(password) = password {
        options.set_credentials("device", password);
    }
    let (client, mut eventloop) = AsyncClient::new(options, 64);
    let mut network = rumqttc::NetworkOptions::new();
    network.set_connection_timeout(within.as_secs());
    eventloop.set_network_options(network);
    loop {
        match tokio::time::timeout(within, eventloop.poll()).await.expect("no CONNACK in time") {
            Ok(MqttEvent::Incoming(Incoming::ConnAck(_))) => break,
            Ok(_) => continue,
            Err(e) => return Err(Box::new(e)),
        }
    }
    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Ok(event) = eventloop.poll().await {
            if let MqttEvent::Incoming(incoming) = event
                && tx.send(incoming).is_err()
            {
                break;
            }
        }
    });
    Ok(Client { client, events, task })
}

impl Client {
    /// The next packet the broker sent that `pick` takes, within 5 seconds.
    async fn next<T>(&mut self, mut pick: impl FnMut(Incoming) -> Option<T>) -> Option<T> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let incoming = tokio::time::timeout_at(deadline, self.events.recv()).await.ok()??;
            if let Some(found) = pick(incoming) {
                return Some(found);
            }
        }
    }

    async fn subscribe(&mut self, filter: &str, qos: QoS) {
        self.client.subscribe(filter, qos).await.unwrap();
        self.next(|i| matches!(i, Incoming::SubAck(_)).then_some(())).await.expect("no SUBACK");
    }

    /// Publishes and waits for the broker's acknowledgement, if the QoS has one.
    async fn publish(&mut self, topic: &str, qos: QoS, retain: bool, payload: &str) {
        self.client.publish(topic, qos, retain, payload.as_bytes().to_vec()).await.unwrap();
        match qos {
            QoS::AtMostOnce => {}
            QoS::AtLeastOnce => self.next(|i| matches!(i, Incoming::PubAck(_)).then_some(())).await.expect("no PUBACK"),
            QoS::ExactlyOnce => self.next(|i| matches!(i, Incoming::PubComp(_)).then_some(())).await.expect("no PUBCOMP"),
        }
    }

    async fn message(&mut self) -> Option<rumqttc::Publish> {
        self.next(|i| match i {
            Incoming::Publish(p) => Some(p),
            _ => None,
        })
        .await
    }

    /// No message arrives for a short while.
    async fn quiet(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Err(_) | Ok(None) => return true,
                Ok(Some(Incoming::Publish(_))) => return false,
                Ok(Some(_)) => continue,
            }
        }
    }
}

fn refusal(result: Result<Client, Box<ConnectionError>>) -> ConnectReturnCode {
    match result.err().map(|e| *e) {
        Some(ConnectionError::ConnectionRefused(code)) => code,
        Some(other) => panic!("refused without a CONNACK code: {other}"),
        None => panic!("connected"),
    }
}

/// Reads one MQTT packet from a byte stream.
async fn read_packet(stream: &mut tokio::net::TcpStream, buffer: &mut BytesMut) -> Option<Packet> {
    loop {
        if let Ok(packet) = Packet::read(buffer, 1 << 20) {
            return Some(packet);
        }
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

/// A device driven byte by byte, for what a client library will not do:
/// a short keep alive it then ignores, or a will it leaves behind.
async fn raw_device(port: u16, id: &str, token: &str, keep_alive: u16, will: Option<LastWill>) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut connect = rumqttc::Connect::new(id);
    connect.keep_alive = keep_alive;
    connect.last_will = will;
    connect.set_login("device", token);
    let mut out = BytesMut::new();
    connect.write(&mut out).unwrap();
    stream.write_all(&out).await.unwrap();
    let mut buffer = BytesMut::new();
    match read_packet(&mut stream, &mut buffer).await {
        Some(Packet::ConnAck(ack)) => assert_eq!(ack.code, ConnectReturnCode::Success),
        other => panic!("no CONNACK: {other:?}"),
    }
    stream
}

/// Whether the broker ends the connection within `within`.
async fn closed_within(stream: &mut tokio::net::TcpStream, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    let mut chunk = [0u8; 1024];
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Err(_) => return false,
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
        }
    }
}

#[tokio::test]
async fn mqtt_broker_lets_a_device_in_by_its_token_and_refuses_any_other_with_the_right_code() {
    let broker = mqtt_broker().await;
    assert!(device(broker.port, "good", Some(&broker.token), true).await.is_ok());

    // A token that is not one, none at all, and a live token of another
    // app: each is turned away with a CONNACK saying why.
    assert_eq!(refusal(device(broker.port, "bad", Some("tsv_not-a-token"), true).await), ConnectReturnCode::BadUserNamePassword);
    assert_eq!(refusal(device(broker.port, "none", None, true).await), ConnectReturnCode::NotAuthorized);
    let (_, theirs) = toolsite::platform::devices::create(&broker.config, "live-board", "theirs").await.unwrap();
    assert_eq!(refusal(device(broker.port, "theirs", Some(&theirs), true).await), ConnectReturnCode::BadUserNamePassword);

    // A revoked token stops working at once.
    let (minted, revoked) = toolsite::platform::devices::create(&broker.config, "mqtt-broker", "old").await.unwrap();
    toolsite::platform::devices::revoke(&broker.config, "mqtt-broker", &minted.id).await.unwrap();
    assert_eq!(refusal(device(broker.port, "old", Some(&revoked), true).await), ConnectReturnCode::BadUserNamePassword);
}

#[tokio::test]
async fn mqtt_broker_delivers_between_two_devices_at_qos_0_1_and_2() {
    let broker = mqtt_broker().await;
    let mut sub = device(broker.port, "sub", Some(&broker.token), true).await.unwrap();
    let mut publisher = device(broker.port, "pub", Some(&broker.token), true).await.unwrap();
    sub.subscribe("plant/+/temp", QoS::ExactlyOnce).await;

    for (n, qos) in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce].into_iter().enumerate() {
        publisher.publish(&format!("plant/line{n}/temp"), qos, false, &format!("{n}1.5")).await;
        let got = sub.message().await.unwrap_or_else(|| panic!("nothing arrived at {qos:?}"));
        assert_eq!(got.topic, format!("plant/line{n}/temp"));
        assert_eq!(got.payload.as_ref(), format!("{n}1.5").as_bytes());
        // rumqttd forwards at the subscription's QoS, not the lower of the
        // two as MQTT says. Upstream's behavior, kept: see FORK.md.
        assert_eq!(got.qos, QoS::ExactlyOnce, "{qos:?}");
    }

    // A topic outside the filter does not arrive.
    publisher.publish("plant/line0/humidity", QoS::AtLeastOnce, false, "40").await;
    assert!(sub.quiet().await, "a topic outside the filter arrived");
}

#[tokio::test]
async fn mqtt_broker_hands_a_retained_message_to_a_later_subscriber() {
    let broker = mqtt_broker().await;
    let mut publisher = device(broker.port, "pub", Some(&broker.token), true).await.unwrap();
    publisher.publish("config/rate", QoS::AtLeastOnce, true, "5s").await;

    let mut later = device(broker.port, "later", Some(&broker.token), true).await.unwrap();
    later.subscribe("config/#", QoS::AtLeastOnce).await;
    let got = later.message().await.expect("the retained message did not arrive");
    assert_eq!((got.topic.as_str(), got.payload.as_ref(), got.retain), ("config/rate", b"5s".as_slice(), true));
}

#[tokio::test]
async fn mqtt_broker_fires_a_will_when_a_device_drops_and_not_when_it_says_goodbye() {
    let broker = mqtt_broker().await;
    let mut watcher = device(broker.port, "watcher", Some(&broker.token), true).await.unwrap();
    watcher.subscribe("status/#", QoS::AtLeastOnce).await;

    let will = LastWill::new("status/pump", "offline", QoS::AtLeastOnce, false);
    let stream = raw_device(broker.port, "pump", &broker.token, 30, Some(will)).await;
    drop(stream);
    let got = watcher.message().await.expect("the will did not fire");
    assert_eq!((got.topic.as_str(), got.payload.as_ref()), ("status/pump", b"offline".as_slice()));

    // A device that sends DISCONNECT first leaves no will behind.
    let will = LastWill::new("status/fan", "offline", QoS::AtLeastOnce, false);
    let mut stream = raw_device(broker.port, "fan", &broker.token, 30, Some(will)).await;
    let mut out = BytesMut::new();
    rumqttc::Disconnect.write(&mut out).unwrap();
    stream.write_all(&out).await.unwrap();
    assert!(closed_within(&mut stream, Duration::from_secs(3)).await, "DISCONNECT did not end the connection");
    assert!(watcher.quiet().await, "a will fired after a clean DISCONNECT");
}

#[tokio::test]
async fn mqtt_broker_closes_a_silent_device_after_its_keep_alive() {
    let broker = mqtt_broker().await;
    let mut watcher = device(broker.port, "watcher", Some(&broker.token), true).await.unwrap();
    watcher.subscribe("status/#", QoS::AtMostOnce).await;

    // Keep alive 1 second: silent for 1.5 seconds is gone, checked each tick.
    let will = LastWill::new("status/meter", "lost", QoS::AtMostOnce, false);
    let mut stream = raw_device(broker.port, "meter", &broker.token, 1, Some(will)).await;
    let started = Instant::now();
    assert!(closed_within(&mut stream, Duration::from_secs(5)).await, "a silent device stayed connected");
    assert!(started.elapsed() >= Duration::from_millis(1400), "closed before its keep alive ran out: {:?}", started.elapsed());
    let got = watcher.message().await.expect("the will of a timed-out device did not fire");
    assert_eq!(got.payload.as_ref(), b"lost");
}

#[tokio::test]
async fn mqtt_broker_keeps_a_persistent_session_and_its_qos_1_messages_while_the_device_is_away() {
    let broker = mqtt_broker().await;
    let mut keeper = device(broker.port, "keeper", Some(&broker.token), false).await.unwrap();
    keeper.subscribe("orders/#", QoS::AtLeastOnce).await;
    drop(keeper);
    // The broker has seen the connection end once a new one with the same
    // client id would not take it over: give the close event a moment.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut publisher = device(broker.port, "pub", Some(&broker.token), true).await.unwrap();
    publisher.publish("orders/42", QoS::AtLeastOnce, false, "picked").await;
    publisher.publish("orders/43", QoS::AtMostOnce, false, "not kept").await;

    let mut back = device(broker.port, "keeper", Some(&broker.token), false).await.unwrap();
    let got = back.message().await.expect("the queued QoS 1 message did not arrive after reconnecting");
    assert_eq!((got.topic.as_str(), got.payload.as_ref()), ("orders/42", b"picked".as_slice()));
    // Still subscribed: the session kept its subscription, not only its queue.
    publisher.publish("orders/44", QoS::AtLeastOnce, false, "packed").await;
    let next = back.next(|i| match i {
        Incoming::Publish(p) if p.topic == "orders/44" => Some(p),
        _ => None,
    });
    assert!(next.await.is_some(), "the session lost its subscription");
}

/// MQTT over the broker's WebSocket, as MQTT.js in a browser speaks it:
/// subprotocol "mqtt", binary frames, and the app cookie the hand-off set.
async fn mqtt_socket(addr: std::net::SocketAddr, cookie: Option<&str>) -> Result<Socket, u16> {
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
    let mut request = format!("ws://{addr}/p/mqtt-broker/mqtt").into_client_request().unwrap();
    request.headers_mut().insert("sec-websocket-protocol", "mqtt".parse().unwrap());
    if let Some(cookie) = cookie {
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, response)) => {
            assert_eq!(response.headers().get("sec-websocket-protocol").map(|v| v.to_str().unwrap()), Some("mqtt"));
            Ok(socket)
        }
        Err(Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("connect failed: {other}"),
    }
}

async fn socket_send(socket: &mut Socket, packet: Packet) {
    use futures_util::SinkExt;
    let mut out = BytesMut::new();
    packet.write(&mut out, 1 << 20).unwrap();
    socket.send(tokio_tungstenite::tungstenite::Message::Binary(out.freeze())).await.unwrap();
}

async fn socket_packet(socket: &mut Socket, buffer: &mut BytesMut) -> Option<Packet> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        if let Ok(packet) = Packet::read(buffer, 1 << 20) {
            return Some(packet);
        }
        match tokio::time::timeout(Duration::from_secs(5), socket.next()).await.ok()??.ok()? {
            Message::Binary(bytes) => buffer.extend_from_slice(&bytes),
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

#[tokio::test]
async fn mqtt_broker_takes_a_signed_in_browser_on_its_socket_without_a_token_and_bridges_it_to_devices() {
    let broker = mqtt_broker().await;
    let alice = person(&broker.config, "alice@example.com");
    toolsite::accounts::users::grant(&broker.config, "alice@example.com", "mqtt-broker", "viewer").unwrap();
    let cookie = app_cookie(&broker.config, &alice, "mqtt-broker").await;

    let mut sensor = device(broker.port, "sensor", Some(&broker.token), true).await.unwrap();
    sensor.subscribe("cmd/#", QoS::AtLeastOnce).await;

    let mut socket = mqtt_socket(broker.addr, Some(&cookie)).await.expect("a signed-in person was refused the socket");
    let mut buffer = BytesMut::new();
    // No username and no password: the person is who toolsite says.
    socket_send(&mut socket, Packet::Connect(rumqttc::Connect::new("web-alice"))).await;
    match socket_packet(&mut socket, &mut buffer).await {
        Some(Packet::ConnAck(ack)) => assert_eq!(ack.code, ConnectReturnCode::Success),
        other => panic!("no CONNACK on the socket: {other:?}"),
    }

    // Browser to device.
    socket_send(&mut socket, Packet::Publish(rumqttc::Publish::new("cmd/valve", QoS::AtMostOnce, "open"))).await;
    let got = sensor.message().await.expect("the browser's publish did not reach the device");
    assert_eq!((got.topic.as_str(), got.payload.as_ref()), ("cmd/valve", b"open".as_slice()));

    // Device to browser.
    let mut subscribe = rumqttc::Subscribe::new("readings/#", QoS::AtMostOnce);
    subscribe.pkid = 1;
    socket_send(&mut socket, Packet::Subscribe(subscribe)).await;
    assert!(matches!(socket_packet(&mut socket, &mut buffer).await, Some(Packet::SubAck(_))));
    sensor.publish("readings/flow", QoS::AtMostOnce, false, "12").await;
    match socket_packet(&mut socket, &mut buffer).await {
        Some(Packet::Publish(p)) => assert_eq!((p.topic.as_str(), p.payload.as_ref()), ("readings/flow", b"12".as_slice())),
        other => panic!("the device's publish did not reach the browser: {other:?}"),
    }

    // The status page shows both, as the broker saved them on a tick.
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let (status, body) = call(&broker.config, &cookie, "GET", "/p/mqtt-broker/api/status", serde_json::Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let clients = body["clients"].as_array().cloned().unwrap_or_default();
        let labels: Vec<&str> = clients.iter().filter_map(|c| c["label"].as_str()).collect();
        if body["running"] == true && labels.contains(&"sensor-1") && labels.contains(&"alice@example.com") {
            let recent = body["recent"].as_array().unwrap();
            assert!(recent.iter().any(|m| m["topic"] == "cmd/valve" && m["client_id"] == "web-alice"), "{body}");
            break;
        }
        assert!(Instant::now() < deadline, "the status never showed both clients: {body}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn mqtt_broker_refuses_its_socket_to_a_stranger_and_to_no_account() {
    let broker = mqtt_broker().await;
    // Dave has an account and no grant: the gate in front of the socket is
    // the app's, so he never reaches the broker.
    let dave = person(&broker.config, "dave@example.com");
    let site_session = format!("ts_session={}", dave.site);
    let status = mqtt_socket(broker.addr, Some(&site_session)).await.expect_err("a stranger got the socket");
    assert!(status == 401 || status == 403, "stranger: {status}");
    let status = mqtt_socket(broker.addr, None).await.expect_err("no account got the socket");
    assert!([401, 403, 303].contains(&status), "no account: {status}");
}

#[tokio::test]
async fn mqtt_broker_speaks_mqtt_5_to_a_v5_client_on_the_same_port() {
    use rumqttc::v5::{mqttbytes::v5::Packet as V5Packet, mqttbytes::QoS as V5QoS, AsyncClient as V5Client, Event, MqttOptions as V5Options};
    let broker = mqtt_broker().await;
    let mut v4 = device(broker.port, "v4", Some(&broker.token), true).await.unwrap();
    v4.subscribe("mixed/#", QoS::AtMostOnce).await;

    let mut options = V5Options::new("v5", "127.0.0.1", broker.port);
    options.set_keep_alive(Duration::from_secs(30)).set_credentials("device", broker.token.clone());
    let (client, mut eventloop) = V5Client::new(options, 16);
    let connack = tokio::time::timeout(Duration::from_secs(10), eventloop.poll()).await.unwrap().unwrap();
    assert!(matches!(connack, Event::Incoming(V5Packet::ConnAck(_))), "{connack:?}");
    client.publish("mixed/from-v5", V5QoS::AtLeastOnce, false, "hello").await.unwrap();
    let acked = async {
        loop {
            if let Event::Incoming(V5Packet::PubAck(_)) = eventloop.poll().await.unwrap() {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), acked).await.expect("no PUBACK for the v5 publish");

    let got = v4.message().await.expect("the v5 publish did not reach the v4 subscriber");
    assert_eq!((got.topic.as_str(), got.payload.as_ref()), ("mixed/from-v5", b"hello".as_slice()));

    // A refused v5 CONNECT gets the v5 reason code for the same refusal.
    let mut options = V5Options::new("v5-bad", "127.0.0.1", broker.port);
    options.set_credentials("device", "tsv_wrong");
    let (_client, mut eventloop) = V5Client::new(options, 16);
    let refused = tokio::time::timeout(Duration::from_secs(10), eventloop.poll()).await.unwrap();
    assert!(
        matches!(refused, Err(rumqttc::v5::ConnectionError::ConnectionRefused(rumqttc::v5::mqttbytes::v5::ConnectReturnCode::BadUserNamePassword))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn mqtt_broker_takes_a_device_off_once_its_token_is_revoked() {
    let broker = mqtt_broker().await;
    let (minted, token) = toolsite::platform::devices::create(&broker.config, "mqtt-broker", "pump-7").await.unwrap();
    let mut stream = raw_device(broker.port, "pump-7", &token, 60, None).await;
    let mut kept = raw_device(broker.port, "sensor-1", &broker.token, 60, None).await;
    toolsite::platform::devices::revoke(&broker.config, "mqtt-broker", &minted.id).await.unwrap();
    // Tokens are checked again every 10 seconds.
    assert!(closed_within(&mut stream, Duration::from_secs(13)).await, "a revoked device stayed connected");
    assert!(!closed_within(&mut kept, Duration::from_millis(200)).await, "a device with a live token was closed too");
}

// --- tcp-chat ------------------------------------------------------------------------

/// A site whose owner gave `app` a free port, as `TOOLSITE_PORTS` would,
/// with the app published declaring that port in place of `declared`, and
/// the HTTP side on a real port. `protocol` is "tcp" or "udp".
async fn on_a_port(app: &str, protocol: &str, declared: u16) -> (TempDir, Arc<Config>, std::net::SocketAddr, u16) {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        data_dir: dir.path().to_path_buf(),
        base_url: Some(BASE.to_string()),
        valid_tokens: vec![TOKEN.to_string()],
        ports: toolsite::platform::ports::PortMap {
            bind: "127.0.0.1".parse().unwrap(),
            mappings: toolsite::platform::ports::parse(&format!("{port}/{protocol}={app}")).unwrap(),
        },
        ..Config::local(dir.path().to_path_buf(), "unused")
    });
    let manifest = std::fs::read_to_string(root().join("examples").join(app).join("toolsite.toml")).unwrap();
    let line = format!("port = {declared}");
    assert!(manifest.contains(&line), "{app} no longer declares {declared}");
    publish_with(&config, app, &manifest.replace(&line, &format!("port = {port}"))).await;
    let addr = listen(&config).await;
    toolsite::platform::ports::listen(config.clone(), Runtime::new().unwrap()).await.unwrap();
    (dir, config, addr, port)
}

struct Chat {
    config: Arc<Config>,
    port: u16,
    _dir: TempDir,
}

/// The chat on a port of its own, its handler already compiled by a first
/// connection, so the tests' timings measure the chat.
async fn tcp_chat() -> Chat {
    let (dir, config, _, port) = on_a_port("tcp-chat", "tcp", 7777).await;
    drop(Talker::connect_within(port, Duration::from_secs(120)).await);
    Chat { config, port, _dir: dir }
}

/// A client on the chat's port, as `nc` is: lines in, lines out.
struct Talker {
    reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl Talker {
    /// Connected and past the greeting.
    async fn connect(port: u16) -> Talker {
        Talker::connect_within(port, Duration::from_secs(5)).await
    }

    async fn connect_within(port: u16, wait: Duration) -> Talker {
        let (read, writer) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap().into_split();
        let mut talker = Talker { reader: tokio::io::BufReader::new(read), writer };
        assert_eq!(talker.line_within(wait).await.as_deref(), Some("Send: token <device-token>"));
        talker
    }

    /// Connected and signed in with a device token of `label`.
    async fn signed_in(chat: &Chat, label: &str) -> Talker {
        let (_, token) = toolsite::platform::devices::create(&chat.config, "tcp-chat", label).await.unwrap();
        let mut talker = Talker::connect(chat.port).await;
        talker.write(format!("token {token}\n").as_bytes()).await;
        assert!(talker.until("Welcome, ").await.contains(label));
        talker.until(&format!("* {label} joined")).await;
        talker
    }

    async fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
    }

    /// The next line, or None at the end of the stream or after 5 seconds.
    async fn line(&mut self) -> Option<String> {
        self.line_within(Duration::from_secs(5)).await
    }

    async fn line_within(&mut self, wait: Duration) -> Option<String> {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        match tokio::time::timeout(wait, self.reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => Some(line.trim_end_matches('\n').to_string()),
            _ => None,
        }
    }

    /// The next line that starts with `prefix`, skipping the others.
    async fn until(&mut self, prefix: &str) -> String {
        loop {
            match self.line().await {
                Some(line) if line.starts_with(prefix) => return line,
                Some(_) => continue,
                None => panic!("no line starting with {prefix:?}"),
            }
        }
    }

    /// The server closed the connection, after any lines still on the way.
    async fn closed(&mut self) -> bool {
        let mut rest = Vec::new();
        matches!(tokio::time::timeout(Duration::from_secs(5), self.reader.read_to_end(&mut rest)).await, Ok(Ok(_)))
    }
}

#[tokio::test]
async fn tcp_chat_two_people_see_each_others_lines_and_the_page_lists_them() {
    let chat = tcp_chat().await;
    let mut ana = Talker::signed_in(&chat, "ana").await;
    let mut bo = Talker::signed_in(&chat, "bo").await;
    assert_eq!(ana.until("* ").await, "* bo joined");

    ana.write(b"hello bo\n").await;
    assert_eq!(bo.until("<").await, "<ana> hello bo");
    assert_eq!(ana.until("<").await, "<ana> hello bo", "the sender gets its own line back");
    bo.write(b"hi ana\r\n").await;
    assert_eq!(ana.until("<").await, "<bo> hi ana");

    // The page's API lists what was said, oldest first, to a granted person.
    let reader = person(&chat.config, "reader@example.com");
    toolsite::accounts::users::grant(&chat.config, "reader@example.com", "tcp-chat", "viewer").unwrap();
    let cookie = app_cookie(&chat.config, &reader, "tcp-chat").await;
    let (status, body) = call(&chat.config, &cookie, "GET", "/p/tcp-chat/api/lines", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let said: Vec<String> =
        body["lines"].as_array().unwrap().iter().map(|l| format!("{} {}", l["nick"], l["text"])).collect();
    assert_eq!(said, ["\"ana\" \"hello bo\"", "\"bo\" \"hi ana\""]);
    // And to nobody else: the gate is restricted.
    let (status, _) = send_text(&chat.config, get("/p/tcp-chat/api/lines")).await;
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test]
async fn tcp_chat_a_line_split_across_writes_arrives_whole_and_two_in_one_write_arrive_apart() {
    let chat = tcp_chat().await;
    let mut ana = Talker::signed_in(&chat, "ana").await;
    let mut bo = Talker::signed_in(&chat, "bo").await;

    // Cut inside the two bytes of the é, too.
    let rest = "line, caf\u{e9}\n".as_bytes();
    ana.write(b"half a ").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ana.write(&rest[..10]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ana.write(&rest[10..]).await;
    assert_eq!(bo.until("<").await, "<ana> half a line, caf\u{e9}");

    ana.write(b"one\ntwo\n").await;
    assert_eq!(bo.until("<").await, "<ana> one");
    assert_eq!(bo.until("<").await, "<ana> two");
}

#[tokio::test]
async fn tcp_chat_refuses_a_line_over_4_kb_whether_it_ends_or_not() {
    let chat = tcp_chat().await;
    let mut ana = Talker::signed_in(&chat, "ana").await;
    let mut bo = Talker::signed_in(&chat, "bo").await;
    ana.write(format!("{}\n", "x".repeat(5 * 1024)).as_bytes()).await;
    assert_eq!(ana.until("error").await, "error: a line is at most 4096 bytes");
    assert!(ana.closed().await, "a client that sent a 5 KB line stayed connected");
    assert_eq!(bo.until("* ").await, "* ana left", "the long line reached the room");

    // The same with no end in sight: a buffer may not grow past 4 KB either.
    let mut cy = Talker::signed_in(&chat, "cy").await;
    cy.write("y".repeat(3 * 1024).as_bytes()).await;
    cy.write("y".repeat(2 * 1024).as_bytes()).await;
    assert_eq!(cy.until("error").await, "error: a line is at most 4096 bytes");
    assert!(cy.closed().await);
}

#[tokio::test]
async fn tcp_chat_closes_a_client_without_a_valid_token() {
    let chat = tcp_chat().await;
    let mut bo = Talker::signed_in(&chat, "bo").await;
    // Another app's token is not this app's.
    publish(&chat.config, "live-board").await;
    let (_, foreign) = toolsite::platform::devices::create(&chat.config, "live-board", "eve").await.unwrap();
    for first in ["token tsv_not-a-token\n".to_string(), format!("token {foreign}\n"), "hello\n".to_string()] {
        let mut eve = Talker::connect(chat.port).await;
        eve.write(first.as_bytes()).await;
        assert_eq!(eve.until("error").await, "error: the first line is token <device-token>");
        assert!(eve.closed().await, "{first:?} left the client connected");
    }
    bo.write(b"/who\n").await;
    assert_eq!(bo.until("here: ").await, "here: bo");
}

#[tokio::test]
async fn tcp_chat_nick_renames_and_who_lists_everyone_here() {
    let chat = tcp_chat().await;
    let mut ana = Talker::signed_in(&chat, "ana").await;
    let mut bo = Talker::signed_in(&chat, "bo").await;
    ana.write(b"/nick ana-desk\n").await;
    assert_eq!(bo.until("* ").await, "* ana is now ana-desk");
    bo.write(b"/who\n").await;
    assert_eq!(bo.until("here: ").await, "here: ana-desk, bo");
    ana.write(b"/nick no spaces!\n").await;
    assert!(ana.until("error").await.starts_with("error: a nickname is"));
    ana.write(b"after\n").await;
    assert_eq!(bo.until("<").await, "<ana-desk> after");

    ana.write(b"/quit\n").await;
    assert_eq!(ana.until("bye").await, "bye");
    assert!(ana.closed().await);
    assert_eq!(bo.until("* ").await, "* ana-desk left");
    bo.write(b"/who\n").await;
    assert_eq!(bo.until("here: ").await, "here: bo");
}

// --- syslog --------------------------------------------------------------------------

struct Syslog {
    config: Arc<Config>,
    addr: std::net::SocketAddr,
    port: u16,
    /// An app cookie of a person granted the app.
    cookie: String,
    _dir: TempDir,
}

/// The receiver on a UDP port of its own, with one granted person, its
/// handler already compiled by a first datagram.
async fn syslog() -> Syslog {
    let (dir, config, addr, port) = on_a_port("syslog", "udp", 5514).await;
    let ops = person(&config, "ops@example.com");
    toolsite::accounts::users::grant(&config, "ops@example.com", "syslog", "viewer").unwrap();
    let cookie = app_cookie(&config, &ops, "syslog").await;
    let s = Syslog { config, addr, port, cookie, _dir: dir };
    datagram(&s, "127.0.0.1", b"<14>warm up").await;
    stored(&s, "warm up", Duration::from_secs(120)).await;
    s
}

/// Sends one datagram from a fresh socket on `from`, so each call is a new
/// remote.
async fn datagram(s: &Syslog, from: &str, body: &[u8]) {
    let socket = tokio::net::UdpSocket::bind((from, 0)).await.unwrap();
    socket.send_to(body, ("127.0.0.1", s.port)).await.unwrap();
}

async fn logs(s: &Syslog, query: &str) -> Vec<serde_json::Value> {
    let (status, body) = call(&s.config, &s.cookie, "GET", &format!("/p/syslog/api/logs?{query}"), serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["logs"].as_array().unwrap().clone()
}

/// The row whose message is `message`, once it is stored.
async fn stored(s: &Syslog, message: &str, within: Duration) -> serde_json::Value {
    let deadline = Instant::now() + within;
    loop {
        if let Some(row) = logs(s, "").await.into_iter().find(|r| r["message"] == message) {
            return row;
        }
        assert!(Instant::now() < deadline, "{message:?} was not stored");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn syslog_stores_rfc_5424_and_rfc_3164_with_their_fields_and_filters_them() {
    let s = syslog().await;
    let wait = Duration::from_secs(10);
    datagram(&s, "127.0.0.1", b"<11>1 2026-10-07T09:05:00Z pump-7 modbusd 812 ID47 [ex@1 note=\"a \\] b\"][ex@2 x=\"1\"] \xef\xbb\xbflost contact").await;
    datagram(&s, "127.0.0.1", b"<134>Oct  7 09:05:00 router-1 dnsmasq[33]: DHCPACK 10.0.0.9\n").await;
    datagram(&s, "127.0.0.1", b"<13>Oct 17 21:00:00 backup: done").await;
    datagram(&s, "127.0.0.1", b"<30>1 - - - - - -").await;

    let pump = stored(&s, "lost contact", wait).await;
    assert_eq!((&pump["host"], &pump["app"], &pump["facility"], &pump["severity"]), (&"pump-7".into(), &"modbusd".into(), &1.into(), &3.into()));
    assert!(pump["remote"].as_str().unwrap().starts_with("127.0.0.1:"), "{pump}");
    assert!(pump["received_at"].as_i64().unwrap() > 1_700_000_000);

    let router = stored(&s, "DHCPACK 10.0.0.9", wait).await;
    assert_eq!((&router["host"], &router["app"], &router["facility"], &router["severity"]), (&"router-1".into(), &"dnsmasq".into(), &16.into(), &6.into()));
    // A sender that leaves out its host name is named by its address.
    let backup = stored(&s, "done", wait).await;
    assert_eq!((&backup["host"], &backup["app"], &backup["severity"]), (&"127.0.0.1".into(), &"backup".into(), &5.into()));
    let empty = stored(&s, "", wait).await;
    assert_eq!((&empty["host"], &empty["app"], &empty["facility"], &empty["severity"]), (&"127.0.0.1".into(), &serde_json::Value::Null, &3.into(), &6.into()));

    // error and worse is the pump alone; one host is that host alone.
    let worst: Vec<_> = logs(&s, "severity=3").await.into_iter().map(|r| r["message"].clone()).collect();
    assert_eq!(worst, ["lost contact"]);
    let router_only: Vec<_> = logs(&s, "host=router-1").await.into_iter().map(|r| r["message"].clone()).collect();
    assert_eq!(router_only, ["DHCPACK 10.0.0.9"]);
    let (_, hosts) = call(&s.config, &s.cookie, "GET", "/p/syslog/api/hosts", serde_json::Value::Null).await;
    assert_eq!(hosts["hosts"], serde_json::json!(["127.0.0.1", "pump-7", "router-1"]));
}

#[tokio::test]
async fn syslog_drops_a_datagram_from_a_source_outside_allowed_sources() {
    let s = syslog().await;
    toolsite::platform::secrets::set(&s.config, "syslog", "ALLOWED_SOURCES", Some(" 10.9.9.9, 127.0.0.2 ")).await.unwrap();
    datagram(&s, "127.0.0.1", b"<14>from outside the list").await;
    datagram(&s, "127.0.0.2", b"<14>from the list").await;
    stored(&s, "from the list", Duration::from_secs(10)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let messages: Vec<_> = logs(&s, "").await.into_iter().map(|r| r["message"].clone()).collect();
    assert!(!messages.contains(&"from outside the list".into()), "{messages:?}");

    // An empty list lets every source in again.
    toolsite::platform::secrets::set(&s.config, "syslog", "ALLOWED_SOURCES", Some("")).await.unwrap();
    datagram(&s, "127.0.0.1", b"<14>back in").await;
    stored(&s, "back in", Duration::from_secs(10)).await;
}

#[tokio::test]
async fn syslog_tails_a_new_line_live_to_a_signed_in_browser_and_to_nobody_else() {
    use futures_util::StreamExt;
    let s = syslog().await;
    assert!(app_socket(s.addr, "syslog", None, "/tail").await.is_err(), "the tail opened with no account");
    let mut tail = app_socket(s.addr, "syslog", Some(&s.cookie), "/tail").await.unwrap_or_else(|status| panic!("refused with {status}"));
    datagram(&s, "127.0.0.1", b"<12>Oct  7 09:05:00 nas-1 smartd[9]: disk 3 failing").await;
    let frame = tokio::time::timeout(Duration::from_secs(10), tail.next()).await.unwrap().unwrap().unwrap();
    let row: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!((&row["host"], &row["app"], &row["severity"], &row["message"]), (&"nas-1".into(), &"smartd".into(), &4.into(), &"disk 3 failing".into()));
    assert!(row["id"].is_i64(), "{row}");
}

#[tokio::test]
async fn syslog_stores_garbage_whole_with_severity_unknown_and_keeps_going() {
    let s = syslog().await;
    let wait = Duration::from_secs(10);
    datagram(&s, "127.0.0.1", b"no priority at all").await;
    datagram(&s, "127.0.0.1", b"<999>out of range").await;
    datagram(&s, "127.0.0.1", b"<12").await;
    datagram(&s, "127.0.0.1", b"\xff\xfe<\x00binary").await;
    datagram(&s, "127.0.0.1", b"<14>1 2026-10-07T09:05:00Z h a - - [unterminated").await;
    for message in ["no priority at all", "<999>out of range", "<12"] {
        let row = stored(&s, message, wait).await;
        assert_eq!((&row["severity"], &row["facility"], &row["app"]), (&serde_json::Value::Null, &serde_json::Value::Null, &serde_json::Value::Null), "{row}");
        assert_eq!(row["host"], "127.0.0.1");
    }
    stored(&s, "\u{fffd}\u{fffd}<\u{0}binary", wait).await;
    // A 5424 header with broken structured data keeps its severity and the
    // rest as the message.
    let broken = stored(&s, "1 2026-10-07T09:05:00Z h a - - [unterminated", wait).await;
    assert_eq!(broken["severity"], 6);
    // Garbage hurt nothing: the next good line is stored as usual.
    datagram(&s, "127.0.0.1", b"<14>still here").await;
    stored(&s, "still here", wait).await;
}

#[tokio::test]
async fn syslog_prune_deletes_rows_older_than_30_days_on_its_schedule_only() {
    let s = syslog().await;
    let (failed, out) = run_sql(
        &s.config,
        "syslog",
        "insert into logs (received_at, remote, host, message) values \
         (cast(strftime('%s', 'now') as integer) - 31 * 86400, '10.0.0.1:514', 'old', 'a month ago'), \
         (cast(strftime('%s', 'now') as integer) - 29 * 86400, '10.0.0.1:514', 'old', 'four weeks ago')",
        None,
    )
    .await;
    assert!(!failed, "{out}");

    // A visitor cannot run it, even forging the header.
    let request = Request::builder()
        .uri("/p/syslog/api/prune")
        .header("cookie", &s.cookie)
        .header("x-toolsite-scheduled", "prune")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send_text(&s.config, request).await.0, StatusCode::FORBIDDEN);
    assert_eq!(logs(&s, "host=old").await.len(), 2);

    // A second past 03:15, when the job is due.
    let state = toolsite::AppState { config: s.config.clone(), runtime: Runtime::new().unwrap() };
    let scheduler = toolsite::platform::schedule::Scheduler::new(state);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let runs = scheduler.tick(now / 86_400 * 86_400 + 3 * 3600 + 15 * 60 + 1).await;
    assert_eq!(runs.len(), 1, "prune was not due a second past 03:15");
    for run in runs {
        run.await.unwrap();
    }
    assert_eq!(toolsite::platform::schedule::read_jobs(&s.config, "syslog")["prune"].last_status.as_deref(), Some("200"));
    let left: Vec<_> = logs(&s, "host=old").await.into_iter().map(|r| r["message"].clone()).collect();
    assert_eq!(left, ["four weeks ago"]);
}

// --- duckdb-report ------------------------------------------------------------------

/// The README's claims: the job writes a real Parquet file with the
/// streaming writer and records it in SQLite; the handler hands the whole
/// file to a person with a grant and refuses everyone else, the platform's
/// gate holding for no account at all.
#[tokio::test]
async fn duckdb_report_job_writes_parquet_that_parses_and_only_a_granted_person_gets_it() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let (_dir, config) = site();
    publish(&config, "duckdb-report").await;
    let jobs = toolsite::platform::schedule::read_jobs(&config, "duckdb-report");
    assert!(jobs.contains_key("build-report"), "{:?}", jobs.keys());

    let state = toolsite::AppState { config: config.clone(), runtime: Runtime::new().unwrap() };
    let status = toolsite::platform::schedule::run_job(&state, "duckdb-report", "build-report").await.unwrap();
    assert_eq!(status, toolsite::platform::schedule::Ran::Finished("200".into()), "the job failed");

    let ann = person(&config, "ann@example.com");
    let bo = person(&config, "bo@example.com");
    toolsite::accounts::users::grant(&config, "ann@example.com", "duckdb-report", "analyst").unwrap();
    let ann_app = app_cookie(&config, &ann, "duckdb-report").await;
    let bo_app = app_cookie(&config, &bo, "duckdb-report").await;

    // SQLite holds the file's row and nothing more.
    let (status, listed) = call(&config, &ann_app, "GET", "/p/duckdb-report/api/files", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let file = listed["files"][0].clone();
    let key = file["key"].as_str().expect("no file recorded").to_string();
    assert!(key.starts_with("reports/sales-") && key.ends_with(".parquet"), "{key}");
    assert_eq!(listed["files"].as_array().unwrap().len(), 1);
    let (failed, tables) = run_sql(&config, "duckdb-report", "select name from sqlite_master where type = 'table' order by name", None).await;
    assert!(!failed, "{tables}");
    assert!(tables.contains("report_file") && !tables.contains("sales"), "{tables}");

    // Ann has a grant: the whole file, as stored.
    let request = |cookie: Option<&str>| {
        let mut builder = Request::builder().uri(format!("/p/duckdb-report/api/files/{key}"));
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        builder.body(Body::empty()).unwrap()
    };
    let (status, body, headers) = send(&config, request(Some(&ann_app))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len() as u64, file["bytes"].as_u64().unwrap(), "the size SQLite recorded is not the file's");
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    assert_eq!(header("content-type").as_deref(), Some("application/vnd.apache.parquet"));
    assert_eq!(header("content-length"), Some(body.len().to_string()));
    assert_eq!(header("x-toolsite-blob"), None, "the pointer leaked to the visitor");
    assert!(body.starts_with(b"PAR1") && body.ends_with(b"PAR1"), "not a Parquet file");

    // The footer parses natively, with every row and the declared columns.
    let reader = SerializedFileReader::new(bytes::Bytes::from(body.clone())).expect("the footer did not parse");
    let meta = reader.metadata();
    assert_eq!(meta.file_metadata().num_rows(), file["rows"].as_i64().unwrap());
    assert_eq!(meta.file_metadata().num_rows(), 730 * 50 * 55, "two years of every store selling every product");
    assert!(meta.num_row_groups() > 1, "written in one row group, not as it went");
    let columns: Vec<String> = meta.file_metadata().schema_descr().columns().iter().map(|c| c.name().to_string()).collect();
    assert_eq!(columns, ["day", "region", "store", "product", "units", "revenue"]);
    let first = reader.get_row_iter(None).unwrap().next().unwrap().unwrap();
    assert!(first.to_string().contains("Store 01"), "{first}");

    // Bo is signed in but has no grant: the handler refuses him.
    let (status, body, _) = send(&config, request(Some(&bo_app))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", String::from_utf8_lossy(&body));
    let (status, _) = call(&config, &bo_app, "GET", "/p/duckdb-report/api/files", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // No account at all: the gate refuses before the handler runs.
    let (status, ..) = send(&config, request(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Only keys the job recorded are served, whatever else the app keeps.
    let (status, _) = call(&config, &ann_app, "GET", "/p/duckdb-report/api/files/reports/other.parquet", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // However the path is spelled: encoded slashes and dots, a traversal
    // back out of reports/, or a recorded key with something on the end.
    toolsite::runtime::blobs::put(&config, "duckdb-report", "private/payroll.csv", "text/csv", b"PAYROLL").unwrap();
    for path in [
        "private/payroll.csv".to_string(),
        "private%2Fpayroll.csv".to_string(),
        "reports/..%2F..%2Fprivate/payroll.csv".to_string(),
        "reports/%2e%2e/private/payroll.csv".to_string(),
        "%70rivate/payroll.csv".to_string(),
        format!("{key}%00private/payroll.csv"),
        format!("{key}/../../private/payroll.csv"),
    ] {
        let (status, body, headers) = send(&config, Request::builder()
            .uri(format!("/p/duckdb-report/api/files/{path}"))
            .header("cookie", &ann_app)
            .body(Body::empty())
            .unwrap()).await;
        assert!(!String::from_utf8_lossy(&body).contains("PAYROLL"), "{path} served the unrecorded file: {status}");
        assert!(headers.iter().all(|(k, _)| k != "x-toolsite-blob"), "{path}");
    }

    // A visitor cannot run the job, even forging the header.
    let forged = Request::builder()
        .uri("/p/duckdb-report/api/build")
        .header("cookie", &ann_app)
        .header("x-toolsite-scheduled", "build-report")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send_text(&config, forged).await.0, StatusCode::FORBIDDEN);
}
