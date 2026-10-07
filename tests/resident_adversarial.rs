//! Resident mode under attack. A resident instance lives for as long as
//! the app runs, so what one person's event leaves in it is there for the
//! next person's, and whatever it holds, a thread, memory, a place in the
//! queue, it holds for a long time. Each test is an attempt by a hostile app
//! author, a visitor to the app or a visitor to another app, and names the
//! property that defeats it.

use futures_util::{SinkExt, StreamExt};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, Message};
use toolsite::{
    accounts::users::{self, Scope},
    build_router,
    content::store::{self, PageMeta, ResidentMeta},
    platform::upload::UploadTicket,
    runtime::{
        resident::{Residents, Status},
        wasm::{ConnectionEvent, ConnectionMessage, Guards, Runtime},
    },
    Config,
};

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");
/// A handler built for the plain `app` world: no on-connection.
const LEGACY: &[u8] = include_bytes!("fixtures/legacy-handler.wasm");

struct Site {
    _dir: TempDir,
    config: Arc<Config>,
    runtime: Arc<Runtime>,
    addr: std::net::SocketAddr,
}

async fn site() -> Site {
    site_with(|_| {}).await
}

/// A site whose resident limits `change` sets.
async fn site_with(change: impl FnOnce(&mut Residents)) -> Site {
    site_at(None, change).await
}

/// A site at `base_url`, which is what turns on sign-in for MCP clients.
async fn site_at(base_url: Option<&str>, change: impl FnOnce(&mut Residents)) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::local(dir.path().to_path_buf(), "test-token");
    config.base_url = base_url.map(str::to_string);
    let mut residents = Residents::default();
    change(&mut residents);
    config.residents = Arc::new(residents);
    let config = Arc::new(config);
    let runtime = Runtime::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(config.clone(), runtime.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Site { _dir: dir, config, runtime, addr }
}

fn edit_meta(config: &Config, name: &str, change: impl FnOnce(&mut PageMeta)) {
    let mut meta = store::read_meta_blocking(config, name);
    change(&mut meta);
    store::write_meta_blocking(config, name, &meta).unwrap();
}

fn app_with(config: &Config, name: &str, wasm: &[u8], resident: Option<ResidentMeta>) {
    let dir = config.data_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("handler.wasm"), wasm).unwrap();
    std::fs::write(dir.join("index.html"), "<title>app</title>").unwrap();
    edit_meta(config, name, |meta| {
        meta.sockets = vec!["/ws".to_string()];
        meta.resident = resident;
    });
}

fn resident(config: &Config, name: &str) {
    resident_with(config, name, None, None);
}

fn resident_with(config: &Config, name: &str, memory_mb: Option<u64>, tick_ms: Option<u64>) {
    app_with(config, name, HANDLER, Some(ResidentMeta { memory_mb, tick_ms }));
}

fn status(site: &Site, app: &str) -> Status {
    site.config.residents.status(app).unwrap_or_default()
}

/// A signed-in person's cookie for one app, as the handoff would set it.
fn app_cookie(config: &Config, email: &str, app: &str) -> (String, String) {
    if users::user_by_email(config, email).is_none() {
        users::sign_up(config, email, "correct horse battery").unwrap();
    }
    let (user, site_token) = users::log_in(config, email, "correct horse battery").unwrap();
    let (_, token, _) = users::create_app_session(config, &site_token, app).unwrap();
    (user.id, format!("ts_app_{app}={token}"))
}

