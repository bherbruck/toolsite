//! Resident mode over a real socket: an app that declares `[resident]` gets
//! one long-lived instance for all of its connection events, which keeps
//! memory between them, and which the platform drops and restarts when it
//! fails, without letting it grow the server or hang it.

use futures_util::{SinkExt, StreamExt};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, Message};
use toolsite::{
    build_router,
    content::store::{self, PageMeta, ResidentMeta},
    platform::upload::UploadTicket,
    runtime::{resident::Status, wasm::Runtime},
    Config,
};

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");
/// A handler built for `app-with-connections` before `on-tick` existed.
const NO_TICK_HANDLER: &[u8] = include_bytes!("fixtures/no-tick-handler.wasm");

struct Site {
    _dir: TempDir,
    config: Arc<Config>,
    addr: std::net::SocketAddr,
}

async fn site() -> Site {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), "test-token"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(config.clone(), Runtime::new().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Site { _dir: dir, config, addr }
}

fn edit_meta(config: &Config, name: &str, change: impl FnOnce(&mut PageMeta)) {
    let mut meta = store::read_meta_blocking(config, name);
    change(&mut meta);
    store::write_meta_blocking(config, name, &meta).unwrap();
}

/// An app with a socket at `/ws`, resident when `resident` is given.
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
    app_with(config, name, HANDLER, Some(ResidentMeta { memory_mb: None, tick_ms: None }));
}

fn status(site: &Site, app: &str) -> Status {
    site.config.residents.status(app).unwrap_or_default()
}

/// PUTs to an upload URL for `app`, as an agent would after create_upload.
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

async fn connect(site: &Site, app: &str) -> Result<Socket, u16> {
    connect_to(site, &format!("/p/{app}/ws")).await
}

async fn connect_to(site: &Site, path: &str) -> Result<Socket, u16> {
    let request = format!("ws://{}{path}", site.addr).into_client_request().unwrap();
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("connect failed: {other}"),
    }
}

/// Connects and reads the `id:<conn>` the fixture sends on connect.
async fn open(site: &Site, app: &str) -> Socket {
    let mut socket = connect(site, app).await.unwrap_or_else(|status| panic!("{app} refused with {status}"));
    let hello = next_text(&mut socket).await.expect("no id after connect");
    assert!(hello.starts_with("id:"), "the first frame names the connection: {hello}");
    socket
}

async fn say(socket: &mut Socket, text: &str) {
    socket.send(Message::Text(text.to_string().into())).await.unwrap();
}

async fn next_text(socket: &mut Socket) -> Option<String> {
    loop {
        match tokio::time::timeout(Duration::from_millis(1500), socket.next()).await {
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(Message::Text(text)))) => return Some(text.to_string()),
            _ => return None,
        }
    }
}

async fn ask(socket: &mut Socket, text: &str) -> Option<String> {
    say(socket, text).await;
    next_text(socket).await
}

/// Waits up to `within` for the server to end the socket.
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

async fn closes(socket: &mut Socket) -> bool {
    closes_within(socket, Duration::from_secs(3)).await
}

/// Opens a connection once the pause after a failure is over.
async fn open_after_pause(site: &Site, app: &str) -> Socket {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match connect(site, app).await {
            Ok(mut socket) => {
                assert!(next_text(&mut socket).await.is_some_and(|hello| hello.starts_with("id:")));
                return socket;
            }
            Err(status) if Instant::now() < deadline => {
                assert_eq!(status, 500, "refused for another reason than the pause");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(status) => panic!("still refused with {status} after the pause"),
        }
    }
}

