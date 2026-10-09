//! Published files through the whole router, on two runners of one site:
//! a deploy through one is served by the other at once, a hide or a
//! removal through one is refused by the other at once, whatever it had
//! cached, and a removal put back from the trash is served again. An inline
//! upload's chunks sent to both runners in turn land as one file; two
//! deploys at once leave one bundle or the other, never a mix; and nothing
//! the platform keeps beside an app's files is ever served.
//!
//! On files the two "runners" are one process on one volume, which is all
//! file mode supports, so there the scenarios prove the rules hold at all.
//! On Postgres they share the database and the bucket and nothing else:
//! each has a data directory of its own, as two containers with no shared
//! volume would.
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

const TOKEN: &str = "test-token";
const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

struct Site {
    _dirs: Vec<tempfile::TempDir>,
    a: Arc<Config>,
    b: Arc<Config>,
    database: Option<common::Database>,
}

impl Site {
    async fn on(postgres: bool) -> Site {
        let here = tempfile::tempdir().unwrap();
        if !postgres {
            let a = Arc::new(Config::local(here.path().to_path_buf(), TOKEN));
            return Site { _dirs: vec![here], b: a.clone(), a, database: None };
        }
        let there = tempfile::tempdir().unwrap();
        let database = common::Database::new().await;
        let runner = |dir: &tempfile::TempDir| {
            Arc::new(Config {
                stores: database.stores(),
                blobs: database.blobs(),
                ..Config::local(dir.path().to_path_buf(), TOKEN)
            })
        };
        let (a, b) = (runner(&here), runner(&there));
        Site { _dirs: vec![here, there], a, b, database: Some(database) }
    }

    fn on_postgres(&self) -> bool {
        self.database.is_some()
    }

    /// On Postgres, neither runner's volume holds a published file: only
    /// the cache, an app's database and the platform's own places.
    fn volumes_hold_no_published_files(&self) {
        if !self.on_postgres() {
            return;
        }
        for config in [&self.a, &self.b] {
            for entry in std::fs::read_dir(&config.data_dir).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(name.starts_with('.') || entry.path().is_dir(), "{name} was written to a runner's volume");
                if entry.path().is_dir() && !name.starts_with('.') {
                    for inner in std::fs::read_dir(entry.path()).unwrap().flatten() {
                        let inner = inner.file_name().to_string_lossy().into_owned();
                        assert!(inner.starts_with("data.db"), "{name}/{inner} was written to a runner's volume");
                    }
                }
            }
        }
    }

    async fn finish(self) {
        self.volumes_hold_no_published_files();
        drop((self.a, self.b));
        if let Some(database) = self.database {
            database.drop().await;
        }
    }
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String) {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn get(config: &Arc<Config>, uri: &str) -> (StatusCode, String) {
    send(config, Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()).await
}

async fn status(config: &Arc<Config>, uri: &str) -> StatusCode {
    get(config, uri).await.0
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
    let (code, text) = send(config, request).await;
    assert_eq!(code, StatusCode::OK);
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

/// An upload URL's path, minted through the tool on `config`.
async fn upload_path(config: &Arc<Config>, slug: &str) -> String {
    let (failed, said) = call(config, "create_upload", serde_json::json!({ "slug": slug })).await;
    assert!(!failed, "{said}");
    let url = said.split_whitespace().find(|word| word.contains("/upload/")).unwrap_or_else(|| panic!("{said}"));
    let path = &url[url.find("/upload/").unwrap()..];
    path.trim_end_matches(['.', ',', ')', '`']).to_string()
}

async fn put(config: &Arc<Config>, path: &str, body: Vec<u8>) -> (StatusCode, String) {
    send(config, Request::builder().method("PUT").uri(path).header("host", "localhost").body(Body::from(body)).unwrap()).await
}

/// A gzipped tar of `(path, contents)`.
fn bundle(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, body.as_bytes()).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut encoder, &tar).unwrap();
    encoder.finish().unwrap()
}

async fn deploy(config: &Arc<Config>, slug: &str, entries: &[(&str, &str)]) {
    let path = upload_path(config, slug).await;
    let (code, said) = put(config, &format!("{path}?bundle"), bundle(entries)).await;
    assert_eq!(code, StatusCode::OK, "{said}");
}

// --- scenarios ----------------------------------------------------------------

