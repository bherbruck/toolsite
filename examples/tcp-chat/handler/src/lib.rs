//! A chat over raw TCP, for `nc host port`. One line in is one command or
//! one message out.
//!
//! The app is not resident: each event runs in a fresh instance, so what a
//! connection needs between its reads is in its state: the bytes of a line
//! not yet ended, and the nickname.
//!
//! A TCP read is not a line. One read may hold half a line or three lines,
//! so the handler adds each read to the buffer and handles each line that
//! ends in `\n`. A line is at most 4 KB. A client that sends a longer one is
//! told so and closed, or the buffer would grow without end.

wit_bindgen::generate!({
    path: "wit",
    world: "app-with-connections",
});

use serde_json::{json, Map, Value};
use toolsite::app::connections::{self, Message};
use toolsite::app::{auth, db};

/// The longest line, in bytes, without its `\n`.
const MAX_LINE: usize = 4096;
/// Every signed-in connection is on this topic.
const ROOM: &str = "room";

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/api/lines") => last_lines(),
            _ => respond(404, json!({ "error": "No such route." })),
        }
    }

    fn on_connection(conn: String, event: Event) -> Result<(), String> {
        match event {
            Event::Connect(_) => say(&conn, "Send: token <device-token>"),
            Event::Message(Message::Binary(bytes)) => received(&conn, &bytes),
            Event::Message(Message::Text(text)) => received(&conn, text.as_bytes()),
            Event::Close => left(&conn),
        }
    }
}

export!(Handler);

// --- framing -------------------------------------------------------------------

/// Adds one read to the buffer and handles each whole line in it. The state
/// holds text, so the buffer is kept one char per byte: a read that ends in
/// the middle of a UTF-8 character is kept as it is until the rest arrives.
fn received(conn: &str, bytes: &[u8]) -> Result<(), String> {
    let mut buffer: Vec<u8> = connections::state_get(conn, "buffer")
        .map(|kept| kept.chars().map(|c| c as u8).collect())
        .unwrap_or_default();
    buffer.extend_from_slice(bytes);

    while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buffer.drain(..=end).collect();
        if line.len() > MAX_LINE + 1 {
            return too_long(conn);
        }
        let line = String::from_utf8_lossy(&line);
        if !handle_line(conn, line.trim_end_matches(['\r', '\n']))? {
            return Ok(());
        }
    }
    if buffer.len() > MAX_LINE {
        return too_long(conn);
    }
    let kept: String = buffer.iter().map(|&b| b as char).collect();
    connections::state_set(conn, "buffer", Some(&kept))
}

fn too_long(conn: &str) -> Result<(), String> {
    say(conn, &format!("error: a line is at most {MAX_LINE} bytes"))?;
    connections::close(conn)
}

// --- the chat --------------------------------------------------------------------

/// One line. Answers false when the connection is closed and the lines
/// after it must not be handled.
fn handle_line(conn: &str, line: &str) -> Result<bool, String> {
    let Some(nick) = connections::state_get(conn, "nick") else {
        return sign_in(conn, line);
    };
    // Control characters would let one client drive the others' terminals.
    let line: String = line.chars().filter(|c| !c.is_control() || *c == '\t').collect();
    match line.split_once(' ').unwrap_or((&line, "")) {
        ("/nick", name) => rename(conn, &nick, name)?,
        ("/who", _) => who(conn)?,
        ("/quit", _) => {
            say(conn, "bye")?;
            connections::close(conn)?;
            return Ok(false);
        }
        (command, _) if command.starts_with('/') => say(conn, "error: the commands are /nick name, /who and /quit")?,
        _ if line.trim().is_empty() => {}
        _ => post(&nick, &line)?,
    }
    Ok(true)
}

