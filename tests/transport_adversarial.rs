//! Attacks on the TCP and UDP transports and on device tokens, over real
//! sockets on 127.0.0.1.
//!
//! The cast: an internet client with no token, a device holding one app's
//! token aiming at another, an app's own handler aiming at another app's
//! connections, a flood, a forged source address and a client too slow to
//! read. Every test is named by the property that holds against them.

use std::{
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};
use toolsite::{
    build_router,
    content::store::{PageMeta, PortProtocol, PortSocket},
    runtime::{connections, wasm::Runtime},
    Config,
};

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

// --- plumbing ---------------------------------------------------------------

struct Site {
    _dir: TempDir,
    config: Arc<Config>,
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

fn free_tcp_port() -> u16 {
    free_port()
}

fn free_udp_port() -> u16 {
    free_port()
}

fn tcp(port: u16) -> PortSocket {
    PortSocket { protocol: PortProtocol::Tcp, port }
}

fn udp(port: u16) -> PortSocket {
    PortSocket { protocol: PortProtocol::Udp, port }
}

/// A site whose owner mapped ports with `map`, as `TOOLSITE_PORTS` would.
async fn site(limits: connections::Limits, map: &str) -> Site {
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
    let runtime = Runtime::new().unwrap();
    let router = build_router(config.clone(), runtime.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    toolsite::platform::ports::listen(config.clone(), runtime).await.unwrap();
    Site { _dir: dir, config }
}

fn edit_meta(config: &Config, name: &str, change: impl FnOnce(&mut PageMeta)) {
    let mut meta = toolsite::content::catalog::meta_blocking(config, name);
    change(&mut meta);
    toolsite::content::catalog::update_meta_blocking(config, name, { let meta = meta.clone(); move |stored| { *stored = meta; Ok(()) } }).unwrap();
}

/// An app with the test handler and `ports` declared.
fn app_on(config: &Config, name: &str, ports: &[PortSocket]) {
    let dir = config.data_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("handler.wasm"), HANDLER).unwrap();
    std::fs::write(dir.join("index.html"), "<title>app</title>").unwrap();
    edit_meta(config, name, |meta| meta.ports = ports.to_vec());
}

fn roomy() -> connections::Limits {
    connections::Limits { rate_per_app: 100_000, ..connections::Limits::default() }
}

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

    /// The next line. The wait is long because the first event compiles the
    /// handler, which in a debug build is slow while other tests compile.
    async fn line(&mut self) -> Option<String> {
        self.line_within(Duration::from_secs(60)).await
    }