async fn a_deploy_on_one_runner_is_served_by_the_other_at_once(site: Site) {
    let (a, b) = (&site.a, &site.b);
    assert_eq!(status(b, "/p/shop/").await, StatusCode::NOT_FOUND);
    deploy(a, "shop", &[("index.html", "<title>Shop</title>v1"), ("assets/app.js", "one()")]).await;
    let (code, page) = get(b, "/p/shop/").await;
    assert_eq!(code, StatusCode::OK);
    assert!(page.contains("v1"), "{page}");
    assert_eq!(get(b, "/p/shop/assets/app.js").await, (StatusCode::OK, "one()".to_string()));

    // B has the first deploy cached now; the second, through A, is what B
    // serves on its very next request. A bundle still overlays: the file
    // the second did not carry stays.
    deploy(a, "shop", &[("index.html", "<title>Shop</title>v2"), ("assets/more.js", "two()")]).await;
    let (_, page) = get(b, "/p/shop/").await;
    assert!(page.contains("v2") && !page.contains("v1"), "B served the old deploy: {page}");
    assert_eq!(get(b, "/p/shop/assets/more.js").await, (StatusCode::OK, "two()".to_string()));
    assert_eq!(get(b, "/p/shop/assets/app.js").await, (StatusCode::OK, "one()".to_string()));

    // A handler published through A runs on B, and a new one replaces it
    // there as soon as it is published.
    let path = upload_path(a, "shop").await;
    let (code, said) = put(a, &format!("{path}?handler"), HANDLER.to_vec()).await;
    assert_eq!(code, StatusCode::OK, "{said}");
    let (code, echoed) = get(b, "/p/shop/api/echo?x=1").await;
    assert_eq!(code, StatusCode::OK, "{echoed}");
    assert!(echoed.contains("GET /api/echo?x=1"), "{echoed}");

    // A page and an icon through B, read through A.
    let (failed, said) = call(b, "push_page", serde_json::json!({ "slug": "shop/about", "html": "<title>About</title>about us" })).await;
    assert!(!failed, "{said}");
    let (code, about) = get(a, "/p/shop/about").await;
    assert_eq!(code, StatusCode::OK);
    assert!(about.contains("about us"));
    let (failed, said) = call(b, "set_icon", serde_json::json!({ "slug": "shop", "icon": "S" })).await;
    assert!(!failed, "{said}");
    assert_eq!(get(a, "/icon/shop").await, (StatusCode::OK, "S".to_string()));
    let (failed, pulled) = call(a, "pull_app", serde_json::json!({ "app": "shop" })).await;
    assert!(!failed && pulled.contains("about us") && pulled.contains("v2"), "{pulled}");
    site.finish().await;
}

async fn a_hide_on_one_runner_is_refused_by_the_other_at_once(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(a, "ledger", &[("index.html", "<title>Ledger</title>rows"), ("app.css", "body{}")]).await;
    let (failed, said) = call(a, "set_icon", serde_json::json!({ "slug": "ledger", "icon": "L" })).await;
    assert!(!failed, "{said}");
    // B has every file cached.
    for uri in ["/p/ledger/", "/p/ledger/app.css", "/icon/ledger"] {
        assert_eq!(status(b, uri).await, StatusCode::OK, "{uri}");
    }
    let (failed, said) = call(b, "set_visibility", serde_json::json!({ "slug": "ledger", "hidden": true })).await;
    assert!(!failed, "{said}");
    for uri in ["/p/ledger/", "/p/ledger/app.css", "/icon/ledger"] {
        assert_eq!(status(a, uri).await, StatusCode::NOT_FOUND, "A served {uri} of a hidden app");
        assert_eq!(status(b, uri).await, StatusCode::NOT_FOUND, "B served {uri} of a hidden app");
    }
    let (failed, said) = call(a, "set_visibility", serde_json::json!({ "slug": "ledger", "hidden": false })).await;
    assert!(!failed, "{said}");
    assert_eq!(status(b, "/p/ledger/app.css").await, StatusCode::OK);
    site.finish().await;
}

/// The trash entry a removal's answer names.
fn entry_of(said: &str) -> String {
    said.rsplit(" as ").next().unwrap_or_else(|| panic!("{said}")).trim().to_string()
}

