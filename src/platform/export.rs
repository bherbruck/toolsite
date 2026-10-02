//! Letting something outside read one app's database: `GET /export/<app>.sqlite`
//! with a bearer token minted for that app and nothing else.
//!
//! The publish token could already read every database through `run_sql`,
//! which is exactly why it is the wrong thing to hand a reporting tool. An
//! export token is narrower on every axis: one app, read only, a whole-file
//! snapshot rather than arbitrary SQL, revocable on its own. It is what an
//! owner pastes into something like a reporting tool, which pulls a SQLite file over
//! HTTP on a schedule.
//!
//! What goes over the wire is a snapshot taken with `VACUUM INTO`, never the
//! live file: the database runs in WAL mode, so a raw copy mid-write would be
//! torn, and a reader that saw it would blame its own tooling.
//!
//! Tokens live hashed in `<app>.exports` beside the app's other sidecars, so
//! removing the app takes them along, and a copy of the data directory is
//! not a set of live credentials.

use crate::{
    config::Config,
    content::slug::{random_token, valid_slug},
    runtime::db,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// Recognisable at a glance in a config field, and in a leak.
const PREFIX: &str = "tse_";
const MAX_LABEL: usize = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportToken {
    /// Short, public, names the token in a listing and a revocation.
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_used: Option<u64>,
    pub created_at: u64,
    /// The token itself, hashed.
    hash: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// For "3 days ago" in a listing.
pub fn seconds_since(then: u64) -> u64 {
    now().saturating_sub(then)
}

fn hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// An app here is a top-level directory with a `data.db`, which is the first
/// segment of any slug. A nested name would make the sidecar path ambiguous.
pub fn valid_app(app: &str) -> bool {
    valid_slug(app) && !app.contains('/')
}

fn path(config: &Config, app: &str) -> Option<PathBuf> {
    valid_app(app).then(|| config.data_dir.join(format!("{app}.exports")))
}

fn read(config: &Config, app: &str) -> Vec<ExportToken> {
    let Some(path) = path(config, app) else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(config: &Config, app: &str, tokens: &[ExportToken]) -> Result<(), String> {
    let path = path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if tokens.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) | Err(_) => return Ok(()),
        }
    }
    let text = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// Mints a token for `app`. The plain token is returned exactly once, here;
/// after this only its hash exists.
pub fn create(config: &Config, app: &str, label: &str) -> Result<(ExportToken, String), String> {
    if !valid_app(app) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    let label = label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL {
        return Err(format!("label must be 1 to {MAX_LABEL} characters: say what will hold the token"));
    }
    let token = format!("{PREFIX}{}", random_token(40));
    let entry = ExportToken {
        id: random_token(8),
        label: label.to_string(),
        last_used: None,
        created_at: now(),
        hash: hash(&token),
    };
    let mut tokens = read(config, app);
    tokens.push(entry.clone());
    write(config, app, &tokens)?;
    Ok((entry, token))
}

pub fn list(config: &Config, app: &str) -> Vec<ExportToken> {
    read(config, app)
}

/// Every app's tokens, for the admin page. Found by their sidecars, so an app
/// with none costs nothing to skip.
pub fn list_all(config: &Config) -> Vec<(String, ExportToken)> {
    let Ok(entries) = std::fs::read_dir(&config.data_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(app) = name.strip_suffix(".exports") else {
            continue;
        };
        if !valid_app(app) {
            continue;
        }
        for token in read(config, app) {
            out.push((app.to_string(), token));
        }
    }
    out.sort_by(|a, b| (&a.0, a.1.created_at).cmp(&(&b.0, b.1.created_at)));
    out
}

pub fn revoke(config: &Config, app: &str, id: &str) -> Result<(), String> {
    let mut tokens = read(config, app);
    let before = tokens.len();
    tokens.retain(|token| token.id != id);
    if tokens.len() == before {
        return Err(format!("no export token {id} on {app}"));
    }
    write(config, app, &tokens)
}

/// Whether `presented` is a live export token for `app`. Marks it used, so a
/// listing can say which tokens still earn their keep.
pub fn authorize(config: &Config, app: &str, presented: &str) -> bool {
    if !presented.starts_with(PREFIX) {
        return false;
    }
    let wanted = hash(presented);
    let mut tokens = read(config, app);
    let Some(token) = tokens.iter_mut().find(|token| token.hash == wanted) else {
        return false;
    };
    token.last_used = Some(now());
    let _ = write(config, app, &tokens);
    true
}

/// A consistent copy of the app's database as a file the caller owns and
/// must delete. Nothing if the app has no database yet.
pub fn snapshot(config: &Config, app: &str) -> Result<Option<PathBuf>, String> {
    let source = db::db_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if !source.is_file() {
        return Ok(None);
    }
    let dir = config.data_dir.join(".tmp");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(format!("export-{}.sqlite", random_token(16)));
    // A plain read-only connection rather than the guarded one: VACUUM INTO
    // attaches its target internally, which the guard's attach limit of zero
    // refuses. It is the one statement this connection runs, and it is ours.
    let conn = rusqlite::Connection::open_with_flags(
        &source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    let target_str = target.to_string_lossy().replace('\'', "''");
    if let Err(error) = conn.execute_batch(&format!("vacuum into '{target_str}'")) {
        let _ = std::fs::remove_file(&target);
        return Err(format!("snapshot failed: {error}"));
    }
    Ok(Some(target))
}

pub fn export_url(config: &Config, app: &str) -> String {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    format!("{base}/export/{app}.sqlite")
}

// --- the endpoint ---------------------------------------------------------

/// `GET /export/<app>.sqlite`. One request, one whole file, as a reporting
/// tool that pulls SQLite over HTTP expects: no range, no redirect, a plain
/// path with no query string.
pub(crate) async fn download(
    axum::extract::State(config): axum::extract::State<std::sync::Arc<Config>>,
    axum::extract::Path(file): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};

    let app = file.strip_suffix(".sqlite").unwrap_or(&file).to_string();
    if !valid_app(&app) {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }
    let presented = crate::platform::bearer::presented_token(&headers).map(str::to_string);
    let authorized = {
        let (config, app) = (config.clone(), app.clone());
        let presented = presented.clone();
        tokio::task::spawn_blocking(move || {
            presented.is_some_and(|token| authorize(&config, &app, &token))
        })
        .await
        .unwrap_or(false)
    };
    if !authorized {
        // The same wording whether the app, the token or both are wrong, so
        // a token cannot be used to learn which apps exist.
        tracing::warn!(
            app = %app,
            token_presented = presented.is_some(),
            user_agent = %headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>"),
            "export refused: no export token for this app"
        );
        return (StatusCode::UNAUTHORIZED, "no export token for this app\n").into_response();
    }

    let copy = {
        let (config, app) = (config.clone(), app.clone());
        tokio::task::spawn_blocking(move || snapshot(&config, &app)).await
    };
    let path = match copy {
        Ok(Ok(Some(path))) => path,
        Ok(Ok(None)) => return (StatusCode::NOT_FOUND, "this app has no database yet\n").into_response(),
        Ok(Err(message)) => {
            tracing::warn!(app = %app, %message, "export snapshot failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not snapshot the database\n").into_response();
        }
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "could not snapshot the database\n").into_response(),
    };

    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "could not read the snapshot\n").into_response(),
    };
    let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    // Unlinked while open: the stream keeps the bytes, the directory does
    // not keep the file, and nothing is left behind if the client goes away.
    let _ = tokio::fs::remove_file(&path).await;
    tracing::info!(app = %app, size, "database exported");
    let stream = tokio_util::io::ReaderStream::new(file);
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/vnd.sqlite3")
        .header(axum::http::header::CONTENT_LENGTH, size)
        .header(axum::http::header::CACHE_CONTROL, "no-store")
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{app}.sqlite\""),
        )
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "publish-token");
        (dir, config)
    }

    #[test]
    fn a_token_is_shown_once_and_stored_only_as_a_hash() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "sales", "reporting").unwrap();
        assert!(token.starts_with("tse_"));
        let stored = std::fs::read_to_string(dir.path().join("sales.exports")).unwrap();
        assert!(!stored.contains(&token), "the plain token is on disk");
        assert!(stored.contains(&entry.id));
        assert!(authorize(&config, "sales", &token));
        assert_eq!(list(&config, "sales")[0].label, "reporting");
        assert!(list(&config, "sales")[0].last_used.is_some(), "use was not recorded");
    }

    #[test]
    fn a_token_opens_one_app_and_no_other() {
        let (_dir, config) = config();
        let (_, token) = create(&config, "sales", "x").unwrap();
        assert!(!authorize(&config, "hr", &token));
        assert!(!authorize(&config, "sales", "publish-token"), "the publish token was accepted");
        assert!(!authorize(&config, "sales", ""));
        assert!(!authorize(&config, "../sales", &token));
    }

    #[test]
    fn revoking_ends_it_and_an_empty_list_leaves_no_file() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "sales", "x").unwrap();
        revoke(&config, "sales", &entry.id).unwrap();
        assert!(!authorize(&config, "sales", &token));
        assert!(!dir.path().join("sales.exports").exists());
        assert!(revoke(&config, "sales", &entry.id).is_err());
    }

    #[test]
    fn an_app_name_that_could_leave_the_data_directory_is_refused() {
        let (dir, config) = config();
        for app in ["../x", "a/b", ".site", "", "a b"] {
            assert!(create(&config, app, "x").is_err(), "{app:?} accepted");
        }
        assert!(!dir.path().join("x.exports").exists());
    }

    #[test]
    fn a_snapshot_is_a_whole_consistent_database_not_the_live_file() {
        let (dir, config) = config();
        assert!(snapshot(&config, "sales").unwrap().is_none(), "a database that does not exist");
        db::run(&config, "sales", "create table t (n integer)", &[]).unwrap();
        db::run(&config, "sales", "insert into t values (1), (2), (3)", &[]).unwrap();
        // A write still open elsewhere must not leak into, or block, the copy.
        let live = db::open_unguarded(&dir.path().join("sales/data.db"), 0).unwrap();
        live.execute_batch("begin; insert into t values (4);").unwrap();

        let copy = snapshot(&config, "sales").unwrap().expect("a snapshot");
        assert!(copy.starts_with(dir.path().join(".tmp")));
        let conn = rusqlite::Connection::open(&copy).unwrap();
        let n: i64 = conn.query_row("select count(*) from t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
        // A plain SQLite file, not a WAL set.
        let mode: String = conn.query_row("pragma journal_mode", [], |r| r.get(0)).unwrap();
        assert_ne!(mode, "wal");
    }
}
