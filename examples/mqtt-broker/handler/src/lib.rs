//! An MQTT broker: rumqttd's router, codecs and sessions, with toolsite
//! holding the sockets.
//!
//! The app runs resident, so one instance gets every connection event of
//! the app in order and keeps the broker in memory between them. This file
//! is only the glue:
//!
//! - `connect` opens a connection in the broker. A TCP device or an MQTT
//!   client over the WebSocket at `/mqtt` arrives the same way.
//! - `message` hands the bytes to the broker, which frames packets, runs
//!   the router and says what to write to which connection.
//! - `close` tells the broker the network went away, so a will can fire.
//! - `on-tick` drives keep alive and CONNECT timeouts and delayed wills,
//!   and saves a summary for `/api/status`, which runs in a fresh instance
//!   and so cannot see this memory.
//!
//! Who may connect is decided at CONNECT: the password must be a device
//! token of this app (`auth.check-token`), and it is checked again every 10
//! seconds, so a revoked token takes its device off the broker. A browser
//! on the WebSocket is already signed in, and the app's gate let it in, so
//! it needs no token; toolsite re-checks the person's access itself.

wit_bindgen::generate!({
    path: "wit",
    world: "app-resident",
});

use rumqttd::protocol::ConnectReturnCode;
use rumqttd::step::{Broker, Hello, Output, Settings};
use rumqttd::RouterConfig;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use toolsite::app::connections::{self, ConnectInfo, Message};
use toolsite::app::{auth, db, identity};

/// toolsite's ceiling on one message.
const MAX_WRITE: usize = 64 * 1024;
/// How often the summary is saved while something changes, and at least.
const SAVE_EVERY_MS: u64 = 2_000;
const SAVE_AT_LEAST_MS: u64 = 30_000;
/// How often a connected device's token is checked again.
const RECHECK_MS: u64 = 10_000;

/// One connection as toolsite knows it.
struct Conn {
    key: u64,
    /// "websocket" or "tcp".
    transport: &'static str,
    remote: Option<String>,
    /// The signed-in person on a WebSocket, by email.
    person: Option<String>,
    /// Who the CONNECT said it was: the person's email or the device
    /// token's label.
    label: Option<String>,
    username: Option<String>,
    /// The device token it connected with, to check again.
    token: Option<String>,
    connected_ms: u64,
}

struct State {
    broker: Broker,
    conns: HashMap<String, Conn>,
    ids: HashMap<u64, String>,
    next_key: u64,
    started_ms: u64,
    saved_ms: u64,
    saved_publishes: u64,
    checked_ms: u64,
    changed: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn broker() -> Broker {
    let router = RouterConfig {
        max_connections: 1000,
        max_outgoing_packet_count: 200,
        // Each subscription filter keeps at most 4 segments of 256 KB.
        max_segment_size: 256 * 1024,
        max_segment_count: 4,
        ..RouterConfig::default()
    };
    let settings = Settings {
        connection_timeout_ms: 10_000,
        max_payload_size: 128 * 1024,
        max_connection_buffer_len: 100,
        dynamic_filters: true,
        max_write_bytes: MAX_WRITE,
    };
    Broker::new(router, settings)
}

/// Runs `f` on the broker's state, starting it on the first event.
fn with_state<T>(now_ms: u64, f: impl FnOnce(&mut State) -> T) -> T {
    STATE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let state = cell.get_or_insert_with(|| State {
            broker: broker(),
            conns: HashMap::new(),
            ids: HashMap::new(),
            next_key: 1,
            started_ms: now_ms,
            saved_ms: 0,
            saved_publishes: 0,
            checked_ms: now_ms,
            changed: true,
        });
        f(state)
    })
}

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        match (req.method.as_str(), req.path.trim_end_matches('/')) {
            ("GET", "/api/status") => respond(200, &status()),
            ("GET", "/api/me") => {
                let me = identity::current_user().map(|u| json!({ "id": u.id, "email": u.email }));
                respond(200, &json!({ "me": me }))
            }
            _ => respond(404, &json!({ "error": "No such route." })),
        }
    }

    fn on_connection(conn: String, event: Event) -> Result<(), String> {
        let now = now_ms();
        with_state(now, |state| {
            match event {
                Event::Connect(info) => connect(state, &conn, &info, now),
                Event::Message(Message::Binary(bytes)) => read(state, &conn, &bytes, now),
                Event::Message(Message::Text(text)) => read(state, &conn, text.as_bytes(), now),
                Event::Close => {
                    if let Some(gone) = state.conns.remove(&conn) {
                        state.ids.remove(&gone.key);
                        state.broker.closed(gone.key, now);
                        state.changed = true;
                    }
                }
            }
            apply(state);
            Ok(())
        })
    }

    fn on_tick(now_ms: u64) {
        with_state(now_ms, |state| {
            state.broker.tick(now_ms);
            apply(state);
            recheck(state, now_ms);
            save(state, now_ms);
        });
    }
}

export!(Handler);

fn connect(state: &mut State, conn: &str, info: &ConnectInfo, now: u64) {
    let key = state.next_key;
    state.next_key += 1;
    // A WebSocket's socket is its path; a port's is "tcp:<port>".
    let websocket = info.socket.starts_with('/');
    state.conns.insert(
        conn.to_string(),
        Conn {
            key,
            transport: if websocket { "websocket" } else { "tcp" },
            remote: connections::remote(conn),
            person: if websocket { identity::current_user().map(|u| u.email) } else { None },
            label: None,
            username: None,
            token: None,
            connected_ms: now,
        },
    );
    state.ids.insert(key, conn.to_string());
    state.broker.open(key, now);
}

