//! A live connection's life, whatever carries it: open, events to the
//! handler, re-checks, close.
//!
//! The transport (a WebSocket, a TCP connection, a UDP remote) only moves
//! frames. Everything else is here, once: the handler is asked whether to
//! accept, gets each frame as an event, one at a time and in order, and gets
//! `close` however the connection ended. Every `check_every` the connection
//! is pinged and its access decided again, as if it opened its door now; a
//! disabled account, a hidden app, a withdrawn socket, a port no longer
//! mapped or a gate that no longer admits the person closes it.
//!
//! A new transport implements `Transport` and calls `open` and `run`; the
//! handler cannot tell which one carried the bytes.

use crate::{
    accounts::users::{self, User},
    content::store::PortSocket,
    runtime::{
        connections::{Message, Outgoing, Registration},
        wasm::{ConnectInfo, ConnectionEvent, ConnectionMessage, User as GuestUser},
    },
    AppState,
};
use std::{future::Future, net::SocketAddr, time::Instant};
use tokio::sync::mpsc;

/// Frames waiting for the handler. A WebSocket that fills this is closed for
/// sending faster than the app can answer.
const EVENT_QUEUE: usize = 64;
/// The same for a transport that holds back, which stops reading instead.
/// Kept short: each frame may be 64 KB, and a device behind a slow handler
/// should wait in its own kernel buffers, not in this process's memory.
const HELD_QUEUE: usize = 2;

/// What a transport hands up.
pub enum Incoming {
    Message(Message),
    /// The answer to a ping.
    Pong,
    /// The other side went away, or the transport failed.
    Ended,
}

/// One way of carrying a connection's frames. It moves bytes and nothing
/// else; who may connect and what happens to a frame are decided above it.
pub trait Transport: Send + 'static {
    fn recv(&mut self) -> impl Future<Output = Incoming> + Send;
    fn send(&mut self, message: Message) -> impl Future<Output = Result<(), ()>> + Send;
    fn ping(&mut self) -> impl Future<Output = Result<(), ()>> + Send;
    fn close(&mut self) -> impl Future<Output = ()> + Send;

    /// Whether the other side answers `ping` with `Incoming::Pong`. One that
    /// does not keeps itself honest another way, such as an idle timeout in
    /// `recv`.
    fn answers_pings(&self) -> bool {
        true
    }

    /// Whether frames wait in the transport while the handler is behind,
    /// rather than the connection closing. TCP's own flow control, or a
    /// dropped datagram, is the right answer to a fast sender there.
    fn holds_back(&self) -> bool {
        false
    }
}

/// What a connection came in through, and so what decides whether it may
/// stay.
#[derive(Debug, Clone)]
pub enum Door {
    /// A declared socket path, behind the app's gate for that path.
    Path(String),
    /// A port the site's owner mapped to the app. No gate: the app decides
    /// with `auth.check-token`.
    Port(PortSocket),
}

/// Why a connection was not opened.
pub enum Refusal {
    /// The app's handler does not export `on-connection`.
    NotOffered,
    /// A ceiling on open connections.
    Full(String),
    /// The handler's own answer to `connect`.
    Refused(String),
    /// The handler failed while deciding.
    Failed(String),
}

/// An accepted connection, ready for its transport.
pub struct Session {
    state: AppState,
    app: String,
    door: Door,
    visitor: Option<User>,
    /// The raw app session token the visitor came in with.
    session: Option<String>,
    registration: Registration,
    outgoing: mpsc::Receiver<Outgoing>,
}

fn guest_user(visitor: &Option<User>) -> Option<GuestUser> {
    visitor.as_ref().map(|user| GuestUser {
        id: user.id.clone(),
        email: user.email.clone(),
    })
}

