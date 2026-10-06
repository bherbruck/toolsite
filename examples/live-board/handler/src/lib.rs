//! A shared board that updates live. Two halves, kept apart on purpose:
//!
//! - Changes go through ordinary requests: `/api/cards` creates, moves and
//!   deletes, and the `add_card` tool calls the same code. Each change is
//!   written to SQLite first and then published to the topic `board`, so a
//!   browser that misses an event still gets the truth on its next connect.
//! - The socket is for listening, presence and nudges. On connect the
//!   handler joins the connection to `board`, `who` and `user:<id>`, sends
//!   the board as the first message, and keeps the person's display name in
//!   the connection's state. On close it updates the presence list.
//!
//! toolsite holds the sockets. This handler only gets events and calls
//! `send`, `subscribe` and `publish`; it keeps no connection itself.

wit_bindgen::generate!({
    path: "wit",
    world: "app-with-connections",
});

use serde_json::{json, Map, Value};
use toolsite::app::connections::{self, ConnectInfo, Message};
use toolsite::app::{db, identity};

const LANES: [&str; 3] = ["todo", "doing", "done"];
/// A presence row with no message for this long is skipped. Browsers send
/// a ping every 25 seconds.
const QUIET_SECONDS: i64 = 90;

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        let body = || serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let result = match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["api", "board"]) => board(),
            ("POST", ["api", "cards"]) => {
                let args = body();
                add_card(&args["title"], &args["lane"])
            }
            ("POST", ["api", "cards", id, "move"]) => {
                let args = body();
                id_of(id).and_then(|id| move_card(id, &args["lane"]))
            }
            ("DELETE", ["api", "cards", id]) => id_of(id).and_then(delete_card),

            // App tools. The platform signs the person in and calls these as
            // them; the arguments arrive under "arguments".
            ("POST", ["api", "tools", "add_card"]) => {
                let args = body()["arguments"].clone();
                add_card(&args["title"], &args["lane"])
            }
            _ => Err(fail(404, "No such route.")),
        };
        match result {
            Ok(value) => respond(200, &value),
            Err((status, value)) => respond(status, &value),
        }
    }

    fn on_connection(conn: String, event: Event) -> Result<(), String> {
        match event {
            Event::Connect(info) => connect(&conn, &info),
            Event::Message(Message::Text(text)) => message(&conn, &text),
            Event::Message(Message::Binary(_)) => Err("send text frames of JSON".to_string()),
            Event::Close => {
                db::query("delete from presence where conn = ?", &[text(&conn)]).map_err(|e| db_message(&e))?;
                publish_who()
            }
        }
    }
}

export!(Handler);

// --- the socket -------------------------------------------------------------

/// Accepts a signed-in person, joins them to the board, and tells everyone
/// they arrived. A visitor with no account is refused: on a public gate the
/// board stays readable through `/api/board`, but presence needs a person.
fn connect(conn: &str, info: &ConnectInfo) -> Result<(), String> {
    let Some(user) = identity::current_user() else {
        return Err("Sign in to join the board.".to_string());
    };
    let name = display_name(&param(&info.query, "name")).unwrap_or_else(|| user.email.clone());
    connections::state_set(conn, "name", Some(&name))?;

    connections::subscribe(conn, "board")?;
    connections::subscribe(conn, "who")?;
    // Allowed only because the connection belongs to this person. The app
    // may publish to any person's topic; a browser may join only its own.
    connections::subscribe(conn, &format!("user:{}", user.id))?;

    // The first message is the whole board, so a browser that reconnects
    // after a gap never needs to replay the events it missed.
    let snapshot = board().map_err(|(_, v)| v["error"].as_str().unwrap_or("board unavailable").to_string())?;
    let mut first = snapshot;
    first["type"] = json!("board");
    first["me"] = json!({ "id": user.id, "email": user.email, "name": name, "conn": conn });
    connections::send(conn, &Message::Text(first.to_string()))?;

    let now = now()?;
    db::query("delete from presence where seen_at < ?", &[db::Value::Integer(now - QUIET_SECONDS * 2)])
        .map_err(|e| db_message(&e))?;
    db::query(
        "insert or replace into presence (conn, user_id, name, seen_at) values (?, ?, ?, ?)",
        &[text(conn), text(&user.id), text(&name), db::Value::Integer(now)],
    )
    .map_err(|e| db_message(&e))?;
    publish_who()
}

