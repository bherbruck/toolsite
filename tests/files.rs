//! Published files through the whole router, on two runners of one site:
//! a deploy through one is served by the other at once, a hide or a
//! removal through one is refused by the other at once, whatever it had
//! cached, and a removal put back from the trash is served again. An inline
//! upload's chunks sent to both runners in turn land as one file; two
//! deploys at once leave one bundle or the other, never a mix; and nothing
//! the platform keeps beside an app's files is ever served.
//!
//! Then what an adversarial pass tried: a hidden or gated page under every
//! other name for it, icons that are HTML or script, a stranger's misses
//! filling a runner's disk, a bundle a store cannot hold refused half way,
//! a removed app's handler still run, a restore handing back retired
//! tokens, and a hide racing a deploy.
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

/// A response's status, headers and body, for the scenarios that read headers.
async fn fetch(config: &Arc<Config>, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let request = Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap();
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap();
    (parts.status, parts.headers, String::from_utf8_lossy(&bytes).to_string())
}

/// Asserts no form of `uri` hands out `secret`: a 404, or a refusal that
/// carries none of the page.
async fn never_served(config: &Arc<Config>, uri: &str, secret: &str) {
    let (code, body) = get(config, uri).await;
    assert!(code != StatusCode::OK && !body.contains(secret), "{uri} was served ({code}): {body}");
}

async fn hidden_and_gated_pages_under_other_names(site: Site) {
    let (a, b) = (&site.a, &site.b);
    // A loose page at the top, hidden.
    let (failed, said) = call(a, "push_page", serde_json::json!({ "slug": "memo", "html": "<title>Memo</title>top memo" })).await;
    assert!(!failed, "{said}");
    let (failed, said) = call(a, "set_icon", serde_json::json!({ "slug": "memo", "icon": "M" })).await;
    assert!(!failed, "{said}");
    // A page in a group with no app of its own, and a page inside an app,
    // each hidden on its own while what is around it stays up.
    for (slug, html) in [("grp/note", "<title>Note</title>grouped note"), ("grp/open", "<title>Open</title>open note")] {
        let (failed, said) = call(a, "push_page", serde_json::json!({ "slug": slug, "html": html })).await;
        assert!(!failed, "{said}");
    }
    let (failed, said) = call(a, "set_icon", serde_json::json!({ "slug": "grp/note", "icon": "N" })).await;
    assert!(!failed, "{said}");
    deploy(a, "shop", &[("index.html", "<title>Shop</title>storefront")]).await;
    let (failed, said) = call(a, "push_page", serde_json::json!({ "slug": "shop/staff", "html": "<title>Staff</title>staff rota" })).await;
    assert!(!failed, "{said}");
    let (failed, said) = call(a, "push_page", serde_json::json!({ "slug": "shop/payroll", "html": "<title>Pay</title>payroll sheet" })).await;
    assert!(!failed, "{said}");
    // Every runner has the pages cached before anything is hidden.
    for uri in ["/p/memo", "/p/grp/note", "/p/shop/staff", "/p/shop/payroll"] {
        assert_eq!(status(b, uri).await, StatusCode::OK, "{uri}");
    }
    for slug in ["memo", "grp/note", "shop/staff"] {
        let (failed, said) = call(b, "set_visibility", serde_json::json!({ "slug": slug, "hidden": true })).await;
        assert!(!failed, "{said}");
    }
    // A private corner of a public app, by path rule.
    let (failed, said) = call(b, "set_visibility", serde_json::json!({ "slug": "shop", "gate": "restricted", "path": "/payroll" })).await;
    assert!(!failed, "{said}");
    // A whole app behind a gate.
    deploy(a, "locked", &[("index.html", "<title>Locked</title>locked front"), ("app.css", "locked-css")]).await;
    let (failed, said) = call(a, "set_icon", serde_json::json!({ "slug": "locked", "icon": "L" })).await;
    assert!(!failed, "{said}");
    let (failed, said) = call(a, "set_visibility", serde_json::json!({ "slug": "locked", "gate": "restricted" })).await;
    assert!(!failed, "{said}");

    for config in [a, b] {
        for uri in [
            "/p/memo", "/p/memo/", "/p/memo.html", "/p/memo.html/", "/p/memo.HTML", "/p/memo.htm", "/p/MEMO.html",
            "/p/Memo", "/p/memo.html.", "/p/memo%2ehtml", "/p/memo%2Ehtml", "/p/memo/index.html", "/p//memo.html",
            "/p/./memo.html", "/p/memo.html%00", "/p/memo%00.html", "/p/memo.html%2f",
        ] {
            never_served(config, uri, "top memo").await;
        }
        for uri in ["/p/grp/note", "/p/grp/note.html", "/p/grp/note.HTML", "/p/grp/note.html/", "/p/grp//note.html", "/p/grp/note/index.html"] {
            never_served(config, uri, "grouped note").await;
        }
        for uri in ["/p/shop/staff", "/p/shop/staff.html", "/p/shop/staff.HTML", "/p/shop/staff.html/"] {
            never_served(config, uri, "staff rota").await;
        }
        for uri in ["/p/shop/payroll", "/p/shop/payroll.html", "/p/shop/payroll/", "/p/shop/payroll.HTML"] {
            never_served(config, uri, "payroll sheet").await;
        }
        for uri in [
            "/p/locked", "/p/locked/", "/p/locked/index.html", "/p/locked.html", "/p/locked/app.css", "/p/Locked/app.css",
            "/p/locked/./app.css", "/p/locked//app.css",
        ] {
            never_served(config, uri, "locked").await;
        }
        for uri in ["/icon/memo", "/icon/grp/note", "/icon/locked", "/icon/memo.html"] {
            assert_eq!(status(config, uri).await, StatusCode::NOT_FOUND, "{uri} was served");
        }
        // What stays up stays up.
        assert!(get(config, "/p/grp/open").await.1.contains("open note"));
        assert!(get(config, "/p/shop/").await.1.contains("storefront"));
    }
    site.finish().await;
}

