//! Live connections over a real socket: toolsite holds the WebSocket, the
//! app's handler gets connect, message and close as events, and the door in
//! front of the socket is the same door every request to the app passes.

use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, Message};
use toolsite::{
    accounts::users,
    build_router,
    content::store::{self, PageMeta, PathRule},
    runtime::{connections, wasm::Runtime},
    Config,
};

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");
/// A handler built before connections existed, kept to prove it still links.
const LEGACY_HANDLER: &[u8] = include_bytes!("fixtures/legacy-handler.wasm");

struct Site {
    _dir: TempDir,
    config: Arc<Config>,
    addr: std::net::SocketAddr,
}

async fn site(limits: connections::Limits) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        connections: Arc::new(connections::Hub::new(limits)),
        ..Config::local(dir.path().to_path_buf(), "test-token")
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(config.clone(), Runtime::new().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Site { _dir: dir, config, addr }
}

fn quick() -> connections::Limits {
    connections::Limits { check_every: Duration::from_millis(150), ..connections::Limits::default() }
}

/// An app with the test handler and one declared socket at `/ws`.
fn app(config: &Config, name: &str) {
    app_with(config, name, HANDLER, &["/ws"]);
}

fn app_with(config: &Config, name: &str, wasm: &[u8], sockets: &[&str]) {
    let dir = config.data_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("handler.wasm"), wasm).unwrap();
    std::fs::write(dir.join("index.html"), "<title>app</title>").unwrap();
    edit_meta(config, name, |meta| meta.sockets = sockets.iter().map(|s| s.to_string()).collect());
}

fn edit_meta(config: &Config, name: &str, change: impl FnOnce(&mut PageMeta)) {
    let mut meta = store::read_meta_blocking(config, name);
    change(&mut meta);
    store::write_meta_blocking(config, name, &meta).unwrap();
}

/// A signed-in person's cookie for one app, as the handoff would set it.
fn app_cookie(config: &Config, email: &str, name: &str) -> (String, String) {
    users::sign_up(config, email, "correct horse battery").unwrap();
    let (user, site_token) = users::log_in(config, email, "correct horse battery").unwrap();
    let (_, token, _) = users::create_app_session(config, &site_token, name).unwrap();
    (user.id, format!("ts_app_{name}={token}"))
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(site: &Site, path: &str, cookie: Option<&str>) -> Result<Socket, u16> {
    let mut request = format!("ws://{}{}", site.addr, path).into_client_request().unwrap();
    if let Some(cookie) = cookie {
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("connect failed: {other}"),
    }
}

/// Connects and reads the `id:<conn>` the fixture sends on connect.
async fn open(site: &Site, path: &str, cookie: Option<&str>) -> (Socket, String) {
    let mut socket = connect(site, path, cookie).await.unwrap_or_else(|status| panic!("{path} refused with {status}"));
    let hello = next_text(&mut socket).await.expect("no id after connect");
    let id = hello.strip_prefix("id:").expect("the first frame names the connection").to_string();
    (socket, id)
}

async fn api(site: &Site, app: &str, route: &str, body: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("http://{}/p/{app}/api/{route}", site.addr))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

async fn say(socket: &mut Socket, text: &str) {
    socket.send(Message::Text(text.to_string().into())).await.unwrap();
}

async fn next_frame(socket: &mut Socket) -> Option<Message> {
    loop {
        match tokio::time::timeout(Duration::from_millis(1500), socket.next()).await {
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(frame @ (Message::Text(_) | Message::Binary(_))))) => return Some(frame),
            _ => return None,
        }
    }
}

async fn next_text(socket: &mut Socket) -> Option<String> {
    match next_frame(socket).await {
        Some(Message::Text(text)) => Some(text.to_string()),
        _ => None,
    }
}

/// Nothing arrives for a short while.
async fn quiet(socket: &mut Socket) -> bool {
    loop {
        match tokio::time::timeout(Duration::from_millis(400), socket.next()).await {
            Err(_) => return true,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(_) => return false,
        }
    }
}

/// Waits for the server to end the socket.
async fn closes(socket: &mut Socket) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return true,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

