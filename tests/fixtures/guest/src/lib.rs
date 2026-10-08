//! Test fixture: the smallest handler that exercises every capability the
//! host grants, plus the ones it doesn't.

wit_bindgen::generate!({
    path: "../../../wit",
    world: "app-resident",
});

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use toolsite::app::auth;
use toolsite::app::blobs;
use toolsite::app::db;
use toolsite::app::connections::{self, Message};
use toolsite::app::identity;
use toolsite::app::jobs;
use toolsite::app::fetch;
use toolsite::app::secrets;

struct Handler;

/// Kept in the instance's memory. Run fresh per event, every `count` says
/// 1; run resident, it counts across events and connections.
static COUNTER: AtomicU64 = AtomicU64::new(0);
/// What each `on-tick` was told, oldest first, the last 1000.
static TICKS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
/// Holds what `grow` allocates, so the allocation cannot be optimised away.
static HOARD: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// What the last `on-tick` saw: the caller, the role, and what `TICK_SQL`
/// returned through `query-scoped`. A tick has no visitor.
static TICK_SAW: Mutex<String> = Mutex::new(String::new());
/// The scoped query each tick runs, set by `tick-sql:<sql>`.
static TICK_SQL: Mutex<String> = Mutex::new(String::new());
/// How long each tick sleeps, in ms, set by `slow-ticks:<ms>`.
static TICK_NAP: AtomicU64 = AtomicU64::new(0);

/// Recurses until the stack runs out. Each frame keeps an array alive so
/// neither the call nor the frame can be optimised away.
#[inline(never)]
fn deep(n: u64) -> u64 {
    let frame = [n; 32];
    std::hint::black_box(&frame);
    if n == 0 {
        0
    } else {
        deep(n - 1).wrapping_add(frame[7])
    }
}

/// Who is calling, as identity reports it: `<email>:<role>`, with
/// `anonymous` and `none` for nobody.
fn caller() -> String {
    let who = identity::current_user().map(|u| u.email).unwrap_or_else(|| "anonymous".to_string());
    let role = identity::current_role().unwrap_or_else(|| "none".to_string());
    format!("{who}:{role}")
}

fn scoped_text(sql: &str) -> String {
    match db::query_scoped(sql, &[]) {
        Ok(rows) => format!("rows:{}", rows_text(&rows)),
        Err(db::Error::Denied(m)) => format!("denied:{m}"),
        Err(db::Error::Failed(m)) => format!("failed:{m}"),
    }
}

/// One value out of `a=1&b=2`. Enough for a fixture.
fn param<'q>(query: &'q str, name: &str) -> &'q str {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .unwrap_or("")
}