/// Messages from the browser, as JSON with a "type":
///
/// - `ping`: keeps the person on the presence list.
/// - `name`: changes the display name for this connection.
/// - `nudge`: sends a short note to one person, on every board they have open.
fn message(conn: &str, raw: &str) -> Result<(), String> {
    let msg: Value = serde_json::from_str(raw).map_err(|_| "send JSON".to_string())?;
    let reply = |value: Value| connections::send(conn, &Message::Text(value.to_string()));
    db::query("update presence set seen_at = ? where conn = ?", &[db::Value::Integer(now()?), text(conn)])
        .map_err(|e| db_message(&e))?;
    match msg["type"].as_str().unwrap_or("") {
        "ping" => Ok(()),
        "name" => {
            let Some(name) = display_name(msg["name"].as_str().unwrap_or("")) else {
                return reply(json!({ "type": "error", "error": "A name is 1 to 40 characters." }));
            };
            connections::state_set(conn, "name", Some(&name))?;
            db::query("update presence set name = ? where conn = ?", &[text(&name), text(conn)])
                .map_err(|e| db_message(&e))?;
            publish_who()
        }
        "nudge" => {
            let to = msg["to"].as_str().unwrap_or("");
            let here = db::query("select 1 from presence where user_id = ?", &[text(to)]).map_err(|e| db_message(&e))?;
            if here.values.is_empty() {
                return reply(json!({ "type": "error", "error": "That person does not have the board open." }));
            }
            let from = connections::state_get(conn, "name").unwrap_or_default();
            let note: String = msg["text"].as_str().unwrap_or("").trim().chars().take(140).collect();
            let note = if note.is_empty() { "Look at the board.".to_string() } else { note };
            let reached = connections::publish(
                &format!("user:{to}"),
                &Message::Text(json!({ "type": "nudge", "from": from, "text": note }).to_string()),
            )?;
            reply(json!({ "type": "nudged", "to": to, "reached": reached }))
        }
        other => reply(json!({ "type": "error", "error": format!("Unknown message type {other:?}.") })),
    }
}

/// The people with the board open, one entry per person however many tabs
/// they have, sent to everyone on `who`.
fn publish_who() -> Result<(), String> {
    let cutoff = now()? - QUIET_SECONDS;
    let rows = db::query(
        "select user_id as id, max(name) as name, count(*) as tabs from presence \
         where seen_at >= ? group by user_id order by name",
        &[db::Value::Integer(cutoff)],
    )
    .map_err(|e| db_message(&e))?;
    let people = to_json(&rows);
    connections::publish("who", &Message::Text(json!({ "type": "who", "people": people }).to_string()))?;
    Ok(())
}

// --- the board, through requests -----------------------------------------------

type Answer = Result<Value, (u16, Value)>;

const CARD_COLUMNS: &str = "id, title, lane, author_email, created_at, updated_at";

fn board() -> Answer {
    let cards = rows(db::query(&format!("select {CARD_COLUMNS} from cards order by created_at, id"), &[]))?;
    Ok(json!({ "cards": cards }))
}

fn add_card(title: &Value, lane: &Value) -> Answer {
    let title = title.as_str().unwrap_or("").trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(fail(400, "A title is 1 to 200 characters."));
    }
    let lane = lane_of(lane)?;
    let user = identity::current_user();
    let card = one(rows(db::query(
        &format!("insert into cards (title, lane, author_id, author_email) values (?, ?, ?, ?) returning {CARD_COLUMNS}"),
        &[
            text(title),
            text(lane),
            user.as_ref().map(|u| text(&u.id)).unwrap_or(db::Value::Null),
            user.as_ref().map(|u| text(&u.email)).unwrap_or(db::Value::Null),
        ],
    )))?;
    announce(json!({ "type": "card", "card": card }));
    Ok(card)
}

