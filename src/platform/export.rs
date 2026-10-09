//! Letting something outside read one app's database: `GET /export/<app>.sqlite`
//! with a bearer token minted for that app and nothing else.
//!
//! The publish token could already read every database through `run_sql`,
//! which is exactly why it is the wrong thing to hand a reporting tool. An
//! export token is narrower on every axis: one app, read only, a whole-file
//! snapshot rather than arbitrary SQL, revocable on its own. It is what an
//! owner pastes into a reporting tool that pulls a SQLite file over
//! HTTP on a schedule.
//!
//! What goes over the wire is a snapshot taken with `VACUUM INTO`, never the
//! live file: the database runs in WAL mode, so a raw copy mid-write would be
//! torn, and a reader that saw it would blame its own tooling.
//!
//! Tokens live hashed in the `tokens` store (`<app>.exports` on files)
//! beside the app's other records, so removing the app takes them along,
//! and a copy of the data directory is not a set of live credentials.

use crate::{
    config::Config,
    content::slug::random_token,
    platform::tokens::{self, Kind},
    runtime::db,
};
use std::path::PathBuf;

pub use crate::platform::tokens::seconds_since;

pub type ExportToken = tokens::Token;

/// An app here is a top-level directory with a `data.db`, which is the first
/// segment of any slug. A nested name would make the sidecar path ambiguous.
pub fn valid_app(app: &str) -> bool {
    tokens::valid_app(app)
}

/// Mints a token for `app`. The plain token is returned exactly once, here;
/// after this only its hash exists.
pub async fn create(config: &Config, app: &str, label: &str) -> Result<(ExportToken, String), String> {
    tokens::create(config, app, Kind::Export, label, "what will hold the token").await
}

pub async fn list(config: &Config, app: &str) -> Vec<ExportToken> {
    tokens::list(config, app, Kind::Export).await
}

/// Every app's tokens, for the admin page.
pub async fn list_all(config: &Config) -> Vec<(String, ExportToken)> {
    tokens::list_all(config, Kind::Export).await
}

pub async fn revoke(config: &Config, app: &str, id: &str) -> Result<(), String> {
    tokens::revoke(config, app, Kind::Export, id).await
}

/// Whether `presented` is a live export token for `app`. Marks it used, so a
/// listing can say which tokens still earn their keep.
pub async fn authorize(config: &Config, app: &str, presented: &str) -> bool {
    tokens::check(config, app, Kind::Export, presented).await.is_some()
}

/// A consistent copy of the app's database as a file the caller owns and
/// must delete. Nothing if the app has no database yet.
pub fn snapshot(config: &Config, app: &str) -> Result<Option<PathBuf>, String> {
    let source = db::app_db(config, app)?;
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
    let authorized = match &presented {
        Some(token) => authorize(&config, &app, token).await,
        None => false,
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

    #[tokio::test]
    async fn a_token_is_shown_once_and_stored_only_as_a_hash() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "sales", "reporting").await.unwrap();
        assert!(token.starts_with("tse_"));
        let stored = std::fs::read_to_string(dir.path().join("sales.exports")).unwrap();
        assert!(!stored.contains(&token), "the plain token is on disk");
        assert!(stored.contains(&entry.id));
        assert!(authorize(&config, "sales", &token).await);
        assert_eq!(list(&config, "sales").await[0].label, "reporting");
        assert!(list(&config, "sales").await[0].last_used.is_some(), "use was not recorded");
    }

    #[tokio::test]
    async fn a_token_opens_one_app_and_no_other() {
        let (_dir, config) = config();
        let (_, token) = create(&config, "sales", "x").await.unwrap();
        assert!(!authorize(&config, "hr", &token).await);
        assert!(!authorize(&config, "sales", "publish-token").await, "the publish token was accepted");
        assert!(!authorize(&config, "sales", "").await);
        assert!(!authorize(&config, "../sales", &token).await);
    }

    #[tokio::test]
    async fn revoking_ends_it_and_an_empty_list_leaves_no_file() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "sales", "x").await.unwrap();
        revoke(&config, "sales", &entry.id).await.unwrap();
        assert!(!authorize(&config, "sales", &token).await);
        assert!(!dir.path().join("sales.exports").exists());
        assert!(revoke(&config, "sales", &entry.id).await.is_err());
    }

    #[tokio::test]
    async fn an_app_name_that_could_leave_the_data_directory_is_refused() {
        let (dir, config) = config();
        for app in ["../x", "a/b", ".site", "", "a b"] {
            assert!(create(&config, app, "x").await.is_err(), "{app:?} accepted");
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