/// Enough of percent-decoding for a fixture: `+` and `%XX`.
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() + 0 && i + 2 <= bytes.len() - 1 => {
                let hex = &raw[i + 1..i + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn rows_text(rows: &db::Rows) -> String {
    rows.values
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    db::Value::Null => "null".to_string(),
                    db::Value::Integer(i) => i.to_string(),
                    db::Value::Real(f) => f.to_string(),
                    db::Value::Text(t) => t.clone(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// A batch from a request body: one statement per line, its parameters
/// after tabs, each `i:<n>` for an integer, `n` for null, or text.
fn batch_of(body: &[u8]) -> Vec<db::Statement> {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let sql = fields.next().unwrap_or("").to_string();
            let params = fields
                .map(|field| match field {
                    "n" => db::Value::Null,
                    f if f.starts_with("i:") => db::Value::Integer(f[2..].parse().unwrap_or(0)),
                    f => db::Value::Text(f.to_string()),
                })
                .collect();
            db::Statement { sql, params }
        })
        .collect()
}

fn batch_response(outcome: Result<Vec<u64>, db::Error>) -> Response {
    match outcome {
        Ok(counts) => respond(200, format!("ok:{}", counts.iter().map(u64::to_string).collect::<Vec<_>>().join(","))),
        Err(db::Error::Denied(m)) => respond(403, format!("denied: {m}")),
        Err(db::Error::Failed(m)) => respond(500, format!("failed: {m}")),
    }
}

/// Notes that a job ran, and as whom, in `marks`.
fn mark(what: &str) -> Result<(), db::Error> {
    db::query("create table if not exists marks (what text, who text)", &[])?;
    db::query(
        "insert into marks values (?, ?)",
        &[db::Value::Text(what.to_string()), db::Value::Text(caller())],
    )?;
    Ok(())
}

fn blob_status(error: &blobs::Error) -> u16 {
    match error {
        blobs::Error::NotFound => 404,
        blobs::Error::InvalidKey(_) => 400,
        blobs::Error::TooLarge(_) => 413,
        blobs::Error::Failed(_) => 500,
    }
}

fn respond(status: u16, body: String) -> Response {
    Response {
        status,
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: body.into_bytes(),
    }
}

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        // The host passes the path relative to the app, /api included, so a
        // handler that answers both API calls and rendered routes sees one
        // path space. Strip the prefix the way any router would.
        let route = req.path.strip_prefix("/api").unwrap_or(&req.path).to_string();
        match route.as_str() {
            "/echo" => respond(200, format!("{} {}?{}", req.method, req.path, req.query)),

            // What an app tool call carries: the tool the host named, the
            // caller, and the body, answered as JSON. Hand-written JSON keeps
            // the guest free of a serde dependency.
            "/tool" => {
                let tool = req
                    .headers
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case("x-toolsite-tool"))
                    .map(|(_, value)| value.clone())
                    .collect::<Vec<_>>()
                    .join(",");
                let user = identity::current_user().map(|u| u.email).unwrap_or_default();
                let body = String::from_utf8_lossy(&req.body).to_string();
                let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
                Response {
                    status: 200,
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body: format!(
                        "{{\"tool\":\"{}\",\"user\":\"{}\",\"method\":\"{}\",\"body\":\"{}\"}}",
                        quote(&tool),
                        quote(&user),
                        quote(&req.method),
                        quote(&body)
                    )
                    .into_bytes(),
                }
            }

            // The cookie header as the handler got it, to prove toolsite's
            // own session cookies never reach app code.
            "/cookies" => respond(
                200,
                req.headers
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.clone())
                    .collect::<Vec<_>>()
                    .join("|"),
            ),

            // Answers with `Set-Cookie: <query>`, to prove an app cannot set
            // one of toolsite's own cookies.
            "/set-cookie" => Response {
                status: 200,
                headers: vec![("set-cookie".to_string(), decode(&req.query))],
                body: b"set".to_vec(),
            },

            "/myrole" => match identity::current_role() {
                Some(role) => respond(200, role),
                None => respond(200, "none".to_string()),
            },

            "/whoami" => match identity::current_user() {
                Some(user) => respond(200, format!("{}:{}", user.id, user.email)),
                None => respond(401, "anonymous".to_string()),
            },

            // Writes and reads through the host's database import.
            "/count" => {
                if let Err(e) = db::query("create table if not exists hits (n integer)", &[]) {
                    return respond(500, format!("{e:?}"));
                }
                if let Err(e) = db::query("insert into hits values (1)", &[]) {
                    return respond(500, format!("{e:?}"));
                }
                match db::query("select count(*) from hits", &[]) {
                    Ok(rows) => match rows.values.first().and_then(|r| r.first()) {
                        Some(db::Value::Integer(n)) => respond(200, n.to_string()),
                        other => respond(500, format!("unexpected {other:?}")),
                    },
                    Err(e) => respond(500, format!("{e:?}")),
                }
            }

            // Proves parameters are bound rather than interpolated.
            "/echo-param" => {
                let body = String::from_utf8_lossy(&req.body).to_string();
                match db::query("select ? as given", &[db::Value::Text(body)]) {
                    Ok(rows) => match rows.values.first().and_then(|r| r.first()) {
                        Some(db::Value::Text(t)) => respond(200, t.clone()),
                        other => respond(500, format!("unexpected {other:?}")),
                    },
                    Err(e) => respond(500, format!("{e:?}")),
                }
            }

            // SQL as the visitor: the full database with the identity bound,
            // or only the declared views.
            "/sql" => match db::query(&decode(param(&req.query, "q")), &[]) {
                Ok(rows) => respond(200, format!("{}:{}", rows.rows_affected, rows_text(&rows))),
                Err(db::Error::Denied(m)) => respond(403, format!("denied: {m}")),
                Err(db::Error::Failed(m)) => respond(500, format!("failed: {m}")),
            },
            "/scoped" => match db::query_scoped(&decode(param(&req.query, "q")), &[]) {
                Ok(rows) => respond(200, format!("{}:{}", rows.rows_affected, rows_text(&rows))),
                Err(db::Error::Denied(m)) => respond(403, format!("denied: {m}")),
                Err(db::Error::Failed(m)) => respond(500, format!("failed: {m}")),
            },

            // How many rows a query came back with, and whether it says it
            // was cut short: `<rows>:<truncated>`.
            "/sql-count" => match db::query(&decode(param(&req.query, "q")), &[]) {
                Ok(rows) => respond(200, format!("{}:{}", rows.values.len(), rows.truncated)),
                Err(e) => respond(500, format!("{e:?}")),
            },
            "/scoped-count" => match db::query_scoped(&decode(param(&req.query, "q")), &[]) {
                Ok(rows) => respond(200, format!("{}:{}", rows.values.len(), rows.truncated)),
                Err(e) => respond(500, format!("{e:?}")),
            },

            // A batch from the body, all or nothing: see `batch_of`.
            "/batch" => batch_response(db::batch(&batch_of(&req.body))),
            "/batch-scoped" => batch_response(db::batch_scoped(&batch_of(&req.body))),

            // Starts one of the app's jobs: `started` or `queued`, or 409
            // with the reason.
            "/jobs-run" => match jobs::run(param(&req.query, "name")) {
                Ok(how) => respond(200, how),
                Err(why) => respond(409, why),
            },
            // Job routes. `mark` records who it ran as; `nap` sleeps first,
            // to be caught running; `chain` counts a stage and asks for
            // itself again until it has run twenty.
            "/job-mark" => match mark("mark") {
                Ok(()) => respond(200, "marked".to_string()),
                Err(e) => respond(500, format!("{e:?}")),
            },
            "/job-nap" => {
                let ms = param(&req.query, "ms").parse().unwrap_or(1500);
                std::thread::sleep(std::time::Duration::from_millis(ms));
                match mark("nap") {
                    Ok(()) => respond(200, "napped".to_string()),
                    Err(e) => respond(500, format!("{e:?}")),
                }
            }
            "/chain" => {
                if let Err(e) = mark("stage") {
                    return respond(500, format!("{e:?}"));
                }
                let stages = match db::query("select count(*) from marks where what = 'stage'", &[]) {
                    Ok(rows) => match rows.values.first().and_then(|r| r.first()) {
                        Some(db::Value::Integer(n)) => *n,
                        _ => 0,
                    },
                    Err(e) => return respond(500, format!("{e:?}")),
                };
                if stages < 20 {
                    if let Err(why) = jobs::run("chain") {
                        return respond(500, why);
                    }
                }
                respond(200, stages.to_string())
            }

            // The host must refuse this, not the guest.
            // Can a guest read the platform's own account tables?
            "/read-users" => match db::query("select email from users", &[]) {
                Ok(rows) => respond(200, format!("LEAKED {:?}", rows.values)),
                Err(db::Error::Denied(m)) => respond(403, format!("denied: {m}")),
                Err(db::Error::Failed(m)) => respond(500, format!("failed: {m}")),
            },

            // Or pull the platform database in sideways?
            "/steal-auth" => match db::query("attach database '../.site/auth.db' as site", &[]) {
                Ok(_) => respond(200, "ATTACHED".to_string()),
                Err(db::Error::Denied(m)) => respond(403, format!("denied: {m}")),
                Err(db::Error::Failed(m)) => respond(500, format!("failed: {m}")),
            },

            "/escape" => match db::query("attach database '../victim/data.db' as v", &[]) {
                Ok(_) => respond(200, "ATTACHED".to_string()),
                Err(db::Error::Denied(m)) => respond(403, format!("denied: {m}")),
                Err(db::Error::Failed(m)) => respond(500, format!("failed: {m}")),
            },

            // wasi is linked because std needs it, so these prove the empty
            // context actually withholds the capabilities.
            // Settings the owner set for this app.
            // Reaching out, which only works for hosts the app declared.
            "/fetch" => {
                let url = req.query.strip_prefix("url=").unwrap_or("");
                let request = fetch::Request {
                    method: "GET".to_string(),
                    url: url.to_string(),
                    headers: vec![],
                    body: vec![],
                };
                match fetch::send(&request) {
                    Ok(response) => respond(
                        200,
                        format!("{} {}", response.status, String::from_utf8_lossy(&response.body)),
                    ),
                    Err(why) => respond(502, format!("refused: {why}")),
                }
            }

            // The app's files, through the host's blobs import.
            // Sends the body to every connection on a topic.
            "/publish" => {
                let topic = decode(param(&req.query, "topic"));
                let data = String::from_utf8_lossy(&req.body).to_string();
                match connections::publish(&topic, &Message::Text(data)) {
                    Ok(reached) => respond(200, format!("reached {reached}")),
                    Err(why) => respond(400, why),
                }
            }

            // Sends the body to one connection by id.
            "/send" => {
                let conn = param(&req.query, "conn");
                let data = String::from_utf8_lossy(&req.body).to_string();
                match connections::send(conn, &Message::Text(data)) {
                    Ok(()) => respond(200, "sent".to_string()),
                    Err(why) => respond(400, why),
                }
            }

            // One connection's kept state, as a request sees it.
            "/conn-state" => {
                match connections::state_get(param(&req.query, "conn"), param(&req.query, "key")) {
                    Some(value) => respond(200, value),
                    None => respond(404, "no such state".to_string()),
                }
            }

            "/blob-put" => {
                let content_type = req
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                    .map(|(_, value)| value.clone())
                    .unwrap_or_else(|| "text/plain".to_string());
                match blobs::put(param(&req.query, "key"), &content_type, &req.body) {
                    Ok(()) => respond(201, "stored".to_string()),
                    Err(e) => respond(blob_status(&e), format!("{e:?}")),
                }
            }

            "/blob-get" => match blobs::get(param(&req.query, "key")) {
                Ok(blob) => Response {
                    status: 200,
                    headers: vec![("content-type".to_string(), blob.content_type)],
                    body: blob.body,
                },
                Err(e) => respond(blob_status(&e), format!("{e:?}")),
            },

            "/blob-stat" => match blobs::stat(param(&req.query, "key")) {
                Ok(Some(entry)) => respond(200, format!("{}:{}", entry.size, entry.content_type)),
                Ok(None) => respond(404, "absent".to_string()),
                Err(e) => respond(blob_status(&e), format!("{e:?}")),
            },

            "/blob-list" => match blobs::list(param(&req.query, "prefix")) {
                Ok(entries) => respond(
                    200,
                    entries.iter().map(|e| e.key.clone()).collect::<Vec<_>>().join(","),
                ),
                Err(e) => respond(blob_status(&e), format!("{e:?}")),
            },

            "/blob-delete" => match blobs::delete(param(&req.query, "key")) {
                Ok(()) => respond(200, "deleted".to_string()),
                Err(e) => respond(blob_status(&e), format!("{e:?}")),
            },

            // A URL for the browser, so the file never comes through here.
            "/blob-upload-url" => {
                let max: u64 = param(&req.query, "max").parse().unwrap_or(0);
                match blobs::upload_url(param(&req.query, "key"), max) {
                    Ok(url) => respond(200, url),
                    Err(e) => respond(blob_status(&e), format!("{e:?}")),
                }
            }

            // Sends a file by pointing at it: the host streams the bytes.
            "/blob-serve" => {
                let key = param(&req.query, "key").to_string();
                Response {
                    status: 200,
                    headers: vec![
                        ("x-toolsite-blob".to_string(), key.clone()),
                        (
                            "content-disposition".to_string(),
                            format!("attachment; filename=\"{key}\""),
                        ),
                    ],
                    body: Vec::new(),
                }
            }

            "/secret" => match secrets::get("API_KEY") {
                Some(value) => respond(200, format!("key={value}")),
                None => respond(404, "no API_KEY set".to_string()),
            },

            "/secret-names" => respond(200, secrets::names().join(",")),

            "/read-file" => match std::fs::read_to_string("/etc/passwd") {
                Ok(contents) => respond(200, format!("READ {} bytes", contents.len())),
                Err(e) => respond(403, format!("denied: {e}")),
            },

            "/list-root" => match std::fs::read_dir("/") {
                Ok(entries) => respond(200, format!("LISTED {}", entries.count())),
                Err(e) => respond(403, format!("denied: {e}")),
            },

            "/env" => {
                let vars: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
                respond(200, format!("{}:{}", vars.len(), vars.join(",")))
            }

            "/connect" => {
                use std::net::TcpStream;
                match TcpStream::connect("127.0.0.1:9") {
                    Ok(_) => respond(200, "CONNECTED".to_string()),
                    Err(e) => respond(403, format!("denied: {e}")),
                }
            }

            "/spin" => {
                let mut n: u64 = 0;
                loop {
                    n = n.wrapping_add(1);
                    std::hint::black_box(n);
                }
            }

            _ => respond(404, "not found".to_string()),
        }
    }

    /// Connections, for the tests. Each event is appended to the
    /// connection's kept state under "log", so order and state can both be
    /// seen from outside. Text frames are small commands:
    /// `echo:<x>`, `log`, `cookie`, `close`, `sub:<topic>`, `unsub:<topic>`,
    /// `publish:<topic>:<data>`. A binary frame is echoed back.
    ///
    /// For resident mode: `count` (a counter kept in memory, incremented
    /// and sent back), `ticks` (how many `on-tick` calls it saw), `clock`
    /// (both wasi clocks), `crash` (panics), `grow` (allocates until the
    /// memory cap stops it) and `hang` (never returns).
    ///
    /// For attacks on resident mode: `who` (identity as this event sees
    /// it), `scoped:<sql>` and `sql:<sql>` (the database as this event's
    /// person), `tick-sql:<sql>` and `tick-saw` (what the last tick saw
    /// running that query), `slow-ticks:<ms>` (each tick sleeps that long),
    /// `nap:<ms>` (sleeps inside the event), `sleep-forever` (a wasi sleep
    /// of an hour), `sql-spin` (a query that never ends), `recurse` (until
    /// the stack runs out), `secret` (whether API_KEY is set) and
    /// `poke:<conn>` (every connection call aimed at another id).
    ///
    /// A TCP connection or UDP remote (one with a `remote`) gets
    /// `id:<conn>\n` on connect, and its bytes are commands only when one
    /// whole read is `token <t>\n` (checked with auth.check-token: `ok
    /// <label>\n`, or `denied\n` and a close), `remote\n`, `log\n`,
    /// `close\n`, `amplify <count> <bytes>\n` (that many replies of that
    /// size), `poke <conn>\n` (every connection call aimed at another id),
    /// `trap\n` or `spin\n`. Anything else is echoed back as it came.
    fn on_connection(conn: String, event: Event) -> Result<(), String> {
        if let Some(remote) = connections::remote(&conn) {
            return on_device(conn, remote, event);
        }
        let who = identity::current_user().map(|u| u.email).unwrap_or_else(|| "anonymous".to_string());
        let mut log = connections::state_get(&conn, "log").unwrap_or_default();
        let mut note = |entry: &str| {
            if !log.is_empty() {
                log.push(',');
            }
            log.push_str(entry);
            connections::state_set(&conn, "log", Some(&log))
        };
        match event {
            Event::Connect(info) => {
                if param(&info.query, "refuse") == "1" {
                    return Err("this app refused the connection".to_string());
                }
                note(&format!("connect {who} {}", info.socket))?;
                let cookie = info
                    .headers
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.clone())
                    .collect::<Vec<_>>()
                    .join("|");
                connections::state_set(&conn, "cookie", Some(&cookie))?;
                for pair in info.query.split('&') {
                    if let Some(("topic", topic)) = pair.split_once('=') {
                        connections::subscribe(&conn, &decode(topic))?;
                    }
                }
                // Waits on the app's database between joining and the first
                // send, so a test holding the database can make another event
                // publish to this connection at exactly that point.
                if param(&info.query, "gate") == "1" {
                    db::query("insert into gate values (1)", &[]).map_err(|e| format!("{e:?}"))?;
                }
                connections::send(&conn, &Message::Text(format!("id:{conn}")))?;
                // Send, publish to a topic this connection joined, send: the
                // three must arrive in that order.
                let ordered = param(&info.query, "ordered");
                if !ordered.is_empty() {
                    connections::subscribe(&conn, ordered)?;
                    connections::send(&conn, &Message::Text("1".to_string()))?;
                    connections::publish(ordered, &Message::Text("2".to_string()))?;
                    connections::send(&conn, &Message::Text("3".to_string()))?;
                }
                Ok(())
            }
            Event::Message(Message::Binary(bytes)) => {
                note("binary")?;
                connections::send(&conn, &Message::Binary(bytes))
            }
            Event::Message(Message::Text(text)) => {
                note("message")?;
                let reply = |text: String| connections::send(&conn, &Message::Text(text));
                if let Some(rest) = text.strip_prefix("echo:") {
                    reply(rest.to_string())
                } else if text == "log" {
                    reply(connections::state_get(&conn, "log").unwrap_or_default())
                } else if text == "cookie" {
                    reply(connections::state_get(&conn, "cookie").unwrap_or_default())
                } else if text == "close" {
                    connections::close(&conn)
                } else if let Some(topic) = text.strip_prefix("sub:") {
                    reply(match connections::subscribe(&conn, topic) {
                        Ok(()) => "ok".to_string(),
                        Err(why) => format!("err:{why}"),
                    })
                } else if let Some(topic) = text.strip_prefix("unsub:") {
                    connections::unsubscribe(&conn, topic)?;
                    reply("ok".to_string())
                } else if let Some((topic, data)) = text.strip_prefix("publish:").and_then(|r| r.split_once(':')) {
                    connections::publish(topic, &Message::Text(data.to_string())).map(|_| ())
                } else if text == "count" {
                    reply((COUNTER.fetch_add(1, Ordering::SeqCst) + 1).to_string())
                } else if text == "ticks" {
                    reply(format!("ticks:{}", TICKS.lock().unwrap().len()))
                } else if text == "clock" {
                    // Both wasi clocks, read inside the sandbox.
                    let wall = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let started = std::time::Instant::now();
                    let mut spin: u64 = 0;
                    for n in 0..10_000u64 {
                        spin = spin.wrapping_add(std::hint::black_box(n));
                    }
                    std::hint::black_box(spin);
                    let later = std::time::Instant::now();
                    reply(format!("wall:{wall} monotonic:{}", later >= started))
                } else if text == "crash" {
                    panic!("the handler crashed on purpose")
                } else if text == "grow" {
                    // Past any cap a test sets: 1 MB at a time, touched, kept.
                    loop {
                        HOARD.lock().unwrap().push(vec![1u8; 1024 * 1024]);
                    }
                } else if text == "hang" {
                    // Slow host calls rather than a spin, so the event runs
                    // out of time before it runs out of fuel. Each one does
                    // its work in SQLite: a `select 1` on the call's kept
                    // connection is fast enough to spend the fuel first.
                    loop {
                        let _ = db::query(
                            "with recursive c(i) as (select 1 union all select i + 1 from c where i < 100000) select count(*) from c",
                            &[],
                        );
                    }
                } else if text == "who" {
                    reply(format!("who:{}", caller()))
                } else if let Some(sql) = text.strip_prefix("scoped:") {
                    reply(scoped_text(sql))
                } else if let Some(sql) = text.strip_prefix("sql:") {
                    reply(match db::query(sql, &[]) {
                        Ok(rows) => format!("rows:{}", rows_text(&rows)),
                        Err(e) => format!("err:{e:?}"),
                    })
                } else if let Some(sql) = text.strip_prefix("tick-sql:") {
                    *TICK_SQL.lock().unwrap() = sql.to_string();
                    reply("ok".to_string())
                } else if text == "tick-saw" {
                    reply(TICK_SAW.lock().unwrap().clone())
                } else if let Some(ms) = text.strip_prefix("slow-ticks:") {
                    TICK_NAP.store(ms.parse().unwrap_or(0), Ordering::SeqCst);
                    reply("ok".to_string())
                } else if let Some(ms) = text.strip_prefix("nap:") {
                    std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap_or(0)));
                    reply("awake".to_string())
                } else if text == "sleep-forever" {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                    reply("awake".to_string())
                } else if text == "sql-spin" {
                    let spin = "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c";
                    reply(format!("{:?}", db::query(spin, &[]).map(|rows| rows.values.len())))
                } else if text == "recurse" {
                    reply(deep(u64::MAX).to_string())
                } else if text == "secret" {
                    reply(format!("secret:{}", secrets::get("API_KEY").is_some()))
                } else if let Some(other) = text.strip_prefix("poke:") {
                    let sent = connections::send(other, &Message::Text("hijack".to_string())).is_ok();
                    let state = connections::state_get(other, "log").is_some();
                    let set = connections::state_set(other, "log", Some("hijacked")).is_ok();
                    let joined = connections::subscribe(other, "hijack").is_ok();
                    let closed = connections::close(other).is_ok();
                    reply(format!("send={sent} state={state} set={set} subscribe={joined} close={closed}"))
                } else {
                    reply(format!("unknown:{text}"))
                }
            }
            Event::Close => {
                note("close")?;
                // Told to anyone watching, since this connection is gone.
                connections::publish("closed", &Message::Text(format!("{who}:{log}"))).map(|_| ())
            }
        }
    }

    /// Notes each tick, so a test can count them with `ticks`.
    fn on_tick(now_ms: u64) {
        let mut ticks = TICKS.lock().unwrap();
        if ticks.len() >= 1000 {
            ticks.remove(0);
        }
        ticks.push(now_ms);
        drop(ticks);
        let sql = TICK_SQL.lock().unwrap().clone();
        let scoped = if sql.is_empty() { String::new() } else { scoped_text(&sql) };
        *TICK_SAW.lock().unwrap() = format!("tick:{} {scoped}", caller());
        let nap = TICK_NAP.load(Ordering::SeqCst);
        if nap > 0 {
            std::thread::sleep(std::time::Duration::from_millis(nap));
        }
    }
}