/// An icon is whatever bytes its publisher sent. Served on toolsite's own
/// host, HTML must not render as a page and an SVG's script must not run,
/// from the bucket as from the volume.
async fn icons_are_served_inert_from_wherever_they_live(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(a, "inert", &[("index.html", "<title>Inert</title>front")]).await;
    let path = upload_path(a, "inert").await;
    for (body, kind) in [
        ("<html><body><script>alert(document.cookie)</script></body></html>", "text/plain"),
        ("<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>", "image/svg+xml"),
    ] {
        assert_eq!(put(a, &format!("{path}?icon"), body.as_bytes().to_vec()).await.0, StatusCode::OK);
        let (code, headers, served) = fetch(b, "/icon/inert").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(served, body);
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        assert!(header("content-type").starts_with(kind), "{}", header("content-type"));
        assert_eq!(header("x-content-type-options"), "nosniff");
        assert!(header("content-security-policy").contains("sandbox"), "{}", header("content-security-policy"));
        assert!(header("content-security-policy").contains("default-src 'none'"));
    }
    site.finish().await;
}

/// Every file under a runner's cache.
fn cached_files(config: &Config) -> usize {
    fn walk(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| if entry.path().is_dir() { walk(&entry.path()) } else { 1 })
            .sum()
    }
    walk(&config.data_dir.join(".tmp").join("content"))
}

/// A stranger asking for names that were never published leaves nothing
/// on a runner's disk: each miss is an answer, not a file.
async fn misses_leave_nothing_on_a_runners_disk(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(a, "spray", &[("index.html", "<title>Spray</title>front")]).await;
    assert_eq!(status(b, "/p/spray/").await, StatusCode::OK);
    let before = cached_files(b);
    for n in 0..200 {
        assert_eq!(status(b, &format!("/p/spray/miss-{n}.js")).await, StatusCode::NOT_FOUND);
    }
    let after = cached_files(b);
    assert!(after <= before + 2, "200 misses left {} files in the cache", after - before);
    // A miss is still remembered no longer than its generation: published
    // now, the file is there at once.
    deploy(a, "spray", &[("miss-7.js", "now here")]).await;
    assert_eq!(get(b, "/p/spray/miss-7.js").await, (StatusCode::OK, "now here".to_string()));
    site.finish().await;
}

