//! What the TCP and UDP transports write to the log when they turn
//! something away. Alone in its own binary: it installs the process's
//! subscriber, which no other test should share.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};
use toolsite::{
    build_router,
    content::store::{self, PortProtocol, PortSocket},
    runtime::{connections, wasm::Runtime},
    Config,
};

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

/// Everything logged, from every thread.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn hide(config: &Config, app: &str, hidden: bool) {
    let mut meta = store::read_meta_blocking(config, app);
    meta.hidden = hidden;
    store::write_meta_blocking(config, app, &meta).unwrap();
}

/// Reads one line, waiting long: the first event compiles the handler.
async fn line(stream: &mut TcpStream) -> Option<String> {
    let mut read = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !read.ends_with(b"\n") {
        let mut byte = [0u8; 1];
        match tokio::time::timeout_at(deadline, stream.read(&mut byte)).await {
            Ok(Ok(1)) => read.push(byte[0]),
            _ => return None,
        }
    }
    Some(String::from_utf8_lossy(&read).trim_end().to_string())
}

#[tokio::test]
async fn refusals_are_logged_at_warn_without_tokens_or_payload_bytes() {
    let captured = Captured::default();
    tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .init();

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dgram = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let limits = connections::Limits { udp_per_second: 1, ..Default::default() };
    let config = Arc::new(Config {
        connections: Arc::new(connections::Hub::new(limits)),
        ports: toolsite::platform::ports::PortMap {
            bind: "127.0.0.1".parse().unwrap(),
            mappings: toolsite::platform::ports::parse(&format!("{port}=broker,{dgram}/udp=broker")).unwrap(),
        },
        ..Config::local(dir.path().to_path_buf(), "test-token")
    });
    let runtime = Runtime::new().unwrap();
    let _router = build_router(config.clone(), runtime.clone());
    toolsite::platform::ports::listen(config.clone(), runtime).await.unwrap();
    let app = config.data_dir.join("broker");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("handler.wasm"), HANDLER).unwrap();
    std::fs::write(app.join("index.html"), "<title>app</title>").unwrap();
    let mut meta = store::read_meta_blocking(&config, "broker");
    meta.ports = vec![
        PortSocket { protocol: PortProtocol::Tcp, port },
        PortSocket { protocol: PortProtocol::Udp, port: dgram },
    ];
    store::write_meta_blocking(&config, "broker", &meta).unwrap();
    let (_, token) = toolsite::platform::devices::create(&config, "broker", "boiler").unwrap();

    // A token accepted, a wrong one refused, a payload echoed.
    let mut device = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    line(&mut device).await.expect("no greeting");
    device.write_all(format!("token {token}\n").as_bytes()).await.unwrap();
    assert_eq!(line(&mut device).await.as_deref(), Some("ok boiler"));
    device.write_all(b"PAYLOAD-MARKER-TCP\n").await.unwrap();
    line(&mut device).await;
    let mut wrong = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    line(&mut wrong).await;
    wrong.write_all(b"token tsv_WRONG-TOKEN-MARKER\n").await.unwrap();
    assert_eq!(line(&mut wrong).await.as_deref(), Some("denied"));

    // A connection to the hidden app.
    hide(&config, "broker", true);
    let mut refused = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert_eq!(line(&mut refused).await, None);
    hide(&config, "broker", false);

    // Datagrams past the rate.
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(("127.0.0.1", dgram)).await.unwrap();
    for _ in 0..3 {
        udp.send(b"PAYLOAD-MARKER-UDP").await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).to_string();
    assert!(
        log.lines().any(|l| l.contains("WARN") && l.contains("tcp connection refused")),
        "no refusal at warn:\n{log}"
    );
    assert!(
        log.lines().any(|l| l.contains("WARN") && l.contains("udp datagram dropped") && l.contains(&udp.local_addr().unwrap().to_string())),
        "no drop at warn naming its sender:\n{log}"
    );
    assert!(!log.contains(&token[4..]), "a device token reached the log");
    assert!(!log.contains("WRONG-TOKEN-MARKER"), "a presented token reached the log");
    assert!(!log.contains("PAYLOAD-MARKER"), "payload bytes reached the log");
}