/// The first line must be `token <device-token>`. The token's label is the
/// first nickname, or "guest" when the label is not a valid one.
fn sign_in(conn: &str, line: &str) -> Result<bool, String> {
    let Some(label) = line.strip_prefix("token ").and_then(|token| auth::check_token(token.trim())) else {
        say(conn, "error: the first line is token <device-token>")?;
        connections::close(conn)?;
        return Ok(false);
    };
    let nick = nick_of(&label).unwrap_or_else(|| "guest".to_string());
    connections::state_set(conn, "nick", Some(&nick))?;
    connections::subscribe(conn, ROOM)?;
    sql("insert or replace into here (conn, nick) values (?, ?)", &[text(conn), text(&nick)])?;
    say(conn, &format!("Welcome, {nick}. The commands are /nick name, /who and /quit."))?;
    announce(&format!("* {nick} joined"))?;
    Ok(true)
}

fn rename(conn: &str, old: &str, name: &str) -> Result<(), String> {
    let Some(nick) = nick_of(name) else {
        return say(conn, "error: a nickname is 1 to 20 letters, digits, - or _");
    };
    connections::state_set(conn, "nick", Some(&nick))?;
    sql("update here set nick = ? where conn = ?", &[text(&nick), text(conn)])?;
    announce(&format!("* {old} is now {nick}"))
}

/// Who is here. A handler cannot list connections, so the table `here` keeps
/// one row per signed-in connection. A row whose connection is gone (the
/// server stopped before its close event) has no remote address, and is
/// removed here.
fn who(conn: &str) -> Result<(), String> {
    let rows = sql("select conn, nick from here order by nick", &[])?;
    let mut nicks = Vec::new();
    for row in &rows.values {
        let (db::Value::Text(other), db::Value::Text(nick)) = (&row[0], &row[1]) else { continue };
        if connections::remote(other).is_some() {
            nicks.push(nick.as_str());
        } else {
            sql("delete from here where conn = ?", &[text(other)])?;
        }
    }
    say(conn, &format!("here: {}", nicks.join(", ")))
}

/// A message: stored, then sent to everyone in the room, the sender too.
fn post(nick: &str, line: &str) -> Result<(), String> {
    sql("insert into lines (nick, text) values (?, ?)", &[text(nick), text(line)])?;
    announce(&format!("<{nick}> {line}"))
}

fn left(conn: &str) -> Result<(), String> {
    let Some(nick) = connections::state_get(conn, "nick") else { return Ok(()) };
    sql("delete from here where conn = ?", &[text(conn)])?;
    announce(&format!("* {nick} left"))
}

/// A nickname: 1 to 20 letters, digits, '-' or '_'.
fn nick_of(raw: &str) -> Option<String> {
    let valid = (1..=20).contains(&raw.len()) && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| raw.to_string())
}

// --- the page ----------------------------------------------------------------------

/// The last 50 lines, oldest first.
fn last_lines() -> Response {
    let rows = match sql("select * from (select id, nick, text, at from lines order by id desc limit 50) order by id", &[]) {
        Ok(rows) => rows,
        Err(e) => return respond(500, json!({ "error": e })),
    };
    let lines: Vec<Value> = rows
        .values
        .iter()
        .map(|row| Value::Object(rows.columns.iter().cloned().zip(row.iter().map(value_json)).collect::<Map<_, _>>()))
        .collect();
    respond(200, json!({ "lines": lines }))
}

// --- helpers -------------------------------------------------------------------------

fn say(conn: &str, line: &str) -> Result<(), String> {
    connections::send(conn, &Message::Text(format!("{line}\n")))
}

fn announce(line: &str) -> Result<(), String> {
    connections::publish(ROOM, &Message::Text(format!("{line}\n"))).map(|_| ())
}

fn sql(statement: &str, params: &[db::Value]) -> Result<db::Rows, String> {
    db::query(statement, params).map_err(|e| match e {
        db::Error::Failed(m) | db::Error::Denied(m) => m,
    })
}

fn text(s: &str) -> db::Value {
    db::Value::Text(s.to_string())
}

fn value_json(v: &db::Value) -> Value {
    match v {
        db::Value::Null => Value::Null,
        db::Value::Integer(i) => json!(i),
        db::Value::Real(f) => json!(f),
        db::Value::Text(s) => json!(s),
    }
}

fn respond(status: u16, value: Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}
