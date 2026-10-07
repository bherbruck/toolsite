//! A syslog receiver. Devices send datagrams to a UDP port; each one is
//! parsed, stored in SQLite and published to the topic `log`, where the page
//! listens on the WebSocket `/tail`.
//!
//! UDP syslog has no authentication. The site's owner maps the port on a
//! private network only, and the optional setting `ALLOWED_SOURCES` (IP
//! addresses, comma-separated) refuses every other source at `connect`.

wit_bindgen::generate!({
    path: "wit",
    world: "app-with-connections",
});

mod parse;

use serde_json::{json, Map, Value};
use toolsite::app::connections::{self, ConnectInfo, Message};
use toolsite::app::{db, identity, secrets};

/// Rows older than this are deleted by the `prune` job.
const KEEP_DAYS: i64 = 30;
const COLUMNS: &str = "id, received_at, remote, host, app, facility, severity, message";

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        let result = match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/api/logs") => logs(&req.query),
            ("GET", "/api/hosts") => hosts(),
            ("GET", "/api/prune") => prune(&req),
            _ => Err((404, "No such route.".to_string())),
        };
        match result {
            Ok(value) => respond(200, value),
            Err((status, error)) => respond(status, json!({ "error": error })),
        }
    }

    fn on_connection(conn: String, event: Event) -> Result<(), String> {
        match event {
            Event::Connect(info) => connect(&conn, &info),
            Event::Message(Message::Binary(datagram)) => receive(&conn, &datagram),
            // The tail only listens.
            Event::Message(Message::Text(_)) | Event::Close => Ok(()),
        }
    }
}

export!(Handler);

// --- connections ---------------------------------------------------------------

/// A UDP remote is let in when `ALLOWED_SOURCES` is empty or names its IP
/// address. A refused remote's datagram is dropped. A browser on `/tail`
/// passed the app's gate already, and joins the topic `log`.
fn connect(conn: &str, info: &ConnectInfo) -> Result<(), String> {
    if info.socket.starts_with("udp:") {
        let ip = connections::remote(conn).map(|r| ip_of(&r).to_string()).unwrap_or_default();
        let allowed = secrets::get("ALLOWED_SOURCES").unwrap_or_default();
        let allowed: Vec<&str> = allowed.split(',').map(str::trim).filter(|a| !a.is_empty()).collect();
        if !allowed.is_empty() && !allowed.contains(&ip.as_str()) {
            return Err(format!("{ip} is not in ALLOWED_SOURCES"));
        }
        return Ok(());
    }
    identity::current_user().ok_or("Sign in to tail the log.")?;
    connections::subscribe(conn, "log")
}

/// One datagram: stored, then published to everyone tailing the log. A
/// datagram that does not parse is stored whole, with no severity.
fn receive(conn: &str, datagram: &[u8]) -> Result<(), String> {
    // Only a UDP remote has an address. A browser sends nothing we keep.
    let Some(remote) = connections::remote(conn) else { return Ok(()) };
    let entry = parse::parse(datagram);
    let host = entry.host.unwrap_or_else(|| ip_of(&remote).to_string());
    let rows = db::query(
        &format!(
            "insert into logs (remote, host, app, facility, severity, message) values (?, ?, ?, ?, ?, ?) returning {COLUMNS}"
        ),
        &[
            text(&remote),
            text(&host),
            entry.app.as_deref().map(text).unwrap_or(db::Value::Null),
            entry.facility.map(db::Value::Integer).unwrap_or(db::Value::Null),
            entry.severity.map(db::Value::Integer).unwrap_or(db::Value::Null),
            text(&entry.message),
        ],
    )
    .map_err(|e| db_message(&e))?;
    if let Some(row) = to_json(&rows).first() {
        connections::publish("log", &Message::Text(row.to_string()))?;
    }
    Ok(())
}

// --- requests ----------------------------------------------------------------------

type Answer = Result<Value, (u16, String)>;

/// The newest 200 rows, newest first. `severity=N` keeps N and worse (a
/// lower number is worse), `host=` one host, `before=<id>` the page before.
fn logs(query: &str) -> Answer {
    let number = |key: &str| param(query, key).parse::<i64>().map(db::Value::Integer).unwrap_or(db::Value::Null);
    let host = param(query, "host");
    let rows = db::query(
        &format!(
            "select {COLUMNS} from logs \
             where (?1 is null or severity <= ?1) and (?2 is null or host = ?2) and (?3 is null or id < ?3) \
             order by id desc limit 200"
        ),
        &[number("severity"), if host.is_empty() { db::Value::Null } else { text(&host) }, number("before")],
    )
    .map_err(|e| (400, db_message(&e)))?;
    Ok(json!({ "logs": to_json(&rows) }))
}

fn hosts() -> Answer {
    let rows = db::query("select distinct host from logs order by host", &[]).map_err(|e| (500, db_message(&e)))?;
    Ok(json!({ "hosts": to_json(&rows).iter().map(|r| r["host"].clone()).collect::<Vec<_>>() }))
}

/// The `prune` job. The host sets `x-toolsite-scheduled` and strips any
/// `x-toolsite-*` header a client sends, so a visitor cannot run it.
fn prune(req: &Request) -> Answer {
    if !req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-toolsite-scheduled")) {
        return Err((403, "This path runs on the schedule only.".to_string()));
    }
    let gone = db::query(
        "delete from logs where received_at < cast(strftime('%s', 'now') as integer) - ? * 86400",
        &[db::Value::Integer(KEEP_DAYS)],
    )
    .map_err(|e| (500, db_message(&e)))?;
    Ok(json!({ "deleted": gone.rows_affected }))
}

// --- helpers -------------------------------------------------------------------------

/// "10.0.0.5:514" to "10.0.0.5", and "[fd00::5]:514" to "fd00::5".
fn ip_of(remote: &str) -> &str {
    remote.rsplit_once(':').map_or(remote, |(ip, _)| ip).trim_matches(['[', ']'])
}

/// One query-string parameter, percent-decoded.
fn param(query: &str, key: &str) -> String {
    let Some((_, raw)) = query.split('&').filter_map(|pair| pair.split_once('=')).find(|(k, _)| *k == key) else {
        return String::new();
    };
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (bytes[i], hex) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(b)) => {
                out.push(b);
                i += 2;
            }
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn text(s: &str) -> db::Value {
    db::Value::Text(s.to_string())
}

fn db_message(e: &db::Error) -> String {
    match e {
        db::Error::Failed(m) | db::Error::Denied(m) => m.clone(),
    }
}

fn to_json(rows: &db::Rows) -> Vec<Value> {
    let value = |v: &db::Value| match v {
        db::Value::Null => Value::Null,
        db::Value::Integer(i) => json!(i),
        db::Value::Real(f) => json!(f),
        db::Value::Text(s) => json!(s),
    };
    rows.values
        .iter()
        .map(|row| Value::Object(rows.columns.iter().cloned().zip(row.iter().map(value)).collect::<Map<_, _>>()))
        .collect()
}

fn respond(status: u16, value: Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}