#[tokio::test]
async fn connect_message_and_close_reach_the_handler_in_order_as_the_person_on_the_socket() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "chat");
    let (_, cookie) = app_cookie(&site.config, "me@example.com", "chat");
    let (mut watcher, _) = open(&site, "/p/chat/ws?topic=closed", None).await;
    let (mut socket, _) = open(&site, "/p/chat/ws", Some(&cookie)).await;

    say(&mut socket, "echo:hi").await;
    assert_eq!(next_text(&mut socket).await.as_deref(), Some("hi"));
    say(&mut socket, "log").await;
    assert_eq!(
        next_text(&mut socket).await.as_deref(),
        Some("connect me@example.com /ws,message,message")
    );
    socket.close(None).await.unwrap();
    // The close event ran last, as the same person, with the state intact.
    assert_eq!(
        next_text(&mut watcher).await.as_deref(),
        Some("me@example.com:connect me@example.com /ws,message,message,close")
    );
}

#[tokio::test]
async fn the_handler_can_send_close_subscribe_and_publish() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "news");
    let (mut reader, reader_id) = open(&site, "/p/news/ws?topic=headlines", None).await;
    let (mut fan, _) = open(&site, "/p/news/ws", None).await;

    // Subscribe from a message, publish from a message.
    say(&mut fan, "sub:sports").await;
    assert_eq!(next_text(&mut fan).await.as_deref(), Some("ok"));
    say(&mut fan, "publish:sports:goal").await;
    assert_eq!(next_text(&mut fan).await.as_deref(), Some("goal"));
    assert!(quiet(&mut reader).await, "a connection off the topic got the message");

    // Publish and send from an ordinary request.
    let (status, body) = api(&site, "news", "publish?topic=headlines", "rain today").await;
    assert_eq!((status, body.as_str()), (200, "reached 1"));
    assert_eq!(next_text(&mut reader).await.as_deref(), Some("rain today"));
    let (status, _) = api(&site, "news", &format!("send?conn={reader_id}"), "just you").await;
    assert_eq!(status, 200);
    assert_eq!(next_text(&mut reader).await.as_deref(), Some("just you"));

    // Unsubscribe, then a publish no longer reaches it.
    say(&mut fan, "unsub:sports").await;
    assert_eq!(next_text(&mut fan).await.as_deref(), Some("ok"));
    let (_, body) = api(&site, "news", "publish?topic=sports", "late goal").await;
    assert_eq!(body, "reached 0");

    // Binary frames reach the handler as binary.
    fan.send(Message::Binary(vec![1, 2, 3].into())).await.unwrap();
    assert_eq!(next_frame(&mut fan).await, Some(Message::Binary(vec![1, 2, 3].into())));

    // Close from the handler.
    say(&mut reader, "close").await;
    assert!(closes(&mut reader).await, "the handler's close left the socket open");
}

#[tokio::test]
async fn a_connect_the_handler_refuses_never_becomes_a_socket() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "picky");
    assert_eq!(connect(&site, "/p/picky/ws?refuse=1", None).await.err(), Some(403));
    assert_eq!(site.config.connections.open("picky"), 0, "a refused connection stayed registered");
}

#[tokio::test]
async fn state_survives_between_events_and_is_gone_when_the_connection_closes() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "kept");
    let (mut socket, id) = open(&site, "/p/kept/ws", None).await;
    say(&mut socket, "echo:x").await;
    next_text(&mut socket).await;
    let (status, body) = api(&site, "kept", &format!("conn-state?conn={id}&key=log"), "").await;
    assert_eq!((status, body.as_str()), (200, "connect anonymous /ws,message"));

    socket.close(None).await.unwrap();
    let mut gone = false;
    for _ in 0..30 {
        if api(&site, "kept", &format!("conn-state?conn={id}&key=log"), "").await.0 == 404 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "the connection's state outlived it");
}

