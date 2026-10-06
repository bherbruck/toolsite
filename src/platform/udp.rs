//! UDP: a port the site's owner mapped to an app, one datagram at a time.
//!
//! UDP has no connection, so toolsite makes one per remote address: the
//! first datagram from an address opens it (`connect`, then the datagram as
//! a `message`), each later one is a `message` on the same connection, and
//! an address that stays quiet for the idle timeout gets `close`. That is
//! what lets `send(conn, data)` reply to the right address and per-connection
//! state work as it does for a socket.
//!
//! Datagrams are never queued without bound and never refused loudly: one
//! over 64 KB, one past an address's rate, or one that arrives while its
//! connection's queue is full is dropped, as the network might have
//! dropped it.

use crate::{
    content::store::PortSocket,
    platform::connections::{self, Door, Incoming, Refusal, Transport},
    runtime::{connections::Message, connections::MAX_MESSAGE_BYTES, wasm::ConnectInfo},
    AppState,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::mpsc};

/// Datagrams waiting for one remote's connection before more are dropped.
const QUEUE: usize = 64;

/// Each remote address's open connection, by the queue its datagrams go to.
type Peers = Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>>;

/// How many datagrams one address sent in the current second.
struct Rate {
    window_start: Instant,
    seen: u32,
}

/// Takes datagrams on one mapped port until the process ends.
pub async fn serve(state: AppState, socket: UdpSocket, app: String, port: PortSocket) {
    let socket = Arc::new(socket);
    let peers: Peers = Arc::default();
    let mut rates: HashMap<IpAddr, Rate> = HashMap::new();
    let mut last_sweep = Instant::now();
    let mut last_warning: Option<Instant> = None;
    // One byte more than a message may carry, so an oversized datagram is
    // seen as one rather than cut to fit.
    let mut buffer = vec![0u8; MAX_MESSAGE_BYTES + 1];
    let per_second = state.config.connections.limits.udp_per_second;

    loop {
        let (read, from) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            Err(error) => {
                // On some systems an ICMP "port unreachable" for an earlier
                // reply surfaces here; it is about that reply, not this port.
                tracing::debug!(app = %app, port = %port, %error, "udp receive failed");
                continue;
            }
        };
        let now = Instant::now();
        let mut warn = |why: &str| {
            // One line a second, not one per dropped datagram.
            if last_warning.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
                last_warning = Some(now);
                tracing::warn!(app = %app, port = %port, peer = %from, "udp datagram dropped: {why}");
            }
        };
        if read > MAX_MESSAGE_BYTES {
            warn("over 64 KB");
            continue;
        }

        if now.duration_since(last_sweep) >= Duration::from_secs(10) {
            rates.retain(|_, rate| now.duration_since(rate.window_start) < Duration::from_secs(1));
            last_sweep = now;
        }
        let rate = rates.entry(from.ip()).or_insert(Rate { window_start: now, seen: 0 });
        if now.duration_since(rate.window_start) >= Duration::from_secs(1) {
            *rate = Rate { window_start: now, seen: 0 };
        }
        if rate.seen >= per_second {
            warn("the address sends faster than its limit per second");
            continue;
        }
        rate.seen += 1;

        let datagram = buffer[..read].to_vec();
        let known = peers.lock().unwrap().get(&from).cloned();
        if let Some(tx) = known {
            match tx.try_send(datagram) {
                Ok(()) => continue,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn("the app is behind on this address");
                    continue;
                }
                // Its connection just ended; this datagram opens a new one.
                Err(mpsc::error::TrySendError::Closed(datagram)) => {
                    open_peer(&state, &socket, &peers, &app, port, from, datagram).await;
                }
            }
        } else {
            open_peer(&state, &socket, &peers, &app, port, from, datagram).await;
        }
    }
}

/// Starts a connection for a new remote, with its first datagram queued.
async fn open_peer(
    state: &AppState,
    socket: &Arc<UdpSocket>,
    peers: &Peers,
    app: &str,
    port: PortSocket,
    from: SocketAddr,
    first: Vec<u8>,
) {
    if !crate::platform::ports::admits(&state.config, app, port).await {
        tracing::warn!(app = %app, port = %port, peer = %from, "udp datagram dropped: the app is hidden, gone, or does not declare this port");
        return;
    }
    let (tx, rx) = mpsc::channel(QUEUE);
    let _ = tx.try_send(first);
    peers.lock().unwrap().insert(from, tx.clone());

    let (state, socket, peers, app) = (state.clone(), socket.clone(), peers.clone(), app.to_string());
    tokio::spawn(async move {
        let info = ConnectInfo {
            socket: port.to_string(),
            path: String::new(),
            query: String::new(),
            headers: Vec::new(),
        };
        let idle = state.config.connections.limits.udp_idle;
        match connections::open(state, app.clone(), Door::Port(port), None, Some(from), info).await {
            Ok(session) => {
                let transport = UdpTransport {
                    socket,
                    peer: from,
                    queue: rx,
                    idle,
                    last_heard: Instant::now(),
                };
                connections::run(transport, session).await;
            }
            Err(Refusal::NotOffered) => {
                tracing::warn!(app = %app, port = %port, peer = %from, "udp remote refused: the app's handler does not export on-connection")
            }
            Err(Refusal::Full(why)) => tracing::warn!(app = %app, port = %port, peer = %from, "udp remote refused: {why}"),
            Err(Refusal::Refused(why)) => {
                tracing::warn!(app = %app, port = %port, peer = %from, "udp remote refused by the app: {why}")
            }
            Err(Refusal::Failed(why)) => {
                tracing::warn!(app = %app, port = %port, peer = %from, "udp remote refused: the handler failed on connect: {why}")
            }
        }
        // Only this connection's own entry: a newer one for the same
        // address may already have taken its place.
        let mut peers = peers.lock().unwrap();
        if peers.get(&from).is_some_and(|current| current.same_channel(&tx)) {
            peers.remove(&from);
        }
    });
}

struct UdpTransport {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    queue: mpsc::Receiver<Vec<u8>>,
    idle: Duration,
    last_heard: Instant,
}

impl UdpTransport {
    /// Refuses datagrams from now on, while the close event still runs, so
    /// the next one from this address opens a new connection rather than
    /// waiting in a queue nobody reads.
    fn stop_taking(&mut self) {
        self.queue.close();
    }
}

impl Transport for UdpTransport {
    async fn recv(&mut self) -> Incoming {
        let deadline = tokio::time::Instant::from_std(self.last_heard + self.idle);
        match tokio::time::timeout_at(deadline, self.queue.recv()).await {
            Ok(Some(datagram)) => {
                self.last_heard = Instant::now();
                Incoming::Message(Message::Binary(datagram))
            }
            Ok(None) | Err(_) => {
                self.stop_taking();
                Incoming::Ended
            }
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), ()> {
        let bytes = match message {
            Message::Text(text) => text.into_bytes(),
            Message::Binary(bytes) => bytes,
        };
        // A reply the network refuses is lost as any datagram may be; it
        // does not end the connection.
        let _ = self.socket.send_to(&bytes, self.peer).await;
        Ok(())
    }

    async fn ping(&mut self) -> Result<(), ()> {
        Ok(())
    }

    /// UDP has nothing to send on close; the remote's next datagram opens
    /// a new connection.
    async fn close(&mut self) {
        self.stop_taking();
    }

    fn answers_pings(&self) -> bool {
        false
    }

    fn holds_back(&self) -> bool {
        true
    }
}