/// Runs one event through the app's handler, reading the handler from disk
/// as a request does, so a redeploy takes effect on the next event. An app
/// that runs resident gets it on its one long-lived instance, in the order
/// events from all its connections arrive; any other gets a fresh one.
async fn deliver(
    state: &AppState,
    app: &str,
    visitor: &Option<User>,
    conn: &str,
    event: ConnectionEvent,
) -> Result<Option<Result<(), String>>, String> {
    let meta = crate::content::catalog::meta(&state.config, app).await;
    // Each event is a call like a request, with the request's limits.
    let guards = state.config.limits.effective(meta.limits.as_ref()).request;
    if let Some(resident) = meta.resident {
        // Read only when the instance starts: a queued event holds no copy.
        if !crate::content::serve::has_handler(&state.config, app).await {
            return Ok(None);
        }
        let settings = state.config.residents.settings(resident.memory_mb, resident.tick_ms);
        let load = crate::content::serve::handler_wasm_blocking;
        return state
            .config
            .residents
            .deliver(&state.runtime, &state.config, load, app, settings, guest_user(visitor), conn, event, guards)
            .await
            .map(Some);
    }
    let Some(handler) = crate::content::serve::handler_wasm(&state.config, app).await else {
        return Ok(None);
    };
    let (runtime, config, app, user, conn) =
        (state.runtime.clone(), state.config.clone(), app.to_string(), guest_user(visitor), conn.to_string());
    tokio::task::spawn_blocking(move || {
        runtime.connection_event(config, &app, handler.generation, &handler.wasm, user, &conn, event, guards)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// A signed-in visitor, and the app session they proved it with. The
/// connection is theirs only while that session lasts: signing out, a new
/// password, turning on two-step sign-in or an admin's reset ends it, and
/// the connection closes at the next check, as a request with the cookie
/// would be refused.
pub struct Visitor {
    pub user: User,
    pub session: String,
}

/// Registers a connection and asks the handler whether to accept it.
pub async fn open(
    state: AppState,
    app: String,
    door: Door,
    visitor: Option<Visitor>,
    remote: Option<SocketAddr>,
    info: ConnectInfo,
) -> Result<Session, Refusal> {
    let (visitor, session) = match visitor {
        Some(Visitor { user, session }) => (Some(user), Some(session)),
        None => (None, None),
    };
    let Some(handler) = crate::content::serve::handler_wasm(&state.config, &app).await else {
        return Err(Refusal::NotOffered);
    };
    // The ceilings first: they cost a lock, where asking the handler costs
    // an instance, and a flood should be turned away for the cheap reason.
    let (registration, outgoing) = state
        .config
        .connections
        .register_from(&app, visitor.as_ref().map(|user| user.id.as_str()), remote)
        .map_err(Refusal::Full)?;
    let takes = {
        let (runtime, app) = (state.runtime.clone(), app.clone());
        tokio::task::spawn_blocking(move || runtime.takes_connections(&app, handler.generation, &handler.wasm)).await
    };
    match takes {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return Err(Refusal::NotOffered),
        Ok(Err(e)) => return Err(Refusal::Failed(format!("{e:#}"))),
        Err(e) => return Err(Refusal::Failed(e.to_string())),
    }
    match deliver(&state, &app, &visitor, &registration.id, ConnectionEvent::Connect(info)).await {
        Ok(Some(Ok(()))) => {
            // What other events sent while `connect` ran follows what it sent.
            state.config.connections.accept(&app, &registration.id);
            Ok(Session {
            state,
            app,
            door,
            visitor,
            session,
            registration,
            outgoing,
        })
        }
        Ok(Some(Err(reason))) => Err(Refusal::Refused(reason)),
        Ok(None) => Err(Refusal::NotOffered),
        Err(why) => Err(Refusal::Failed(why)),
    }
}

/// Whether `user` (or nobody) may still hold this socket: the app exists,
/// is not hidden, still declares the socket, and its access for the
/// socket's path admits them. A port asks only that it is still mapped to
/// the app and declared by it.
async fn may_stay(state: &AppState, app: &str, door: &Door, user: Option<&User>) -> bool {
    let config = &state.config;
    let socket = match door {
        Door::Path(socket) => socket,
        Door::Port(port) => return crate::platform::ports::admits(config, app, *port).await,
    };
    if crate::content::store::is_hidden(config, app).await || !crate::content::store::app_exists(config, app).await {
        return false;
    }
    if !crate::content::catalog::meta(config, app).await.sockets.iter().any(|s| s == socket) {
        return false;
    }
    let gate = crate::content::store::effective_gate(config, app, socket).await.gate;
    crate::content::serve::admits(config, &gate, app, user).await
}

/// Carries an accepted connection until it ends, then delivers `close`.
pub async fn run<T: Transport>(mut transport: T, session: Session) {
    let Session {
        state,
        app,
        door,
        visitor,
        session,
        registration,
        mut outgoing,
    } = session;
    let conn = registration.id.clone();

    // Events run in their own task, one at a time, so a slow handler never
    // stops this loop from sending, pinging or noticing the browser left.
    let holds_back = transport.holds_back();
    let (events, mut queue) = mpsc::channel::<ConnectionEvent>(if holds_back { HELD_QUEUE } else { EVENT_QUEUE });
    let worker = {
        let (state, app, visitor, conn) = (state.clone(), app.clone(), visitor.clone(), conn.clone());
        tokio::spawn(async move {
            while let Some(event) = queue.recv().await {
                let closing = matches!(event, ConnectionEvent::Close);
                match deliver(&state, &app, &visitor, &conn, event).await {
                    Ok(Some(Err(why))) => tracing::info!(app = %app, "connection handler answered an error: {why}"),
                    // A trap, or a handler stopped at its time or fuel: what
                    // it meant to do with this connection is unknown, so the
                    // connection ends rather than carry on half handled.
                    Err(why) => {
                        tracing::warn!(app = %app, "connection closed: the handler failed: {why}");
                        if !closing {
                            let _ = state.config.connections.close(&app, &conn, None);
                        }
                    }
                    _ => {}
                }
                if closing {
                    break;
                }
            }
            // The connection, its topics and its state go only now, so the
            // close event could still read them.
            drop(registration);
        })
    };

    let mut checks = tokio::time::interval(state.config.connections.limits.check_every);
    checks.tick().await;
    let mut last_pong = Instant::now();
    let mut pinged_at: Option<Instant> = None;
    let user_id = visitor.as_ref().map(|user| user.id.clone());
    let answers_pings = transport.answers_pings();
    // The other side stopped sending. A TCP peer that half-closed still
    // reads, so replies to what it sent last are still delivered.
    let mut sender_done = false;

    loop {
        tokio::select! {
            out = outgoing.recv() => {
                match out {
                    // Cut off for falling behind: what is still queued is
                    // not worth waiting on a peer that does not read.
                    Some(_) if outgoing.is_closed() => {
                        transport.close().await;
                        break;
                    }
                    Some(Outgoing::Message(message)) => {
                        if transport.send(message).await.is_err() {
                            break;
                        }
                    }
                    Some(Outgoing::Close) => {
                        transport.close().await;
                        break;
                    }
                    // Cut off for falling behind.
                    None => {
                        transport.close().await;
                        break;
                    }
                }
            }
            // A transport that holds back is not read while the handler is
            // behind, so its frames wait where they are; reading resumes as
            // soon as the handler takes one.
            incoming = async {
                if holds_back {
                    let _ = events.reserve().await;
                }
                transport.recv().await
            } => {
                match incoming {
                    Incoming::Message(message) => {
                        let message = match message {
                            Message::Text(text) => ConnectionMessage::Text(text),
                            Message::Binary(bytes) => ConnectionMessage::Binary(bytes),
                        };
                        if events.try_send(ConnectionEvent::Message(message)).is_err() {
                            tracing::warn!(app = %app, "connection closed: the browser sends faster than the app answers");
                            transport.close().await;
                            break;
                        }
                    }
                    Incoming::Pong => last_pong = Instant::now(),
                    Incoming::Ended => {
                        sender_done = true;
                        break;
                    }
                }
            }
            _ = checks.tick() => {
                // A connection that did not answer the last ping is gone.
                if answers_pings
                    && let Some(at) = pinged_at
                    && last_pong < at
                {
                    break;
                }
                let current = match (&user_id, &session) {
                    (Some(id), session) => {
                        let (config, id, session, scope) = (state.config.clone(), id.clone(), session.clone(), app.clone());
                        let found = tokio::task::spawn_blocking(move || match session {
                            Some(token) => users::app_session_user(&config, &token, &scope).filter(|user| user.id == id),
                            None => users::user_by_id(&config, &id),
                        });
                        match found.await.ok().flatten() {
                            Some(user) => Some(user),
                            None => {
                                tracing::info!(app = %app, "connection closed: the account is disabled or gone, or its session ended");
                                transport.close().await;
                                break;
                            }
                        }
                    }
                    (None, _) => None,
                };
                if !may_stay(&state, &app, &door, current.as_ref()).await {
                    tracing::info!(app = %app, door = ?door, "connection closed: it may no longer come in this way");
                    transport.close().await;
                    break;
                }
                if answers_pings {
                    pinged_at = Some(Instant::now());
                    if transport.ping().await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    let _ = events.send(ConnectionEvent::Close).await;
    drop(events);
    if sender_done {
        let mut worker = worker;
        let mut open = true;
        loop {
            tokio::select! {
                _ = &mut worker => break,
                out = outgoing.recv(), if open => match out {
                    Some(Outgoing::Message(message)) => open = transport.send(message).await.is_ok(),
                    _ => open = false,
                },
            }
        }
        while let Ok(Outgoing::Message(message)) = outgoing.try_recv() {
            if !open || transport.send(message).await.is_err() {
                break;
            }
        }
    } else {
        let _ = worker.await;
    }
    // Held until the close event ran, so a send during it does not count
    // as falling behind.
    drop(outgoing);
}