/// A bundle holding a name the store cannot keep is refused before any of
/// it is written: the app goes on serving the bundle before it, whole,
/// rather than the half of the new one written before the refusal.
async fn bundles_the_store_cannot_hold(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(a, "whole", &[("index.html", "<title>Whole</title>first"), ("app.js", "first()")]).await;
    let long_segment = format!("{}.js", "s".repeat(300));
    let long_key = format!("{}/x.js", vec!["d".repeat(200); 5].join("/"));
    for long in [long_segment.as_str(), long_key.as_str()] {
        let path = upload_path(a, "whole").await;
        let body = bundle(&[("index.html", "<title>Whole</title>second"), ("app.js", "second()"), (long, "x")]);
        let (code, said) = put(a, &format!("{path}?bundle"), body).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{said}");
        for config in [a, b] {
            assert!(get(config, "/p/whole/").await.1.contains("first"), "a refused bundle was half written");
            assert_eq!(get(config, "/p/whole/app.js").await.1, "first()", "a refused bundle was half written");
        }
    }
    site.finish().await;
}

/// A request through a router on `runtime`, whose compiled handlers last
/// from one request to the next, as a running server's do.
async fn status_with(config: &Arc<Config>, runtime: &Arc<Runtime>, uri: &str) -> StatusCode {
    let request = Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap();
    build_router(config.clone(), runtime.clone()).oneshot(request).await.unwrap().status()
}

/// A removed app's handler runs on no runner, though one ran it a moment
/// before; and the next app at the name, published without one, has none.
async fn a_removed_apps_handler_runs_nowhere(site: Site) {
    let (a, b) = (&site.a, &site.b);
    let runtime = Runtime::new().unwrap();

    deploy(a, "coded", &[("index.html", "<title>Coded</title>front")]).await;
    let path = upload_path(a, "coded").await;
    assert_eq!(put(a, &format!("{path}?handler"), HANDLER.to_vec()).await.0, StatusCode::OK);
    // B has the handler on its disk and compiled in its runtime.
    assert_eq!(status_with(b, &runtime, "/p/coded/api/echo").await, StatusCode::OK);
    assert_eq!(status_with(b, &runtime, "/p/coded/api/echo").await, StatusCode::OK);
    let (failed, said) = call(a, "remove_page", serde_json::json!({ "slug": "coded", "confirm": "coded" })).await;
    assert!(!failed, "{said}");
    assert_eq!(status_with(b, &runtime, "/p/coded/api/echo").await, StatusCode::NOT_FOUND, "B ran a removed app's handler");
    deploy(a, "coded", &[("index.html", "<title>Coded</title>second app")]).await;
    assert_eq!(status_with(b, &runtime, "/p/coded/api/echo").await, StatusCode::NOT_FOUND, "a new app ran the old one's handler");
    site.finish().await;
}

/// Putting a removal back brings its files, not the tokens it took out of
/// use: a deploy token minted for the app before still opens nothing.
async fn a_restored_app_gets_no_token_back(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(a, "minted", &[("index.html", "<title>Minted</title>first")]).await;
    let (failed, said) = call(a, "app_deploy_tokens", serde_json::json!({ "app": "minted", "action": "create", "label": "ci" })).await;
    assert!(!failed, "{said}");
    let token = said.split_whitespace().find(|word| word.starts_with("tsd_")).unwrap_or_else(|| panic!("{said}")).to_string();
    let deploy_with = |config: Arc<Config>| {
        let request = Request::builder()
            .method("PUT")
            .uri("/deploy/minted?bundle")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(bundle(&[("index.html", "<title>Minted</title>by token")])))
            .unwrap();
        async move { send(&config, request).await }
    };
    assert_eq!(deploy_with(b.clone()).await.0, StatusCode::OK);
    let (failed, said) = call(a, "remove_page", serde_json::json!({ "slug": "minted", "confirm": "minted" })).await;
    assert!(!failed, "{said}");
    let entry = entry_of(&said);
    let restoring = (b.clone(), entry);
    tokio::task::spawn_blocking(move || toolsite::platform::trash::restore(&restoring.0, &restoring.1)).await.unwrap().unwrap();
    assert!(get(a, "/p/minted/").await.1.contains("by token"), "the restore did not put the files back");
    for config in [a, b] {
        assert_eq!(deploy_with(config.clone()).await.0, StatusCode::UNAUTHORIZED, "a retired token opened a restored app");
    }
    site.finish().await;
}

