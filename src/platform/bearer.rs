use crate::{config::Config, platform::client_oauth};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use std::sync::Arc;

/// Who is behind an MCP request. `None` is a static token or the stdio
/// transport, which have every power; `Some` is the account behind an OAuth
/// token, whose scopes decide what each tool may do for it.
#[derive(Clone, Debug)]
pub struct Caller {
    pub user: Option<crate::accounts::users::User>,
}

/// Clients disagree about how to present a static token: most send
/// `Authorization: Bearer <token>`, some send `x-api-key`. Accept either —
/// it's the same secret.
pub(crate) fn presented_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        })
    {
        return Some(bearer.trim());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

pub(crate) async fn require_bearer(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    mut request: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let presented = presented_token(&headers);
    if presented.is_some_and(|token| config.valid_tokens.iter().any(|v| v == token)) {
        request.extensions_mut().insert(Caller { user: None });
        return next.run(request).await;
    }
    // Not a static token: perhaps one the OAuth server issued to a person.
    // That lookup also re-asks accounts whether the person may still publish,
    // so a disabled account is refused on its next request, not at expiry.
    if let Some(token) = presented.filter(|_| config.oauth_enabled())
        && let Some(user) = client_oauth::token_user(&config, token).await
    {
        let (may_publish_config, may_publish_user) = (config.clone(), user.clone());
        let may_publish = tokio::task::spawn_blocking(move || {
            crate::accounts::users::holds_anywhere(&may_publish_config, &may_publish_user, crate::accounts::users::Scope::Editor)
        })
        .await
        .unwrap_or(false);
        if may_publish {
            tracing::debug!(email = %user.email, "mcp request as a signed-in account");
            request.extensions_mut().insert(Caller { user: Some(user) });
            return next.run(request).await;
        }
        // A real account, but one that may publish nowhere. Its client
        // belongs on /me/mcp; say so rather than a bare 401.
        tracing::warn!(email = %user.email, path = %request.uri().path(), "401: the account holds no editor or admin scope; use /me/mcp");
        return (
            StatusCode::UNAUTHORIZED,
            "this account cannot publish; connect to /me/mcp to read what it may open\n",
        )
            .into_response();
    }

    // A rejected client usually reports nothing more than "can't connect", so
    // say here exactly what arrived. Never the token itself — only its shape.
    let scheme = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_whitespace().next())
        .unwrap_or("<none>");
    let header_names: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
    tracing::warn!(
        method = %request.method(),
        path = %request.uri().path(),
        user_agent = %headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>"),
        auth_scheme = %scheme,
        x_api_key = headers.contains_key("x-api-key"),
        token_presented = presented.is_some(),
        token_len = presented.map(str::len).unwrap_or(0),
        headers = ?header_names,
        "401: no valid token"
    );

    let mut response = StatusCode::UNAUTHORIZED.into_response();
    // Per MCP's auth spec, point OAuth-capable clients at the metadata rather
    // than leaving them to guess.
    if let Some(base) = config.base_url.as_deref()
        && let Ok(value) =
            format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource""#)
                .parse()
    {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// `/me/mcp`: any active account's OAuth token, and the account travels with
/// the request so the tools know who is asking. A static token is refused
/// here: it names nobody, and everything on this endpoint is about who.
pub(crate) async fn require_person(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    mut request: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let presented = presented_token(&headers);
    if let Some(token) = presented.filter(|_| config.oauth_enabled())
        && let Some(user) = client_oauth::token_user(&config, token).await
    {
        tracing::debug!(email = %user.email, "me/mcp request");
        request.extensions_mut().insert(user);
        return next.run(request).await;
    }
    tracing::warn!(
        method = %request.method(),
        path = %request.uri().path(),
        token_presented = presented.is_some(),
        "401: no account token for /me/mcp"
    );
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    if let Some(base) = config.base_url.as_deref()
        && let Ok(value) =
            format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/me/mcp""#)
                .parse()
    {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// The app whose tools a `/p/<app>/mcp` request is for, put on the request
/// by `require_app_caller` from the path it arrived on.
#[derive(Clone, Debug)]
pub struct ToolApp(pub String);

/// `/p/<app>/mcp`: an app's tools as a connector of their own. Any account's
/// OAuth token, as on `/me/mcp`, or a static token, which calls as nobody.
/// Whether the person may see this app's tools is the server's question,
/// asked per call, so a person with no access sees an empty list rather
/// than a refusal that would confirm the app exists.
pub(crate) async fn require_app_caller(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    mut request: Request<Body>,
    next: Next,
) -> axum::response::Response {
    let Some(app) = crate::platform::app_tools::connector_app(request.uri().path()).map(str::to_string) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let presented = presented_token(&headers);
    if presented.is_some_and(|token| config.valid_tokens.iter().any(|v| v == token)) {
        request.extensions_mut().insert(Caller { user: None });
        request.extensions_mut().insert(ToolApp(app));
        return next.run(request).await;
    }
    if let Some(token) = presented.filter(|_| config.oauth_enabled())
        && let Some(user) = client_oauth::token_user(&config, token).await
    {
        tracing::debug!(email = %user.email, %app, "app tools request");
        request.extensions_mut().insert(Caller { user: Some(user) });
        request.extensions_mut().insert(ToolApp(app));
        return next.run(request).await;
    }
    tracing::warn!(
        method = %request.method(),
        path = %request.uri().path(),
        token_presented = presented.is_some(),
        "401: no account token for an app's tools"
    );
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    if let Some(base) = config.base_url.as_deref()
        && let Ok(value) =
            format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/p/{app}/mcp""#).parse()
    {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}
