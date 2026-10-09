//! Per-app records, tokens and job turns, attacked: a token tried on every
//! route but its own, a token minted at a name before anyone published
//! there, listings searched for a digest, a job or a setting named for a
//! path inside an app, and a scheduled turn claimed out of order or
//! claimed until the table fills.
//!
//! Each scenario runs on the backend `TOOLSITE_TEST_BACKEND` names (files by
//! default) and has an `_on_postgres` twin; the turn claims are Postgres's
//! alone, since files have one scheduler.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use toolsite::{build_router, runtime::wasm::Runtime, Config};
use tower::ServiceExt;

mod common;
use common::blocking;

const TOKEN: &str = "test-token";

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

    async fn rows(&self, sql: &str) -> i64 {
        let client = self.database.as_ref().expect("a Postgres site").postgres.pool.get().await.unwrap();
        client.query_one(sql, &[]).await.unwrap().get(0)
    }

    async fn finish(self) {
        drop(self.config);
        if let Some(database) = self.database {
            database.drop().await;
        }
    }
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String) {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
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
    let (status, text) = send(config, request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let json: serde_json::Value = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let result = &json["result"];
    (result["isError"] == true, result["content"][0]["text"].as_str().unwrap_or_default().to_string())
}

/// The plain token from a token tool's answer.
async fn mint(config: &Arc<Config>, tool: &str, app: &str, label: &str) -> String {
    let (failed, said) = call(config, tool, serde_json::json!({ "app": app, "action": "create", "label": label })).await;
    assert!(!failed, "{said}");
    said.split("Shown once:\n\n").nth(1).and_then(|rest| rest.lines().next()).unwrap_or_else(|| panic!("{said}")).to_string()
}

fn seed(config: &Config, app: &str) {
    blocking(|| {
        toolsite::runtime::db::run(config, app, "create table orders (id integer)", &[]).unwrap();
        toolsite::runtime::db::run(config, app, "insert into orders values (1)", &[]).unwrap();
    });
}

fn digest(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn device_check(config: &Arc<Config>, app: &'static str, presented: &str) -> Option<String> {
    let (config, presented) = (config.clone(), presented.to_string());
    blocking(move || toolsite::platform::devices::check(&config, app, &presented))
}

// --- scenarios ------------------------------------------------------------------

/// Each token opens its own route for its own app and nothing else: not
/// another app's, not another kind's, not with its prefix swapped for one
/// that route takes.
async fn every_route_but_its_own_refuses_a_token(site: Site) {
    let config = site.config.clone();
    call(&config, "push_app", serde_json::json!({ "app": "alpha", "pages": { "index": "<title>A</title>" } })).await;
    call(&config, "push_app", serde_json::json!({ "app": "beta", "pages": { "index": "<title>B</title>" } })).await;
    seed(&config, "alpha");
    seed(&config, "beta");
    let export = mint(&config, "app_exports", "alpha", "bi").await;
    let deploy = mint(&config, "app_deploy_tokens", "alpha", "ci").await;
    let device = mint(&config, "app_device_tokens", "alpha", "pump").await;
    let swapped = |token: &str, to: &str| format!("{to}{}", &token[4..]);

    for (token, what) in [(&export, "export"), (&deploy, "deploy"), (&device, "device")] {
        let mut tries = vec![
            ("GET", "/export/beta.sqlite".to_string()),
            ("PUT", "/deploy/beta".to_string()),
            ("PUT", "/deploy/beta/about".to_string()),
        ];
        if what != "export" {
            tries.push(("GET", "/export/alpha.sqlite".to_string()));
        }
        if what != "deploy" {
            tries.push(("PUT", "/deploy/alpha".to_string()));
            tries.push(("PUT", "/deploy/alpha/about".to_string()));
        }
        for (method, uri) in tries {
            let (status, body) = send(&config, bearer(method, &uri, token, "<title>x</title>")).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "a {what} token opened {method} {uri}: {body}");
        }
        for (to, uri, method) in [("tse_", "/export/alpha.sqlite", "GET"), ("tsd_", "/deploy/alpha", "PUT")] {
            if !token.starts_with(to) {
                let (status, _) = send(&config, bearer(method, uri, &swapped(token, to), "<title>x</title>")).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "a {what} token with its prefix swapped opened {uri}");
            }
        }
        assert_eq!(device_check(&config, "beta", token), None, "another app's handler accepted a {what} token");
        if what != "device" {
            assert_eq!(device_check(&config, "alpha", token), None, "a {what} token checked in as a device");
            assert_eq!(device_check(&config, "alpha", &swapped(token, "tsv_")), None, "a {what} token with a device prefix checked in");
        }
    }
    // Each still opens its own.
    assert_eq!(send(&config, bearer("GET", "/export/alpha.sqlite", &export, "")).await.0, StatusCode::OK);
    assert_eq!(send(&config, bearer("PUT", "/deploy/alpha/about", &deploy, "<title>x</title>")).await.0, StatusCode::OK);
    assert_eq!(device_check(&config, "alpha", &device).as_deref(), Some("pump"));
    assert_eq!(send(&config, bearer("GET", "/p/beta/", TOKEN, "")).await.0, StatusCode::OK, "beta was replaced");
    site.finish().await;
}