async fn upload(site: &Site, app: &str, kind: &str, body: Vec<u8>) -> (u16, String) {
    let ticket = toolsite::content::slug::random_token(24);
    site.config.uploads.lock().unwrap().insert(
        ticket.clone(),
        UploadTicket {
            slug: app.to_string(),
            expires_at: Instant::now() + Duration::from_secs(60),
            user: None,
            project: None,
        },
    );
    let response = reqwest::Client::new()
        .put(format!("http://{}/upload/{ticket}?{kind}", site.addr))
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(site: &Site, path: &str, cookie: Option<&str>) -> Result<Socket, u16> {
    let mut request = format!("ws://{}{path}", site.addr).into_client_request().unwrap();
    if let Some(cookie) = cookie {
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("connect failed: {other}"),
    }
}

/// Connects as `cookie`'s person, or nobody, and reads the `id:<conn>` the
/// fixture sends on connect.
async fn open_as(site: &Site, app: &str, cookie: Option<&str>) -> (Socket, String) {
    let mut socket = connect(site, &format!("/p/{app}/ws"), cookie)
        .await
        .unwrap_or_else(|status| panic!("{app} refused with {status}"));
    let hello = next_text(&mut socket).await.expect("no id after connect");
    let id = hello.strip_prefix("id:").expect("the first frame names the connection").to_string();
    (socket, id)
}

async fn open(site: &Site, app: &str) -> Socket {
    open_as(site, app, None).await.0
}

/// Opens a connection once a pause after a failure is over.
async fn open_after_pause(site: &Site, app: &str, within: Duration) -> Socket {
    let deadline = Instant::now() + within;
    loop {
        match connect(site, &format!("/p/{app}/ws"), None).await {
            Ok(mut socket) => {
                assert!(next_text(&mut socket).await.is_some_and(|hello| hello.starts_with("id:")));
                return socket;
            }
            Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(status) => panic!("{app} still refused with {status}"),
        }
    }
}

async fn say(socket: &mut Socket, text: &str) {
    socket.send(Message::Text(text.to_string().into())).await.unwrap();
}

async fn next_text_within(socket: &mut Socket, within: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(Message::Text(text)))) => return Some(text.to_string()),
            _ => return None,
        }
    }
}

async fn next_text(socket: &mut Socket) -> Option<String> {
    next_text_within(socket, Duration::from_millis(1500)).await
}

async fn ask(socket: &mut Socket, text: &str) -> Option<String> {
    say(socket, text).await;
    next_text(socket).await
}

async fn closes_within(socket: &mut Socket, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return true,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// Waits up to `within` for the socket to close, and says what arrived
/// before it did, if anything: a word from a guest that should have been
/// stopped first.
async fn closes_silently_within(socket: &mut Socket, within: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return Err("still open".to_string()),
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return Ok(()),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(message))) => return Err(format!("the guest said {message:?}")),
        }
    }
}

async fn closes(socket: &mut Socket) -> bool {
    closes_within(socket, Duration::from_secs(3)).await
}