#[tokio::test]
async fn one_app_can_never_reach_another_apps_connections() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "shop");
    app(&site.config, "ledger");
    let (mut mine, mine_id) = open(&site, "/p/shop/ws?topic=orders", None).await;
    let (mut theirs, _) = open(&site, "/p/ledger/ws?topic=orders", None).await;

    let (_, body) = api(&site, "shop", "publish?topic=orders", "order 7").await;
    assert_eq!(body, "reached 1");
    assert_eq!(next_text(&mut mine).await.as_deref(), Some("order 7"));
    assert!(quiet(&mut theirs).await, "another app's connection got the message");

    // An id from another app names nothing there.
    let (status, _) = api(&site, "ledger", &format!("send?conn={mine_id}"), "hijack").await;
    assert_eq!(status, 400);
    let (status, _) = api(&site, "ledger", &format!("conn-state?conn={mine_id}&key=log"), "").await;
    assert_eq!(status, 404);
    assert!(quiet(&mut mine).await);
}

#[tokio::test]
async fn a_person_joins_only_their_own_user_topic() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "inbox");
    let (me, cookie) = app_cookie(&site.config, "me@example.com", "inbox");
    let (mut socket, _) = open(&site, &format!("/p/inbox/ws?topic=user:{me}"), Some(&cookie)).await;
    // Even when the handler asks for it, someone else's topic is refused.
    assert_eq!(connect(&site, "/p/inbox/ws?topic=user:somebody-else", Some(&cookie)).await.err(), Some(403));
    assert_eq!(connect(&site, &format!("/p/inbox/ws?topic=user:{me}"), None).await.err(), Some(403));
    // The app may publish to any person's topic.
    let (_, body) = api(&site, "inbox", &format!("publish?topic=user:{me}"), "for you").await;
    assert_eq!(body, "reached 1");
    assert_eq!(next_text(&mut socket).await.as_deref(), Some("for you"));
}

#[tokio::test]
async fn a_handler_built_before_connections_still_links_and_its_app_refuses_upgrades() {
    let site = site(connections::Limits::default()).await;
    app_with(&site.config, "older", LEGACY_HANDLER, &["/ws"]);
    let response = reqwest::get(format!("http://{}/p/older/api/echo", site.addr)).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(connect(&site, "/p/older/ws", None).await.err(), Some(501));
}

#[tokio::test]
async fn only_a_declared_path_takes_an_upgrade_and_it_follows_that_paths_route_rule() {
    let site = site(connections::Limits::default()).await;
    app_with(&site.config, "orders", HANDLER, &["/live/ws", "/ws"]);
    std::fs::create_dir_all(site.config.data_dir.join("orders/live")).unwrap();
    std::fs::write(site.config.data_dir.join("orders/live/ws"), "a file at the same path").unwrap();
    edit_meta(&site.config, "orders", |meta| {
        meta.gate = Some("restricted".into());
        meta.rules = vec![PathRule { prefix: "/live".into(), gate: "public".into() }];
    });

    // The rule opens /live to anyone, so a stranger's socket there is taken.
    open(&site, "/p/orders/live/ws", None).await;
    // Outside the rule the app is restricted, and the socket is refused.
    assert_eq!(connect(&site, "/p/orders/ws", None).await.err(), Some(401));
    // A path the app did not declare takes no upgrade at all.
    assert_eq!(connect(&site, "/p/orders/live/other", None).await.err(), Some(404));
    // A plain request to the socket's path still gets the app's file.
    let body = reqwest::get(format!("http://{}/p/orders/live/ws", site.addr)).await.unwrap().text().await.unwrap();
    assert_eq!(body, "a file at the same path");
}

#[tokio::test]
async fn a_stranger_is_refused_on_a_restricted_app_and_let_in_on_a_public_one() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "open");
    app(&site.config, "closed");
    edit_meta(&site.config, "closed", |meta| meta.gate = Some("restricted".into()));
    open(&site, "/p/open/ws", None).await;
    assert_eq!(connect(&site, "/p/closed/ws", None).await.err(), Some(401));
    assert_eq!(connect(&site, "/p/nothing/ws", None).await.err(), Some(404));
}