#[tokio::test]
async fn memory_survives_between_events_and_across_connections_of_one_app() {
    let site = site().await;
    resident(&site.config, "broker");
    let mut first = open(&site, "broker").await;
    let mut second = open(&site, "broker").await;

    assert_eq!(ask(&mut first, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut second, "count").await.as_deref(), Some("2"));
    assert_eq!(ask(&mut first, "count").await.as_deref(), Some("3"));

    let now = status(&site, "broker");
    assert!(now.running_since.is_some(), "the status does not say it runs: {now:?}");
    assert_eq!(now.connections, 2);
    assert!(now.memory_bytes > 0 && now.memory_bytes <= now.memory_limit_bytes, "{now:?}");
    assert_eq!(now.memory_limit_bytes, 128 * 1024 * 1024, "the default memory is the site's");
}

#[tokio::test]
async fn two_resident_apps_never_share_memory() {
    let site = site().await;
    resident(&site.config, "left");
    resident(&site.config, "right");
    let mut left = open(&site, "left").await;
    let mut right = open(&site, "right").await;

    assert_eq!(ask(&mut left, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut right, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut left, "count").await.as_deref(), Some("2"));
    assert_eq!(ask(&mut right, "count").await.as_deref(), Some("2"));
}

#[tokio::test]
async fn an_app_that_is_not_resident_still_gets_a_fresh_instance_per_event() {
    let site = site().await;
    app_with(&site.config, "plain", HANDLER, None);
    let mut first = open(&site, "plain").await;
    let mut second = open(&site, "plain").await;

    assert_eq!(ask(&mut first, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut first, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut second, "count").await.as_deref(), Some("1"));
    assert!(site.config.residents.status("plain").is_none(), "a resident instance started for a plain app");
}

#[tokio::test]
async fn a_crash_closes_every_connection_of_the_app_and_the_next_one_starts_fresh() {
    let site = site().await;
    resident(&site.config, "fragile");
    resident(&site.config, "bystander");
    let mut first = open(&site, "fragile").await;
    let mut second = open(&site, "fragile").await;
    let mut other = open(&site, "bystander").await;
    assert_eq!(ask(&mut first, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut first, "count").await.as_deref(), Some("2"));
    assert_eq!(ask(&mut other, "count").await.as_deref(), Some("1"));

    say(&mut second, "crash").await;
    assert!(closes(&mut second).await, "the connection that crashed it stayed open");
    assert!(closes(&mut first).await, "another connection of the crashed instance stayed open");
    assert_eq!(
        ask(&mut other, "count").await.as_deref(),
        Some("2"),
        "another app's instance was touched by the crash"
    );

    let crashed = status(&site, "fragile");
    assert_eq!(crashed.restarts, 1);
    assert!(crashed.running_since.is_none());
    assert!(
        crashed.last_crash.as_deref().is_some_and(|why| why.contains("trapped")),
        "the crash is not reported: {crashed:?}"
    );
    let metadata = toolsite::platform::knowledge::fetch_page(&site.config, "fragile").await.unwrap().metadata;
    assert_eq!(metadata["resident"]["restarts"], 1, "MCP fetch does not report the crash: {metadata}");

    let mut fresh = open_after_pause(&site, "fragile").await;
    assert_eq!(ask(&mut fresh, "count").await.as_deref(), Some("1"), "the memory survived the crash");
}