/// A hide that lands while a deploy is under way holds: the deploy writes
/// files and its own flag, never the hidden one, on whichever runner.
async fn a_hide_during_a_deploy_holds(site: Site) {
    let (a, b) = (&site.a, &site.b);
    deploy(a, "racing", &[("index.html", "<title>Racing</title>v0"), ("app.js", "v0")]).await;
    for round in 0..3 {
        let path = format!("{}?bundle&spa", upload_path(a, "racing").await);
        let body = bundle(&[("index.html", &format!("<title>Racing</title>v{round}")), ("app.js", &format!("v{round}"))]);
        let hiding = call(b, "set_visibility", serde_json::json!({ "slug": "racing", "hidden": true }));
        let ((code, said), (failed, hid)) = tokio::join!(put(a, &path, body), hiding);
        assert_eq!(code, StatusCode::OK, "{said}");
        assert!(!failed, "{hid}");
        for config in [a, b] {
            for uri in ["/p/racing/", "/p/racing/app.js", "/p/racing/some/route"] {
                assert_eq!(status(config, uri).await, StatusCode::NOT_FOUND, "round {round}: {uri} of a hidden app was served");
            }
        }
        let (failed, said) = call(a, "set_visibility", serde_json::json!({ "slug": "racing", "hidden": false })).await;
        assert!(!failed, "{said}");
        assert_eq!(get(b, "/p/racing/app.js").await.1, format!("v{round}"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hidden_and_gated_pages_are_served_under_no_other_name() {
    hidden_and_gated_pages_under_other_names(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn hidden_and_gated_pages_are_served_under_no_other_name_on_postgres() {
    hidden_and_gated_pages_under_other_names(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn icons_are_served_inert() {
    icons_are_served_inert_from_wherever_they_live(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn icons_are_served_inert_on_postgres() {
    icons_are_served_inert_from_wherever_they_live(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_strangers_misses_leave_nothing_on_a_runners_disk() {
    misses_leave_nothing_on_a_runners_disk(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_strangers_misses_leave_nothing_on_a_runners_disk_on_postgres() {
    misses_leave_nothing_on_a_runners_disk(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bundle_the_store_cannot_hold_is_refused_whole() {
    bundles_the_store_cannot_hold(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_bundle_the_store_cannot_hold_is_refused_whole_on_postgres() {
    bundles_the_store_cannot_hold(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_apps_handler_runs_on_no_runner() {
    a_removed_apps_handler_runs_nowhere(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_removed_apps_handler_runs_on_no_runner_on_postgres() {
    a_removed_apps_handler_runs_nowhere(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_app_gets_no_retired_token_back() {
    a_restored_app_gets_no_token_back(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_restored_app_gets_no_retired_token_back_on_postgres() {
    a_restored_app_gets_no_token_back(Site::on(true).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hide_landing_during_a_deploy_holds() {
    a_hide_during_a_deploy_holds(Site::on(common::wants_postgres()).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL and TOOLSITE_TEST_S3_*; scripts/test-postgres.sh starts both"]
async fn a_hide_landing_during_a_deploy_holds_on_postgres() {
    a_hide_during_a_deploy_holds(Site::on(true).await).await;
}
