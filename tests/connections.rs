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

/// Connects with an `Origin` header, as a browser always sends one.
async fn connect_from(site: &Site, path: &str, origin: &str, cookie: Option<&str>) -> Result<Socket, u16> {
    let mut request = format!("ws://{}{}", site.addr, path).into_client_request().unwrap();
    request.headers_mut().insert("origin", origin.parse().unwrap());
    if let Some(cookie) = cookie {
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("connect failed: {other}"),
    }
}

#[tokio::test]
async fn a_page_on_another_site_cannot_open_a_socket_with_the_visitors_cookies() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "team");
    edit_meta(&site.config, "team", |meta| meta.gate = Some("authenticated".into()));
    let (_, cookie) = app_cookie(&site.config, "me@example.com", "team");
    for origin in ["http://evil.example", "null", "https://evil.example", &format!("http://{}.evil.example", site.addr.ip())] {
        assert_eq!(
            connect_from(&site, "/p/team/ws", origin, Some(&cookie)).await.err(),
            Some(403),
            "an upgrade from {origin} was taken"
        );
    }
    // The site's own pages may, whichever spelling of the origin they send.
    let ours = format!("http://{}", site.addr);
    let mut socket = connect_from(&site, "/p/team/ws", &ours, Some(&cookie)).await.expect("own origin refused");
    assert!(next_text(&mut socket).await.unwrap().starts_with("id:"));
}

#[tokio::test]
async fn app_code_never_sees_the_visitors_toolsite_sessions_on_a_socket_or_a_request() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "chat");
    let (_, app_cookie) = app_cookie(&site.config, "me@example.com", "chat");
    let jar = format!("ts_session=site-secret; {app_cookie}; ts_app_bank=other-secret; theme=dark");

    let (mut socket, _) = open(&site, "/p/chat/ws", Some(&jar)).await;
    say(&mut socket, "log").await;
    assert!(next_text(&mut socket).await.unwrap().contains("me@example.com"), "the person was not recognised");
    say(&mut socket, "cookie").await;
    assert_eq!(next_text(&mut socket).await.as_deref(), Some("theme=dark"));

    let get = |cookie: String| {
        let url = format!("http://{}/p/chat/api/cookies", site.addr);
        async move { reqwest::Client::new().get(url).header("cookie", cookie).send().await.unwrap().text().await.unwrap() }
    };
    assert_eq!(get(jar.clone()).await, "theme=dark");
    assert_eq!(get("ts_session=site-secret".into()).await, "", "a jar of only toolsite cookies reached the handler");
}

#[tokio::test]
async fn an_app_cannot_set_one_of_toolsites_own_cookies() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "fixer");
    let set_cookie = |value: &str| {
        let url = format!("http://{}/p/fixer/api/set-cookie?{}", site.addr, value);
        async move {
            let response = reqwest::get(url).await.unwrap();
            response.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect::<Vec<_>>()
        }
    };
    for forged in ["ts_session%3Dforged%3B%20Path%3D%2F", "ts_app_bank%3Dforged%3B%20Path%3D%2Fp%2Fbank", "%20ts_session%3Dx"] {
        assert!(set_cookie(forged).await.is_empty(), "{forged} reached the browser");
    }
    assert_eq!(set_cookie("theme%3Ddark").await, vec!["theme=dark".to_string()]);
}

#[tokio::test]
async fn hiding_or_removing_an_app_closes_its_sockets_at_once_not_at_the_next_check() {
    // The default check is every 30 seconds; `closes` waits 3.
    let site = site(connections::Limits::default()).await;
    app(&site.config, "leak");
    let (mut socket, _) = open(&site, "/p/leak/ws", None).await;
    edit_meta(&site.config, "leak", |meta| meta.hidden = true);
    assert!(closes(&mut socket).await, "the hidden app's socket stayed open");

    app(&site.config, "gone");
    let (mut socket, _) = open(&site, "/p/gone/ws", None).await;
    toolsite::platform::trash::remove(&site.config, "gone", 1).unwrap();
    assert!(closes(&mut socket).await, "the removed app's socket stayed open");
    assert_eq!(connect(&site, "/p/gone/ws", None).await.err(), Some(404));
}

