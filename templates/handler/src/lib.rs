//! A handler has no clock: `std::time` does not link, because the world
//! imports no clock capability. Timestamps come from SQLite, which is the
//! same clock the rows are stamped with:
//!
//!     select cast(strftime('%s','now') as integer)

wit_bindgen::generate!({
    path: "wit",
    world: "app",
});

use toolsite::app::db;

struct Handler;

fn text(status: u16, body: impl Into<String>) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
        body: body.into().into_bytes(),
    }
}

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        // The host passes the path relative to this app, with /api still on
        // it, so strip the prefix the way any router would.
        let route = req.path.strip_prefix("/api").unwrap_or(&req.path);

        match (req.method.as_str(), route) {
            ("GET", "/hello") => {
                // The table comes from migrations/001_initial.sql, applied at
                // deploy. State must live here rather than in a global: every
                // request gets a fresh instance.
                if let Err(e) = db::query("insert into visits values (0)", &[]) {
                    return text(500, format!("{e:?}"));
                }
                match db::query("select count(*) from visits", &[]) {
                    Ok(rows) => match rows.values.first().and_then(|r| r.first()) {
                        Some(db::Value::Integer(n)) => text(200, format!("visit #{n}")),
                        _ => text(500, "unexpected shape"),
                    },
                    Err(e) => text(500, format!("{e:?}")),
                }
            }
            _ => text(404, "not found"),
        }
    }
}

export!(Handler);