/// A connection from a port rather than a browser. Same log as a socket's,
/// so order and state can be seen from outside the same way.
fn on_device(conn: String, remote: String, event: Event) -> Result<(), String> {
    let mut log = connections::state_get(&conn, "log").unwrap_or_default();
    let mut note = |entry: &str| {
        if !log.is_empty() {
            log.push(',');
        }
        log.push_str(entry);
        connections::state_set(&conn, "log", Some(&log))
    };
    let reply = |text: String| connections::send(&conn, &Message::Binary(text.into_bytes()));
    match event {
        Event::Connect(info) => {
            note(&format!("connect {}", info.socket))?;
            reply(format!("id:{conn}\n"))
        }
        Event::Message(message) => {
            note("message")?;
            let bytes = match message {
                Message::Binary(bytes) => bytes,
                Message::Text(text) => text.into_bytes(),
            };
            let text = String::from_utf8_lossy(&bytes).to_string();
            if let Some(token) = text.strip_prefix("token ").and_then(|t| t.strip_suffix('\n')) {
                match auth::check_token(token) {
                    Some(label) => reply(format!("ok {label}\n")),
                    None => {
                        reply("denied\n".to_string())?;
                        connections::close(&conn)
                    }
                }
            } else if text == "remote\n" {
                reply(format!("{remote}\n"))
            } else if text == "log\n" {
                reply(format!("{}\n", connections::state_get(&conn, "log").unwrap_or_default()))
            } else if text == "close\n" {
                connections::close(&conn)
            } else if let Some(topic) = text.strip_prefix("ordered ").and_then(|t| t.strip_suffix('\n')) {
                connections::subscribe(&conn, topic)?;
                reply("1\n".to_string())?;
                connections::publish(topic, &Message::Binary(b"2\n".to_vec()))?;
                reply("3\n".to_string())
            } else if let Some(args) = text.strip_prefix("amplify ").and_then(|t| t.strip_suffix('\n')) {
                // An app that answers a small request with a lot: what a
                // forged source address would turn on a victim.
                let (count, size) = args.split_once(' ').unwrap_or(("0", "0"));
                let (count, size): (u32, usize) = (count.parse().unwrap_or(0), size.parse().unwrap_or(0));
                for _ in 0..count {
                    let _ = connections::send(&conn, &Message::Binary(vec![b'a'; size]));
                }
                Ok(())
            } else if let Some(other) = text.strip_prefix("poke ").and_then(|t| t.strip_suffix('\n')) {
                // Everything a handler can do to a connection, aimed at an id
                // that is not one of this app's.
                let sent = connections::send(other, &Message::Text("hijack".to_string())).is_ok();
                let state = connections::state_get(other, "log").is_some();
                let set = connections::state_set(other, "log", Some("hijacked")).is_ok();
                let remote = connections::remote(other).is_some();
                let joined = connections::subscribe(other, "hijack").is_ok();
                let closed = connections::close(other).is_ok();
                reply(format!("send={sent} state={state} set={set} remote={remote} subscribe={joined} close={closed}\n"))
            } else if text == "trap\n" {
                panic!("the handler trapped on purpose")
            } else if text == "spin\n" {
                let mut n: u64 = 0;
                loop {
                    n = n.wrapping_add(1);
                    std::hint::black_box(n);
                }
            } else {
                connections::send(&conn, &Message::Binary(bytes))
            }
        }
        Event::Close => {
            note("close")?;
            connections::publish("closed", &Message::Text(format!("{remote}:{log}"))).map(|_| ())
        }
    }
}

export!(Handler);