#[tokio::test]
async fn the_server_wide_ceiling_holds_across_apps() {
    let site = site(connections::Limits { total: 2, ..connections::Limits::default() }).await;
    app(&site.config, "one");
    app(&site.config, "two");
    let _a = open(&site, "/p/one/ws", None).await;
    let _b = open(&site, "/p/two/ws", None).await;
    assert_eq!(connect(&site, "/p/one/ws", None).await.err(), Some(429));
}

// --- TCP and UDP ------------------------------------------------------------

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};
use toolsite::content::store::{PortProtocol, PortSocket};

/// A port nothing listens on now. Bound and let go, so the site can take it.
fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn tcp(port: u16) -> PortSocket {
    PortSocket { protocol: PortProtocol::Tcp, port }
}

fn udp(port: u16) -> PortSocket {
    PortSocket { protocol: PortProtocol::Udp, port }
}

/// A site whose owner mapped ports with `map`, as `TOOLSITE_PORTS` would.
async fn site_with_ports(limits: connections::Limits, map: &str) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        connections: Arc::new(connections::Hub::new(limits)),
        ports: toolsite::platform::ports::PortMap {
            bind: "127.0.0.1".parse().unwrap(),
            mappings: toolsite::platform::ports::parse(map).unwrap(),
        },
        ..Config::local(dir.path().to_path_buf(), "test-token")
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let runtime = Runtime::new().unwrap();
    let router = build_router(config.clone(), runtime.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    toolsite::platform::ports::listen(config.clone(), runtime).await.unwrap();
    Site { _dir: dir, config, addr }
}

/// An app with the test handler, a WebSocket at `/ws` for watching, and
/// `ports` declared.
fn app_on(config: &Config, name: &str, ports: &[PortSocket]) {
    app(config, name);
    edit_meta(config, name, |meta| meta.ports = ports.to_vec());
}

/// One device's TCP connection.
struct Device {
    stream: TcpStream,
    read: Vec<u8>,
}

impl Device {
    async fn connect(port: u16) -> Device {
        Device { stream: TcpStream::connect(("127.0.0.1", port)).await.unwrap(), read: Vec::new() }
    }

    /// Connects and reads the `id:<conn>` the fixture sends on connect.
    async fn open(port: u16) -> (Device, String) {
        let mut device = Device::connect(port).await;
        let hello = device.line().await.expect("no id after connect");
        let id = hello.strip_prefix("id:").expect("the first line names the connection").to_string();
        (device, id)
    }

    async fn say(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).await.unwrap();
    }

    /// The next line, without its newline. None when the server ends the
    /// connection or says nothing for a while. The while is long: the first
    /// event compiles the handler, which in a debug build takes most of a
    /// minute while every other test compiles its own.
    async fn line(&mut self) -> Option<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(at) = self.read.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.read.drain(..=at).collect();
                return Some(String::from_utf8_lossy(&line[..at]).to_string());
            }
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return None,
                Ok(Ok(n)) => self.read.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// Waits for the server to end the connection, with nothing more said.
    async fn ends(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(_)) => continue,
                Err(_) => return false,
            }
        }
    }
}

/// A connection the site accepts at the TCP level and closes before the
/// app says anything.
async fn turned_away(port: u16) -> bool {
    let mut device = Device::connect(port).await;
    device.line().await.is_none()
}