/// Waits up to `within` for `done`.
async fn eventually(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// A resident app with a `notes` table and a policy that shows each person
/// their own rows: `my_notes`. Alice holds one row, Bob two.
async fn vault(site: &Site, tick_ms: Option<u64>) -> (String, String) {
    let config = &site.config;
    resident_with(config, "vault", None, tick_ms);
    toolsite::runtime::db::run(config, "vault", "create table notes (owner text, body text)", &[]).unwrap();
    let mut manifest = "[[socket]]\npath = \"/ws\"\n\n[resident]\nenabled = true\n".to_string();
    if let Some(tick) = tick_ms {
        manifest.push_str(&format!("tick_ms = {tick}\n"));
    }
    manifest.push_str("\n[[access.table]]\ntable = \"notes\"\nwhere = \"owner = current_user()\"\n");
    toolsite::platform::manifest::apply(config, "vault", &manifest).await.unwrap();
    let (alice, alice_cookie) = app_cookie(config, "alice@x.test", "vault");
    let (bob, _) = app_cookie(config, "bob@x.test", "vault");
    users::grant(config, "alice@x.test", "vault", "editor").unwrap();
    for (owner, body) in [(&alice, "a1"), (&bob, "b1"), (&bob, "b2")] {
        toolsite::runtime::db::run(
            config,
            "vault",
            "insert into notes values (?, ?)",
            &[serde_json::json!(owner), serde_json::json!(body)],
        )
        .unwrap();
    }
    (alice, alice_cookie)
}

// --- identity -----------------------------------------------------------------

#[tokio::test]
async fn every_resident_event_runs_as_its_own_connection_s_person_even_when_people_interleave() {
    let site = site().await;
    let (alice_id, alice_cookie) = vault(&site, None).await;
    let (bob_id, bob_cookie) = app_cookie(&site.config, "bob@x.test", "vault");
    let (mut alice, _) = open_as(&site, "vault", Some(&alice_cookie)).await;
    let (mut bob, _) = open_as(&site, "vault", Some(&bob_cookie)).await;
    let (mut nobody, _) = open_as(&site, "vault", None).await;

    for round in 0..5 {
        assert_eq!(ask(&mut alice, "who").await.as_deref(), Some("who:alice@x.test:editor"), "round {round}");
        assert_eq!(ask(&mut bob, "who").await.as_deref(), Some("who:bob@x.test:none"), "round {round}");
        assert_eq!(ask(&mut nobody, "who").await.as_deref(), Some("who:anonymous:none"), "round {round}");

        let count = "scoped:select count(*) from my_notes";
        assert_eq!(ask(&mut alice, count).await.as_deref(), Some("rows:1"), "round {round}");
        assert_eq!(ask(&mut nobody, count).await.as_deref(), Some("rows:0"), "nobody saw a row, round {round}");
        assert_eq!(ask(&mut bob, count).await.as_deref(), Some("rows:2"), "round {round}");

        // The full database's identity functions, bound per call too.
        let me = "sql:select current_user()";
        assert_eq!(ask(&mut bob, me).await, Some(format!("rows:{bob_id}")), "round {round}");
        assert_eq!(ask(&mut alice, me).await, Some(format!("rows:{alice_id}")), "round {round}");
        assert_eq!(ask(&mut nobody, me).await.as_deref(), Some("rows:null"), "round {round}");
    }
    // A temp table one person's event made is not there for the next.
    assert_eq!(ask(&mut alice, "sql:create temp table stash (x)").await.as_deref(), Some("rows:"));
    let leaked = ask(&mut bob, "sql:select * from stash").await.unwrap();
    assert!(leaked.contains("no such table"), "a temp table outlived its call: {leaked}");
}

#[tokio::test]
async fn on_tick_runs_as_nobody_and_a_scoped_query_there_sees_no_one_s_rows() {
    let site = site().await;
    let (_, alice_cookie) = vault(&site, Some(100)).await;
    let (mut alice, _) = open_as(&site, "vault", Some(&alice_cookie)).await;
    assert_eq!(ask(&mut alice, "tick-sql:select count(*) from my_notes").await.as_deref(), Some("ok"));
    for _ in 0..5 {
        // Alice's event, then a tick: the tick must not be Alice.
        assert_eq!(ask(&mut alice, "who").await.as_deref(), Some("who:alice@x.test:editor"));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(ask(&mut alice, "tick-saw").await.as_deref(), Some("tick:anonymous:none rows:0"));
    }
}

#[tokio::test]
async fn a_refused_connect_leaves_its_person_behind_for_no_later_event() {
    let site = site().await;
    let (_, alice_cookie) = vault(&site, Some(100)).await;
    let (mut nobody, _) = open_as(&site, "vault", None).await;
    assert_eq!(ask(&mut nobody, "tick-sql:select count(*) from my_notes").await.as_deref(), Some("ok"));
    // Alice's connect runs and is refused by the app: her event ended
    // without a trap, so the instance carries on.
    assert!(connect(&site, "/p/vault/ws?refuse=1", Some(&alice_cookie)).await.is_err());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(ask(&mut nobody, "tick-saw").await.as_deref(), Some("tick:anonymous:none rows:0"));
    assert_eq!(ask(&mut nobody, "who").await.as_deref(), Some("who:anonymous:none"));
    assert_eq!(ask(&mut nobody, "scoped:select count(*) from my_notes").await.as_deref(), Some("rows:0"));
}

#[tokio::test]
async fn a_resident_instance_reads_its_own_app_s_secrets_only() {
    let site = site().await;
    resident(&site.config, "keeper");
    resident(&site.config, "snoop");
    toolsite::platform::secrets::set(&site.config, "keeper", "API_KEY", Some("hunter2")).unwrap();
    let mut keeper = open(&site, "keeper").await;
    let mut snoop = open(&site, "snoop").await;
    assert_eq!(ask(&mut keeper, "secret").await.as_deref(), Some("secret:true"));
    assert_eq!(ask(&mut snoop, "secret").await.as_deref(), Some("secret:false"));
}

// --- other apps ---------------------------------------------------------------

#[tokio::test]
async fn a_resident_handler_cannot_reach_another_app_s_connection_by_its_id() {
    let site = site().await;
    resident(&site.config, "attacker");
    resident(&site.config, "victim");
    let (mut victim, victim_id) = open_as(&site, "victim", None).await;
    let mut attacker = open(&site, "attacker").await;
    assert_eq!(
        ask(&mut attacker, &format!("poke:{victim_id}")).await.as_deref(),
        Some("send=false state=false set=false subscribe=false close=false")
    );
    assert_eq!(ask(&mut victim, "log").await.as_deref(), Some("connect anonymous /ws,message"), "the victim was touched");
    assert_eq!(ask(&mut victim, "count").await.as_deref(), Some("1"), "the victim shares the attacker's memory");
}

#[tokio::test]
async fn an_event_for_a_connection_the_instance_never_saw_is_refused() {
    let site = site().await;
    resident(&site.config, "stale");
    let mut socket = open(&site, "stale").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
    // A message from a connection of an earlier instance, or of another
    // app, naming an id this one never connected.
    let load: toolsite::runtime::resident::Loader = |config, app| std::fs::read(config.data_dir.join(app).join("handler.wasm")).ok();
    let settings = site.config.residents.settings(None, None);
    let answer = site
        .config
        .residents
        .deliver(
            &site.runtime,
            &site.config,
            load,
            "stale",
            settings,
            None,
            "not-a-connection-of-this-instance",
            ConnectionEvent::Message(ConnectionMessage::Text("count".to_string())),
            Guards::default(),
        )
        .await;
    assert!(answer.is_err(), "an unknown connection's event ran: {answer:?}");
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("2"), "the forged event reached the handler");
}