#[tokio::test]
async fn the_connection_ceilings_per_person_and_per_app_hold() {
    let site = site(connections::Limits { per_app: 3, per_person: 2, ..connections::Limits::default() }).await;
    app(&site.config, "busy");
    let (_, cookie) = app_cookie(&site.config, "busy@example.com", "busy");
    let _a = open(&site, "/p/busy/ws", Some(&cookie)).await;
    let _b = open(&site, "/p/busy/ws", Some(&cookie)).await;
    assert_eq!(connect(&site, "/p/busy/ws", Some(&cookie)).await.err(), Some(429), "per person");
    let _c = open(&site, "/p/busy/ws", None).await;
    assert_eq!(connect(&site, "/p/busy/ws", None).await.err(), Some(429), "per app");
}

#[tokio::test]
async fn a_message_over_64_kb_is_refused_either_way() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "big");
    let (status, body) = api(&site, "big", "publish?topic=a", &"x".repeat(connections::MAX_MESSAGE_BYTES + 1)).await;
    assert_eq!(status, 400);
    assert!(body.contains("bytes"), "{body}");
    let (mut socket, _) = open(&site, "/p/big/ws", None).await;
    let _ = socket.send(Message::Text("y".repeat(connections::MAX_MESSAGE_BYTES + 1).into())).await;
    assert!(closes(&mut socket).await, "an oversized frame was taken");
}

#[tokio::test]
async fn a_disabled_accounts_connection_closes_on_the_next_check() {
    let site = site(quick()).await;
    app(&site.config, "team");
    edit_meta(&site.config, "team", |meta| meta.gate = Some("authenticated".into()));
    let (_, cookie) = app_cookie(&site.config, "leaver@example.com", "team");
    let (mut socket, _) = open(&site, "/p/team/ws", Some(&cookie)).await;
    users::set_active(&site.config, "leaver@example.com", false).unwrap();
    assert!(closes(&mut socket).await, "the disabled account's socket stayed open");
}

#[tokio::test]
async fn losing_access_closes_the_connection_on_the_next_check() {
    let site = site(quick()).await;

    // The app's own gate changes.
    app(&site.config, "board");
    let (mut socket, _) = open(&site, "/p/board/ws", None).await;
    edit_meta(&site.config, "board", |meta| meta.gate = Some("restricted".into()));
    assert!(closes(&mut socket).await, "the socket stayed open after the app was restricted");

    // A lock on the project above it.
    app(&site.config, "notice");
    store::create_folder(&site.config, "", "team").await.unwrap();
    edit_meta(&site.config, "notice", |meta| {
        meta.gate = Some("public".into());
        meta.project = Some("team".into());
    });
    let (mut socket, _) = open(&site, "/p/notice/ws", None).await;
    store::set_folder_gate(&site.config, "team", Some("restricted")).await.unwrap();
    store::set_locked(&site.config, "team", true).await.unwrap();
    assert!(closes(&mut socket).await, "the socket stayed open under the lock");

    // The socket is withdrawn from toolsite.toml.
    app(&site.config, "feed");
    let (mut socket, _) = open(&site, "/p/feed/ws", None).await;
    toolsite::platform::manifest::apply(&site.config, "feed", "").await.unwrap();
    assert!(closes(&mut socket).await, "the socket stayed open after it was withdrawn");
}

#[tokio::test]
async fn toolsite_toml_declares_sockets_wholesale_within_limits() {
    let site = site(connections::Limits::default()).await;
    app_with(&site.config, "decl", HANDLER, &[]);
    let apply = |text: String| {
        let config = site.config.clone();
        async move { toolsite::platform::manifest::apply(&config, "decl", &text).await }
    };
    apply("[[socket]]\npath = \"/live/ws\"\n".into()).await.unwrap();
    assert_eq!(store::read_meta_blocking(&site.config, "decl").sockets, vec!["/live/ws"]);
    open(&site, "/p/decl/live/ws", None).await;

    let many: String = (0..17).map(|n| format!("[[socket]]\npath = \"/s{n}\"\n")).collect();
    assert!(apply(many).await.unwrap_err().contains("at most 16"));
    for bad in ["live", "/mcp", "/a/../b", "/.x", "/"] {
        assert!(apply(format!("[[socket]]\npath = \"{bad}\"\n")).await.is_err(), "{bad} was taken");
    }
    // A refused manifest changed nothing.
    assert_eq!(store::read_meta_blocking(&site.config, "decl").sockets, vec!["/live/ws"]);
}
