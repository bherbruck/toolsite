//! The ticket store on both backends: one conformance suite, run on memory
//! always and on Postgres when TOOLSITE_TEST_DATABASE_URL is set
//! (`scripts/test-postgres.sh`). Then a subdomain handoff begun on one
//! router and finished on another that shares only the database, which is
//! the reason tickets left `Config`.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use toolsite::{
    build_router,
    content::origins::AppsDomain,
    runtime::wasm::Runtime,
    state::{
        pg,
        tickets::{digest, Kind, Tickets},
        Backend, Stores,
    },
    Config,
};
use tower::ServiceExt;

mod common;

const NEEDS: &str = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one";
const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct Pass {
    app: String,
    /// Stands for the credential a payload may carry: a session token.
    token: String,
}

fn pass(app: &str) -> Pass {
    Pass { app: app.to_string(), token: format!("session-{}", toolsite::content::slug::random_token(32)) }
}

const MINUTE: Duration = Duration::from_secs(60);

// --- the conformance suite ----------------------------------------------------

async fn round_trip_and_reuse(tickets: &Tickets) {
    let issued = pass("orders");
    let id = tickets.put(Kind::Upload, MINUTE, &issued).await.unwrap();
    assert!(id.len() >= 32, "an id short enough to guess: {id}");
    // A reusable ticket reads the same however often it is read.
    for _ in 0..3 {
        assert_eq!(tickets.get::<Pass>(Kind::Upload, &id).await.unwrap(), Some(issued.clone()));
    }
    assert_eq!(tickets.get::<Pass>(Kind::Upload, "made-up").await.unwrap(), None);
    // Another kind with the same id finds nothing.
    assert_eq!(tickets.get::<Pass>(Kind::SettingsLink, &id).await.unwrap(), None);
    assert_eq!(tickets.take::<Pass>(Kind::Handoff, &id).await.unwrap(), None);
    assert_eq!(tickets.get::<Pass>(Kind::Upload, &id).await.unwrap(), Some(issued));
}

async fn a_single_use_ticket_is_spent_once(tickets: &Tickets) {
    let issued = pass("orders");
    let id = tickets.put(Kind::Handoff, MINUTE, &issued).await.unwrap();
    assert_eq!(tickets.take::<Pass>(Kind::Handoff, &id).await.unwrap(), Some(issued));
    assert_eq!(tickets.take::<Pass>(Kind::Handoff, &id).await.unwrap(), None, "spent twice");
    assert_eq!(tickets.get::<Pass>(Kind::Handoff, &id).await.unwrap(), None, "read after it was spent");
}

/// Many redeemers at once, released together: exactly one wins.
async fn two_redeems_at_once_one_succeeds(tickets: &Tickets) {
    for round in 0..5 {
        let id = tickets.put(Kind::BlobUpload, MINUTE, &pass("orders")).await.unwrap();
        let start = Arc::new(tokio::sync::Barrier::new(8));
        let mut racers = Vec::new();
        for _ in 0..8 {
            let (tickets, id, start) = (tickets.clone(), id.clone(), start.clone());
            racers.push(tokio::spawn(async move {
                start.wait().await;
                tickets.take::<Pass>(Kind::BlobUpload, &id).await.unwrap()
            }));
        }
        let mut won = 0;
        for racer in racers {
            won += racer.await.unwrap().is_some() as usize;
        }
        assert_eq!(won, 1, "round {round}: {won} redeemers spent one ticket");
    }
}

