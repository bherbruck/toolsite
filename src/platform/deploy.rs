//! Letting something outside publish one app and nothing else:
//! `PUT /deploy/<app>` with a bearer token minted for that app.
//!
//! This is what a build in GitHub Actions uses. The publish token could do
//! it, and could also run SQL against every other app and manage accounts,
//! which is the wrong thing to put in a repository secret. A deploy token is
//! scoped to one app, accepts exactly what an upload ticket accepts, and is
//! revocable on its own; rotate it and the old one stops at once.
//!
//! Tokens live hashed in `<app>.deploys` beside the app's other sidecars, so
//! removing the app takes them along and a copy of the data directory is not
//! a set of live credentials. Same shape as `export.rs`, for the same reasons.

use crate::{
    config::Config,
    content::slug::{random_token, valid_slug},
    platform::upload::{self, UploadQuery},
    AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// Recognisable in a secrets list, and in a leak.
pub const PREFIX: &str = "tsd_";
const MAX_LABEL: usize = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeployToken {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_used: Option<u64>,
    pub created_at: u64,
    hash: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// An app here is a top-level directory, the first segment of any slug.
pub fn valid_app(app: &str) -> bool {
    valid_slug(app) && !app.contains('/')
}

fn path(config: &Config, app: &str) -> Option<PathBuf> {
    valid_app(app).then(|| config.data_dir.join(format!("{app}.deploys")))
}

fn read(config: &Config, app: &str) -> Vec<DeployToken> {
    let Some(path) = path(config, app) else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(config: &Config, app: &str, tokens: &[DeployToken]) -> Result<(), String> {
    let path = path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if tokens.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let text = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// Mints a token for `app`. The plain token is returned exactly once, here.
pub fn create(config: &Config, app: &str, label: &str) -> Result<(DeployToken, String), String> {
    if !valid_app(app) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    let label = label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL {
        return Err(format!("label must be 1 to {MAX_LABEL} characters: say what will hold the token"));
    }
    let token = format!("{PREFIX}{}", random_token(40));
    let entry = DeployToken {
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

pub fn list(config: &Config, app: &str) -> Vec<DeployToken> {
    read(config, app)
}

pub fn revoke(config: &Config, app: &str, id: &str) -> Result<(), String> {
    let mut tokens = read(config, app);
    let before = tokens.len();
    tokens.retain(|token| token.id != id);
    if tokens.len() == before {
        return Err(format!("no deploy token {id} on {app}"));
    }
    write(config, app, &tokens)
}

/// Every token for the app, gone. What disconnecting a repository does.
pub fn revoke_all(config: &Config, app: &str) -> Result<(), String> {
    write(config, app, &[])
}

/// Whether `presented` is a live deploy token for `app`. Marks it used.
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

pub fn deploy_url(config: &Config, app: &str) -> String {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    format!("{base}/deploy/{app}")
}

// --- the endpoint ---------------------------------------------------------

async fn admitted(state: &AppState, app: &str, headers: &HeaderMap) -> Result<(), Response> {
    if !valid_app(app) {
        return Err((StatusCode::NOT_FOUND, "not found\n").into_response());
    }
    let presented = crate::platform::bearer::presented_token(headers).map(str::to_string);
    let authorized = {
        let (config, app) = (state.config.clone(), app.to_string());
        let presented = presented.clone();
        tokio::task::spawn_blocking(move || {
            presented.is_some_and(|token| authorize(&config, &app, &token))
        })
        .await
        .unwrap_or(false)
    };
    if authorized {
        return Ok(());
    }
    // One wording whether the app, the token or both are wrong, so a token
    // cannot be used to learn which apps exist.
    tracing::warn!(
        app = %app,
        token_presented = presented.is_some(),
        user_agent = %headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>"),
        "deploy refused: no deploy token for this app"
    );
    Err((StatusCode::UNAUTHORIZED, "no deploy token for this app\n").into_response())
}

fn refuse_unknown(uri: &Uri) -> Option<Response> {
    let unknown = upload::unknown_flags(uri);
    if unknown.is_empty() {
        return None;
    }
    Some((StatusCode::BAD_REQUEST, format!("unknown upload flag(s): {}\n", unknown.join(", "))).into_response())
}

/// `PUT /deploy/<app>`: the app's bundle, handler, migrations, manifest,
/// source, a blob, or a single page, exactly as `/upload/<ticket>` takes them.
pub(crate) async fn deploy_root(
    State(state): State<AppState>,
    Path(app): Path<String>,
    Query(query): Query<UploadQuery>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = admitted(&state, &app, &headers).await {
        return response;
    }
    if let Some(response) = refuse_unknown(&uri) {
        return response;
    }
    tracing::info!(app = %app, bytes = body.len(), "deploy by token");
    upload::store_for_slug(&state.config, &state.runtime, app, upload::upload_kind(&query), body).await
}

/// `PUT /deploy/<app>/<page>`: one page of a multi-page app.
pub(crate) async fn deploy_sub(
    State(state): State<AppState>,
    Path((app, sub)): Path<(String, String)>,
    Query(query): Query<UploadQuery>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = admitted(&state, &app, &headers).await {
        return response;
    }
    if let Some(response) = refuse_unknown(&uri) {
        return response;
    }
    let slug = format!("{app}/{}", sub.trim_end_matches(".html"));
    if !valid_slug(&slug) {
        return (StatusCode::BAD_REQUEST, "page name must be path segments of letters, numbers, '-' or '_'\n").into_response();
    }
    upload::store_for_slug(&state.config, &state.runtime, slug, upload::upload_kind(&query), body).await
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
    fn a_token_is_shown_once_stored_hashed_and_opens_one_app() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "shop", "actions").unwrap();
        assert!(token.starts_with("tsd_"));
        let stored = std::fs::read_to_string(dir.path().join("shop.deploys")).unwrap();
        assert!(!stored.contains(&token));
        assert!(stored.contains(&entry.id));
        assert!(authorize(&config, "shop", &token));
        assert!(!authorize(&config, "other", &token));
        assert!(!authorize(&config, "shop", "publish-token"));
        assert!(list(&config, "shop")[0].last_used.is_some());
    }

    #[test]
    fn revoking_one_or_all_ends_them_and_leaves_no_file_behind() {
        let (dir, config) = config();
        let (a, token_a) = create(&config, "shop", "a").unwrap();
        let (_, token_b) = create(&config, "shop", "b").unwrap();
        revoke(&config, "shop", &a.id).unwrap();
        assert!(!authorize(&config, "shop", &token_a));
        assert!(authorize(&config, "shop", &token_b));
        revoke_all(&config, "shop").unwrap();
        assert!(!authorize(&config, "shop", &token_b));
        assert!(!dir.path().join("shop.deploys").exists());
    }

    #[test]
    fn an_app_name_that_could_leave_the_data_directory_is_refused() {
        let (dir, config) = config();
        for app in ["../x", "a/b", ".site", ""] {
            assert!(create(&config, app, "x").is_err(), "{app:?}");
        }
        assert!(!dir.path().join("x.deploys").exists());
    }
}
