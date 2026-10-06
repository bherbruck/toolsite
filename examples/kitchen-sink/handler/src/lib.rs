//! The kitchen sink: one handler that uses every capability the platform
//! grants, so each screen of the app has something real behind it.
//!
//! A handler has no clock. Timestamps come from SQLite:
//!
//!     select cast(strftime('%s','now') as integer)

wit_bindgen::generate!({
    path: "wit",
    world: "app",
});

use serde_json::{json, Map, Value};
use toolsite::app::{blobs, db, fetch, identity, secrets};

/// What the handler checks with current-role. toolsite.toml lists the same
/// names so whoever grants access sees them as suggestions.
const ROLES: [&str; 2] = ["viewer", "manager"];

/// Ten MiB a file. The platform has its own ceiling as well.
const MAX_FILE: u64 = 10 * 1024 * 1024;

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        let path = req.path.clone();
        let method = req.method.clone();
        match (method.as_str(), path.as_str()) {
            // Public by a route rule: a monitor with no account reads this.
            ("GET", "/status") => status(),
            // Restricted by a route rule, then the role is checked here.
            ("GET", "/admin") => admin_page(),

            ("GET", "/api/overview") => overview(),
            ("GET", "/api/me") => me(),
            ("GET", "/api/orders") => list_orders(&json!({})),
            ("POST", "/api/orders") => create_order(&body(&req)),
            ("POST", "/api/sql") => scoped_sql(&body(&req)),
            ("GET", "/api/files") => list_files(),
            ("POST", "/api/files") => file_upload_url(&body(&req)),
            ("GET", "/api/settings") => settings(),
            ("POST", "/api/fetch") => outbound(&body(&req)),
            ("GET", "/api/heartbeat") => heartbeat(&req),
            ("GET", "/api/heartbeats") => heartbeats(),
            ("GET", "/api/admin/members") => members(),
            ("POST", "/api/admin/members") => place_member(&body(&req)),

            // App tools: the platform signed the person in and calls these
            // as them, with the arguments under "arguments".
            ("POST", "/api/tools/create_order") => create_order(&body(&req)["arguments"]),
            ("POST", "/api/tools/list_my_orders") => list_orders(&body(&req)["arguments"]),

            (m, p) => {
                if let Some(rest) = p.strip_prefix("/api/orders/") {
                    if let Some(id) = rest.strip_suffix("/status") {
                        if m == "POST" {
                            return set_order_status(id, &body(&req));
                        }
                    }
                }
                if let Some(name) = p.strip_prefix("/api/files/") {
                    return match m {
                        "GET" => serve_file(name),
                        "DELETE" => delete_file(name),
                        _ => error(405, "Use GET or DELETE on a file."),
                    };
                }
                error(404, "No such route.")
            }
        }
    }
}

export!(Handler);

// --- orders and row-level access ------------------------------------------

/// The unrestricted view: counts across every location. The handler's own
/// `db::query` sees the whole database; only totals leave it here.
fn overview() -> Response {
    let totals = match db::query(
        "select location, status, count(*) as orders, sum(quantity) as units \
         from orders group by location, status order by location, status",
        &[],
    ) {
        Ok(rows) => rows_json(&rows),
        Err(e) => return db_error(&e),
    };
    let tables = match db::query(
        "select name, type from sqlite_master \
         where type in ('table', 'view') and name not like 'ts\\_%' escape '\\' \
         and name not like 'sqlite\\_%' escape '\\' order by type, name",
        &[],
    ) {
        Ok(rows) => rows_json(&rows),
        Err(e) => return db_error(&e),
    };
    json_response(200, json!({ "totals": totals, "tables": tables }))
}

/// Orders through the policy: the view `my_orders` holds only the rows of
/// the person's locations, whatever the SQL asks for.
fn list_orders(args: &Value) -> Response {
    let status = args["status"].as_str().map(|s| db::Value::Text(s.into())).unwrap_or(db::Value::Null);
    match db::query_scoped(
        "select id, location, customer, item, quantity, status, note, created_at \
         from my_orders where (?1 is null or status = ?1) order by id desc limit 200",
        &[status],
    ) {
        Ok(rows) => json_response(200, json!({ "orders": rows_json(&rows) })),
        Err(e) => db_error(&e),
    }
}