/// A token minted at a name nobody has published at is held by whoever
/// minted it. The first publish there by someone else starts the app with
/// nothing held on it, as it starts with no access rows: that token must
/// not open the new app's database or replace its pages.
async fn a_token_minted_before_its_app_opens_nothing_once_it_is_published(site: Site) {
    let config = site.config.clone();
    let export = mint(&config, "app_exports", "future", "early").await;
    let deploy = mint(&config, "app_deploy_tokens", "future", "early").await;
    let device = mint(&config, "app_device_tokens", "future", "early").await;
    let (failed, said) = call(&config, "push_app", serde_json::json!({ "app": "future", "pages": { "index": "<title>Mine</title>" } })).await;
    assert!(!failed, "{said}");
    seed(&config, "future");
    assert_eq!(send(&config, bearer("GET", "/export/future.sqlite", &export, "")).await.0, StatusCode::UNAUTHORIZED, "an early export token opened the app");
    assert_eq!(send(&config, bearer("PUT", "/deploy/future", &deploy, "<title>Theirs</title>")).await.0, StatusCode::UNAUTHORIZED, "an early deploy token replaced the app");
    assert_eq!(device_check(&config, "future", &device), None, "an early device token checked in");
    let (_, listed) = call(&config, "app_exports", serde_json::json!({ "app": "future", "action": "list" })).await;
    assert!(listed.contains("no export tokens"), "{listed}");

    // The same for a page published inline, the other first publish.
    let early = mint(&config, "app_exports", "later", "early").await;
    let (failed, said) = call(&config, "push_page", serde_json::json!({ "slug": "later", "html": "<title>Later</title>" })).await;
    assert!(!failed, "{said}");
    seed(&config, "later");
    assert_eq!(send(&config, bearer("GET", "/export/later.sqlite", &early, "")).await.0, StatusCode::UNAUTHORIZED, "an early export token opened a page's app");

    // A pipeline that makes the first publish itself keeps its token: it
    // is the one publishing.
    let ci = mint(&config, "app_deploy_tokens", "piped", "ci").await;
    assert_eq!(send(&config, bearer("PUT", "/deploy/piped", &ci, "<title>v1</title>")).await.0, StatusCode::OK);
    assert_eq!(send(&config, bearer("PUT", "/deploy/piped", &ci, "<title>v2</title>")).await.0, StatusCode::OK);
    site.finish().await;
}

/// A listing names a token by id and label; the digest a check compares
/// stays in the store, on every surface that lists tokens.
async fn no_listing_shows_a_digest(site: Site) {
    let config = site.config.clone();
    call(&config, "push_app", serde_json::json!({ "app": "alpha", "pages": { "index": "<title>A</title>" } })).await;
    let mut digests = Vec::new();
    for (tool, label) in [("app_exports", "bi"), ("app_deploy_tokens", "ci"), ("app_device_tokens", "pump")] {
        let token = mint(&config, tool, "alpha", label).await;
        let (_, listed) = call(&config, tool, serde_json::json!({ "app": "alpha", "action": "list" })).await;
        assert!(listed.contains(label), "{listed}");
        assert!(!listed.contains(&digest(&token)) && !listed.contains(&token), "{tool} listed a secret: {listed}");
        digests.push(digest(&token));
    }
    let admin = blocking(|| {
        toolsite::accounts::users::sign_up_as(&config, "boss@example.com", "correct horse battery", true).unwrap();
        toolsite::accounts::users::log_in(&config, "boss@example.com", "correct horse battery").unwrap().1
    });
    for page in ["/admin/apps/alpha/exports", "/admin/apps/alpha/repo", "/admin/apps/alpha/connections", "/admin/exports"] {
        let request = Request::builder()
            .uri(page)
            .header("host", "localhost")
            .header("cookie", format!("ts_session={admin}"))
            .body(Body::empty())
            .unwrap();
        let (status, html) = send(&config, request).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for digest in &digests {
            assert!(!html.contains(digest.as_str()), "{page} showed a digest");
        }
    }
    site.finish().await;
}