#[tokio::test]
async fn memory_past_the_cap_traps_and_restarts_rather_than_growing_the_server() {
    let site = site().await;
    app_with(&site.config, "hoarder", HANDLER, Some(ResidentMeta { memory_mb: Some(16), tick_ms: None }));
    let mut socket = open(&site, "hoarder").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));

    say(&mut socket, "grow").await;
    assert!(closes(&mut socket).await, "the instance kept running past its cap");
    let crashed = status(&site, "hoarder");
    assert!(
        crashed.last_crash.as_deref().is_some_and(|why| why.contains("memory cap of 16 MB")),
        "{crashed:?}"
    );
    assert_eq!(crashed.memory_bytes, 0, "the memory was not let go");

    let mut fresh = open_after_pause(&site, "hoarder").await;
    assert_eq!(ask(&mut fresh, "count").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn a_hung_event_hits_the_wall_clock_and_restarts() {
    let site = site().await;
    resident(&site.config, "stuck");
    let mut socket = open(&site, "stuck").await;
    let started = Instant::now();
    say(&mut socket, "hang").await;
    assert!(closes_within(&mut socket, Duration::from_secs(10)).await, "the hung instance was never stopped");
    assert!(started.elapsed() < Duration::from_secs(8), "stopped late: {:?}", started.elapsed());
    let crashed = status(&site, "stuck");
    assert!(crashed.last_crash.as_deref().is_some_and(|why| why.contains("time limit")), "{crashed:?}");

    let mut fresh = open_after_pause(&site, "stuck").await;
    assert_eq!(ask(&mut fresh, "count").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn an_instance_that_keeps_crashing_waits_longer_each_time() {
    let site = site().await;
    resident(&site.config, "looping");

    let mut socket = open(&site, "looping").await;
    say(&mut socket, "crash").await;
    assert!(closes(&mut socket).await);
    // One second after the first crash.
    assert_eq!(connect(&site, "looping").await.err(), Some(500), "it started again with no pause");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut socket = open(&site, "looping").await;
    say(&mut socket, "crash").await;
    assert!(closes(&mut socket).await);

    // Two seconds after the second.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(connect(&site, "looping").await.err(), Some(500), "the pause did not grow");
    let pausing = status(&site, "looping");
    assert_eq!(pausing.restarts, 2);
    assert!(pausing.next_start_at.is_some(), "{pausing:?}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut socket = open(&site, "looping").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn on_tick_fires_at_roughly_the_declared_rate_and_not_when_undeclared() {
    let site = site().await;
    app_with(&site.config, "ticking", HANDLER, Some(ResidentMeta { memory_mb: None, tick_ms: Some(100) }));
    resident(&site.config, "still");
    let mut ticking = open(&site, "ticking").await;
    let mut still = open(&site, "still").await;

    let mut ticks = async || -> u32 {
        let reply = ask(&mut ticking, "ticks").await.unwrap();
        reply.strip_prefix("ticks:").unwrap().parse().unwrap()
    };
    let before = ticks().await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let during = ticks().await - before;
    assert!((6..=14).contains(&during), "{during} ticks in a second at 100 ms");
    assert_eq!(ask(&mut still, "ticks").await.as_deref(), Some("ticks:0"), "an app with no tick_ms was ticked");
    assert_eq!(status(&site, "ticking").tick_ms, Some(100));
}

#[tokio::test]
async fn both_wasi_clocks_work_inside_the_instance() {
    let site = site().await;
    resident(&site.config, "clock");
    let mut socket = open(&site, "clock").await;
    let reply = ask(&mut socket, "clock").await.unwrap();
    let (wall, monotonic) = reply.strip_prefix("wall:").and_then(|r| r.split_once(" monotonic:")).unwrap();
    let wall: u128 = wall.parse().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    assert!(now.abs_diff(wall) < 10_000, "the guest's wall clock is {wall}, the host's {now}");
    assert_eq!(monotonic, "true");
}

#[tokio::test]
async fn republishing_the_handler_drops_the_instance() {
    let site = site().await;
    resident(&site.config, "redeployed");
    let mut socket = open(&site, "redeployed").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("2"));

    let (code, body) = upload(&site, "redeployed", "handler", HANDLER.to_vec()).await;
    assert_eq!(code, 200, "{body}");
    assert!(closes(&mut socket).await, "a connection of the replaced instance stayed open");
    assert!(status(&site, "redeployed").running_since.is_none(), "the old instance still runs");

    let mut socket = open(&site, "redeployed").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"), "the new handler got the old memory");
    assert_eq!(status(&site, "redeployed").restarts, 0, "a redeploy counted as a crash");
}

#[tokio::test]
async fn hiding_the_app_drops_the_instance() {
    let site = site().await;
    resident(&site.config, "hidden");
    let mut socket = open(&site, "hidden").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));

    edit_meta(&site.config, "hidden", |meta| meta.hidden = true);
    assert!(closes(&mut socket).await);
    let deadline = Instant::now() + Duration::from_secs(2);
    while status(&site, "hidden").running_since.is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(status(&site, "hidden").running_since.is_none(), "the hidden app's instance still runs");

    edit_meta(&site.config, "hidden", |meta| meta.hidden = false);
    let mut socket = open(&site, "hidden").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
}

#[tokio::test]
async fn a_component_built_before_on_tick_still_links_and_works_resident() {
    let site = site().await;
    app_with(&site.config, "older", NO_TICK_HANDLER, Some(ResidentMeta { memory_mb: None, tick_ms: Some(100) }));
    let mut socket = open(&site, "older").await;
    assert_eq!(ask(&mut socket, "echo:hi").await.as_deref(), Some("hi"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(ask(&mut socket, "log").await.as_deref(), Some("connect anonymous /ws,message,message"));
    let now = status(&site, "older");
    assert!(now.running_since.is_some() && now.restarts == 0, "{now:?}");
}

#[tokio::test]
async fn events_across_connections_of_a_resident_app_run_in_one_global_order() {
    let site = site().await;
    resident(&site.config, "ordered");
    const EACH: usize = 20;
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let mut socket = open(&site, "ordered").await;
        tasks.push(tokio::spawn(async move {
            for _ in 0..EACH {
                say(&mut socket, "count").await;
            }
            let mut seen = Vec::new();
            for _ in 0..EACH {
                seen.push(next_text(&mut socket).await.unwrap().parse::<usize>().unwrap());
            }
            seen
        }));
    }
    let mut all = Vec::new();
    for task in tasks {
        let seen = task.await.unwrap();
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "one connection's events ran out of order: {seen:?}");
        all.extend(seen);
    }
    // One counter, every event exactly once: no two ran at the same time.
    all.sort();
    assert_eq!(all, (1..=3 * EACH).collect::<Vec<_>>());
}

#[tokio::test]
async fn what_connect_sends_on_a_resident_instance_still_arrives_first_and_in_order() {
    let site = site().await;
    resident(&site.config, "snapshot");
    let mut socket = connect_to(&site, "/p/snapshot/ws?ordered=feed").await.unwrap();
    let mut frames = Vec::new();
    for _ in 0..4 {
        frames.push(next_text(&mut socket).await.unwrap_or_default());
    }
    assert!(frames[0].starts_with("id:"), "{frames:?}");
    assert_eq!(frames[1..], ["1", "2", "3"]);
}

#[tokio::test]
async fn the_manifest_turns_resident_mode_on_and_off_and_a_handler_without_connections_is_refused() {
    let site = site().await;
    let legacy: &[u8] = include_bytes!("fixtures/legacy-handler.wasm");
    app_with(&site.config, "flip", HANDLER, None);

    let (code, body) = upload(&site, "flip", "manifest", b"[[socket]]\npath = \"/ws\"\n\n[resident]\nenabled = true\nmemory_mb = 32\n".to_vec()).await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("resident, 32 MB"), "{body}");
    let mut socket = open(&site, "flip").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("2"));

    // A handler that cannot take connections is no handler for a resident app.
    let (code, body) = upload(&site, "flip", "handler", legacy.to_vec()).await;
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("[resident]"), "{body}");

    // Withdrawn: the instance goes, and events run fresh again.
    let (code, body) = upload(&site, "flip", "manifest", b"[[socket]]\npath = \"/ws\"\n".to_vec()).await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("resident mode withdrawn"), "{body}");
    assert!(closes(&mut socket).await);
    let mut socket = open(&site, "flip").await;
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
    assert_eq!(ask(&mut socket, "count").await.as_deref(), Some("1"));
}