fn create_order(args: &Value) -> Response {
    if identity::current_user().is_none() {
        return error(401, "Sign in to create an order.");
    }
    let customer = args["customer"].as_str().unwrap_or("").trim();
    let item = args["item"].as_str().unwrap_or("").trim();
    let quantity = args["quantity"].as_i64().unwrap_or(0);
    if customer.is_empty() || item.is_empty() {
        return error(400, "Give a customer and an item.");
    }
    if quantity < 1 {
        return error(400, "The quantity must be 1 or more.");
    }
    let location = match args["location"].as_str().filter(|s| !s.is_empty()) {
        Some(code) => code.to_string(),
        None => match first_location() {
            Some(code) => code,
            None => {
                return error(
                    403,
                    "You are not a member of a location. Ask a manager to add you on the admin page.",
                )
            }
        },
    };
    let note = args["note"].as_str().map(|s| db::Value::Text(s.into())).unwrap_or(db::Value::Null);

    // Through the view: the policy fills owner_id and refuses a location the
    // person does not belong to.
    if let Err(e) = db::query_scoped(
        "insert into my_orders (location, customer, item, quantity, note) values (?, ?, ?, ?, ?)",
        &[
            db::Value::Text(location.clone()),
            db::Value::Text(customer.into()),
            db::Value::Text(item.into()),
            db::Value::Integer(quantity),
            note,
        ],
    ) {
        return match e {
            db::Error::Failed(m) | db::Error::Denied(m) => error(
                403,
                &format!("You cannot create an order at '{location}'. {m}"),
            ),
        };
    }
    match db::query_scoped(
        "select id, location, customer, item, quantity, status, note, created_at \
         from my_orders where owner_id = current_user() order by id desc limit 1",
        &[],
    ) {
        Ok(rows) => {
            let order = rows_json(&rows).as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null);
            json_response(201, json!({ "order": order }))
        }
        Err(e) => db_error(&e),
    }
}

fn set_order_status(id: &str, args: &Value) -> Response {
    let Ok(id) = id.parse::<i64>() else {
        return error(400, "The order id must be a number.");
    };
    let status = args["status"].as_str().unwrap_or("");
    if !["open", "shipped", "cancelled"].contains(&status) {
        return error(400, "The status must be open, shipped or cancelled.");
    }
    match db::query_scoped(
        "update my_orders set status = ? where id = ?",
        &[db::Value::Text(status.into()), db::Value::Integer(id)],
    ) {
        Ok(rows) if rows.rows_affected == 0 => error(404, "No order with that id at your locations."),
        Ok(_) => json_response(200, json!({ "id": id, "status": status })),
        Err(e) => db_error(&e),
    }
}