#[tokio::test]
async fn the_instance_s_status_is_shown_only_to_those_who_manage_the_app() {
    let site = site_at(Some("https://site.test"), |_| {}).await;
    resident(&site.config, "watched");
    let mut socket = open(&site, "watched").await;
    say(&mut socket, "crash").await;
    assert!(closes(&mut socket).await);
    for (email, scope) in [("viewer@x.test", Some(Scope::Viewer)), ("stranger@x.test", None), ("owner@x.test", Some(Scope::Admin))] {
        users::sign_up(&site.config, email, "correct horse battery").unwrap();
        if let Some(scope) = scope {
            users::grant_scope(&site.config, email, "watched", scope, None).unwrap();
        }
    }

    let fetch = async |email: &str| -> serde_json::Value {
        let token = me_token(&site.config, email);
        me_tool(&site, &token, "fetch", serde_json::json!({"id": "watched"})).await
    };
    for email in ["viewer@x.test", "stranger@x.test"] {
        let result = fetch(email).await;
        let metadata = &result["structuredContent"]["metadata"];
        assert!(metadata.is_object(), "{email} could not fetch the public app at all: {result}");
        assert!(metadata["resident"].is_null(), "{email} saw the resident instance: {metadata}");
    }
    let result = fetch("owner@x.test").await;
    let resident = &result["structuredContent"]["metadata"]["resident"];
    assert_eq!(resident["restarts"], 1, "the manager does not see the instance: {result}");
    assert!(resident["last_crash"].as_str().is_some_and(|why| why.contains("trapped")), "{resident}");
}