/// Waits until `app` holds `open` connections.
async fn settles_at(config: &Config, app: &str, open: usize) -> bool {
    for _ in 0..30 {
        if config.connections.open(app) == open {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn tcp_bytes_reach_the_handler_in_order_and_its_reply_returns() {
    let port = free_tcp_port();
    let site = site_with_ports(connections::Limits::default(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (device, _) = Device::open(port).await;

    // More than one read's worth, written while the echo comes back, so the
    // order holds across chunks of up to 64 KB.
    let sent: Vec<u8> = (0..150_000u32).map(|n| (n % 251) as u8).collect();
    let Device { stream, read } = device;
    let (mut reader, mut writer) = stream.into_split();
    let writing = {
        let sent = sent.clone();
        tokio::spawn(async move { writer.write_all(&sent).await.unwrap(); writer })
    };
    let mut echoed = read;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while echoed.len() < sent.len() {
        let mut chunk = vec![0u8; 64 * 1024];
        match tokio::time::timeout_at(deadline, reader.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => echoed.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    assert_eq!(echoed.len(), sent.len(), "the echo came back short");
    assert!(echoed == sent, "the bytes came back out of order");

    let writer = writing.await.unwrap();
    let mut device = Device { stream: reader.reunite(writer).unwrap(), read: Vec::new() };
    let local = device.stream.local_addr().unwrap();
    device.say("remote\n").await;
    assert_eq!(device.line().await, Some(local.to_string()), "the handler did not see where the device is");
    device.say("log\n").await;
    let log = device.line().await.unwrap();
    assert!(log.starts_with(&format!("connect tcp:{port},message")), "{log}");
}

#[tokio::test]
async fn either_side_closing_a_tcp_connection_delivers_close_and_frees_it() {
    let port = free_tcp_port();
    let site = site_with_ports(connections::Limits::default(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (mut watcher, _) = open(&site, "/p/broker/ws?topic=closed", None).await;

    // The device goes away.
    let (device, _) = Device::open(port).await;
    let local = device.stream.local_addr().unwrap();
    drop(device);
    assert_eq!(next_text(&mut watcher).await, Some(format!("{local}:connect tcp:{port},close")));
    assert!(settles_at(&site.config, "broker", 1).await, "the closed connection stayed registered");

    // The app ends it.
    let (mut device, _) = Device::open(port).await;
    let local = device.stream.local_addr().unwrap();
    device.say("close\n").await;
    assert!(device.ends().await, "the handler's close left the connection open");
    assert_eq!(next_text(&mut watcher).await, Some(format!("{local}:connect tcp:{port},message,close")));
    assert!(settles_at(&site.config, "broker", 1).await, "the closed connection stayed registered");
}

#[tokio::test]
async fn a_device_that_stops_sending_still_gets_the_reply_to_what_it_sent_last() {
    let port = free_tcp_port();
    let site = site_with_ports(connections::Limits::default(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (mut device, _) = Device::open(port).await;
    // `printf reading | nc host port`: write, then close the sending half.
    device.say("reading 21.5").await;
    device.stream.shutdown().await.unwrap();
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(10), device.stream.read_to_end(&mut rest)).await;
    assert!(read.is_ok(), "the connection stayed open after the device stopped sending");
    assert_eq!(String::from_utf8_lossy(&rest), "reading 21.5");
    assert!(settles_at(&site.config, "broker", 0).await);
}

#[tokio::test]
async fn a_tcp_connection_that_sends_nothing_is_closed_after_the_idle_timeout() {
    let port = free_tcp_port();
    let limits = connections::Limits { tcp_idle: Duration::from_millis(300), ..connections::Limits::default() };
    let site = site_with_ports(limits, &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (mut device, _) = Device::open(port).await;
    assert!(device.ends().await, "an idle connection stayed open");
    assert!(settles_at(&site.config, "broker", 0).await);
}

#[tokio::test]
async fn a_port_is_live_only_where_the_owner_mapped_it_to_the_app_that_declares_it() {
    let (unmapped, theirs) = (free_tcp_port(), free_tcp_port());
    let site = site_with_ports(connections::Limits::default(), &format!("{theirs}=other")).await;
    app_on(&site.config, "mine", &[tcp(unmapped), tcp(theirs)]);

    // A port nobody mapped opens no listener at all.
    assert!(TcpStream::connect(("127.0.0.1", unmapped)).await.is_err(), "an unmapped port took a connection");
    // A port mapped to another app never reaches this one, and the other
    // app takes nothing on it until it declares it.
    assert!(turned_away(theirs).await, "a port mapped to another app reached this one");
    app_on(&site.config, "other", &[tcp(theirs)]);
    Device::open(theirs).await;
    assert_eq!(site.config.connections.open("mine"), 0);

    // Deploying says which declared ports are not live.
    let said = toolsite::platform::manifest::apply(
        &site.config,
        "mine",
        &format!("[[socket]]\nprotocol = \"tcp\"\nport = {unmapped}\n"),
    )
    .await
    .unwrap();
    assert!(said.iter().any(|line| line.contains("not live")), "{said:?}");

    // Two apps can never be given one port.
    assert!(toolsite::platform::ports::parse(&format!("{theirs}=mine,{theirs}=other")).is_err());
}

#[tokio::test]
async fn toolsite_toml_declares_tcp_and_udp_ports_and_refuses_malformed_ones() {
    let site = site(connections::Limits::default()).await;
    app_with(&site.config, "decl", HANDLER, &[]);
    let apply = |text: &str| {
        let (config, text) = (site.config.clone(), text.to_string());
        async move { toolsite::platform::manifest::apply(&config, "decl", &text).await }
    };
    apply("[[socket]]\npath = \"/ws\"\n\n[[socket]]\nprotocol = \"tcp\"\nport = 1883\n\n[[socket]]\nprotocol = \"udp\"\nport = 5514\n")
        .await
        .unwrap();
    let meta = store::read_meta_blocking(&site.config, "decl");
    assert_eq!(meta.sockets, vec!["/ws"]);
    assert_eq!(meta.ports, vec![tcp(1883), udp(5514)]);
    for bad in [
        "[[socket]]\nprotocol = \"tcp\"\n",
        "[[socket]]\nprotocol = \"tcp\"\nport = 80\n",
        "[[socket]]\nprotocol = \"tcp\"\nport = 1883\npath = \"/x\"\n",
        "[[socket]]\npath = \"/x\"\nport = 1883\n",
        "[[socket]]\nprotocol = \"sctp\"\nport = 1883\n",
        "[[socket]]\nprotocol = \"tcp\"\nport = 1883\n\n[[socket]]\nprotocol = \"tcp\"\nport = 1883\n",
        "[[socket]]\nprotocol = \"tcp\"\nport = 70000\n",
    ] {
        assert!(apply(bad).await.is_err(), "{bad} was taken");
    }
    assert_eq!(store::read_meta_blocking(&site.config, "decl").ports, vec![tcp(1883), udp(5514)]);
    // Withdrawn wholesale.
    apply("").await.unwrap();
    assert!(store::read_meta_blocking(&site.config, "decl").ports.is_empty());
}

#[tokio::test]
async fn a_device_token_lets_a_device_in_and_a_revoked_or_another_apps_token_does_not() {
    let port = free_tcp_port();
    let site = site_with_ports(connections::Limits::default(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (entry, token) = toolsite::platform::devices::create(&site.config, "broker", "boiler").unwrap();
    let (_, elsewhere) = toolsite::platform::devices::create(&site.config, "syslog", "boiler").unwrap();
    let (_, export) = toolsite::platform::export::create(&site.config, "broker", "reporting").unwrap();

    let (mut device, _) = Device::open(port).await;
    device.say(&format!("token {token}\n")).await;
    assert_eq!(device.line().await.as_deref(), Some("ok boiler"));

    for wrong in [elsewhere.as_str(), export.as_str(), "test-token", "tsv_guess"] {
        let (mut device, _) = Device::open(port).await;
        device.say(&format!("token {wrong}\n")).await;
        assert_eq!(device.line().await.as_deref(), Some("denied"), "{wrong} let a device in");
        assert!(device.ends().await);
    }

    toolsite::platform::devices::revoke(&site.config, "broker", &entry.id).unwrap();
    let (mut device, _) = Device::open(port).await;
    device.say(&format!("token {token}\n")).await;
    assert_eq!(device.line().await.as_deref(), Some("denied"), "a revoked token let a device in");
}

#[tokio::test]
async fn tcp_connections_are_capped_per_address_and_per_app() {
    let port = free_tcp_port();
    let limits = connections::Limits { per_ip: 2, ..connections::Limits::default() };
    let site = site_with_ports(limits, &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let _a = Device::open(port).await;
    let _b = Device::open(port).await;
    assert!(turned_away(port).await, "a third connection from one address was taken");
    drop(_a);
    assert!(settles_at(&site.config, "broker", 1).await);
    let _c = Device::open(port).await;

    let port = free_tcp_port();
    let limits = connections::Limits { raw_per_app: 2, ..connections::Limits::default() };
    let site = site_with_ports(limits, &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let _a = Device::open(port).await;
    let _b = Device::open(port).await;
    assert!(turned_away(port).await, "a third connection to the app was taken");
    // A browser's socket is not a device and is not counted against them.
    open(&site, "/p/broker/ws", None).await;
}

#[tokio::test]
async fn hiding_an_app_closes_its_tcp_connections_and_turns_new_ones_away() {
    let port = free_tcp_port();
    let site = site_with_ports(quick(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (mut device, _) = Device::open(port).await;
    edit_meta(&site.config, "broker", |meta| meta.hidden = true);
    assert!(device.ends().await, "the hidden app's connection stayed open");
    assert!(turned_away(port).await, "the hidden app took a new connection");

    // Withdrawing the port from toolsite.toml closes its connections on
    // the next check.
    edit_meta(&site.config, "broker", |meta| meta.hidden = false);
    let (mut device, _) = Device::open(port).await;
    toolsite::platform::manifest::apply(&site.config, "broker", "[[socket]]\npath = \"/ws\"\n").await.unwrap();
    assert!(device.ends().await, "the connection stayed open after its port was withdrawn");
    assert!(turned_away(port).await);
}

#[tokio::test]
async fn a_handler_without_on_connection_turns_a_tcp_connection_away() {
    let port = free_tcp_port();
    let site = site_with_ports(connections::Limits::default(), &format!("{port}=older")).await;
    app_with(&site.config, "older", LEGACY_HANDLER, &[]);
    edit_meta(&site.config, "older", |meta| meta.ports = vec![tcp(port)]);
    assert!(turned_away(port).await);
    assert_eq!(site.config.connections.open("older"), 0);
}

/// The next datagram. The first one waits long, as `Device::line` does.
async fn datagram(socket: &UdpSocket) -> Option<String> {
    datagram_within(socket, Duration::from_secs(60)).await
}

async fn datagram_within(socket: &UdpSocket, wait: Duration) -> Option<String> {
    let mut buffer = vec![0u8; 70_000];
    match tokio::time::timeout(wait, socket.recv(&mut buffer)).await {
        Ok(Ok(n)) => Some(String::from_utf8_lossy(&buffer[..n]).to_string()),
        _ => None,
    }
}

async fn udp_device(port: u16) -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.connect(("127.0.0.1", port)).await.unwrap();
    socket
}

#[tokio::test]
async fn a_udp_datagram_reaches_the_handler_with_its_remote_and_the_reply_reaches_the_sender() {
    let port = free_udp_port();
    let site = site_with_ports(connections::Limits::default(), &format!("{port}/udp=syslog")).await;
    app_on(&site.config, "syslog", &[udp(port)]);

    let device = udp_device(port).await;
    device.send(b"<13>boot").await.unwrap();
    let id = datagram(&device).await.expect("no id after the first datagram");
    assert!(id.starts_with("id:"), "{id}");
    assert_eq!(datagram(&device).await.as_deref(), Some("<13>boot"));
    device.send(b"remote\n").await.unwrap();
    assert_eq!(datagram(&device).await, Some(format!("{}\n", device.local_addr().unwrap())));
    // One connection per address: the same one, with its state.
    device.send(b"log\n").await.unwrap();
    assert_eq!(datagram(&device).await, Some(format!("connect udp:{port},message,message,message\n")));

    // Another address is another connection.
    let other = udp_device(port).await;
    other.send(b"x").await.unwrap();
    let other_id = datagram(&other).await.unwrap();
    assert_ne!(other_id, id);
    assert_eq!(site.config.connections.open("syslog"), 2);
}

#[tokio::test]
async fn a_quiet_udp_remote_is_closed_and_its_next_datagram_opens_a_new_connection() {
    let port = free_udp_port();
    let limits = connections::Limits { udp_idle: Duration::from_millis(300), ..connections::Limits::default() };
    let site = site_with_ports(limits, &format!("{port}/udp=syslog")).await;
    app_on(&site.config, "syslog", &[udp(port)]);
    let (mut watcher, _) = open(&site, "/p/syslog/ws?topic=closed", None).await;

    let device = udp_device(port).await;
    device.send(b"a").await.unwrap();
    let first = datagram(&device).await.unwrap();
    datagram(&device).await;
    assert_eq!(next_text(&mut watcher).await, Some(format!("{}:connect udp:{port},message,close", device.local_addr().unwrap())));
    device.send(b"b").await.unwrap();
    let second = datagram(&device).await.unwrap();
    assert!(second.starts_with("id:"));
    assert_ne!(first, second, "the quiet remote kept its connection");
}

#[tokio::test]
async fn udp_datagrams_past_an_addresss_rate_are_dropped() {
    let port = free_udp_port();
    let limits = connections::Limits { udp_per_second: 5, ..connections::Limits::default() };
    let site = site_with_ports(limits, &format!("{port}/udp=syslog")).await;
    app_on(&site.config, "syslog", &[udp(port)]);
    let device = udp_device(port).await;
    for n in 0..20 {
        device.send(format!("burst {n}").as_bytes()).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // Drain the echoes, then ask how many arrived.
    while datagram_within(&device, Duration::from_millis(500)).await.is_some() {}
    device.send(b"log\n").await.unwrap();
    let log = loop {
        let got = datagram(&device).await.expect("no log");
        if got.starts_with("connect") {
            break got;
        }
    };
    let messages = log.matches("message").count();
    assert_eq!(messages, 6, "five of the burst and the log request, got {log}");
}

#[tokio::test]
async fn a_hidden_apps_udp_port_takes_nothing() {
    let port = free_udp_port();
    let site = site_with_ports(connections::Limits::default(), &format!("{port}/udp=syslog")).await;
    app_on(&site.config, "syslog", &[udp(port)]);
    edit_meta(&site.config, "syslog", |meta| meta.hidden = true);
    let device = udp_device(port).await;
    device.send(b"x").await.unwrap();
    assert_eq!(datagram_within(&device, Duration::from_secs(1)).await, None);
    assert_eq!(site.config.connections.open("syslog"), 0);
}

#[tokio::test]
async fn the_device_tokens_sidecar_is_never_served_and_goes_to_the_trash_with_the_app() {
    let site = site(connections::Limits::default()).await;
    app(&site.config, "broker");
    toolsite::platform::devices::create(&site.config, "broker", "boiler").unwrap();
    assert!(site.config.data_dir.join("broker.devices").is_file());
    for path in ["/p/broker.devices", "/p/broker/../broker.devices"] {
        let status = reqwest::get(format!("http://{}{path}", site.addr)).await.unwrap().status().as_u16();
        assert_eq!(status, 404, "{path} was served");
    }
    toolsite::platform::trash::remove(&site.config, "broker", 1).unwrap();
    assert!(!site.config.data_dir.join("broker.devices").exists(), "the tokens outlived the app");
}