/// A job and a setting belong to an app, which is one segment. Named for a
/// path inside one, a job escaped the app's limits (its jobs counted, its
/// running cap and its start rate all keyed by a name nobody else used),
/// and a setting was a record no app reads, written beside the app's files.
async fn a_job_or_setting_cannot_name_a_path_inside_an_app(site: Site) {
    let config = site.config.clone();
    call(&config, "push_app", serde_json::json!({ "app": "alpha", "pages": { "index": "<title>A</title>" } })).await;
    for app in ["alpha/sub", "alpha/sub/deeper"] {
        let (failed, said) =
            call(&config, "app_jobs", serde_json::json!({ "app": app, "name": "tick", "schedule": "* * * * * *", "path": "/api/x" })).await;
        assert!(failed, "a job was scheduled for {app}: {said}");
        let (failed, said) = call(&config, "app_settings", serde_json::json!({ "app": app, "name": "KEY", "value": "v" })).await;
        assert!(failed, "a setting was stored for {app}: {said}");
        assert!(toolsite::platform::schedule::set_job(&config, app, "tick", "* * * * * *", "/api/x").await.is_err());
        assert!(toolsite::platform::secrets::set(&config, app, "KEY", Some("v")).await.is_err());
    }
    // Nor can a job's name carry the separator a slot's name is built
    // with: app `a` and job `b/c` would be the slot of app `a/b`'s job `c`.
    for name in ["sub/tick", "sub:tick", "../tick"] {
        let (failed, said) =
            call(&config, "app_jobs", serde_json::json!({ "app": "alpha", "name": name, "schedule": "0 0 3 * * *", "path": "/api/x" })).await;
        assert!(failed, "a job called {name} was scheduled: {said}");
    }
    let (failed, said) =
        call(&config, "app_jobs", serde_json::json!({ "app": "alpha", "name": "tick", "schedule": "0 0 3 * * *", "path": "/api/x" })).await;
    assert!(!failed, "{said}");
    assert!(!config.data_dir.join("alpha/sub.jobs").exists() && !config.data_dir.join("alpha/sub.secrets").exists());
    site.finish().await;
}

/// What a deploy token publishes is what an editor's upload ticket
/// publishes, and a manifest it carries is held to the site's ceilings
/// the same way: no limit past them, no file the platform keeps.
async fn a_deploy_tokens_manifest_gets_no_more_than_an_editors(site: Site) {
    let config = site.config.clone();
    let deploy = mint(&config, "app_deploy_tokens", "alpha", "ci").await;
    assert_eq!(send(&config, bearer("PUT", "/deploy/alpha", &deploy, "<title>A</title>")).await.0, StatusCode::OK);
    let manifest = "[limits]\nrequest_seconds = 86400\njob_seconds = 86400\nmemory_mb = 1000000\nquery_rows = 100000000\n";
    let (status, said) = send(&config, bearer("PUT", "/deploy/alpha?manifest", &deploy, manifest)).await;
    assert_eq!(status, StatusCode::OK, "{said}");
    let effective = toolsite::runtime::limits::of(&config, "alpha").await;
    let ceilings = toolsite::runtime::limits::Ceilings::default();
    assert!(effective.job.wall_clock.as_secs() <= ceilings.job_seconds, "{effective:?}");
    assert!(effective.request.wall_clock.as_secs() <= ceilings.request_seconds, "{effective:?}");
    assert!(effective.request.query_rows as u64 <= ceilings.query_rows, "{effective:?}");
    // A bundle cannot carry the app's records or tokens in either.
    let bundle = {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        for (name, body) in [("index.html", "<title>A</title>"), ("index.meta", "{\"gate\":\"public\"}"), ("x.deploys", "[]"), ("x.jobs", "{}")] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, body.as_bytes()).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    };
    let request = Request::builder()
        .method("PUT")
        .uri("/deploy/alpha?bundle")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {deploy}"))
        .body(Body::from(bundle))
        .unwrap();
    let (status, said) = send(&config, request).await;
    assert_eq!(status, StatusCode::OK, "{said}");
    for kept in ["alpha/x.deploys", "alpha/x.jobs", "alpha/index.meta"] {
        assert!(!config.data_dir.join(kept).exists(), "a bundle wrote {kept}");
    }
    site.finish().await;
}