fn read(state: &mut State, conn: &str, bytes: &[u8], now: u64) {
    let Some(c) = state.conns.get_mut(conn) else { return };
    let key = c.key;
    let person = c.person.clone();
    let mut admitted = None;
    state.broker.read(key, bytes, now, |hello: &Hello| {
        admitted = Some(authenticate(person.as_deref(), hello)?);
        Ok(())
    });
    if let Some(who) = admitted {
        if let Some(c) = state.conns.get_mut(conn) {
            c.label = Some(who.label);
            c.username = who.username;
            c.token = who.token;
            c.connected_ms = now;
        }
        state.changed = true;
    }
}

/// Who a CONNECT turned out to be.
struct Admitted {
    label: String,
    username: Option<String>,
    token: Option<String>,
}

/// Who a CONNECT is, or the CONNACK code that refuses it.
fn authenticate(person: Option<&str>, hello: &Hello) -> Result<Admitted, ConnectReturnCode> {
    let username = hello.login.map(|l| l.username.clone()).filter(|u| !u.is_empty());
    if let Some(email) = person {
        return Ok(Admitted { label: email.to_string(), username, token: None });
    }
    let Some(login) = hello.login.filter(|l| !l.password.is_empty()) else {
        return Err(ConnectReturnCode::NotAuthorized);
    };
    let label = auth::check_token(&login.password).ok_or(ConnectReturnCode::BadUserNamePassword)?;
    Ok(Admitted { label, username, token: Some(login.password.clone()) })
}

/// Closes every device whose token no longer passes, every `RECHECK_MS`.
/// Its close event then ends its session as a dropped network would.
fn recheck(state: &mut State, now: u64) {
    if now.saturating_sub(state.checked_ms) < RECHECK_MS {
        return;
    }
    state.checked_ms = now;
    for (conn, c) in &state.conns {
        if let Some(token) = &c.token {
            if auth::check_token(token).is_none() {
                let _ = connections::close(conn);
            }
        }
    }
}

/// Does what the broker asked: writes in order, then closes.
fn apply(state: &mut State) {
    for output in state.broker.take_outputs() {
        match output {
            Output::Write(key, bytes) => {
                let Some(conn) = state.ids.get(&key) else { continue };
                // A lost write would cut a packet in half, so a connection
                // that cannot take one is closed instead.
                if connections::send(conn, &Message::Binary(bytes)).is_err() {
                    let _ = connections::close(conn);
                }
            }
            Output::Close(key) => {
                if let Some(conn) = state.ids.get(&key) {
                    let _ = connections::close(conn);
                }
                state.changed = true;
            }
        }
    }
}

/// Saves what `/api/status` shows, when something changed and at least
/// every 30 seconds, so a stale `updated_ms` says the instance is gone.
fn save(state: &mut State, now: u64) {
    let publishes = state.broker.publishes();
    let changed = state.changed || publishes != state.saved_publishes;
    let since = now.saturating_sub(state.saved_ms);
    if !(changed && since >= SAVE_EVERY_MS || since >= SAVE_AT_LEAST_MS) {
        return;
    }
    let mut clients: Vec<Value> = state
        .broker
        .clients()
        .filter_map(|(key, client_id)| {
            let conn = state.conns.get(state.ids.get(&key)?)?;
            Some(json!({
                "client_id": client_id,
                "label": conn.label,
                "username": conn.username,
                "transport": conn.transport,
                "remote": conn.remote,
                "connected_ms": conn.connected_ms,
            }))
        })
        .collect();
    clients.sort_by_key(|c| c["connected_ms"].as_u64());
    let recent: Vec<Value> = state
        .broker
        .recent()
        .rev()
        .map(|seen| {
            json!({
                "at_ms": seen.at_ms,
                "client_id": seen.client_id,
                "topic": seen.topic,
                "qos": seen.qos,
                "retain": seen.retain,
                "size": seen.size,
                "preview": String::from_utf8_lossy(&seen.preview),
            })
        })
        .collect();
    let saved = db::query(
        "insert into status (id, started_ms, updated_ms, publishes, clients, recent) values (1, ?, ?, ?, ?, ?)
         on conflict (id) do update set started_ms = excluded.started_ms, updated_ms = excluded.updated_ms,
           publishes = excluded.publishes, clients = excluded.clients, recent = excluded.recent",
        &[
            db::Value::Integer(state.started_ms as i64),
            db::Value::Integer(now as i64),
            db::Value::Integer(publishes as i64),
            db::Value::Text(Value::Array(clients).to_string()),
            db::Value::Text(Value::Array(recent).to_string()),
        ],
    );
    if saved.is_ok() {
        state.saved_ms = now;
        state.saved_publishes = publishes;
        state.changed = false;
    }
}

/// What the resident instance last saved, read from a fresh instance.
fn status() -> Value {
    let rows = db::query("select started_ms, updated_ms, publishes, clients, recent from status where id = 1", &[]);
    let Some(row) = rows.ok().and_then(|r| r.values.into_iter().next()) else {
        return json!({ "running": false, "clients": [], "recent": [] });
    };
    let int = |v: &db::Value| match v {
        db::Value::Integer(n) => *n,
        _ => 0,
    };
    let list = |v: &db::Value| match v {
        db::Value::Text(t) => serde_json::from_str(t).unwrap_or(json!([])),
        _ => json!([]),
    };
    let updated = int(&row[1]);
    json!({
        // Saved at least every 30 seconds while the instance runs.
        "running": (now_ms() as i64) - updated < 2 * SAVE_AT_LEAST_MS as i64,
        "started_ms": int(&row[0]),
        "updated_ms": updated,
        "publishes": int(&row[2]),
        "clients": list(&row[3]),
        "recent": list(&row[4]),
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn respond(status: u16, body: &Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: body.to_string().into_bytes(),
    }
}