    async fn line_within(&mut self, wait: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + wait;
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

    async fn ends(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
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

async fn turned_away(port: u16) -> bool {
    Device::connect(port).await.line().await.is_none()
}

async fn settles_at(config: &Config, app: &str, open: usize) -> bool {
    for _ in 0..250 {
        if config.connections.open(app) == open {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn udp_device(port: u16) -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.connect(("127.0.0.1", port)).await.unwrap();
    socket
}

async fn datagram_within(socket: &UdpSocket, wait: Duration) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; 70_000];
    match tokio::time::timeout(wait, socket.recv(&mut buffer)).await {
        Ok(Ok(n)) => Some(buffer[..n].to_vec()),
        _ => None,
    }
}

async fn datagram(socket: &UdpSocket) -> Option<String> {
    datagram_within(socket, Duration::from_secs(60)).await.map(|b| String::from_utf8_lossy(&b).to_string())
}

/// Opens a UDP remote and returns its connection id.
async fn udp_open(port: u16, first: &[u8]) -> (UdpSocket, String) {
    let device = udp_device(port).await;
    device.send(first).await.unwrap();
    let hello = datagram(&device).await.expect("no id after the first datagram");
    let id = hello.strip_prefix("id:").and_then(|id| id.strip_suffix('\n')).expect("an id").to_string();
    (device, id)
}

// --- which app a port reaches -------------------------------------------------

#[tokio::test]
async fn a_port_reaches_nothing_for_an_app_that_is_hidden_removed_or_does_not_declare_it() {
    let (port, dgram) = (free_tcp_port(), free_udp_port());
    let site = site(roomy(), &format!("{port}=broker,{dgram}/udp=broker")).await;

    // Mapped, but the app does not exist yet, then exists without declaring.
    assert!(turned_away(port).await, "a mapped port reached an app that is not there");
    app_on(&site.config, "broker", &[]);
    assert!(turned_away(port).await, "a port the app does not declare reached it");
    let quiet = udp_device(dgram).await;
    quiet.send(b"x").await.unwrap();
    assert_eq!(datagram_within(&quiet, Duration::from_secs(1)).await, None);

    // Declared: live. Hidden: not.
    app_on(&site.config, "broker", &[tcp(port), udp(dgram)]);
    Device::open(port).await;
    edit_meta(&site.config, "broker", |meta| meta.hidden = true);
    assert!(turned_away(port).await, "a hidden app took a connection");

    // Removed: not, and a device token of the removed app is no token for
    // whatever is published at its name next.
    edit_meta(&site.config, "broker", |meta| meta.hidden = false);
    let (_, token) = toolsite::platform::devices::create(&site.config, "broker", "boiler").await.unwrap();
    toolsite::platform::trash::remove(&site.config, "broker", 1).unwrap();
    assert!(turned_away(port).await, "a removed app took a connection");
    let late = udp_device(dgram).await;
    late.send(b"x").await.unwrap();
    assert_eq!(datagram_within(&late, Duration::from_secs(1)).await, None, "a removed app took a datagram");
    assert_eq!(site.config.connections.open("broker"), 0);

    app_on(&site.config, "broker", &[tcp(port)]);
    let (mut device, _) = Device::open(port).await;
    device.say(&format!("token {token}\n")).await;
    assert_eq!(device.line().await.as_deref(), Some("denied"), "the removed app's token opened its successor");
}

#[tokio::test]
async fn a_port_mapped_to_one_app_never_reaches_another_that_declares_it() {
    let port = free_tcp_port();
    let dgram = free_udp_port();
    let site = site(roomy(), &format!("{port}=owner,{dgram}/udp=owner")).await;
    // The squatter declares both ports; the owner declares neither yet.
    app_on(&site.config, "squatter", &[tcp(port), udp(dgram)]);
    app_on(&site.config, "owner", &[]);
    assert!(turned_away(port).await);
    let device = udp_device(dgram).await;
    device.send(b"x").await.unwrap();
    assert_eq!(datagram_within(&device, Duration::from_secs(1)).await, None);
    assert_eq!(site.config.connections.open("squatter"), 0);
    assert_eq!(site.config.connections.open("owner"), 0);
}

#[tokio::test]
async fn a_manifest_cannot_claim_a_system_port_and_a_map_cannot_give_one() {
    let site = site(roomy(), "").await;
    app_on(&site.config, "decl", &[]);
    for port in [0u32, 22, 80, 443, 1023, 65536] {
        let manifest = format!("[[socket]]\nprotocol = \"tcp\"\nport = {port}\n");
        assert!(toolsite::platform::manifest::apply(&site.config, "decl", &manifest).await.is_err(), "port {port} declared");
        assert!(toolsite::platform::ports::parse(&format!("{port}=decl")).is_err(), "port {port} mapped");
    }
    assert!(toolsite::content::catalog::meta_blocking(&site.config, "decl").ports.is_empty());
}

// --- one app's handler against another's connections --------------------------

#[tokio::test]
async fn a_handler_reaches_no_connection_of_another_app_by_id_tcp_or_udp() {
    let (a, b) = (free_tcp_port(), free_udp_port());
    let site = site(roomy(), &format!("{a}=alpha,{b}/udp=beta")).await;
    app_on(&site.config, "alpha", &[tcp(a)]);
    app_on(&site.config, "beta", &[udp(b)]);

    let (victim, victim_id) = udp_open(b, b"hello").await;
    datagram(&victim).await; // the echo of "hello"
    let (mut attacker, _) = Device::open(a).await;
    attacker.say(&format!("poke {victim_id}\n")).await;
    assert_eq!(
        attacker.line().await.as_deref(),
        Some("send=false state=false set=false remote=false subscribe=false close=false"),
        "alpha's handler reached beta's UDP remote"
    );
    // The victim is untouched: still open, nothing arrived, its state its own.
    assert_eq!(datagram_within(&victim, Duration::from_millis(500)).await, None, "something was sent to the victim");
    victim.send(b"log\n").await.unwrap();
    assert_eq!(datagram(&victim).await, Some(format!("connect udp:{b},message,message\n")));

    // Its own id, once closed, is a handle on nothing either.
    let (mut gone, gone_id) = Device::open(a).await;
    gone.say("close\n").await;
    assert!(gone.ends().await);
    assert!(settles_at(&site.config, "alpha", 1).await);
    attacker.say(&format!("poke {gone_id}\n")).await;
    assert_eq!(
        attacker.line().await.as_deref(),
        Some("send=false state=false set=false remote=false subscribe=false close=false"),
        "a closed connection's id still worked"
    );
}

#[test]
fn connection_ids_are_long_random_and_never_repeat() {
    let hub = Arc::new(connections::Hub::new(connections::Limits { per_app: 100_000, total: 100_000, ..Default::default() }));
    let mut seen = std::collections::HashSet::new();
    for _ in 0..5_000 {
        let (registration, _rx) = hub.register("app", None).unwrap();
        assert!(registration.id.len() >= 24 && registration.id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(seen.insert(registration.id.clone()), "an id came round again");
    }
}

// --- UDP against a third party ---------------------------------------------------

#[tokio::test]
async fn one_forged_udp_datagram_buys_a_few_hundred_bytes_not_a_flood() {
    let port = free_udp_port();
    let site = site(roomy(), &format!("{port}/udp=syslog")).await;
    app_on(&site.config, "syslog", &[udp(port)]);

    // The "victim" is whoever the source address names: here, this socket.
    let victim = udp_device(port).await;
    let ask = b"amplify 200 1200\n";
    victim.send(ask).await.unwrap();
    let mut received = 0;
    while let Some(bytes) = datagram_within(&victim, Duration::from_secs(if received == 0 { 60 } else { 2 })).await {
        received += bytes.len();
    }
    assert!(received > 0, "not even the greeting arrived");
    assert!(
        received <= 128 + 3 * ask.len(),
        "a {}-byte datagram drew {received} bytes of replies",
        ask.len()
    );

    // A device that keeps talking keeps getting answers in proportion.
    let device = udp_device(port).await;
    device.send(b"x").await.unwrap();
    datagram(&device).await;
    datagram(&device).await;
    for n in 0..10 {
        let reading = format!("reading {n} {}", "x".repeat(400));
        device.send(reading.as_bytes()).await.unwrap();
        let echo = datagram_within(&device, Duration::from_secs(5)).await;
        assert_eq!(echo.as_deref(), Some(reading.as_bytes()), "a talking device lost its echo");
    }
}

#[tokio::test]
async fn the_udp_rate_is_per_address_however_many_source_ports_it_uses() {
    let port = free_udp_port();
    let limits = connections::Limits { udp_per_second: 5, ..roomy() };
    let site = site(limits, &format!("{port}/udp=syslog")).await;
    app_on(&site.config, "syslog", &[udp(port)]);
    let mut sockets = Vec::new();
    for _ in 0..20 {
        let socket = udp_device(port).await;
        socket.send(b"x").await.unwrap();
        sockets.push(socket);
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let opened = site.config.connections.open("syslog");
    assert!(opened <= 5, "20 source ports on one address opened {opened} remotes in one second");
}

#[tokio::test]
async fn an_apps_live_udp_remotes_are_capped_and_the_rest_are_dropped_unopened() {
    let port = free_udp_port();
    let limits = connections::Limits { raw_per_app: 3, per_ip: 1000, ..roomy() };
    let site = site(limits, &format!("{port}/udp=syslog")).await;
    app_on(&site.config, "syslog", &[udp(port)]);
    // One first, so the handler is compiled before the rest arrive.
    let (_first, _) = udp_open(port, b"x").await;
    let mut answered = 1;
    let mut sockets = Vec::new();
    for _ in 0..7 {
        let socket = udp_device(port).await;
        socket.send(b"x").await.unwrap();
        sockets.push(socket);
    }
    for socket in &sockets {
        if datagram_within(socket, Duration::from_secs(3)).await.is_some() {
            answered += 1;
        }
    }
    assert_eq!(answered, 3, "remotes past the app's ceiling were opened");
    assert_eq!(site.config.connections.open("syslog"), 3);
}

#[tokio::test]
async fn a_datagram_to_a_declared_but_unmapped_port_reaches_nothing() {
    let port = free_udp_port();
    let site = site(roomy(), "").await;
    app_on(&site.config, "syslog", &[udp(port)]);
    let device = udp_device(port).await;
    let _ = device.send(b"x").await;
    assert_eq!(datagram_within(&device, Duration::from_secs(1)).await, None);
    assert_eq!(site.config.connections.open("syslog"), 0);
}

// --- TCP floods and slow clients -------------------------------------------------

#[tokio::test]
async fn racing_connects_never_get_past_the_per_address_or_per_app_ceiling() {
    for (limits, most) in [
        (connections::Limits { per_ip: 3, ..roomy() }, 3),
        (connections::Limits { raw_per_app: 4, ..roomy() }, 4),
    ] {
        let port = free_tcp_port();
        let site = site(limits, &format!("{port}=broker")).await;
        app_on(&site.config, "broker", &[tcp(port)]);
        // Compile the handler first, so the race is about the ceilings.
        drop(Device::open(port).await);
        assert!(settles_at(&site.config, "broker", 0).await);

        let racers: Vec<_> = (0..30)
            .map(|_| {
                tokio::spawn(async move {
                    let mut device = Device::connect(port).await;
                    let hello = device.line_within(Duration::from_secs(30)).await;
                    (device, hello.is_some())
                })
            })
            .collect();
        let mut admitted = Vec::new();
        for racer in racers {
            let (device, got_in) = racer.await.unwrap();
            if got_in {
                admitted.push(device);
            }
        }
        assert_eq!(admitted.len(), most, "a race let {} connections in", admitted.len());
        assert_eq!(site.config.connections.open("broker"), most);
    }
}

#[tokio::test]
async fn a_client_that_never_reads_keeps_its_place_counted_until_it_is_closed() {
    let port = free_tcp_port();
    let limits = connections::Limits {
        per_ip: 1,
        tcp_send_timeout: Duration::from_secs(10),
        ..roomy()
    };
    let site = site(limits, &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (device, _) = Device::open(port).await;

    // Ask for far more than the buffers between us hold, and read none of
    // it: the replies back up until the hub cuts the connection off.
    let (_reader, mut writer) = device.stream.into_split();
    writer.write_all(b"amplify 400 60000\n").await.unwrap();
    // Stuck on a peer that does not read, and cut off by the hub for
    // falling behind, the connection still holds its address's one place.
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(turned_away(port).await, "a second connection got in while the first held its place unread");
    }

    // Then the stuck write times out, the connection goes and its place
    // with it.
    assert!(settles_at(&site.config, "broker", 0).await, "the unread connection was never closed");
    Device::open(port).await;
}

#[tokio::test]
async fn half_closed_and_reset_connections_leave_nothing_in_the_registry() {
    let port = free_tcp_port();
    let site = site(roomy(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    drop(Device::open(port).await);
    for n in 0..10 {
        let (mut device, _) = Device::open(port).await;
        device.say("reading").await;
        if n % 2 == 0 {
            device.stream.shutdown().await.unwrap();
            let mut rest = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(10), device.stream.read_to_end(&mut rest)).await;
        } else {
            // A reset rather than a goodbye.
            device.stream.set_zero_linger().unwrap();
            drop(device);
        }
    }
    assert!(settles_at(&site.config, "broker", 0).await, "a half-closed or reset connection stayed registered");
}

#[tokio::test]
async fn a_ten_megabyte_burst_arrives_whole_and_in_order() {
    let port = free_tcp_port();
    let site = site(roomy(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (device, _) = Device::open(port).await;
    let sent: Vec<u8> = (0..10 * 1024 * 1024u32).map(|n| (n % 253) as u8).collect();
    let (mut reader, mut writer) = device.stream.into_split();
    let writing = {
        let sent = sent.clone();
        tokio::spawn(async move { writer.write_all(&sent).await.unwrap() })
    };
    // The echo is the handler sending back each message it got, and a
    // message over 64 KB would have been refused, so a whole echo also
    // proves every read was at most 64 KB.
    let mut echoed = device.read;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while echoed.len() < sent.len() {
        let mut chunk = vec![0u8; 256 * 1024];
        match tokio::time::timeout_at(deadline, reader.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => echoed.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    writing.await.unwrap();
    assert_eq!(echoed.len(), sent.len(), "the burst came back short");
    assert!(echoed == sent, "the burst came back out of order");
}

#[tokio::test]
async fn a_handler_that_traps_or_hangs_closes_only_its_own_connection() {
    let port = free_tcp_port();
    let site = site(roomy(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (mut bystander, _) = Device::open(port).await;

    for poison in ["trap\n", "spin\n"] {
        let (mut victim, _) = Device::open(port).await;
        victim.say(poison).await;
        // While the handler is stuck on one connection, another is served.
        bystander.say("still here\n").await;
        let mut heard = String::new();
        while heard.len() < "still here\n".len() {
            let mut chunk = [0u8; 64];
            let n = tokio::time::timeout(Duration::from_secs(4), bystander.stream.read(&mut chunk))
                .await
                .expect("a stuck handler on one connection held up another")
                .unwrap();
            heard.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
        assert!(victim.ends().await, "{poison:?} left its connection open");
        assert!(settles_at(&site.config, "broker", 1).await, "{poison:?} left its connection registered");
    }
}

// --- device tokens ------------------------------------------------------------

#[tokio::test]
async fn a_device_presenting_any_token_but_its_own_apps_live_one_is_denied() {
    let port = free_tcp_port();
    let site = site(roomy(), &format!("{port}=broker")).await;
    app_on(&site.config, "broker", &[tcp(port)]);
    let (_, real) = toolsite::platform::devices::create(&site.config, "broker", "boiler").await.unwrap();
    let (_, other) = toolsite::platform::devices::create(&site.config, "syslog", "boiler").await.unwrap();
    let near_miss = format!("{}{}", &real[..real.len() - 1], if real.ends_with('a') { 'b' } else { 'a' });
    let long = format!("tsv_{}", "a".repeat(3_000));
    let wrong = [other, near_miss, long, "tsv_".to_string(), real.to_uppercase(), format!("{real}x")];
    for token in &wrong {
        let (mut device, _) = Device::open(port).await;
        device.say(&format!("token {token}\n")).await;
        assert_eq!(device.line().await.as_deref(), Some("denied"), "{} let a device in", &token[..token.len().min(40)]);
    }
    // Bytes that are not UTF-8 reach the handler as bytes and are no token.
    let (mut device, _) = Device::open(port).await;
    let mut bytes = b"token ".to_vec();
    bytes.extend_from_slice(real.as_bytes());
    bytes[10] = 0xff;
    bytes.push(b'\n');
    device.stream.write_all(&bytes).await.unwrap();
    assert_eq!(device.line().await.as_deref(), Some("denied"));
    let (mut device, _) = Device::open(port).await;
    device.say(&format!("token {real}\n")).await;
    assert_eq!(device.line().await.as_deref(), Some("ok boiler"));
    // Past what any read carries, straight to the check.
    let huge = format!("{real}{}", "a".repeat(1 << 20));
    assert_eq!(toolsite::platform::devices::check(&site.config, "broker", &huge), None);
}
