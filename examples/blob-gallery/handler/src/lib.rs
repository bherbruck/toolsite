//! A photo gallery on the app's file store.
//!
//! The handler never holds a photo's bytes. To take one, it hands the
//! browser two upload URLs (the full image and a thumbnail the browser drew
//! itself) and the browser PUTs straight to storage. To show one, it answers
//! with `x-toolsite-blob: <key>` and an empty body, and the platform streams
//! the file in its place.

wit_bindgen::generate!({
    path: "wit",
    world: "app",
});

use serde_json::{json, Map, Value};
use toolsite::app::{blobs, db, identity};

/// What a full image may weigh. The thumbnail gets a tenth of it.
const MAX_PHOTO: u64 = 20 * 1024 * 1024;

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let body = || serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
        let answer = match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["api", "me"]) => me(),
            ("GET", ["api", "photos"]) => list(),
            ("POST", ["api", "photos"]) => begin(&body()),
            ("POST", ["api", "photos", id, "ready"]) => id_of(id).and_then(ready),
            ("GET", ["api", "photos", id, size @ ("full" | "thumb")]) => id_of(id).and_then(|id| serve(id, size)),
            ("DELETE", ["api", "photos", id]) => id_of(id).and_then(remove),
            _ => Err(fail(404, "No such route.")),
        };
        answer.unwrap_or_else(|(status, value)| json_response(status, &value))
    }
}

export!(Handler);

type Answer = Result<Response, (u16, Value)>;

fn me() -> Answer {
    let user = identity::current_user().map(|u| json!({ "id": u.id, "email": u.email }));
    Ok(json_response(200, &json!({ "user": user, "is_curator": is_curator() })))
}

fn list() -> Answer {
    let photos = rows(db::query(
        "select id, caption, owner_id, owner_email, content_type, size, created_at \
         from photos where ready = 1 order by id desc limit 500",
        &[],
    ))?;
    Ok(json_response(200, &json!({ "photos": photos })))
}

/// Step one: a row, and two URLs to PUT to. Nothing shows until `ready`.
fn begin(args: &Value) -> Answer {
    let Some(who) = identity::current_user() else {
        return Err(fail(401, "Sign in to add photos."));
    };
    let caption: String = args["caption"].as_str().unwrap_or("").trim().chars().take(200).collect();
    db::query(
        "insert into photos (caption, owner_id, owner_email) values (?, ?, ?)",
        &[db::Value::Text(caption), db::Value::Text(who.id.clone()), db::Value::Text(who.email)],
    )
    .map_err(|e| db_fail(&e))?;
    let id = rows(db::query(
        "select max(id) as id from photos where owner_id = ? and ready = 0",
        &[db::Value::Text(who.id)],
    ))?
    .as_array()
    .and_then(|a| a.first())
    .and_then(|r| r["id"].as_i64())
    .ok_or_else(|| fail(500, "The photo was not recorded."))?;

    let full = blobs::upload_url(&key(id, "full"), MAX_PHOTO).map_err(|e| blob_fail(&e))?;
    let thumb = blobs::upload_url(&key(id, "thumb"), MAX_PHOTO / 10).map_err(|e| blob_fail(&e))?;
    Ok(json_response(200, &json!({ "id": id, "full": full, "thumb": thumb })))
}

/// Step two: the browser says both PUTs finished. Check that they did, and
/// that what arrived is an image, before the photo is listed.
fn ready(id: i64) -> Answer {
    let owner = owner_of(id)?;
    let me = identity::current_user().ok_or_else(|| fail(401, "Sign in first."))?;
    if owner != me.id {
        return Err(fail(403, "That upload is not yours."));
    }
    let mut stats = Vec::new();
    for size in ["full", "thumb"] {
        match blobs::stat(&key(id, size)).map_err(|e| blob_fail(&e))? {
            Some(entry) => stats.push(entry),
            None => return Err(fail(409, &format!("The {size} image has not arrived yet."))),
        }
    }
    if stats.iter().any(|e| !e.content_type.starts_with("image/")) {
        // Not an image: take it back out rather than serve it.
        for size in ["full", "thumb"] {
            let _ = blobs::delete(&key(id, size));
        }
        let _ = db::query("delete from photos where id = ?", &[db::Value::Integer(id)]);
        return Err(fail(415, "Only images can go in the gallery."));
    }
    db::query(
        "update photos set ready = 1, content_type = ?, size = ? where id = ?",
        &[
            db::Value::Text(stats[0].content_type.clone()),
            db::Value::Integer(stats[0].size as i64),
            db::Value::Integer(id),
        ],
    )
    .map_err(|e| db_fail(&e))?;
    Ok(json_response(200, &json!({ "id": id, "ready": true })))
}

/// A header and no body: the platform streams the file, at any size.
fn serve(id: i64, size: &str) -> Answer {
    let k = key(id, size);
    match blobs::stat(&k).map_err(|e| blob_fail(&e))? {
        Some(_) => Ok(Response {
            status: 200,
            headers: vec![
                ("x-toolsite-blob".into(), k),
                ("cache-control".into(), "private, max-age=3600".into()),
            ],
            body: Vec::new(),
        }),
        None => Err(fail(404, "No such photo.")),
    }
}

fn remove(id: i64) -> Answer {
    let owner = owner_of(id)?;
    let me = identity::current_user().ok_or_else(|| fail(401, "Sign in first."))?;
    if owner != me.id && !is_curator() {
        return Err(fail(403, "Only the person who added a photo, or a curator, can remove it."));
    }
    for size in ["full", "thumb"] {
        match blobs::delete(&key(id, size)) {
            Ok(()) | Err(blobs::Error::NotFound) => {}
            Err(e) => return Err(blob_fail(&e)),
        }
    }
    db::query("delete from photos where id = ?", &[db::Value::Integer(id)]).map_err(|e| db_fail(&e))?;
    Ok(json_response(200, &json!({ "removed": id })))
}

// --- helpers -------------------------------------------------------------------

fn key(id: i64, size: &str) -> String {
    format!("photos/{id}/{size}")
}

fn is_curator() -> bool {
    identity::current_role().as_deref() == Some("curator")
}

fn owner_of(id: i64) -> Result<String, (u16, Value)> {
    rows(db::query("select owner_id from photos where id = ?", &[db::Value::Integer(id)]))?
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r["owner_id"].as_str().map(str::to_string))
        .ok_or_else(|| fail(404, "No such photo."))
}

fn id_of(raw: &str) -> Result<i64, (u16, Value)> {
    raw.parse().map_err(|_| fail(400, "An id must be a number."))
}

fn fail(status: u16, message: &str) -> (u16, Value) {
    (status, json!({ "error": message }))
}

fn db_fail(e: &db::Error) -> (u16, Value) {
    match e {
        db::Error::Failed(m) => fail(400, m),
        db::Error::Denied(m) => fail(403, m),
    }
}

fn blob_fail(e: &blobs::Error) -> (u16, Value) {
    match e {
        blobs::Error::NotFound => fail(404, "No such file."),
        blobs::Error::InvalidKey(m) => fail(400, m),
        blobs::Error::TooLarge(n) => fail(413, &format!("Too large: {n} bytes.")),
        blobs::Error::Failed(m) => fail(500, m),
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

fn rows(result: Result<db::Rows, db::Error>) -> Result<Value, (u16, Value)> {
    let rows = result.map_err(|e| db_fail(&e))?;
    Ok(Value::Array(
        rows.values
            .iter()
            .map(|row| Value::Object(rows.columns.iter().cloned().zip(row.iter().map(value_json)).collect::<Map<_, _>>()))
            .collect(),
    ))
}

fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}