fn me_token(config: &Config, email: &str) -> String {
    let user = users::user_by_email(config, email).unwrap();
    let client = toolsite::platform::oauth_store::register_client(config, Some("t"), &["https://c.test/cb".into()]).unwrap();
    toolsite::platform::oauth_store::issue_tokens(config, &client.id, &user.id, None).unwrap().access_token
}

/// Calls one /me/mcp tool, initialising first, and returns its result.
async fn me_tool(site: &Site, token: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    let post = async |body: serde_json::Value| -> serde_json::Value {
        let response = reqwest::Client::new()
            .post(format!("http://{}/me/mcp", site.addr))
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"))
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .unwrap();
        let text = response.text().await.unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
            .next_back()
            .or_else(|| serde_json::from_str(&text).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    post(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).await;
    post(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}})).await["result"].clone()
}

// --- the host's time ------------------------------------------------------------

#[tokio::test]
async fn a_wasi_sleep_cannot_hold_a_resident_instance_past_its_wall_clock() {
    let site = site().await;
    resident(&site.config, "sleeper");
    let mut socket = open(&site, "sleeper").await;
    let wall_clock = Guards::default().wall_clock;
    let started = Instant::now();
    say(&mut socket, "sleep-forever").await;
    // The sleep wakes at the deadline and the call ends there: the guest
    // never runs on to answer, however far the epoch ticker lags.
    let closed = closes_silently_within(&mut socket, wall_clock * 2).await;
    assert_eq!(closed, Ok(()), "the sleep did not end the call at its deadline");
    assert!(started.elapsed() < wall_clock + Duration::from_secs(1), "stopped late: {:?}", started.elapsed());
    let crashed = status(&site, "sleeper");
    assert!(crashed.last_crash.as_deref().is_some_and(|why| why.contains("time limit")), "{crashed:?}");
    // A short sleep is still a sleep.
    let mut socket = open_after_pause(&site, "sleeper", Duration::from_secs(5)).await;
    let started = Instant::now();
    assert_eq!(ask(&mut socket, "nap:200").await.as_deref(), Some("awake"));
    assert!(started.elapsed() >= Duration::from_millis(200));
}

#[tokio::test]
async fn a_wasi_sleep_cannot_hold_a_fresh_event_past_its_wall_clock() {
    let site = site().await;
    app_with(&site.config, "plain-sleeper", HANDLER, None);
    let mut socket = open(&site, "plain-sleeper").await;
    let wall_clock = Guards::default().wall_clock;
    let started = Instant::now();
    say(&mut socket, "sleep-forever").await;
    let closed = closes_silently_within(&mut socket, wall_clock * 2).await;
    assert_eq!(closed, Ok(()), "the sleep did not end the event at its deadline");
    assert!(started.elapsed() < wall_clock + Duration::from_secs(1), "stopped late: {:?}", started.elapsed());
}

#[tokio::test]
async fn a_query_that_never_ends_is_stopped_at_the_wall_clock() {
    let site = site().await;
    resident(&site.config, "spinner");
    let mut socket = open(&site, "spinner").await;
    let started = Instant::now();
    say(&mut socket, "sql-spin").await;
    // The query is interrupted and the call ends, by the guest answering
    // or by the epoch stopping it: either way, within the wall clock.
    let ended = tokio::time::timeout(Duration::from_secs(10), socket.next()).await.is_ok();
    assert!(ended, "a query with no end held the instance");
    assert!(started.elapsed() < Duration::from_secs(8), "stopped late: {:?}", started.elapsed());
    let mut socket = open_after_pause(&site, "spinner", Duration::from_secs(5)).await;
    assert!(ask(&mut socket, "count").await.is_some());
}