fn move_card(id: i64, lane: &Value) -> Answer {
    let lane = lane_of(lane)?;
    let card = one(rows(db::query(
        &format!(
            "update cards set lane = ?, updated_at = cast(strftime('%s', 'now') as integer) where id = ? returning {CARD_COLUMNS}"
        ),
        &[text(lane), db::Value::Integer(id)],
    )))
    .map_err(|_| fail(404, &format!("No card {id}.")))?;
    announce(json!({ "type": "card", "card": card }));
    Ok(card)
}

fn delete_card(id: i64) -> Answer {
    let gone = db::query("delete from cards where id = ?", &[db::Value::Integer(id)]).map_err(|e| db_fail(&e))?;
    if gone.rows_affected == 0 {
        return Err(fail(404, &format!("No card {id}.")));
    }
    announce(json!({ "type": "deleted", "id": id }));
    Ok(json!({ "deleted": id }))
}

/// Tells every open board about a change already written. A failed publish
/// does not undo the change: the next connect sends the board as it is.
fn announce(event: Value) {
    let _ = connections::publish("board", &Message::Text(event.to_string()));
}

// --- helpers -----------------------------------------------------------------

fn lane_of(lane: &Value) -> Result<&'static str, (u16, Value)> {
    match lane.as_str() {
        None | Some("") => Ok("todo"),
        Some(l) => LANES.iter().find(|x| **x == l).copied().ok_or_else(|| fail(400, "lane is todo, doing or done.")),
    }
}

/// A display name: trimmed, 1 to 40 characters, no control characters.
fn display_name(raw: &str) -> Option<String> {
    let name: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    (!name.is_empty() && name.chars().count() <= 40).then_some(name)
}

fn now() -> Result<i64, String> {
    let rows = db::query("select cast(strftime('%s', 'now') as integer)", &[]).map_err(|e| db_message(&e))?;
    match rows.values.first().and_then(|r| r.first()) {
        Some(db::Value::Integer(n)) => Ok(*n),
        _ => Err("no clock".to_string()),
    }
}

/// One query-string parameter, percent-decoded.
fn param(query: &str, key: &str) -> String {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| decode(v))
        .unwrap_or_default()
}

fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match bytes.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()) {
                Some(b) => {
                    out.push(b);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn id_of(raw: &str) -> Result<i64, (u16, Value)> {
    raw.parse().map_err(|_| fail(400, "An id must be a number."))
}

fn text(s: &str) -> db::Value {
    db::Value::Text(s.to_string())
}

fn fail(status: u16, message: &str) -> (u16, Value) {
    (status, json!({ "error": message }))
}

fn db_message(e: &db::Error) -> String {
    match e {
        db::Error::Failed(m) | db::Error::Denied(m) => m.clone(),
    }
}

fn db_fail(e: &db::Error) -> (u16, Value) {
    match e {
        db::Error::Failed(m) => fail(400, m),
        db::Error::Denied(m) => fail(403, m),
    }
}

fn value_json(v: &db::Value) -> Value {
    match v {
        db::Value::Null => Value::Null,
        db::Value::Integer(i) => json!(i),
        db::Value::Real(f) => json!(f),
        db::Value::Text(s) => json!(s),
    }
}

fn to_json(rows: &db::Rows) -> Value {
    Value::Array(
        rows.values
            .iter()
            .map(|row| Value::Object(rows.columns.iter().cloned().zip(row.iter().map(value_json)).collect::<Map<_, _>>()))
            .collect(),
    )
}

fn rows(result: Result<db::Rows, db::Error>) -> Answer {
    result.map(|rows| to_json(&rows)).map_err(|e| db_fail(&e))
}

fn one(rows: Answer) -> Answer {
    rows?.as_array().and_then(|a| a.first().cloned()).ok_or_else(|| fail(404, "Not found."))
}

fn respond(status: u16, value: &Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}
