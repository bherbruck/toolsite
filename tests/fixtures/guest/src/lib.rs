//! Test fixture: the smallest handler that exercises every capability the
//! host grants, plus the ones it doesn't.

wit_bindgen::generate!({
    path: "../../../wit",
    world: "app",
});

use toolsite::app::db;
use toolsite::app::identity;
use toolsite::app::fetch;
use toolsite::app::secrets;

struct Handler;

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
