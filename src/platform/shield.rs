//! Keeps a published app's scripts away from toolsite's own pages.
//!
//! In path mode every app is served under `/p/<app>/` on the same origin as
//! `/admin`, `/account`, the consent screen and the settings entry form. The
//! browser treats them as one site: an app's script may fetch any of those
//! pages, frame them, or open them in a window and read the result. That is
//! how a script in an app could read an admin's form token and act as the
//! admin. The full fix is a separate origin for apps, which is subdomain
//! mode. Without it, this module closes the paths that do not need one:
//!
//! - A sensitive page is handed only to a top-level navigation, as the
//!   browser's fetch metadata reports it, or to a request that already
//!   carries the visitor's form token (toolsite's own scripts do; an app's
//!   script cannot, because it can no longer read a page that holds it).
//! - Those pages refuse to be framed.
//! - Those pages and app pages never share a browsing context group, so a
//!   window an app opens on `/admin` is not one it can reach into.
//!
//! What this does not close in path mode: an app's script can still call
//! another app's API as the visitor, because each app's cookie is scoped by
//! path on one origin and the browser attaches it to any request to that
//! path. Subdomain mode (`platform::app_hosts`) closes that with an origin
//! per app; this module still guards the main host's pages there.

use crate::{accounts::users, config::Config};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// The header toolsite's own scripts send their page's form token in.
pub const FORM_TOKEN_HEADER: &str = "x-toolsite-form";

/// Pages that carry a form token, a secret, or the visitor's own data, and
/// the JSON their scripts read. An app has no business loading any of them.
pub(crate) fn is_sensitive(path: &str) -> bool {
    path == "/"
        || path == "/browse"
        || path.starts_with("/browse/")
        || path == "/admin"
        || path.starts_with("/admin/")
        || path == "/account"
        || path.starts_with("/account/")
        || path == "/settings"
        || path.starts_with("/settings/")
        || path == "/authorize"
        || path.starts_with("/auth/setup")
        || path.starts_with("/auth/mfa")
}

/// Pages toolsite renders itself, as opposed to what apps publish and the
/// machine endpoints (MCP, uploads, exports, webhooks).
pub(crate) fn is_toolsite_page(path: &str) -> bool {
    is_sensitive(path) || path.starts_with("/auth/") || path == "/guide" || path == "/examples"
}

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Whether the browser says this is the person opening a page in a tab.
/// A request with no fetch metadata at all is a client that is not a
/// modern browser (curl, the test suite): taken at face value, as the
/// handoff already does.
fn is_top_level_navigation(headers: &HeaderMap) -> Option<bool> {
    let mode = header_str(headers, "sec-fetch-mode")?;
    let dest = header_str(headers, "sec-fetch-dest").unwrap_or("");
    // A download started from a link is a navigation whose destination is
    // reported as empty; a frame's is `iframe`, and is not accepted.
    Some(mode == "navigate" && matches!(dest, "document" | "empty" | ""))
}

pub(crate) async fn shield(State(config): State<Arc<Config>>, request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_string();

    if path.starts_with("/p/") {
        let mut response = next.run(request).await;
        // An app page that declared `same-origin` itself would share its
        // browsing context group with every toolsite page that does the
        // same, and could then reach into a window it opened on `/admin`.
        // The values have to differ for the browser to keep them apart, so
        // an app page always gets `same-origin-allow-popups`: it may still
        // open its own popups, and a toolsite page it opens, which says
        // `same-origin`, lands in a group of its own.
        response.headers_mut().remove(header::HeaderName::from_static("cross-origin-opener-policy"));
        response.headers_mut().insert(
            header::HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin-allow-popups"),
        );
        return response;
    }

    let sensitive = is_sensitive(&path);
    if sensitive && matches!(*request.method(), Method::GET | Method::HEAD) {
        let navigation = is_top_level_navigation(request.headers());
        if navigation == Some(false) && !carries_form_token(&config, request.headers()).await {
            let header_names: Vec<&str> = request.headers().keys().map(|k| k.as_str()).collect();
            tracing::warn!(
                path = %path,
                fetch_mode = header_str(request.headers(), "sec-fetch-mode").unwrap_or("-"),
                fetch_dest = header_str(request.headers(), "sec-fetch-dest").unwrap_or("-"),
                fetch_site = header_str(request.headers(), "sec-fetch-site").unwrap_or("-"),
                headers = ?header_names,
                "403: a toolsite page was asked for by a script or a frame, not opened in a tab"
            );
            return (
                StatusCode::FORBIDDEN,
                "Open this page in a browser tab. It cannot be loaded by a script or in a frame.",
            )
                .into_response();
        }
    }

    let mut response = next.run(request).await;
    if is_toolsite_page(&path) {
        let headers = response.headers_mut();
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("frame-ancestors 'none'"),
        );
        headers.insert(
            header::HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        );
    }
    response
}

/// Whether the request carries the signed-in visitor's own form token, which
/// only a page of toolsite's could have given it.
async fn carries_form_token(config: &Arc<Config>, headers: &HeaderMap) -> bool {
    let Some(presented) = header_str(headers, FORM_TOKEN_HEADER) else {
        return false;
    };
    let Some(user) = users::current_site_user(config, headers).await else {
        return false;
    };
    let expected = users::derive_form_token(config, &user.id);
    expected.len() == presented.len() && expected.as_bytes() == presented.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn an_app_cannot_declare_the_opener_policy_toolsites_pages_use() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config::local(dir.path().to_path_buf(), "t"));
        let app = Router::new()
            .route(
                "/p/{*rest}",
                get(|| async { ([("cross-origin-opener-policy", "same-origin")], "app") }),
            )
            .layer(axum::middleware::from_fn_with_state(config, shield));
        let response = app
            .oneshot(Request::builder().uri("/p/evil/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let values: Vec<_> = response.headers().get_all("cross-origin-opener-policy").iter().collect();
        assert_eq!(values, vec![HeaderValue::from_static("same-origin-allow-popups")]);
    }
}
