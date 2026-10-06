//! WebSockets: the first transport for live connections.
//!
//! An app declares its socket paths in toolsite.toml (`[[socket]] path =
//! "/live/ws"`). A WebSocket upgrade under `/p/<app>/` is taken here before
//! any file or handler route sees it, and accepted only at a declared path.
//! A plain request to the same path is served as always: files, the SPA
//! fallback, the handler.
//!
//! Before the upgrade the request passes the same door every request to the
//! app passes: the app must exist and not be hidden, and the visitor (from
//! the cookie scoped to this app) must be admitted by the app's access for
//! that exact path, route rules and project locks included. Then the
//! handler's `connect` event decides. Everything after the upgrade is
//! `platform::connections`, which does not know this is a WebSocket.

use crate::{
    accounts::users,
    platform::connections::{self, Incoming, Refusal, Transport},
    runtime::{connections::Message, connections::MAX_MESSAGE_BYTES, wasm::ConnectInfo},
    AppState,
};
use axum::{
    extract::{
        ws::{self, WebSocket, WebSocketUpgrade},
        FromRequestParts, Request, State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

/// Sockets one app may declare.
pub const MAX_SOCKETS: usize = 16;

/// A path an app may declare as a socket: segments as a tool path allows
/// them, anywhere in the app but `/mcp`, which is its connector.
pub fn valid_socket_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    !rest.is_empty()
        && rest != "mcp"
        && !rest.starts_with("mcp/")
        && rest.split('/').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('.')
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

/// Whether a request asks to become a WebSocket under an app's path.
pub fn is_upgrade<B>(request: &axum::http::Request<B>) -> bool {
    let wants_websocket = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    wants_websocket && request.uri().path().starts_with("/p/")
}

/// Splits `/p/<app>/<rest>` into the app and the path within it.
fn app_and_within(path: &str) -> Option<(String, String)> {
    let slug = path.strip_prefix("/p/")?.trim_end_matches('/');
    if !crate::content::slug::valid_asset_path(slug) {
        return None;
    }
    let (app, rest) = slug.split_once('/').unwrap_or((slug, ""));
    crate::platform::export::valid_app(app).then(|| (app.to_string(), format!("/{rest}")))
}

pub(crate) async fn upgrade(State(state): State<AppState>, request: Request) -> Response {
    let config = state.config.clone();
    let Some((app, within)) = app_and_within(request.uri().path()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if crate::content::store::is_hidden(&config, &app).await || !crate::content::store::app_exists(&config, &app).await {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if !crate::content::store::read_meta(&config, &app).await.sockets.contains(&within) {
        tracing::warn!(app = %app, path = %within, "404: websocket upgrade at a path the app does not declare as a socket");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let visitor = users::current_app_user(&config, &app, request.headers()).await;
    let site_token = users::token_from_cookies(request.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()));
    let path = request.uri().path().trim_end_matches('/').to_string();
    // The refusal any API request gets: no redirect, since a socket cannot
    // follow one.
    if let Some(denied) =
        crate::content::serve::gate_check(&config, &app, visitor.as_ref(), site_token.as_deref(), &path, true, false).await
    {
        tracing::warn!(app = %app, path = %within, signed_in = visitor.is_some(), "websocket refused: no access to this path");
        return denied;
    }

    let (mut parts, _body) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => {
            tracing::warn!(app = %app, headers = ?parts.headers.keys().collect::<Vec<_>>(), "websocket refused: not a valid upgrade");
            return rejection.into_response();
        }
    };
    let info = ConnectInfo {
        socket: within.clone(),
        path: within.clone(),
        query: parts.uri.query().unwrap_or_default().to_string(),
        // x-toolsite-* is the host speaking; a browser's copy is dropped,
        // as it is for requests.
        headers: parts
            .headers
            .iter()
            .filter(|(name, _)| !name.as_str().starts_with("x-toolsite-"))
            .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str().to_string(), v.to_string())))
            .collect(),
    };

    match connections::open(state, app.clone(), within.clone(), visitor, info).await {
        Ok(session) => upgrade
            .max_message_size(MAX_MESSAGE_BYTES)
            .on_upgrade(move |socket| connections::run(WsTransport(socket), session)),
        Err(Refusal::NotOffered) => {
            tracing::warn!(app = %app, path = %within, "websocket refused: the app's handler does not export on-connection");
            (StatusCode::NOT_IMPLEMENTED, "this app's handler takes no connections: build it for the app-with-connections world").into_response()
        }
        Err(Refusal::Full(why)) => {
            tracing::warn!(app = %app, "websocket refused: {why}");
            (StatusCode::TOO_MANY_REQUESTS, why).into_response()
        }
        Err(Refusal::Refused(why)) => {
            tracing::warn!(app = %app, path = %within, "websocket refused by the app: {why}");
            (StatusCode::FORBIDDEN, why).into_response()
        }
        Err(Refusal::Failed(why)) => {
            tracing::warn!(app = %app, path = %within, "websocket refused: the handler failed on connect: {why}");
            (StatusCode::INTERNAL_SERVER_ERROR, "the app's handler failed").into_response()
        }
    }
}

struct WsTransport(WebSocket);

impl Transport for WsTransport {
    async fn recv(&mut self) -> Incoming {
        loop {
            return match self.0.recv().await {
                Some(Ok(ws::Message::Text(text))) => Incoming::Message(Message::Text(text.as_str().to_string())),
                Some(Ok(ws::Message::Binary(bytes))) => Incoming::Message(Message::Binary(bytes.to_vec())),
                Some(Ok(ws::Message::Pong(_))) => Incoming::Pong,
                // axum answers pings itself.
                Some(Ok(ws::Message::Ping(_))) => continue,
                Some(Ok(ws::Message::Close(_))) | Some(Err(_)) | None => Incoming::Ended,
            };
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), ()> {
        let frame = match message {
            Message::Text(text) => ws::Message::Text(text.into()),
            Message::Binary(bytes) => ws::Message::Binary(bytes.into()),
        };
        self.0.send(frame).await.map_err(|_| ())
    }

    async fn ping(&mut self) -> Result<(), ()> {
        self.0.send(ws::Message::Ping(Vec::new().into())).await.map_err(|_| ())
    }

    async fn close(&mut self) {
        let _ = self.0.send(ws::Message::Close(None)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_path_is_checked_like_a_tool_path_and_never_the_connector() {
        assert!(valid_socket_path("/live/ws"));
        assert!(valid_socket_path("/api/ws"));
        assert!(!valid_socket_path("/"));
        assert!(!valid_socket_path("live"));
        assert!(!valid_socket_path("/mcp"));
        assert!(!valid_socket_path("/a/../b"));
        assert!(!valid_socket_path("/.hidden"));
        assert!(!valid_socket_path("/a//b"));
        assert!(!valid_socket_path("/a b"));
    }

    #[test]
    fn a_socket_path_names_its_app_and_the_path_inside_it() {
        assert_eq!(app_and_within("/p/shop/live/ws"), Some(("shop".into(), "/live/ws".into())));
        assert_eq!(app_and_within("/p/shop"), Some(("shop".into(), "/".into())));
        assert_eq!(app_and_within("/p/../ws"), None);
        assert_eq!(app_and_within("/p/.site/ws"), None);
    }
}