#[tokio::test]
async fn recursion_without_end_traps_cleanly_and_the_instance_restarts() {
    let site = site().await;
    resident(&site.config, "deep");
    let mut socket = open(&site, "deep").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
    say(&mut socket, "recurse").await;
    assert!(closes(&mut socket).await, "a stack overflow did not stop the instance");
    let crashed = status(&site, "deep");
    assert!(crashed.last_crash.as_deref().is_some_and(|why| why.contains("trapped")), "{crashed:?}");
    let mut socket = open_after_pause(&site, "deep", Duration::from_secs(5)).await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn slow_ticks_are_coalesced_rather_than_queued_and_events_still_run_between_them() {
    let site = site().await;
    resident_with(&site.config, "laggard", None, Some(100));
    let mut socket = open(&site, "laggard").await;
    let ticks = async |socket: &mut Socket| -> u32 {
        let reply = ask(socket, "ticks").await.expect("an event waited behind the ticks");
        reply.strip_prefix("ticks:").unwrap().parse().unwrap()
    };
    assert_eq!(ask(&mut socket, "slow-ticks:300").await.as_deref(), Some("ok"));
    let before = ticks(&mut socket).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let during = ticks(&mut socket).await - before;
    // A 300 ms tick every 100 ms: at most one per 300 ms, never a backlog.
    assert!((2..=7).contains(&during), "{during} ticks in 1.5 s of 300 ms ticks");
    // And none are owed once they are fast again.
    assert_eq!(ask(&mut socket, "slow-ticks:0").await.as_deref(), Some("ok"));
    let before = ticks(&mut socket).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = ticks(&mut socket).await - before;
    assert!(after <= 8, "{after} ticks in 0.5 s at 100 ms: the missed ones were run");
}

// --- what the site spends -------------------------------------------------------

#[tokio::test]
async fn a_full_queue_refuses_the_event_rather_than_growing() {
    let site = site_with(|residents| residents.queue_depth = 1).await;
    resident(&site.config, "jammed");
    let mut busy = open(&site, "jammed").await;
    let mut waiting = open(&site, "jammed").await;
    let mut late = open(&site, "jammed").await;

    say(&mut busy, "nap:1500").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    say(&mut waiting, "count").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    say(&mut late, "count").await;
    assert!(closes(&mut late).await, "an event past the queue's depth was kept");
    assert_eq!(next_text_within(&mut busy, Duration::from_secs(3)).await.as_deref(), Some("awake"));
    assert_eq!(next_text_within(&mut waiting, Duration::from_secs(3)).await.as_deref(), Some("1"));
    assert_eq!(status(&site, "jammed").restarts, 0, "a full queue counted as a crash");
}

#[tokio::test]
async fn a_site_runs_no_more_resident_instances_than_it_allows() {
    let site = site_with(|residents| residents.max_instances = 2).await;
    for app in ["one", "two", "three"] {
        resident(&site.config, app);
    }
    let mut one = open(&site, "one").await;
    let _two = open(&site, "two").await;
    assert!(connect(&site, "/p/three/ws", None).await.is_err(), "a third instance started past the cap of 2");
    assert_eq!(site.config.residents.threads(), 2);

    // A crash loop takes no more threads than the instance it replaces.
    say(&mut one, "crash").await;
    assert!(closes(&mut one).await);
    let _one = open_after_pause(&site, "one", Duration::from_secs(5)).await;
    assert_eq!(site.config.residents.threads(), 2);

    // An instance stopped gives its place back.
    edit_meta(&site.config, "two", |meta| meta.hidden = true);
    assert!(eventually(Duration::from_secs(2), || site.config.residents.threads() == 1).await, "the stopped thread lingered");
    let mut three = open(&site, "three").await;
    assert_eq!(ask(&mut three, "count").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn the_memory_resident_instances_reserve_is_capped_across_the_site() {
    let site = site_with(|residents| residents.total_memory_mb = 48).await;
    resident_with(&site.config, "big", Some(32), None);
    resident_with(&site.config, "bigger", Some(32), None);
    resident_with(&site.config, "small", Some(16), None);
    let _big = open(&site, "big").await;
    assert!(connect(&site, "/p/bigger/ws", None).await.is_err(), "64 MB of caps were let in under 48");
    let mut small = open(&site, "small").await;
    assert_eq!(ask(&mut small, "count").await.as_deref(), Some("1"));
}

// --- restarts -------------------------------------------------------------------

#[tokio::test]
async fn republishing_after_a_crash_does_not_end_the_pause() {
    let site = site().await;
    resident(&site.config, "impatient");
    for _ in 0..3 {
        let mut socket = open_after_pause(&site, "impatient", Duration::from_secs(5)).await;
        say(&mut socket, "crash").await;
        assert!(closes(&mut socket).await);
    }
    // Four seconds of pause now. A republish is not a way around it: the
    // pause it ends at is the same after the republish as before.
    let paused_until = status(&site, "impatient").next_start_at.expect("a crash starts a pause");
    let (code, body) = upload(&site, "impatient", "handler", HANDLER.to_vec()).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(status(&site, "impatient").next_start_at, Some(paused_until), "republishing reset the pause");
    let refused = connect(&site, "/p/impatient/ws", None).await.is_err();
    // The status is whole seconds. Only a connect made a full second before
    // the pause ends must be refused; a slow machine that got there later
    // has still proved the pause survived, above.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    if now + 1 < paused_until {
        assert!(refused, "a connect during the pause was let in after a republish");
    }
    let mut socket = open_after_pause(&site, "impatient", Duration::from_secs(6)).await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn republishing_in_a_loop_never_leaves_more_than_one_thread_for_the_app() {
    let site = site().await;
    resident_with(&site.config, "churn", None, Some(60_000));
    for round in 0..6 {
        let mut socket = open(&site, "churn").await;
        assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"), "round {round}");
        let (code, body) = upload(&site, "churn", "handler", HANDLER.to_vec()).await;
        assert_eq!(code, 200, "{body}");
        assert!(closes(&mut socket).await, "round {round}");
    }
    // Each old thread ends at once, ticking or not: none waits out its tick.
    assert!(eventually(Duration::from_secs(2), || site.config.residents.threads() == 0).await, "{} threads", site.config.residents.threads());
}

#[tokio::test]
async fn a_republish_while_an_event_runs_lets_it_finish_then_drops_the_old_instance() {
    let site = site().await;
    resident(&site.config, "midflight");
    let mut socket = open(&site, "midflight").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
    say(&mut socket, "nap:800").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (code, body) = upload(&site, "midflight", "handler", HANDLER.to_vec()).await;
    assert_eq!(code, 200, "{body}");

    // The new instance starts while the old one still sleeps.
    let mut fresh = open(&site, "midflight").await;
    assert_eq!(ask(&mut fresh, "count").await.as_deref(), Some("1"), "the new instance has the old memory");
    assert_eq!(next_text_within(&mut socket, Duration::from_secs(3)).await.as_deref(), Some("awake"));
    assert!(closes(&mut socket).await, "a connection of the replaced instance stayed open");
    assert!(eventually(Duration::from_secs(2), || site.config.residents.threads() == 1).await, "the old thread lives on");
    assert_eq!(ask(&mut fresh, "count").await.as_deref(), Some("2"));
}

#[tokio::test]
async fn hiding_or_removing_the_app_ends_its_thread_and_lets_its_memory_go() {
    let site = site().await;
    resident_with(&site.config, "hidden", None, Some(60_000));
    resident_with(&site.config, "removed", None, Some(60_000));
    let mut hidden = open(&site, "hidden").await;
    let mut removed = open(&site, "removed").await;
    assert_eq!(ask(&mut hidden, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut removed, "count").await.as_deref(), Some("1"));
    assert_eq!(site.config.residents.threads(), 2);

    edit_meta(&site.config, "hidden", |meta| meta.hidden = true);
    toolsite::platform::trash::remove(&site.config, "removed", 1).unwrap();
    assert!(closes(&mut hidden).await && closes(&mut removed).await);
    assert!(eventually(Duration::from_secs(2), || site.config.residents.threads() == 0).await, "a thread outlived its app");
    for app in ["hidden", "removed"] {
        let now = status(&site, app);
        assert!(now.running_since.is_none() && now.memory_bytes == 0, "{app}: {now:?}");
    }
}

// --- handlers that cannot run resident ---------------------------------------------

#[tokio::test]
async fn a_handler_that_does_not_compile_fails_with_a_reason_and_no_instance() {
    let site = site().await;
    app_with(&site.config, "broken", b"\0asm not really", Some(ResidentMeta { memory_mb: None, tick_ms: None }));
    assert!(connect(&site, "/p/broken/ws", None).await.is_err());
    assert!(status(&site, "broken").running_since.is_none());
    assert_eq!(site.config.residents.threads(), 0, "a handler that cannot run holds a thread");
}

#[tokio::test]
async fn an_app_left_resident_with_a_handler_that_takes_no_connections_fails_safe() {
    // What a lost race between a manifest and a handler would leave.
    let site = site().await;
    app_with(&site.config, "mismatch", LEGACY, Some(ResidentMeta { memory_mb: None, tick_ms: None }));
    assert!(connect(&site, "/p/mismatch/ws", None).await.is_err());
    assert!(status(&site, "mismatch").running_since.is_none());
    assert_eq!(site.config.residents.threads(), 0, "a handler that takes no connections holds a thread");
    // Should an event reach the instance anyway, it is refused by name.
    let load: toolsite::runtime::resident::Loader = |config, app| std::fs::read(config.data_dir.join(app).join("handler.wasm")).ok();
    let connect_info = toolsite::runtime::wasm::ConnectInfo {
        socket: "/ws".to_string(),
        path: "/ws".to_string(),
        query: String::new(),
        headers: Vec::new(),
    };
    let settings = site.config.residents.settings(None, None);
    let answer = site
        .config
        .residents
        .deliver(&site.runtime, &site.config, load, "mismatch", settings, None, "c1", ConnectionEvent::Connect(connect_info), Guards::default())
        .await;
    assert!(answer.as_ref().is_err_and(|why| why.contains("on-connection")), "{answer:?}");
}

#[tokio::test]
async fn a_manifest_and_a_handler_uploaded_at_once_never_leave_a_resident_app_without_on_connection() {
    let site = site().await;
    let manifest = b"[[socket]]\npath = \"/ws\"\n\n[resident]\nenabled = true\n".to_vec();
    let plain = b"[[socket]]\npath = \"/ws\"\n".to_vec();
    for round in 0..10 {
        app_with(&site.config, "racy", HANDLER, None);
        let (code, body) = upload(&site, "racy", "manifest", plain.clone()).await;
        assert_eq!(code, 200, "{body}");
        let (declared, replaced) = tokio::join!(
            upload(&site, "racy", "manifest", manifest.clone()),
            upload(&site, "racy", "handler", LEGACY.to_vec()),
        );
        let resident = store::read_meta(&site.config, "racy").await.resident.is_some();
        let handler = std::fs::read(site.config.data_dir.join("racy/handler.wasm")).unwrap();
        assert!(
            !(resident && handler == LEGACY),
            "round {round}: both passed, leaving a resident app with no on-connection: {declared:?} {replaced:?}"
        );
    }
}