/// The person's own SQL, run inside the declared views and nothing else.
fn scoped_sql(args: &Value) -> Response {
    let sql = args["sql"].as_str().unwrap_or("").trim();
    if sql.is_empty() {
        return error(400, "Type a statement.");
    }
    match db::query_scoped(sql, &[]) {
        Ok(rows) => json_response(
            200,
            json!({
                "columns": rows.columns,
                "rows": rows.values.iter().map(|r| r.iter().map(value_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "truncated": rows.truncated,
                "rows_affected": rows.rows_affected,
            }),
        ),
        Err(e) => db_error(&e),
    }
}

fn first_location() -> Option<String> {
    let rows = db::query_scoped("select code from my_locations order by code limit 1", &[]).ok()?;
    match rows.values.first()?.first()? {
        db::Value::Text(code) => Some(code.clone()),
        _ => None,
    }
}

// --- identity and roles ---------------------------------------------------

fn me() -> Response {
    let user = identity::current_user().map(|u| json!({ "id": u.id, "email": u.email }));
    let role = identity::current_role();
    // The same three answers, asked of SQL: the host binds them on every
    // connection, which is what row-level policies are written against.
    let sql = match db::query("select current_user(), current_email(), current_role()", &[]) {
        Ok(rows) => rows.values.first().map(|r| r.iter().map(value_json).collect::<Vec<_>>()).unwrap_or_default(),
        Err(e) => return db_error(&e),
    };
    let locations = match db::query_scoped("select code, name from my_locations order by code", &[]) {
        Ok(rows) => rows_json(&rows),
        Err(e) => return db_error(&e),
    };
    json_response(
        200,
        json!({
            "user": user,
            "role": role,
            "roles": ROLES,
            "is_manager": is_manager(),
            "sql": { "current_user": sql.first(), "current_email": sql.get(1), "current_role": sql.get(2) },
            "locations": locations,
        }),
    )
}

fn is_manager() -> bool {
    identity::current_role().as_deref() == Some("manager")
}

// --- admin: restricted by a route rule, then by role ----------------------

fn members() -> Response {
    if !is_manager() {
        return error(403, "Only a manager can see who works where.");
    }
    match db::query("select email, location from members order by location, email", &[]) {
        Ok(rows) => json_response(200, json!({ "members": rows_json(&rows) })),
        Err(e) => db_error(&e),
    }
}

fn place_member(args: &Value) -> Response {
    if !is_manager() {
        return error(403, "Only a manager can place people.");
    }
    let email = args["email"].as_str().unwrap_or("").trim().to_lowercase();
    let location = args["location"].as_str().unwrap_or("").trim();
    if !email.contains('@') {
        return error(400, "Give an email address.");
    }
    let result = if location.is_empty() {
        db::query("delete from members where email = ?", &[db::Value::Text(email.clone())])
    } else {
        db::query(
            "insert into members (email, location) values (?, ?) \
             on conflict (email) do update set location = excluded.location",
            &[db::Value::Text(email.clone()), db::Value::Text(location.into())],
        )
    };
    match result {
        Ok(_) => json_response(200, json!({ "email": email, "location": location })),
        Err(e) => db_error(&e),
    }
}

fn admin_page() -> Response {
    if !is_manager() {
        return html(
            403,
            "<h1>Managers only</h1><p>You have access to this app, but not the manager role. \
             Ask the site owner to grant it.</p>",
        );
    }
    let rows = match db::query("select email, location from members order by location, email", &[]) {
        Ok(rows) => rows,
        Err(e) => return html(500, &format!("<p>{}</p>", escape(&format!("{e:?}")))),
    };
    let mut table = String::from("<table><tr><th>Email</th><th>Location</th></tr>");
    for row in &rows.values {
        let cells: Vec<String> = row.iter().map(|v| escape(&value_json(v).as_str().unwrap_or("").to_string())).collect();
        table.push_str(&format!("<tr><td>{}</td><td>{}</td></tr>", cells[0], cells[1]));
    }
    table.push_str("</table>");
    html(
        200,
        &format!(
            "<h1>Members</h1><p>This page is behind a route rule (<code>/admin</code>, restricted) \
             and a role check in the handler. Change members on the Admin screen of the app.</p>{table}\
             <p><a href=\"./\">Back to the app</a></p>"
        ),
    )
}

// --- files ----------------------------------------------------------------

/// Listed from the store, so a file shows only once its bytes arrived.
fn list_files() -> Response {
    let entries = match blobs::list("files/") {
        Ok(entries) => entries,
        Err(e) => return blob_error(&e),
    };
    let names: Map<String, Value> = match db::query("select key, name, uploaded_by, at from files", &[]) {
        Ok(rows) => rows_json(&rows)
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some((r["key"].as_str()?.to_string(), r.clone())))
            .collect(),
        Err(e) => return db_error(&e),
    };
    let files: Vec<Value> = entries
        .iter()
        .map(|e| {
            let meta = names.get(&e.key).cloned().unwrap_or(Value::Null);
            json!({
                "key": e.key,
                "name": e.key.trim_start_matches("files/"),
                "size": e.size,
                "uploaded_by": meta["uploaded_by"],
                "at": meta["at"],
            })
        })
        .collect();
    json_response(200, json!({ "files": files }))
}

/// Hands the browser a URL to PUT the file to. The bytes go straight to
/// storage; this handler never holds them.
fn file_upload_url(args: &Value) -> Response {
    let Some(who) = identity::current_user() else {
        return error(401, "Sign in to upload.");
    };
    let Some(name) = file_name(args["name"].as_str().unwrap_or("")) else {
        return error(400, "Use a file name of letters, digits, '.', '-' and '_'.");
    };
    let key = format!("files/{name}");
    let url = match blobs::upload_url(&key, MAX_FILE) {
        Ok(url) => url,
        Err(e) => return blob_error(&e),
    };
    if let Err(e) = db::query(
        "insert into files (key, name, uploaded_by) values (?, ?, ?) \
         on conflict (key) do update set uploaded_by = excluded.uploaded_by, at = excluded.at",
        &[db::Value::Text(key.clone()), db::Value::Text(name.clone()), db::Value::Text(who.email)],
    ) {
        return db_error(&e);
    }
    json_response(200, json!({ "key": key, "name": name, "url": url, "max_bytes": MAX_FILE }))
}

/// Answers with a header and no body: the platform streams the stored file
/// in its place, at any size.
fn serve_file(name: &str) -> Response {
    let Some(name) = file_name(name) else {
        return error(400, "Not a file name.");
    };
    let key = format!("files/{name}");
    match blobs::stat(&key) {
        Ok(Some(_)) => Response {
            status: 200,
            headers: vec![
                ("x-toolsite-blob".into(), key),
                ("content-disposition".into(), format!("inline; filename=\"{name}\"")),
            ],
            body: Vec::new(),
        },
        Ok(None) => error(404, "No such file."),
        Err(e) => blob_error(&e),
    }
}

fn delete_file(name: &str) -> Response {
    let Some(name) = file_name(name) else {
        return error(400, "Not a file name.");
    };
    let key = format!("files/{name}");
    if let Err(e) = blobs::delete(&key) {
        return blob_error(&e);
    }
    if let Err(e) = db::query("delete from files where key = ?", &[db::Value::Text(key)]) {
        return db_error(&e);
    }
    json_response(200, json!({ "deleted": name }))
}

/// One path segment the blob store accepts: letters, digits, '.', '-', '_',
/// not starting with '.'.
fn file_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    ok.then(|| name.to_string())
}

