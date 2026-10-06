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
const EXAMPLES: [&str; 6] = ["kitchen-sink", "orders", "static-report", "blob-gallery", "inventory-policies", "live-board"];

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
        uploads: std::sync::Mutex::new(std::collections::HashMap::new()),
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

fn ticket(config: &Config, slug: &str) -> String {
    let token = format!("ticket{}", config.uploads.lock().unwrap().len());
    config.uploads.lock().unwrap().insert(
        token.clone(),
        UploadTicket { slug: slug.to_string(), expires_at: Instant::now() + Duration::from_secs(600), user: None, project: None },
    );
    token
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
    let source = root().join("examples").join(name);
    let fixtures = root().join("tests/fixtures/examples");
    let ticket = ticket(config, name);
    if source.join("migrations").is_dir() {
        upload(config, &ticket, "migrations", migrations_archive(&source.join("migrations"))).await;
    }
    upload(config, &ticket, "manifest", std::fs::read(source.join("toolsite.toml")).unwrap()).await;
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
    for name in ["kitchen-sink", "orders", "blob-gallery", "inventory-policies", "live-board"] {
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

    let state = toolsite::AppState { config: config.clone(), runtime: Runtime::new().unwrap() };
    let status = toolsite::platform::schedule::run_job(&state, "kitchen-sink", "heartbeat").await.unwrap();
    assert_eq!(status, "200");
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

    toolsite::platform::secrets::set(&config, "kitchen-sink", "GREETING", Some("s3cret-hello-value")).unwrap();
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
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
    let mut request = format!("ws://{addr}/p/live-board{path}").into_client_request().unwrap();
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
