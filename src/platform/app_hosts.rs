//! Subdomain mode at the door: which host serves what.
//!
//! With `TOOLSITE_APPS_DOMAIN` set, each app has a host of its own (see
//! `content::origins`). This layer keeps the two kinds of host apart:
//!
//! - The main host serves toolsite and no app content. A page navigation to
//!   `/p/<app>/...` is sent to the same path and query on the app's host;
//!   any other request under `/p/` (a POST, a WebSocket, an MCP call) is
//!   refused, so the old shared origin cannot be used at all.
//! - An app host serves that app's `/p/<app>/...` and the few routes it
//!   needs besides: its connector's OAuth metadata, the end of a sign-in
//!   handoff, a preview sign-in and a browser file upload. A navigation to
//!   one of toolsite's pages goes to the main host; anything else is 404.
//! - Any other host is 404.
//!
//! An app host also refuses a state-changing request whose `Origin` is not
//! its own. Hosts under one apps domain are one site to the browser, so a
//! cookie with `SameSite=Lax` is still sent with a POST from a sibling app's
//! script; reading the answer is blocked, but the POST would have run.
//!
//! Every redirect is built from the configuration and an app's stored
//! label. Nothing in the request's `Host` header is ever echoed back.

use crate::{
    config::Config,
    content::origins::{self, AppHost, Host},
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// The host a request names: `Host`, or the authority of an HTTP/2 URI.
fn host_of(request: &Request<Body>) -> Option<String> {
    request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_string()))
}

/// The app a `/p/` path names: its first segment.
fn app_in_path(path: &str) -> Option<&str> {
    let app = path.strip_prefix("/p/")?.split('/').next()?;
    crate::platform::export::valid_app(app).then_some(app)
}

fn path_and_query(request: &Request<Body>) -> String {
    request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string())
}

/// `302 Found`: where this content lives now, for this request only.
fn found(location: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, location.to_string())]).into_response()
}

fn not_found(why: &str, request: &Request<Body>, host: Option<&str>) -> Response {
    let header_names: Vec<&str> = request.headers().keys().map(|k| k.as_str()).collect();
    tracing::warn!(
        method = %request.method(),
        path = %request.uri().path(),
        host = host.unwrap_or("-"),
        headers = ?header_names,
        "404: {why}"
    );
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Whether an app host serves this path for `app`, beside `/p/<app>/`.
fn app_host_serves(app: &str, path: &str) -> bool {
    path == format!("/p/{app}")
        || path.starts_with(&format!("/p/{app}/"))
        || path == format!("/.well-known/oauth-protected-resource/p/{app}/mcp")
        || path == "/auth/landing"
        || path.starts_with("/preview/")
        || path.starts_with("/blob/")
}

pub(crate) async fn route_by_host(State(config): State<Arc<Config>>, mut request: Request<Body>, next: Next) -> Response {
    if config.apps.is_none() {
        return next.run(request).await;
    }
    let host = host_of(&request);
    let lookup = (config.clone(), host.clone());
    let Ok(kind) = tokio::task::spawn_blocking(move || origins::classify(&lookup.0, lookup.1.as_deref())).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "host lookup failed").into_response();
    };
    let path = request.uri().path().to_string();
    match kind {
        Host::Unknown => not_found("a host that is neither the site nor one of its apps", &request, host.as_deref()),
        Host::Main => {
            if path.starts_with("/.well-known/oauth-protected-resource/p/") {
                return not_found("an app connector's metadata lives on the app's host", &request, host.as_deref());
            }
            if !path.starts_with("/p/") {
                return next.run(request).await;
            }
            let navigable = matches!(*request.method(), Method::GET | Method::HEAD)
                && !crate::platform::websocket::is_upgrade(&request);
            match app_in_path(&path) {
                Some(app) if navigable => {
                    let (lookup, app) = (config.clone(), app.to_string());
                    let Ok(base) = tokio::task::spawn_blocking(move || origins::app_base(&lookup, &app)).await else {
                        return (StatusCode::INTERNAL_SERVER_ERROR, "host lookup failed").into_response();
                    };
                    found(&format!("{base}{}", path_and_query(&request)))
                }
                _ => not_found("app content is served from the app's own host, not the main host", &request, host.as_deref()),
            }
        }
        Host::App(app) => {
            if path == "/" {
                return found(&format!("/p/{app}/"));
            }
            if !app_host_serves(&app, &path) {
                if matches!(*request.method(), Method::GET | Method::HEAD) && crate::platform::shield::is_toolsite_page(&path) {
                    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
                    return found(&format!("{base}{}", path_and_query(&request)));
                }
                return not_found("an app host serves only its own app", &request, host.as_deref());
            }
            if let Some(refused) = foreign_write(&config, &app, &request) {
                return refused;
            }
            request.extensions_mut().insert(AppHost(app));
            next.run(request).await
        }
    }
}

/// A request that could change something, sent by a page on another
/// origin. A browser sends `Origin` with every such request; a client that
/// sends none is not a browser and carries no cookie it did not choose to.
fn foreign_write(config: &Config, app: &str, request: &Request<Body>) -> Option<Response> {
    if matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    let origin = request.headers().get(header::ORIGIN)?;
    let theirs = origin.to_str().unwrap_or("");
    if crate::platform::websocket::same_origin(config, app, theirs, request.headers()) {
        return None;
    }
    tracing::warn!(
        app,
        method = %request.method(),
        path = %request.uri().path(),
        origin = ?origin,
        "403: a request that changes state, from a page on another origin"
    );
    Some((StatusCode::FORBIDDEN, "a page on another origin may not send this request").into_response())
}
