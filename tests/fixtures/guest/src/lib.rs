//! Test fixture: the smallest handler that exercises every capability the
//! host grants, plus the ones it doesn't.

wit_bindgen::generate!({
    path: "../../../wit",
    world: "app",
});

use toolsite::app::blobs;
use toolsite::app::db;
use toolsite::app::identity;
use toolsite::app::fetch;
use toolsite::app::secrets;

struct Handler;

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
}

export!(Handler);
