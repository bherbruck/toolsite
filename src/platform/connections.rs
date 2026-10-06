//! A live connection's life, whatever carries it: open, events to the
//! handler, re-checks, close.
//!
//! The transport (a WebSocket today) only moves frames. Everything else is
//! here, once: the handler is asked whether to accept, gets each frame as an
//! event, one at a time and in order, and gets `close` however the
//! connection ended. Every `check_every` the connection is pinged and its
//! person's access decided again, as if they opened the socket's path now;
//! a disabled account, a hidden app, a withdrawn socket or a gate that no
//! longer admits them closes it.
//!
//! A new transport implements `Transport` and calls `open` and `run`; the
//! handler cannot tell which one carried the bytes.

use crate::{
    accounts::users::{self, User},
    runtime::{
        connections::{Message, Outgoing, Registration},
        wasm::{ConnectInfo, ConnectionEvent, ConnectionMessage, Guards, User as GuestUser},
    },
    AppState,
};
use std::{future::Future, time::Instant};
use tokio::sync::mpsc;

/// Frames from the browser waiting for the handler before the connection
/// is closed for sending faster than the app can answer.
const EVENT_QUEUE: usize = 64;

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
    socket: String,
    visitor: Option<User>,
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
/// as a request does, so a redeploy takes effect on the next event.
async fn deliver(
    state: &AppState,
    app: &str,
    visitor: &Option<User>,
    conn: &str,
    event: ConnectionEvent,
) -> Result<Option<Result<(), String>>, String> {
    let Some(wasm) = crate::content::serve::handler_wasm(&state.config, app).await else {
        return Ok(None);
    };
    let (runtime, config, app, user, conn) =
        (state.runtime.clone(), state.config.clone(), app.to_string(), guest_user(visitor), conn.to_string());
    tokio::task::spawn_blocking(move || {
        runtime.connection_event(config, &app, &wasm, user, &conn, event, Guards::default())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Registers a connection and asks the handler whether to accept it.
pub async fn open(
    state: AppState,
    app: String,
    socket: String,
    visitor: Option<User>,
    info: ConnectInfo,
) -> Result<Session, Refusal> {
    let Some(wasm) = crate::content::serve::handler_wasm(&state.config, &app).await else {
        return Err(Refusal::NotOffered);
    };
    let takes = {
        let (runtime, app) = (state.runtime.clone(), app.clone());
        tokio::task::spawn_blocking(move || runtime.takes_connections(&app, &wasm)).await
    };
    match takes {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return Err(Refusal::NotOffered),
        Ok(Err(e)) => return Err(Refusal::Failed(format!("{e:#}"))),
        Err(e) => return Err(Refusal::Failed(e.to_string())),
    }

    let (registration, outgoing) = state
        .config
        .connections
        .register(&app, visitor.as_ref().map(|user| user.id.as_str()))
        .map_err(Refusal::Full)?;
    match deliver(&state, &app, &visitor, &registration.id, ConnectionEvent::Connect(info)).await {
        Ok(Some(Ok(()))) => Ok(Session {
            state,
            app,
            socket,
            visitor,
            registration,
            outgoing,
        }),
        Ok(Some(Err(reason))) => Err(Refusal::Refused(reason)),
        Ok(None) => Err(Refusal::NotOffered),
        Err(why) => Err(Refusal::Failed(why)),
    }
}

/// Whether `user` (or nobody) may still hold this socket: the app exists,
/// is not hidden, still declares the socket, and its access for the
/// socket's path admits them.
async fn may_stay(state: &AppState, app: &str, socket: &str, user: Option<&User>) -> bool {
    let config = &state.config;
    if crate::content::store::is_hidden(config, app).await || !crate::content::store::app_exists(config, app).await {
        return false;
    }
    if !crate::content::store::read_meta(config, app).await.sockets.iter().any(|s| s == socket) {
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
        socket,
        visitor,
        registration,
        mut outgoing,
    } = session;
    let conn = registration.id.clone();

    // Events run in their own task, one at a time, so a slow handler never
    // stops this loop from sending, pinging or noticing the browser left.
    let (events, mut queue) = mpsc::channel::<ConnectionEvent>(EVENT_QUEUE);
    let worker = {
        let (state, app, visitor, conn) = (state.clone(), app.clone(), visitor.clone(), conn.clone());
        tokio::spawn(async move {
            while let Some(event) = queue.recv().await {
                let closing = matches!(event, ConnectionEvent::Close);
                match deliver(&state, &app, &visitor, &conn, event).await {
                    Ok(Some(Err(why))) => tracing::info!(app = %app, "connection handler answered an error: {why}"),
                    Err(why) => tracing::warn!(app = %app, "connection handler failed: {why}"),
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

    loop {
        tokio::select! {
            out = outgoing.recv() => {
                match out {
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
            incoming = transport.recv() => {
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
                    Incoming::Ended => break,
                }
            }
            _ = checks.tick() => {
                // A connection that did not answer the last ping is gone.
                if let Some(at) = pinged_at
                    && last_pong < at
                {
                    break;
                }
                let current = match &user_id {
                    Some(id) => {
                        let (config, id) = (state.config.clone(), id.clone());
                        match tokio::task::spawn_blocking(move || users::user_by_id(&config, &id)).await.ok().flatten() {
                            Some(user) => Some(user),
                            None => {
                                tracing::info!(app = %app, "connection closed: the account is disabled or gone");
                                transport.close().await;
                                break;
                            }
                        }
                    }
                    None => None,
                };
                if !may_stay(&state, &app, &socket, current.as_ref()).await {
                    tracing::info!(app = %app, socket = %socket, "connection closed: the person may no longer open this socket");
                    transport.close().await;
                    break;
                }
                pinged_at = Some(Instant::now());
                if transport.ping().await.is_err() {
                    break;
                }
            }
        }
    }

    let _ = events.send(ConnectionEvent::Close).await;
    drop(events);
    let _ = worker.await;
    // Held until the close event ran, so a send during it does not count
    // as falling behind.
    drop(outgoing);
}