/// Edits at once all land: an inline upload's chunks arrive in parallel.
async fn concurrent_updates_all_land(tickets: &Tickets) {
    let id = tickets.put(Kind::InlineUpload, MINUTE, &BTreeMap::<u32, u64>::new()).await.unwrap();
    let start = Arc::new(tokio::sync::Barrier::new(12));
    let mut writers = Vec::new();
    for index in 0..12u32 {
        let (tickets, id, start) = (tickets.clone(), id.clone(), start.clone());
        writers.push(tokio::spawn(async move {
            start.wait().await;
            tickets
                .update(Kind::InlineUpload, &id, move |chunks: &mut BTreeMap<u32, u64>| {
                    chunks.insert(index, u64::from(index) * 10);
                    Ok(chunks.len())
                })
                .await
                .unwrap()
                .expect("the upload vanished")
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    let chunks = tickets.get::<BTreeMap<u32, u64>>(Kind::InlineUpload, &id).await.unwrap().unwrap();
    assert_eq!(chunks.len(), 12, "an edit was lost: {chunks:?}");

    // A refused edit leaves the ticket as it was; a missing one is None.
    let refused = tickets
        .update(Kind::InlineUpload, &id, |chunks: &mut BTreeMap<u32, u64>| {
            chunks.clear();
            Err::<(), _>("over the ceiling".to_string())
        })
        .await;
    assert_eq!(refused, Err("over the ceiling".to_string()));
    assert_eq!(tickets.get::<BTreeMap<u32, u64>>(Kind::InlineUpload, &id).await.unwrap().unwrap().len(), 12);
    let missing = tickets.update(Kind::InlineUpload, "made-up", |_: &mut BTreeMap<u32, u64>| Ok(())).await;
    assert_eq!(missing, Ok(None));
}

async fn an_expired_ticket_opens_nothing_and_is_swept(tickets: &Tickets) {
    // Spent before it was ever good.
    let id = tickets.put(Kind::Upload, Duration::ZERO, &pass("orders")).await.unwrap();
    assert_eq!(tickets.get::<Pass>(Kind::Upload, &id).await.unwrap(), None);
    assert_eq!(tickets.take::<Pass>(Kind::Upload, &id).await.unwrap(), None);
    assert_eq!(tickets.update(Kind::Upload, &id, |_: &mut Pass| Ok(())).await, Ok(None));

    // Good, then out of time.
    let id = tickets.put(Kind::Preview, MINUTE, &pass("orders")).await.unwrap();
    let live = tickets.live(Kind::Preview).await.unwrap();
    assert!(tickets.expire(Kind::Preview, &id).await.unwrap());
    assert_eq!(tickets.take::<Pass>(Kind::Preview, &id).await.unwrap(), None, "an expired ticket was spent");
    assert_eq!(tickets.live(Kind::Preview).await.unwrap(), live - 1);

    // The next ticket minted sweeps it from the store altogether.
    tickets.put(Kind::Preview, MINUTE, &pass("orders")).await.unwrap();
    let held = digest(Kind::Preview, &id);
    assert!(!tickets.rows().await.unwrap().iter().any(|row| row.digest == held), "an expired ticket stayed");

    // A real clock, too: a one-second ticket is gone two seconds later.
    let id = tickets.put(Kind::Upload, Duration::from_secs(1), &pass("orders")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(tickets.get::<Pass>(Kind::Upload, &id).await.unwrap(), None);
}

/// What a dump shows: no id, in any form, and no token from any payload.
fn assert_nothing_plain(dump: &[u8], ids: &[String], secrets: &[String]) {
    let text = String::from_utf8_lossy(dump).to_lowercase();
    for needle in ids.iter().chain(secrets) {
        let hex = data_encoding::HEXLOWER.encode(needle.as_bytes());
        assert!(!text.contains(&needle.to_lowercase()), "{needle} is in the store as it is");
        assert!(!text.contains(&hex), "{needle} is in the store as hex");
    }
}

async fn nothing_plain_at_rest(tickets: &Tickets) -> (Vec<String>, Vec<String>) {
    let mut ids = Vec::new();
    let mut secrets = Vec::new();
    for kind in [Kind::Upload, Kind::SettingsLink, Kind::InlineUpload, Kind::BlobUpload, Kind::Login, Kind::Preview, Kind::Handoff] {
        let issued = pass("orders");
        secrets.push(issued.token.clone());
        ids.push(tickets.put(kind, MINUTE, &issued).await.unwrap());
    }
    let rows = tickets.rows().await.unwrap();
    let mut dump = Vec::new();
    for row in &rows {
        dump.extend_from_slice(row.kind.as_bytes());
        dump.extend_from_slice(row.digest.as_bytes());
        dump.extend_from_slice(&row.sealed);
        dump.extend_from_slice(data_encoding::HEXLOWER.encode(&row.sealed).as_bytes());
    }
    assert!(rows.len() >= ids.len());
    assert_nothing_plain(&dump, &ids, &secrets);
    // And each one still opens for whoever holds its id.
    assert_eq!(tickets.take::<Pass>(Kind::Handoff, &ids[6]).await.unwrap().map(|p| p.token), Some(secrets[6].clone()));
    (ids, secrets)
}

async fn hostile_ids_are_only_ever_digests(tickets: &Tickets) {
    let long = "x".repeat(64 * 1024);
    for hostile in ["'); drop table state.tickets; --", "\0", "../../.site/auth.db", "", "ｏｒｄｅｒｓ", long.as_str()] {
        assert_eq!(tickets.get::<Pass>(Kind::Upload, hostile).await.unwrap(), None);
        assert_eq!(tickets.take::<Pass>(Kind::Handoff, hostile).await.unwrap(), None);
    }
    // Payloads with the same come back unchanged.
    let odd = Pass { app: "'); drop table state.tickets; --\0ｏｒｄｅｒｓ".into(), token: long.clone() };
    let id = tickets.put(Kind::Upload, MINUTE, &odd).await.unwrap();
    assert_eq!(tickets.get::<Pass>(Kind::Upload, &id).await.unwrap(), Some(odd));
}

async fn conformance(tickets: &Tickets) {
    round_trip_and_reuse(tickets).await;
    a_single_use_ticket_is_spent_once(tickets).await;
    two_redeems_at_once_one_succeeds(tickets).await;
    concurrent_updates_all_land(tickets).await;
    an_expired_ticket_opens_nothing_and_is_swept(tickets).await;
    nothing_plain_at_rest(tickets).await;
    hostile_ids_are_only_ever_digests(tickets).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tickets_keep_their_rules_in_memory() {
    conformance(&Tickets::memory()).await;
}

// --- Postgres -----------------------------------------------------------------

/// A database for one test: its URL, and its name for `drop_database`.
async fn fresh_database() -> (String, String) {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
    let name = format!("t_{}", toolsite::content::slug::random_token(12));
    let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("create database {name}")).await.unwrap();
    let mut url = url::Url::parse(&server).unwrap();
    url.set_path(&name);
    (url.to_string(), name)
}

async fn drop_database(name: &str) {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
    let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("drop database if exists {name} with (force)")).await.unwrap();
}

/// One runner's connection to the shared database, migrated.
async fn runner(url: &str) -> Arc<pg::Postgres> {
    let postgres = pg::connect(url, 4).await.unwrap();
    pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap();
    Arc::new(postgres)
}

fn site_key() -> [u8; 32] {
    toolsite::seal::parse_key(KEY).unwrap()
}

/// The whole table as Postgres prints it, bytea in hex.
async fn table_text(postgres: &pg::Postgres) -> Vec<u8> {
    let client = postgres.pool.get().await.unwrap();
    let rows = client.query("select t::text from state.tickets t", &[]).await.unwrap();
    rows.iter().flat_map(|row| row.get::<_, String>(0).into_bytes()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn tickets_keep_their_rules_on_postgres() {
    let (url, name) = fresh_database().await;
    let postgres = runner(&url).await;
    let tickets = Tickets::postgres(postgres.pool.clone(), &site_key());
    conformance(&tickets).await;

    // The table itself, as a dump would read it.
    let (ids, secrets) = nothing_plain_at_rest(&tickets).await;
    assert_nothing_plain(&table_text(&postgres).await, &ids, &secrets);
    drop(tickets);
    drop(postgres);
    drop_database(&name).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_ticket_minted_on_one_runner_is_spent_once_across_runners_on_postgres() {
    let (url, name) = fresh_database().await;
    let (a, b) = (runner(&url).await, runner(&url).await);
    let on_a = Tickets::postgres(a.pool.clone(), &site_key());
    let on_b = Tickets::postgres(b.pool.clone(), &site_key());

    let issued = pass("orders");
    let id = on_a.put(Kind::Upload, MINUTE, &issued).await.unwrap();
    assert_eq!(on_b.get::<Pass>(Kind::Upload, &id).await.unwrap(), Some(issued));

    // Both runners redeem one code at the same moment: one wins.
    for _ in 0..5 {
        let id = on_a.put(Kind::Handoff, MINUTE, &pass("orders")).await.unwrap();
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let race = |tickets: Tickets| {
            let (id, start) = (id.clone(), start.clone());
            tokio::spawn(async move {
                start.wait().await;
                tickets.take::<Pass>(Kind::Handoff, &id).await.unwrap().is_some()
            })
        };
        let (first, second) = (race(on_a.clone()), race(on_b.clone()));
        let won = first.await.unwrap() as u8 + second.await.unwrap() as u8;
        assert_eq!(won, 1, "two runners both spent one code");
    }

    // A runner with another key opens nothing another sealed.
    let id = on_a.put(Kind::Upload, MINUTE, &pass("orders")).await.unwrap();
    let stranger = Tickets::postgres(b.pool.clone(), &[9u8; 32]);
    assert!(stranger.get::<Pass>(Kind::Upload, &id).await.is_err());
    drop((on_a, on_b, stranger, a, b));
    drop_database(&name).await;
}

// --- a handoff across two routers ----------------------------------------------

const BASE: &str = "https://site.test";
const MAIN: &str = "site.test";
const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

/// One runner of a subdomain-mode site: its own process state, sharing the
/// volume and the database with every other runner.
fn runner_config(data_dir: &std::path::Path, stores: Stores, bucket: &toolsite::runtime::blobs::S3) -> Arc<Config> {
    Arc::new(Config {
        base_url: Some(BASE.to_string()),
        apps: Some(AppsDomain::parse("apps.test", Some(BASE), None).unwrap()),
        stores,
        blobs: common::blobs_in(bucket),
        ..Config::local(data_dir.to_path_buf(), "test-token")
    })
}

struct Reply {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == "location")
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no Location: {} {}", self.status, self.body))
    }
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .find_map(|(_, c)| c.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap_or("").to_string()))
    }
}

async fn get(config: &Arc<Config>, host: &str, uri: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::builder().method("GET").uri(uri).header("host", host);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let response = build_router(config.clone(), Runtime::new().unwrap())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await.unwrap();
    Reply { status, headers, body: String::from_utf8_lossy(&bytes).to_string() }
}

/// A gated app, written to the shared volume.
fn app(config: &Config, name: &str) {
    common::publish(config, &format!("{name}/index.html"), format!("<title>{name}</title><h1>{name} home</h1>"));
    common::publish(config, &format!("{name}/handler.wasm"), HANDLER);
    let mut meta = toolsite::content::catalog::meta_blocking(config, name);
    meta.gate = Some("authenticated".to_string());
    toolsite::content::catalog::update_meta_blocking(config, name, { let meta = meta.clone(); move |stored| { *stored = meta; Ok(()) } }).unwrap();
}

/// The first two legs of a handoff on `begin`: the app host sends the
/// browser to the main host, which mints a code. Returns the app host, the
/// browser's handoff nonce and the landing path carrying the code.
async fn begin_handoff(begin: &Arc<Config>, app: &str, site_token: &str) -> (String, String, String) {
    let host = format!("{}.apps.test", toolsite::content::origins::label_for(begin, app));
    let start = get(begin, &host, &format!("/p/{app}/"), None).await;
    assert_eq!(start.status, StatusCode::SEE_OTHER, "{}", start.body);
    let state = start.cookie("__Host-ts_handoff").expect("no handoff cookie");
    let handoff = start.location().strip_prefix(BASE).expect("the handoff is on the main host").to_string();
    let back = get(begin, MAIN, &handoff, Some(&format!("__Host-ts_session={site_token}"))).await;
    assert_eq!(back.status, StatusCode::SEE_OTHER, "{}", back.body);
    let landing = back.location().strip_prefix(&format!("https://{host}")).expect("the code went elsewhere").to_string();
    (host, state, landing)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_handoff_begun_on_one_router_finishes_on_another_on_postgres() {
    let (url, name) = fresh_database().await;
    let volume = tempfile::tempdir().unwrap();
    let (pg_a, pg_b) = (runner(&url).await, runner(&url).await);
    let stores = |postgres: &Arc<pg::Postgres>| {
        Stores::new(Backend::Postgres(postgres.clone()), None, Some(KEY)).unwrap()
    };
    let bucket = common::bucket().await;
    let a = runner_config(volume.path(), stores(&pg_a), &bucket);
    let b = runner_config(volume.path(), stores(&pg_b), &bucket);

    app(&a, "members");
    // Accounts live in Postgres here too, and an account call blocks its
    // thread, as it does under spawn_blocking in the server.
    let accounts = a.clone();
    let (_, site_token) = tokio::task::spawn_blocking(move || {
        toolsite::accounts::users::sign_up(&accounts, "reader@example.com", "correct horse battery").unwrap();
        toolsite::accounts::users::log_in(&accounts, "reader@example.com", "correct horse battery").unwrap()
    })
    .await
    .unwrap();

    // The main host is runner A: it mints the code.
    let (host, state, landing) = begin_handoff(&a, "members", &site_token).await;
    let code = landing.split("code=").nth(1).unwrap().to_string();
    let at_rest = table_text(&pg_a).await;

    // The app host is runner B: it trades the code for the app's cookie.
    let landed = get(&b, &host, &landing, Some(&format!("__Host-ts_handoff={state}"))).await;
    assert_eq!(landed.status, StatusCode::SEE_OTHER, "runner B could not finish A's handoff: {}", landed.body);
    let session = landed.cookie("__Host-ts_app").expect("no app cookie");
    let page = get(&b, &host, "/p/members/", Some(&format!("__Host-ts_app={session}"))).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("members home"));

    // While it waited, the table held neither the code nor the session.
    assert!(!at_rest.is_empty(), "the code was not in the shared table");
    assert_nothing_plain(&at_rest, &[code], &[session]);

    // Spent on B, it is spent on A too.
    let again = get(&a, &host, &landing, Some(&format!("__Host-ts_handoff={state}"))).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST, "a code was used twice across runners");

    // Control: with each runner's tickets in its own memory, as in file
    // mode, B has never heard of A's code. This is what the shared store fixes.
    let lone_a = runner_config(volume.path(), Stores { tickets: Tickets::memory(), ..stores(&pg_a) }, &bucket);
    let lone_b = runner_config(volume.path(), Stores { tickets: Tickets::memory(), ..stores(&pg_b) }, &bucket);
    let (host, state, landing) = begin_handoff(&lone_a, "members", &site_token).await;
    let landed = get(&lone_b, &host, &landing, Some(&format!("__Host-ts_handoff={state}"))).await;
    assert_eq!(landed.status, StatusCode::BAD_REQUEST);

    drop((a, b, lone_a, lone_b, pg_a, pg_b));
    drop_database(&name).await;
}
