//! Letting something outside publish one app and nothing else:
//! `PUT /deploy/<app>` with a bearer token minted for that app.
//!
//! This is for a CI system of your own. The publish token could do
//! it, and could also run SQL against every other app and manage accounts,
//! which is the wrong thing to put in a repository secret. A deploy token is
//! scoped to one app, accepts exactly what an upload ticket accepts, and is
//! revocable on its own; rotate it and the old one stops at once.
//!
//! Tokens live hashed in the `tokens` store (`<app>.deploys` on files)
//! beside the app's other records, so removing the app takes them along and
//! a copy of the data directory is not a set of live credentials. Same shape
//! as `export.rs`, for the same reasons.

use crate::{
    config::Config,
    content::slug::valid_slug,
    platform::{
        tokens::{self, Kind},
        upload::{self, UploadQuery},
    },
    AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};

/// Recognisable in a secrets list, and in a leak.
pub const PREFIX: &str = Kind::Deploy.prefix();

pub type DeployToken = tokens::Token;

/// An app here is a top-level directory, the first segment of any slug.
pub fn valid_app(app: &str) -> bool {
    tokens::valid_app(app)
}

/// Mints a token for `app`. The plain token is returned exactly once, here.
pub async fn create(config: &Config, app: &str, label: &str) -> Result<(DeployToken, String), String> {
    tokens::create(config, app, Kind::Deploy, label, "what will hold the token").await
}

pub async fn list(config: &Config, app: &str) -> Vec<DeployToken> {
    tokens::list(config, app, Kind::Deploy).await
}

pub async fn revoke(config: &Config, app: &str, id: &str) -> Result<(), String> {
    tokens::revoke(config, app, Kind::Deploy, id).await
}

/// Every token for the app, gone. What disconnecting a repository does.
pub async fn revoke_all(config: &Config, app: &str) -> Result<(), String> {
    tokens::revoke_all(config, app, Kind::Deploy).await
}

/// Whether `presented` is a live deploy token for `app`. Marks it used.
pub async fn authorize(config: &Config, app: &str, presented: &str) -> bool {
    tokens::check(config, app, Kind::Deploy, presented).await.is_some()
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
    let authorized = match &presented {
        Some(token) => authorize(&state.config, app, token).await,
        None => false,
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
/// `&commit=<sha>` says which commit the build came from.
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
    // A pipeline's source came from the repository; it is never pushed back.
    let meta = upload::SourceMeta::from_request(&query, &headers, false);
    upload::store_for_slug(&state.config, &state.runtime, app, upload::upload_kind(&query), body, meta).await
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
    let meta = upload::SourceMeta::from_request(&query, &headers, false);
    upload::store_for_slug(&state.config, &state.runtime, slug, upload::upload_kind(&query), body, meta).await
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
    async fn a_token_is_shown_once_stored_hashed_and_opens_one_app() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "shop", "actions").await.unwrap();
        assert!(token.starts_with("tsd_"));
        let stored = std::fs::read_to_string(dir.path().join("shop.deploys")).unwrap();
        assert!(!stored.contains(&token));
        assert!(stored.contains(&entry.id));
        assert!(authorize(&config, "shop", &token).await);
        assert!(!authorize(&config, "other", &token).await);
        assert!(!authorize(&config, "shop", "publish-token").await);
        assert!(list(&config, "shop").await[0].last_used.is_some());
    }

    #[tokio::test]
    async fn revoking_one_or_all_ends_them_and_leaves_no_file_behind() {
        let (dir, config) = config();
        let (a, token_a) = create(&config, "shop", "a").await.unwrap();
        let (_, token_b) = create(&config, "shop", "b").await.unwrap();
        revoke(&config, "shop", &a.id).await.unwrap();
        assert!(!authorize(&config, "shop", &token_a).await);
        assert!(authorize(&config, "shop", &token_b).await);
        revoke_all(&config, "shop").await.unwrap();
        assert!(!authorize(&config, "shop", &token_b).await);
        assert!(!dir.path().join("shop.deploys").exists());
    }

    #[tokio::test]
    async fn an_app_name_that_could_leave_the_data_directory_is_refused() {
        let (dir, config) = config();
        for app in ["../x", "a/b", ".site", ""] {
            assert!(create(&config, app, "x").await.is_err(), "{app:?}");
        }
        assert!(!dir.path().join("x.deploys").exists());
    }
}
