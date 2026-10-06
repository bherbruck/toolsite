//! TCP: a port the site's owner mapped to an app, carried as a connection.
//!
//! Each accepted connection becomes `connect`, then a `message` for each
//! read of up to 64 KB, in order, then `close`, through the same
//! `platform::connections` a WebSocket goes through. A read is not a
//! message boundary the sender chose: framing is the app's protocol.
//!
//! No gate stands in front: a device carries no cookie. The app decides who
//! stays, usually by asking `auth.check-token` about what the device sent
//! first. A connection that sends nothing for the idle timeout is closed,
//! and while the handler is behind nothing more is read, so a fast sender
//! meets TCP's own flow control rather than an unbounded queue.

use crate::{
    content::store::PortSocket,
    platform::connections::{self, Door, Incoming, Refusal, Transport},
    runtime::{connections::Message, connections::MAX_MESSAGE_BYTES, wasm::ConnectInfo},
    AppState,
};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
};

/// Takes connections on one mapped port until the process ends.
pub async fn serve(state: AppState, listener: TcpListener, app: String, socket: PortSocket) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(accept(state.clone(), stream, peer, app.clone(), socket));
            }
            Err(error) => {
                // Out of file descriptors, most likely; accepting again at
                // once would spin.
                tracing::warn!(app = %app, port = %socket, %error, "tcp accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn accept(state: AppState, stream: TcpStream, peer: SocketAddr, app: String, socket: PortSocket) {
    if !crate::platform::ports::admits(&state.config, &app, socket).await {
        tracing::warn!(app = %app, port = %socket, peer = %peer, "tcp connection refused: the app is hidden, gone, or does not declare this port");
        return;
    }
    let _ = stream.set_nodelay(true);
    let info = ConnectInfo {
        socket: socket.to_string(),
        path: String::new(),
        query: String::new(),
        headers: Vec::new(),
    };
    let (idle, send_timeout) = (state.config.connections.limits.tcp_idle, state.config.connections.limits.tcp_send_timeout);
    match connections::open(state, app.clone(), Door::Port(socket), None, Some(peer), info).await {
        Ok(session) => connections::run(TcpTransport::new(stream, idle, send_timeout), session).await,
        Err(Refusal::NotOffered) => {
            tracing::warn!(app = %app, port = %socket, peer = %peer, "tcp connection refused: the app's handler does not export on-connection")
        }
        Err(Refusal::Full(why)) => tracing::warn!(app = %app, port = %socket, peer = %peer, "tcp connection refused: {why}"),
        Err(Refusal::Refused(why)) => {
            tracing::warn!(app = %app, port = %socket, peer = %peer, "tcp connection refused by the app: {why}")
        }
        Err(Refusal::Failed(why)) => {
            tracing::warn!(app = %app, port = %socket, peer = %peer, "tcp connection refused: the handler failed on connect: {why}")
        }
    }
}

struct TcpTransport {
    read: OwnedReadHalf,
    write: OwnedWriteHalf,
    buffer: Vec<u8>,
    idle: Duration,
    send_timeout: Duration,
    /// When the other side last sent anything. Kept here rather than as a
    /// timer per read, since `recv` is dropped and called again whenever
    /// anything else happens on the connection.
    last_heard: Instant,
}

impl TcpTransport {
    fn new(stream: TcpStream, idle: Duration, send_timeout: Duration) -> Self {
        let (read, write) = stream.into_split();
        Self {
            read,
            write,
            buffer: vec![0; MAX_MESSAGE_BYTES],
            idle,
            send_timeout,
            last_heard: Instant::now(),
        }
    }
}

impl Transport for TcpTransport {
    async fn recv(&mut self) -> Incoming {
        let deadline = tokio::time::Instant::from_std(self.last_heard + self.idle);
        match tokio::time::timeout_at(deadline, self.read.read(&mut self.buffer)).await {
            Ok(Ok(0)) | Ok(Err(_)) => Incoming::Ended,
            Ok(Ok(read)) => {
                self.last_heard = Instant::now();
                Incoming::Message(Message::Binary(self.buffer[..read].to_vec()))
            }
            Err(_) => {
                tracing::info!(idle_seconds = self.idle.as_secs(), "tcp connection closed: idle");
                Incoming::Ended
            }
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), ()> {
        let bytes = match message {
            Message::Text(text) => text.into_bytes(),
            Message::Binary(bytes) => bytes,
        };
        // A peer that stops reading would otherwise hold this connection's
        // loop, and its place under the ceilings, for as long as it liked.
        match tokio::time::timeout(self.send_timeout, self.write.write_all(&bytes)).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(()),
        }
    }

    async fn ping(&mut self) -> Result<(), ()> {
        Ok(())
    }

    async fn close(&mut self) {
        let _ = self.write.shutdown().await;
    }

    fn answers_pings(&self) -> bool {
        false
    }

    fn holds_back(&self) -> bool {
        true
    }
}
