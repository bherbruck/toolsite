//! A one-time sign-in for a headless browser: `GET /preview/<token>`.
//!
//! A screenshot has to show the page as a person would see it, data and all,
//! so the renderer needs to arrive signed in as that person. It cannot set a
//! cookie itself, but it can load a URL. The screenshot path mints a token
//! that names an app, a path and optionally an account; the browser loads
//! `/preview/<token>`, this handler mints an app session for that account
//! exactly as the handoff would, sets the app-scoped cookie, and redirects to
//! the page. The token works once and for a minute. No route mints one.

use crate::{
    accounts::users,
    config::Config,
    content::slug::{random_token, valid_slug},
};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// A token is spent by the first load; a render that takes longer than this
/// to start is not coming.
pub const PREVIEW_TTL: Duration = Duration::from_secs(60);
/// The app session a preview signs in with. The render takes seconds; the
/// cookie dies with the browser process anyway.
const PREVIEW_SESSION: Duration = Duration::from_secs(600);

pub struct PreviewTicket {
    pub app: String,
    /// Within the app, starting with `/`.
    pub path: String,
    pub user_id: Option<String>,
    pub expires_at: Instant,
}

/// A path within an app: starts with `/`, no `..`, no scheme, no `//`.
pub fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.split('/').any(|segment| segment == "..")
        && !path.contains("://")
        && path.len() <= 512
}

/// Mints a token for one render. Only the screenshot code calls this.
pub fn issue(config: &Config, app: &str, path: &str, user_id: Option<&str>) -> Result<String, String> {
    if !valid_slug(app) || app.contains('/') {
        return Err("invalid app name".into());
    }
    if !valid_path(path) {
        return Err("path must start with '/' and stay within the app".into());
    }
    let token = random_token(40);
    let now = Instant::now();
    let mut previews = config.previews.lock().unwrap();
    previews.retain(|_, ticket| ticket.expires_at > now);
    previews.insert(
        token.clone(),
        PreviewTicket {
            app: app.to_string(),
            path: path.to_string(),
            user_id: user_id.map(str::to_string),
            expires_at: now + PREVIEW_TTL,
        },
    );
    Ok(token)
}

fn take(config: &Config, token: &str) -> Option<PreviewTicket> {
    let now = Instant::now();
    let mut previews = config.previews.lock().unwrap();
    previews.retain(|_, ticket| ticket.expires_at > now);
    previews.remove(token)
}

pub(crate) async fn open(State(config): State<Arc<Config>>, Path(token): Path<String>) -> Response {
    let Some(ticket) = take(&config, &token) else {
        tracing::warn!("preview refused: token unknown, expired or already used");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let target = format!("/p/{}{}", ticket.app, ticket.path);
    let Some(user_id) = ticket.user_id else {
        return Redirect::to(&target).into_response();
    };
    let (worker, app) = (config.clone(), ticket.app.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        users::create_app_session_for(&worker, &user_id, &app, PREVIEW_SESSION)
    })
    .await;
    match outcome {
        Ok(Ok((session, max_age))) => (
            [(header::SET_COOKIE, preview_cookie(&config, &ticket.app, &session, max_age))],
            Redirect::to(&target),
        )
            .into_response(),
        Ok(Err(why)) => {
            tracing::warn!(app = %ticket.app, %why, "preview could not sign the account in");
            (StatusCode::NOT_FOUND, "not found").into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "preview failed").into_response(),
    }
}


/// The app cookie for the renderer's browser. It arrives on the preview
/// base, which for a sidecar is a plain-http private address: a browser
/// drops a `Secure` cookie set over http from anywhere but localhost, and
/// the render would show the sign-in page. Over an http preview base the flag
/// is left off; the session is an app session that lives a day at most and
/// is only ever handed to the renderer's own browser.
fn preview_cookie(config: &Config, app: &str, session: &str, max_age: u64) -> String {
    let cookie = users::set_app_cookie_header(config, app, session, max_age);
    if config.preview_base.starts_with("http://") {
        cookie.replace(" Secure;", "")
    } else {
        cookie
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sidecar_on_plain_http_gets_a_cookie_it_will_keep_and_everything_else_keeps_secure() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::local(dir.path().to_path_buf(), "t");
        config.base_url = Some("https://site.test".to_string());
        config.preview_base = "http://toolsite.railway.internal:8080".to_string();
        let cookie = preview_cookie(&config, "app", "s", 60);
        assert!(!cookie.contains("Secure"), "{cookie}");
        assert!(cookie.contains("HttpOnly") && cookie.contains("Path=/p/app/"), "{cookie}");
        config.preview_base = "https://site.test".to_string();
        assert!(preview_cookie(&config, "app", "s", 60).contains("Secure"));
    }

    #[test]
    fn a_preview_path_stays_inside_the_app() {
        assert!(valid_path("/"));
        assert!(valid_path("/reports/2026?q=1"));
        assert!(!valid_path("reports"));
        assert!(!valid_path("//evil.test/"));
        assert!(!valid_path("/../other/"));
        assert!(!valid_path("/x/../../.site"));
        assert!(!valid_path("/https://evil.test"));
    }

    #[test]
    fn a_token_is_single_use_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        let token = issue(&config, "app", "/", None).unwrap();
        assert!(take(&config, &token).is_some());
        assert!(take(&config, &token).is_none(), "a token was taken twice");

        let token = issue(&config, "app", "/", None).unwrap();
        config.previews.lock().unwrap().get_mut(&token).unwrap().expires_at =
            Instant::now() - Duration::from_secs(1);
        assert!(take(&config, &token).is_none(), "an expired token was taken");
        assert!(issue(&config, "../x", "/", None).is_err());
        assert!(issue(&config, "a/b", "/", None).is_err());
    }
}