/// No sidecar a record or a token is kept in is served, by any spelling.
async fn no_sidecar_is_served(site: Site) {
    let config = site.config.clone();
    call(&config, "push_app", serde_json::json!({ "app": "alpha", "pages": { "index": "<title>A</title>" } })).await;
    mint(&config, "app_exports", "alpha", "bi").await;
    mint(&config, "app_deploy_tokens", "alpha", "ci").await;
    mint(&config, "app_device_tokens", "alpha", "pump").await;
    call(&config, "app_jobs", serde_json::json!({ "app": "alpha", "name": "tick", "schedule": "0 0 3 * * *", "path": "/api/x" })).await;
    call(&config, "app_settings", serde_json::json!({ "app": "alpha", "name": "KEY", "value": "v" })).await;
    for extension in ["exports", "deploys", "devices", "jobs", "secrets"] {
        for uri in [format!("/alpha.{extension}"), format!("/p/alpha.{extension}"), format!("/p/alpha/..%2Falpha.{extension}"), format!("/p/alpha/%2E%2E/alpha.{extension}")] {
            let request = Request::builder().uri(&uri).header("host", "localhost").body(Body::empty()).unwrap();
            let (status, body) = send(&config, request).await;
            assert!(!status.is_success(), "{uri} was served: {body}");
        }
    }
    site.finish().await;
}

/// The record of which turns were claimed is one row per job, however
/// often the job fires, and a turn no later than one already claimed is
/// refused: a scheduler whose clock runs behind cannot fire an old turn
/// after a newer one ran.
async fn a_claimed_turn_is_one_row_per_job_and_never_runs_backwards(site: Site) {
    let records = toolsite::platform::records::of(&site.config);
    let base = 2_000_000_000u64;
    for turn in base..base + 500 {
        assert!(records.fire("app", "tick", turn).await.unwrap(), "turn {turn} was refused");
        assert!(!records.fire("app", "tick", turn).await.unwrap(), "turn {turn} fired twice");
    }
    assert!(!records.fire("app", "tick", base + 10).await.unwrap(), "a turn fired again");
    assert!(!records.fire("app", "tick", base - 5).await.unwrap(), "an older turn fired after a newer one");
    // Another job, and another app's job of the same name, are their own.
    assert!(records.fire("app", "tock", base).await.unwrap());
    assert!(records.fire("app2", "tick", base).await.unwrap());
    let kept = site.rows("select count(*) from platform.job_turns where app = 'app' and name = 'tick'").await;
    assert_eq!(kept, 1, "every turn of an every-second job kept a row");
    site.finish().await;
}

// --- on the backend TOOLSITE_TEST_BACKEND names ------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_route_but_a_tokens_own_refuses_it() {
    every_route_but_its_own_refuses_a_token(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_minted_before_its_app_was_published_opens_nothing_after() {
    a_token_minted_before_its_app_opens_nothing_once_it_is_published(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_token_listing_shows_a_digest() {
    no_listing_shows_a_digest(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_or_a_setting_cannot_name_a_path_inside_an_app() {
    a_job_or_setting_cannot_name_a_path_inside_an_app(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_tokens_manifest_gets_no_more_than_an_editors_does() {
    a_deploy_tokens_manifest_gets_no_more_than_an_editors(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_record_or_token_sidecar_is_served() {
    no_sidecar_is_served(Site::on(common::wants_postgres()).await).await;
}

// --- on Postgres, for scripts/test-postgres.sh ---------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_deploy_tokens_manifest_gets_no_more_than_an_editors_does_on_postgres() {
    a_deploy_tokens_manifest_gets_no_more_than_an_editors(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn every_route_but_a_tokens_own_refuses_it_on_postgres() {
    every_route_but_its_own_refuses_a_token(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_token_minted_before_its_app_was_published_opens_nothing_after_on_postgres() {
    a_token_minted_before_its_app_opens_nothing_once_it_is_published(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn no_token_listing_shows_a_digest_on_postgres() {
    no_listing_shows_a_digest(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_job_or_a_setting_cannot_name_a_path_inside_an_app_on_postgres() {
    a_job_or_setting_cannot_name_a_path_inside_an_app(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_claimed_turn_is_one_row_per_job_and_never_runs_backwards_on_postgres() {
    a_claimed_turn_is_one_row_per_job_and_never_runs_backwards(Site::on(true).await).await;
}