async fn removal_and_restore_across_runners(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(a, "board", &[("index.html", "<title>Board</title>first board"), ("old.js", "old()")]).await;
    assert_eq!(status(b, "/p/board/old.js").await, StatusCode::OK);
    assert_eq!(status(b, "/p/board/").await, StatusCode::OK);

    let (failed, said) = call(a, "remove_page", serde_json::json!({ "slug": "board", "confirm": "board" })).await;
    assert!(!failed, "{said}");
    let first = entry_of(&said);
    for uri in ["/p/board/", "/p/board/old.js"] {
        assert_eq!(status(b, uri).await, StatusCode::NOT_FOUND, "B served {uri} of a removed app from its cache");
    }
    let (_, listed) = call(b, "list_pages", serde_json::json!({ "include_all": true })).await;
    assert!(!listed.contains("\"board\""), "a removed app was listed: {listed}");

    // A new app at the name has none of the old one's files, on either
    // runner, though B cached them.
    deploy(b, "board", &[("index.html", "<title>Board</title>second board")]).await;
    let (_, page) = get(a, "/p/board/").await;
    assert!(page.contains("second board"), "{page}");
    assert_eq!(status(a, "/p/board/old.js").await, StatusCode::NOT_FOUND, "the new app served the old one's file");
    assert_eq!(status(b, "/p/board/old.js").await, StatusCode::NOT_FOUND, "the new app served the old one's file");

    // The first cannot come back over the second; once the second is gone,
    // it can, and both runners serve it.
    let restoring = (b.clone(), first.clone());
    let refused = tokio::task::spawn_blocking(move || toolsite::platform::trash::restore(&restoring.0, &restoring.1)).await.unwrap();
    assert!(refused.is_err(), "a removal was put back over a newer app");
    let (failed, said) = call(b, "remove_page", serde_json::json!({ "slug": "board", "confirm": "board" })).await;
    assert!(!failed, "{said}");
    let restoring = (b.clone(), first.clone());
    let back = tokio::task::spawn_blocking(move || toolsite::platform::trash::restore(&restoring.0, &restoring.1))
        .await
        .unwrap()
        .unwrap();
    assert!(back.iter().any(|item| item == "board/"), "{back:?}");
    for config in [a, b] {
        let (code, page) = get(config, "/p/board/").await;
        assert_eq!(code, StatusCode::OK);
        assert!(page.contains("first board"), "{page}");
        assert_eq!(get(config, "/p/board/old.js").await, (StatusCode::OK, "old()".to_string()));
    }
    site.finish().await;
}

async fn inline_chunks_sent_to_both_runners_land_as_one(site: Site) {
    let (a, b) = (&site.a, &site.b);
    let body = bundle(&[("index.html", "<title>Inline</title>pieced together"), ("data.json", &"7".repeat(5000))]);
    let (failed, said) = call(a, "upload_begin", serde_json::json!({ "slug": "inline", "kind": "bundle" })).await;
    assert!(!failed, "{said}");
    let id = said.split_whitespace().nth(1).unwrap_or_else(|| panic!("{said}")).to_string();
    use base64::Engine as _;
    let pieces: Vec<&[u8]> = body.chunks(body.len().div_ceil(4)).collect();
    // Out of order, and alternating: a balancer's coin.
    for index in [2usize, 0, 3, 1] {
        let runner = if index % 2 == 0 { a } else { b };
        let data = base64::engine::general_purpose::STANDARD.encode(pieces[index]);
        let (failed, said) = call(runner, "upload_chunk", serde_json::json!({ "id": id, "index": index, "data": data })).await;
        assert!(!failed, "chunk {index}: {said}");
    }
    let (failed, said) = call(b, "upload_finish", serde_json::json!({ "id": id, "chunks": pieces.len() })).await;
    assert!(!failed, "{said}");
    let (code, page) = get(a, "/p/inline/").await;
    assert_eq!(code, StatusCode::OK);
    assert!(page.contains("pieced together"));
    assert_eq!(get(a, "/p/inline/data.json").await.1, "7".repeat(5000));
    site.finish().await;
}

