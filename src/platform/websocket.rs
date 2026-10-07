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
    platform::connections::{self, Door, Incoming, Refusal, Transport},
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

/// Subprotocols one socket may agree to.
pub const MAX_SUBPROTOCOLS: usize = 8;

/// A socket's declared subprotocols, trimmed, or why they are refused.
/// Each is a token as RFC 6455 allows one, kept to letters, digits and
/// `.`, `_`, `-`, `+`, at most 64 characters, so it is a header value as
/// it stands.
pub fn check_subprotocols(declared: &[String]) -> Result<Vec<String>, String> {
    if declared.len() > MAX_SUBPROTOCOLS {
        return Err(format!("at most {MAX_SUBPROTOCOLS} subprotocols, got {}", declared.len()));
    }
    let mut checked: Vec<String> = Vec::new();
    for name in declared {
        let name = name.trim();
        let valid = !name.is_empty()
            && name.len() <= 64
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
        if !valid {
            return Err(format!(
                "a subprotocol is 1 to 64 letters, digits, '.', '_', '-' and '+', got {name:?}"
            ));
        }
        if checked.iter().any(|seen| seen == name) {
            return Err(format!("subprotocol {name:?} declared twice"));
        }
        checked.push(name.to_string());
    }
    Ok(checked)
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

/// The `scheme://host[:port]` of a URL, lower-cased, with a default port
/// left out, so two spellings of one origin compare equal.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let authority = match (scheme.as_str(), authority.rsplit_once(':')) {
        ("http", Some((host, "80"))) | ("https", Some((host, "443"))) => host.to_string(),
        _ => authority,
    };
    Some(format!("{scheme}://{authority}"))
}

/// Whether an upgrade's `Origin` is this site: the configured base URL's
/// origin, or, with none configured, the host the request was sent to. In
/// subdomain mode it must be the app's own host, so another app's page,
/// a sibling on the same site, cannot open the socket as the visitor.
fn same_origin(config: &crate::Config, app: &str, origin: &str, headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = origin_of(origin) else {
        return false;
    };
    if config.apps.is_some() {
        return origin_of(&crate::content::origins::app_base(config, app)).is_some_and(|ours| ours == origin);
    }
    if let Some(base) = config.base_url.as_deref() {
        return origin_of(base).is_some_and(|base| base == origin);
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    ["http", "https"]
        .iter()
        .any(|scheme| origin_of(&format!("{scheme}://{host}")).is_some_and(|ours| ours == origin))
}

/// Whether a request was sent by a page on another origin than the app's.
/// A browser names the page in `Origin` on every write and every upgrade;
/// one that leaves it out but says through fetch metadata that the request
/// crossed sites is treated the same. A client with neither is not a
/// browser page, and carries no cookie it did not choose to send.
pub(crate) fn from_foreign_page(config: &crate::Config, app: &str, headers: &axum::http::HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        Some(origin) => !same_origin(config, app, origin.to_str().unwrap_or(""), headers),
        None => headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|site| site.eq_ignore_ascii_case("same-site") || site.eq_ignore_ascii_case("cross-site")),
    }
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
    let meta = crate::content::store::read_meta(&config, &app).await;
    if !meta.sockets.contains(&within) {
        tracing::warn!(app = %app, path = %within, "404: websocket upgrade at a path the app does not declare as a socket");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let subprotocols = meta.socket_protocols.get(&within).cloned().unwrap_or_default();

    // A browser always says which page opened the socket. A page on another
    // site must not open one with this visitor's cookies (cross-site
    // WebSocket hijacking), so a browser's upgrade from any other origin is
    // refused. A client with no Origin is not a browser and carries no
    // cookie it did not choose to send.
    if from_foreign_page(&config, &app, request.headers()) {
        tracing::warn!(
            app = %app,
            origin = ?request.headers().get(header::ORIGIN),
            fetch_site = ?request.headers().get("sec-fetch-site"),
            host = ?request.headers().get(header::HOST),
            "403: websocket upgrade from a page on another origin"
        );
        return (StatusCode::FORBIDDEN, "a page on another site may not open this socket").into_response();
    }

    let visitor = users::current_app_user(&config, &app, request.headers()).await;
    let path = request.uri().path().trim_end_matches('/').to_string();
    // The refusal any API request gets: no redirect, since a socket cannot
    // follow one.
    if let Some(denied) =
        crate::content::serve::gate_check(&config, &app, visitor.as_ref(), &path, true, request.headers()).await
    {
        tracing::warn!(app = %app, path = %within, signed_in = visitor.is_some(), "websocket refused: no access to this path");
        return denied;
    }

    let (mut parts, _body) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        // The first declared subprotocol the client offers is echoed back.
        // None offered, or none in common, upgrades with none, and a browser
        // that asked for one then gives up on the socket itself.
        Ok(upgrade) => upgrade.protocols(subprotocols),
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
        // The cookie header keeps only the app's own cookies, never the
        // visitor's toolsite sessions.
        headers: parts
            .headers
            .iter()
            .filter(|(name, _)| !name.as_str().starts_with("x-toolsite-"))
            .filter_map(|(name, value)| {
                let value = value.to_str().ok()?;
                if name == header::COOKIE {
                    return users::without_platform_cookies(value).map(|kept| (name.as_str().to_string(), kept));
                }
                Some((name.as_str().to_string(), value.to_string()))
            })
            .collect(),
    };

    match connections::open(state, app.clone(), Door::Path(within.clone()), visitor, None, info).await {
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
    fn an_origin_compares_by_scheme_host_and_port_only() {
        assert_eq!(origin_of("https://Site.example/p/x").as_deref(), Some("https://site.example"));
        assert_eq!(origin_of("https://site.example:443").as_deref(), Some("https://site.example"));
        assert_eq!(origin_of("http://site.example:8080").as_deref(), Some("http://site.example:8080"));
        assert_eq!(origin_of("null"), None);
        assert_eq!(origin_of("https://user@site.example"), None);
    }

    #[test]
    fn a_socket_path_names_its_app_and_the_path_inside_it() {
        assert_eq!(app_and_within("/p/shop/live/ws"), Some(("shop".into(), "/live/ws".into())));
        assert_eq!(app_and_within("/p/shop"), Some(("shop".into(), "/".into())));
        assert_eq!(app_and_within("/p/../ws"), None);
        assert_eq!(app_and_within("/p/.site/ws"), None);
    }
}