// --- settings and outbound fetch -------------------------------------------

/// Which settings exist, and whether GREETING is set. Never a value: the
/// response reaches a browser.
fn settings() -> Response {
    json_response(
        200,
        json!({
            "names": secrets::names(),
            "greeting_set": secrets::get("GREETING").is_some_and(|v| !v.is_empty()),
        }),
    )
}

/// A GET to any URL the person types. Only hosts under allow_http answer;
/// the error says why the others did not.
fn outbound(args: &Value) -> Response {
    let url = args["url"].as_str().unwrap_or("https://api.github.com/zen").trim().to_string();
    let request = fetch::Request {
        method: "GET".into(),
        url: url.clone(),
        headers: vec![
            ("user-agent".into(), "toolsite-kitchen-sink".into()),
            ("accept".into(), "text/plain, application/json".into()),
        ],
        body: Vec::new(),
    };
    match fetch::send(&request) {
        Ok(response) => {
            let text = String::from_utf8_lossy(&response.body);
            let text: String = text.chars().take(2000).collect();
            json_response(200, json!({ "ok": true, "url": url, "status": response.status, "body": text }))
        }
        Err(reason) => json_response(200, json!({ "ok": false, "url": url, "error": reason })),
    }
}

// --- the scheduled job ------------------------------------------------------

/// Runs on the schedule only. The host sets x-toolsite-scheduled and strips
/// any x-toolsite-* header a client sends, so a visitor cannot forge it.
fn heartbeat(req: &Request) -> Response {
    if header(req, "x-toolsite-scheduled").is_none() {
        return error(403, "This path runs on the schedule only.");
    }
    if let Err(e) = db::query("insert into heartbeats default values", &[]) {
        return db_error(&e);
    }
    // Keep a day of five-minute beats.
    if let Err(e) = db::query("delete from heartbeats where id <= (select max(id) - 288 from heartbeats)", &[]) {
        return db_error(&e);
    }
    text(200, "beat")
}

fn heartbeats() -> Response {
    match db::query("select id, at from heartbeats order by id desc limit 10", &[]) {
        Ok(rows) => json_response(200, json!({ "beats": rows_json(&rows) })),
        Err(e) => db_error(&e),
    }
}

fn status() -> Response {
    let last = db::query("select max(at) from heartbeats", &[])
        .ok()
        .and_then(|r| r.values.first().and_then(|row| row.first().map(value_json)))
        .unwrap_or(Value::Null);
    json_response(200, json!({ "ok": true, "last_heartbeat": last }))
}

// --- small helpers ---------------------------------------------------------

fn body(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap_or(Value::Null)
}

fn header<'r>(req: &'r Request, name: &str) -> Option<&'r str> {
    req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn value_json(v: &db::Value) -> Value {
    match v {
        db::Value::Null => Value::Null,
        db::Value::Integer(i) => json!(i),
        db::Value::Real(f) => json!(f),
        db::Value::Text(s) => json!(s),
    }
}

/// Rows as objects keyed by column, which is what a front end wants.
fn rows_json(rows: &db::Rows) -> Value {
    Value::Array(
        rows.values
            .iter()
            .map(|row| {
                Value::Object(
                    rows.columns.iter().cloned().zip(row.iter().map(value_json)).collect::<Map<_, _>>(),
                )
            })
            .collect(),
    )
}

fn json_response(status: u16, value: Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}

fn error(status: u16, message: &str) -> Response {
    json_response(status, json!({ "error": message }))
}

fn db_error(e: &db::Error) -> Response {
    match e {
        db::Error::Failed(m) => error(400, m),
        db::Error::Denied(m) => error(403, m),
    }
}

fn blob_error(e: &blobs::Error) -> Response {
    match e {
        blobs::Error::NotFound => error(404, "No such file."),
        blobs::Error::InvalidKey(m) => error(400, m),
        blobs::Error::TooLarge(n) => error(413, &format!("Too large: {n} bytes.")),
        blobs::Error::Failed(m) => error(500, m),
    }
}

fn text(status: u16, body: &str) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
        body: body.as_bytes().to_vec(),
    }
}

fn html(status: u16, inner: &str) -> Response {
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Kitchen sink admin</title><style>body{{font:15px/1.5 system-ui,sans-serif;\
         max-width:40rem;margin:3rem auto;padding:0 1rem;color-scheme:light dark}}\
         table{{border-collapse:collapse;width:100%}}td,th{{text-align:left;padding:.3rem .5rem;\
         border-bottom:1px solid #8884}}</style></head><body>{inner}</body></html>"
    );
    Response {
        status,
        headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
        body: page.into_bytes(),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