async fn two_deploys_at_once_leave_one_bundle_or_the_other(site: Site) {
    let (a, b) = (&site.a, &site.b);
    let files: Vec<String> = (0..24).map(|n| format!("part-{n}.txt")).collect();
    let made = |mark: &str| {
        let mut entries: Vec<(String, String)> = files.iter().map(|name| (name.clone(), mark.to_string())).collect();
        entries.push(("index.html".to_string(), format!("<title>Race</title>{mark}")));
        entries
    };
    for round in 0..3 {
        let (x, y) = (made(&format!("x{round}")), made(&format!("y{round}")));
        fn as_refs(entries: &[(String, String)]) -> Vec<(&str, &str)> {
            entries.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect()
        }
        let path_a = format!("{}?bundle", upload_path(a, "race").await);
        let path_b = format!("{}?bundle", upload_path(b, "race").await);
        let (body_x, body_y) = (bundle(&as_refs(&x)), bundle(&as_refs(&y)));
        let (first, second) = tokio::join!(put(a, &path_a, body_x), put(b, &path_b, body_y));
        assert_eq!(first.0, StatusCode::OK, "{}", first.1);
        assert_eq!(second.0, StatusCode::OK, "{}", second.1);
        for config in [a, b] {
            let mut seen = std::collections::BTreeSet::new();
            for name in &files {
                seen.insert(get(config, &format!("/p/race/{name}")).await.1);
            }
            assert_eq!(seen.len(), 1, "round {round}: two deploys at once left a mix: {seen:?}");
            let winner = seen.into_iter().next().unwrap();
            let (_, index) = get(config, "/p/race/").await;
            assert!(index.contains(&winner), "round {round}: the index is from one deploy and the files from another");
        }
    }
    site.finish().await;
}

async fn nothing_the_platform_keeps_is_served(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(
        a,
        "vault",
        &[("index.html", "<title>Vault</title>front"), ("data.db", "rows"), ("index.meta", r#"{"gate":"public"}"#)],
    )
    .await;
    let path = upload_path(a, "vault").await;
    assert_eq!(put(a, &format!("{path}?handler"), HANDLER.to_vec()).await.0, StatusCode::OK);
    assert_eq!(put(a, &format!("{path}?source"), bundle(&[("src/main.rs", "fn main() {}")])).await.0, StatusCode::OK);
    assert_eq!(put(a, &format!("{path}?icon"), b"V".to_vec()).await.0, StatusCode::OK);
    for config in [a, b] {
        for uri in [
            "/p/vault/handler.wasm",
            "/p/vault/data.db",
            "/p/vault/index.meta",
            "/p/vault.source",
            "/p/vault.icon",
            "/p/vault/index.icon",
            "/p/.toolsite/content/vault/index.html",
            "/p/.tmp/content",
        ] {
            assert_eq!(status(config, uri).await, StatusCode::NOT_FOUND, "{uri} was served");
        }
        assert_eq!(status(config, "/p/vault/").await, StatusCode::OK);
    }

    // A loose page at the top, hidden: its file is not served under its
    // file name either, which would pass by the page's own hidden flag.
    let (failed, said) = call(a, "push_page", serde_json::json!({ "slug": "memo", "html": "<title>Memo</title>secret memo" })).await;
    assert!(!failed, "{said}");
    let (failed, said) = call(a, "set_visibility", serde_json::json!({ "slug": "memo", "hidden": true })).await;
    assert!(!failed, "{said}");
    for config in [a, b] {
        assert_eq!(status(config, "/p/memo").await, StatusCode::NOT_FOUND);
        let (code, body) = get(config, "/p/memo.html").await;
        assert!(code == StatusCode::NOT_FOUND && !body.contains("secret memo"), "a hidden page was served as a file");
    }
    site.finish().await;
}

// --- each scenario, on the backend asked for and on Postgres -----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_on_one_runner_is_served_by_the_other() {
    a_deploy_on_one_runner_is_served_by_the_other_at_once(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hide_on_one_runner_is_refused_by_the_other() {
    a_hide_on_one_runner_is_refused_by_the_other_at_once(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_is_refused_everywhere_and_put_back_everywhere() {
    removal_and_restore_across_runners(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inline_upload_split_between_runners_lands_whole() {
    inline_chunks_sent_to_both_runners_land_as_one(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_deploys_at_once_never_interleave() {
    two_deploys_at_once_leave_one_bundle_or_the_other(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn platform_files_and_hidden_pages_are_never_served() {
    nothing_the_platform_keeps_is_served(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_deploy_on_one_runner_is_served_by_the_other_on_postgres() {
    a_deploy_on_one_runner_is_served_by_the_other_at_once(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_hide_on_one_runner_is_refused_by_the_other_on_postgres() {
    a_hide_on_one_runner_is_refused_by_the_other_at_once(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_removal_is_refused_everywhere_and_put_back_everywhere_on_postgres() {
    removal_and_restore_across_runners(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn an_inline_upload_split_between_runners_lands_whole_on_postgres() {
    inline_chunks_sent_to_both_runners_land_as_one(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn two_deploys_at_once_never_interleave_on_postgres() {
    two_deploys_at_once_leave_one_bundle_or_the_other(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn platform_files_and_hidden_pages_are_never_served_on_postgres() {
    nothing_the_platform_keeps_is_served(Site::on(true).await).await;
}
