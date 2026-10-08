//! The OAuth 2.1 server an MCP client signs in through. This is who may
//! PUBLISH: a person with an admin account connects Claude, Claude Code or an
//! IDE to this site by signing in with the account they already have, and
//! nothing is pasted anywhere.
//!
//! The moving parts, in the order a client meets them:
//!
//! 1. Discovery: `/.well-known/…` says where everything is.
//! 2. Registration: a client that has never seen this server registers itself
//!    (RFC 7591) and gets an id. That proves nothing and grants nothing; it
//!    pins the redirect URIs a code may later be sent to.
//! 3. `/authorize`: the person signs in if they have not, sees who is asking
//!    and where the answer will go, and says yes or no. Only an admin account
//!    can say yes, because a connection publishes with the account's standing.
//! 4. `/token`: the client trades the code, plus PKCE proof that it is the
//!    same client that started, for an access token and a refresh token.
//!
//! Every client is public (no secret) and PKCE S256 is required, which is what
//! OAuth 2.1 asks. A client's name is shown but never believed; the host the
//! code is sent back to is what the consent screen leads with.
//!
//! Visitor sign-in lives in `accounts` and is only *used* here, to know who is
//! sitting at the consent screen. Deciding that an admin may connect is this
//! module's call, not theirs.

use crate::{
    accounts::users::{self, User},
    config::Config,
    platform::oauth_store::{self as store, Client, Grant},
};
use axum::{
    extract::{Form, Query, RawQuery, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use maud::{html, Markup};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Whether a client may ask for a token for this resource: the publishing
/// connector, a person's own, or one app's tools at `/p/<app>/mcp`. They
/// share one sign-in; what a token may do is decided where it is used.
/// An app's connector is named exactly as `connector_url` builds it: on the
/// app's own host in subdomain mode, so the path-mode spelling is refused
/// there.
pub(crate) fn allowed_resource(config: &Config, resource: &str) -> bool {
    let Some(base) = config.base_url.as_deref() else {
        return false;
    };
    let resource = resource.trim_end_matches('/');
    if resource == format!("{base}/mcp") || resource == format!("{base}/me/mcp") {
        return true;
    }
    path_of(resource)
        .and_then(crate::platform::app_tools::connector_app)
        .is_some_and(|app| resource == crate::platform::app_tools::connector_url(config, app))
}

/// The path of an absolute URL.
fn path_of(url: &str) -> Option<&str> {
    let rest = &url[url.find("://")? + 3..];
    rest.find('/').map(|i| &rest[i..])
}

fn issuer(config: &Config) -> &str {
    config
        .base_url
        .as_deref()
        .expect("OAuth routes are only mounted when base_url is set")
}

// --- discovery ------------------------------------------------------------

pub(crate) async fn oauth_protected_resource_metadata(
    State(config): State<Arc<Config>>,
) -> impl IntoResponse {
    let base = issuer(&config);
    Json(serde_json::json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
    }))
}

/// The same for `/me/mcp`, so a person's client discovers the same server.
pub(crate) async fn me_protected_resource_metadata(
    State(config): State<Arc<Config>>,
) -> impl IntoResponse {
    let base = issuer(&config);
    Json(serde_json::json!({
        "resource": format!("{base}/me/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
    }))
}

/// The same for one app's tools at `/p/<app>/mcp`.
pub(crate) async fn app_protected_resource_metadata(
    State(config): State<Arc<Config>>,
    axum::extract::Path(app): axum::extract::Path<String>,
) -> axum::response::Response {
    if !crate::platform::export::valid_app(&app) {
        return (axum::http::StatusCode::NOT_FOUND, "not found").into_response();
    }
    let base = issuer(&config);
    Json(serde_json::json!({
        "resource": crate::platform::app_tools::connector_url(&config, &app),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
    }))
    .into_response()
}

pub(crate) async fn oauth_authorization_server_metadata(
    State(config): State<Arc<Config>>,
) -> impl IntoResponse {
    let base = issuer(&config);
    Json(serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        // Every client is public: the code is bound to it by PKCE, not by a
        // secret it would have to keep somewhere.
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

// --- redirect URIs -------------------------------------------------------

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Where a code may be sent: anywhere over HTTPS, or a program on the
/// person's own machine over plain HTTP (Claude Code, a desktop app). Plain
/// HTTP to anywhere else would put the code on the wire in the clear, and a
/// custom scheme is a claim any app on a machine can make.
pub(crate) fn redirect_uri_allowed(uri: &str) -> bool {
    let Ok(parsed) = uri.parse::<Uri>() else {
        return false;
    };
    let Some(host) = parsed.host() else {
        return false;
    };
    match parsed.scheme_str() {
        Some("https") => true,
        Some("http") => is_loopback(host),
        _ => false,
    }
}

/// Whether the URI a request presents is one the client registered. An exact
/// match, except that a loopback redirect may use any port: a program on the
/// person's machine binds whichever is free when it starts (RFC 8252 §7.3).
pub(crate) fn redirect_uri_matches(registered: &str, presented: &str) -> bool {
    if registered == presented {
        return true;
    }
    let (Ok(a), Ok(b)) = (registered.parse::<Uri>(), presented.parse::<Uri>()) else {
        return false;
    };
    a.scheme_str() == Some("http")
        && b.scheme_str() == Some("http")
        && a.host().is_some_and(is_loopback)
        && a.host() == b.host()
        && a.path() == b.path()
        && a.query() == b.query()
}

fn client_accepts(client: &Client, redirect_uri: &str) -> bool {
    client
        .redirect_uris
        .iter()
        .any(|registered| redirect_uri_matches(registered, redirect_uri))
}

// --- registration --------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct Registration {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

const MAX_REDIRECT_URIS: usize = 16;
const MAX_URI_LEN: usize = 1024;
const MAX_NAME_LEN: usize = 120;

/// RFC 7591, open to anyone. Registering is not a privilege: the id it hands
/// back cannot do anything until a person signs in and consents to it, and
/// it is swept if nobody ever does.
pub(crate) async fn register(
    State(config): State<Arc<Config>>,
    Json(registration): Json<Registration>,
) -> Response {
    let uris = &registration.redirect_uris;
    if uris.is_empty() || uris.len() > MAX_REDIRECT_URIS {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri");
    }
    if let Some(bad) = uris
        .iter()
        .find(|uri| uri.len() > MAX_URI_LEN || !redirect_uri_allowed(uri))
    {
        tracing::warn!(redirect_uri = %bad, "registration refused: redirect_uri not allowed");
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri");
    }
    let name = registration
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(MAX_NAME_LEN).collect::<String>());

    let outcome = store::of(&config).register_client(name.as_deref(), uris).await;
    match outcome {
        Ok(client) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "client_id": client.id,
                "client_name": client.name,
                "redirect_uris": client.redirect_uris,
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
            })),
        )
            .into_response(),
        Err(message) => {
            tracing::warn!(%message, "registration failed");
            oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// --- authorize -----------------------------------------------------------

#[derive(Deserialize, Clone)]
pub(crate) struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    /// RFC 8707. Ignored unless it names somewhere other than here.
    resource: Option<String>,
}

/// The same parameters come back with the person's answer.
#[derive(Deserialize)]
pub(crate) struct Decision {
    #[serde(flatten)]
    request: AuthorizeParams,
    token: String,
    decision: String,
}

fn redirect_with(base: &str, pairs: &[(&str, &str)]) -> Response {
    let mut url = String::from(base);
    let mut sep = if base.contains('?') { '&' } else { '?' };
    for (key, value) in pairs {
        url.push(sep);
        url.push_str(key);
        url.push('=');
        url.push_str(&urlencoding::encode(value));
        sep = '&';
    }
    Redirect::to(&url).into_response()
}

fn redirect_error(params: &AuthorizeParams, error: &str) -> Response {
    let mut pairs = vec![("error", error)];
    if let Some(state) = params.state.as_deref() {
        pairs.push(("state", state));
    }
    redirect_with(&params.redirect_uri, &pairs)
}

fn plain_page(status: StatusCode, title: &str, body: Markup) -> Response {
    let markup = crate::ui::form_page(title, html! { h1 { (title) } (body) });
    (status, Html(markup.into_string())).into_response()
}

/// Looks the client up and checks the redirect URI against it. Anything wrong
/// here is answered on the page, never by redirecting: until the redirect URI
/// is known to be the client's, sending the browser there would make this
/// server an open redirect.
async fn validated_client(
    config: &Arc<Config>,
    params: &AuthorizeParams,
) -> Result<Client, Response> {
    let client = store::of(config).client(&params.client_id).await;
    let Some(client) = client else {
        tracing::warn!(client_id = %params.client_id, "authorize refused: unknown client");
        return Err(plain_page(
            StatusCode::BAD_REQUEST,
            "Unknown client",
            html! { p."muted" { "This client is not registered with this server. Start the connection again." } },
        ));
    };
    if !client_accepts(&client, &params.redirect_uri) {
        tracing::warn!(
            client_id = %params.client_id,
            redirect_uri = %params.redirect_uri,
            "authorize refused: redirect_uri not registered"
        );
        return Err(plain_page(
            StatusCode::BAD_REQUEST,
            "Redirect not allowed",
            html! { p."muted" { "The client asked for a redirect to an address it did not register." } },
        ));
    }
    Ok(client)
}

/// The request itself, once the redirect URI is known to be safe to use.
fn validate_request(config: &Config, params: &AuthorizeParams) -> Result<(), Response> {
    if params.response_type != "code" {
        return Err(redirect_error(params, "unsupported_response_type"));
    }
    match (params.code_challenge.as_deref(), params.code_challenge_method.as_deref()) {
        (Some(challenge), Some("S256")) if !challenge.is_empty() => {}
        _ => {
            tracing::warn!(client_id = %params.client_id, "authorize refused: PKCE S256 required");
            return Err(redirect_error(params, "invalid_request"));
        }
    }
    if let Some(resource) = params.resource.as_deref()
        && !allowed_resource(config, resource)
    {
        tracing::warn!(client_id = %params.client_id, %resource, "authorize refused: wrong resource");
        return Err(redirect_error(params, "invalid_target"));
    }
    Ok(())
}

/// Who is at the consent screen. Any active account may connect a client:
/// an admin's client publishes through `/mcp`, anyone else's reads what the
/// account may open through `/me/mcp`. Which one is decided where the token
/// is presented, not here.
async fn consenting_user(
    config: &Arc<Config>,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<User, Response> {
    match users::current_site_user(config, headers).await {
        Some(user) => {
            // A client connected now would outlive the session, so one the
            // site's policy holds for two-step setup does not get to
            // connect one until it has.
            let (worker, who) = (config.clone(), user.clone());
            if tokio::task::spawn_blocking(move || crate::accounts::mfa::owes_setup(&worker, &who)).await.unwrap_or(true) {
                tracing::warn!(email = %user.email, "consent refused: the site requires two-step sign-in of this account and it is off");
                return Err(plain_page(
                    StatusCode::FORBIDDEN,
                    "Set up two-step sign-in",
                    maud::html! {
                        p { "This site requires two-step sign-in for your account. Set it up, then connect the client again." }
                        a."btn" href="/account#two-step" { "Set up two-step sign-in" }
                    },
                ));
            }
            Ok(user)
        }
        None => {
            let here = match query {
                Some(query) => format!("/authorize?{query}"),
                None => "/authorize".to_string(),
            };
            Err(Redirect::to(&format!("/auth/login?next={}", urlencoding::encode(&here))).into_response())
        }
    }
}

pub(crate) async fn authorize_form(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    let client = match validated_client(&config, &params).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    if let Err(response) = validate_request(&config, &params) {
        return response;
    }
    let admin = match consenting_user(&config, &headers, query.as_deref()).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let token = users::derive_form_token(&config, &admin.id);
    let markup = crate::ui::form_page("Connect", consent(&client, &params, &admin, &token));
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(markup.into_string()),
    )
        .into_response()
}

fn consent(client: &Client, params: &AuthorizeParams, admin: &User, token: &str) -> Markup {
    // What the token will be good for decides what the screen promises.
    let resource_path = params
        .resource
        .as_deref()
        .map(normalise_resource)
        .and_then(|r| r.find("://").and_then(|i| r[i + 3..].find('/').map(|j| r[i + 3 + j..].to_string())));
    let app_connector = resource_path
        .as_deref()
        .and_then(crate::platform::app_tools::connector_app)
        .map(str::to_string);
    let for_person = resource_path.as_deref() == Some("/me/mcp");
    let destination = params.redirect_uri.parse::<Uri>().ok();
    let host = destination
        .as_ref()
        .and_then(|uri| uri.host())
        .unwrap_or("?")
        .to_string();
    let local = is_loopback(&host);
    html! {
        form."column" method="post" action="/authorize" {
            h1 { "Connect this client?" }
            // What is proven comes first: where the code goes. What the
            // client says about itself comes second, as a claim.
            @if local {
                p { strong { "A program on this computer" } " requests access to publish here." }
            } @else {
                p { strong { (host) } " requests access to publish here." }
            }
            @if let Some(name) = &client.name {
                p."muted" { "The client name is " (name) "." }
            }
            @if local {
                p."muted" { "Warning: Any program on this computer can make this request. Continue only if you started this connection." }
            }
            @if let Some(app) = app_connector {
                p."muted" {
                    "The client acts as " (admin.email) " and can use the tools of the app " strong { (app) } " only, "
                    "with the access this account has in that app. It cannot publish or manage anything, "
                    "and the token does not work on any other connector."
                }
            } @else if admin.is_admin && !for_person {
                p."muted" {
                    "The client acts as " (admin.email) " with all permissions of this account. "
                    "The client can publish and remove apps, run SQL on each app, and manage accounts and access."
                }
            } @else {
                p."muted" {
                    "The client acts as " (admin.email) ". "
                    "The client can list the apps this account may open and read the data those apps "
                    "share with you. It can change data only where an app permits that. It cannot publish "
                    "or manage anything."
                }
            }
            input type="hidden" name="token" value=(token);
            input type="hidden" name="response_type" value=(params.response_type);
            input type="hidden" name="client_id" value=(params.client_id);
            input type="hidden" name="redirect_uri" value=(params.redirect_uri);
            @if let Some(state) = &params.state {
                input type="hidden" name="state" value=(state);
            }
            @if let Some(challenge) = &params.code_challenge {
                input type="hidden" name="code_challenge" value=(challenge);
            }
            @if let Some(method) = &params.code_challenge_method {
                input type="hidden" name="code_challenge_method" value=(method);
            }
            @if let Some(resource) = &params.resource {
                input type="hidden" name="resource" value=(resource);
            }
            div."actions consent" {
                button type="submit" name="decision" value="allow" { "Allow access" }
                button."quiet" type="submit" name="decision" value="deny" { "Deny access" }
            }
        }
    }
}

pub(crate) async fn authorize_decide(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(decision): Form<Decision>,
) -> Response {
    let params = decision.request;
    let client = match validated_client(&config, &params).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    if let Err(response) = validate_request(&config, &params) {
        return response;
    }
    let admin = match consenting_user(&config, &headers, None).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let expected = users::derive_form_token(&config, &admin.id);
    if expected.len() != decision.token.len() || expected != decision.token {
        tracing::warn!(email = %admin.email, "authorize refused: form token mismatch");
        return (StatusCode::FORBIDDEN, "The form is out of date. Reload the page and try again.").into_response();
    }
    if decision.decision != "allow" {
        tracing::info!(email = %admin.email, client_id = %client.id, "connection declined");
        return redirect_error(&params, "access_denied");
    }

    let challenge = params.code_challenge.clone().unwrap_or_default();
    let resource = params.resource.as_deref().map(normalise_resource);
    let code = store::of(&config)
        .issue_code(&Grant {
            client_id: &client.id,
            user_id: &admin.id,
            redirect_uri: &params.redirect_uri,
            code_challenge: &challenge,
            resource: resource.as_deref(),
        })
        .await;
    match code {
        Ok(code) => {
            tracing::info!(email = %admin.email, client_id = %client.id, "connection approved");
            let mut pairs = vec![("code", code.as_str())];
            if let Some(state) = params.state.as_deref() {
                pairs.push(("state", state));
            }
            redirect_with(&params.redirect_uri, &pairs)
        }
        Err(error) => {
            tracing::warn!(email = %admin.email, client_id = %client.id, %error, "authorize failed: no code issued");
            (StatusCode::INTERNAL_SERVER_ERROR, "could not issue a code").into_response()
        }
    }
}

// --- token ---------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct TokenRequest {
    grant_type: String,
    client_id: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

pub(crate) fn oauth_error(status: StatusCode, error: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    computed.len() == challenge.len() && computed == challenge
}

/// The account a token is about to be issued to, if it is still active. A
/// code or refresh token outlives nothing: an account disabled in between is
/// refused here, and every request after is refused by the bearer check.
/// Whether the account may publish is `/mcp`'s question, asked there.
async fn publishing_user(config: &Arc<Config>, user_id: &str) -> Option<User> {
    let (config, id) = (config.clone(), user_id.to_string());
    tokio::task::spawn_blocking(move || users::user_by_id(&config, &id))
        .await
        .ok()
        .flatten()
}

fn token_response(issued: store::Issued) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "access_token": issued.access_token,
            "token_type": "Bearer",
            "expires_in": issued.expires_in,
            "refresh_token": issued.refresh_token,
        })),
    )
        .into_response()
}

pub(crate) async fn token_endpoint(
    State(config): State<Arc<Config>>,
    Form(body): Form<TokenRequest>,
) -> Response {
    let Some(client_id) = body.client_id.clone().filter(|id| !id.is_empty()) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    };

    match body.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(verifier)) = (body.code.clone(), body.code_verifier.clone()) else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            let redeemed = store::of(&config).redeem_code(&code).await;
            let Some(redeemed) = redeemed else {
                tracing::warn!(%client_id, "token refused: unknown, expired or spent code");
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            };
            if redeemed.client_id != client_id
                || body.redirect_uri.as_deref() != Some(redeemed.redirect_uri.as_str())
                || !pkce_matches(&verifier, &redeemed.code_challenge)
            {
                tracing::warn!(
                    %client_id,
                    client_matches = redeemed.client_id == client_id,
                    redirect_matches = body.redirect_uri.as_deref() == Some(redeemed.redirect_uri.as_str()),
                    "token refused: code was issued to a different request"
                );
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            let Some(user) = publishing_user(&config, &redeemed.user_id).await else {
                tracing::warn!(%client_id, "token refused: account can no longer publish");
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            };
            let issued = store::of(&config)
                .issue_tokens(&client_id, &user.id, redeemed.resource.as_deref())
                .await;
            match issued {
                Ok(issued) => token_response(issued),
                Err(error) => {
                    tracing::warn!(%client_id, %error, "token refused: tokens could not be stored");
                    oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
                }
            }
        }
        "refresh_token" => {
            let Some(refresh_token) = body.refresh_token.clone() else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            // Whose token this is has to be known before it is rotated, so
            // the account check runs first against the live row.
            let oauth = store::of(&config);
            let issued = oauth.rotate_refresh(&client_id, &refresh_token).await;
            let Some(issued) = issued else {
                tracing::warn!(%client_id, "token refused: unknown, expired or retired refresh token");
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            };
            // The rotation already spent the old token; if the account has
            // gone, the new pair is left to expire unused and refused on
            // every request in the meantime.
            let holder = oauth.access_token_grant(&issued.access_token).await;
            match holder {
                Some((user_id, _, _)) if publishing_user(&config, &user_id).await.is_some() => {
                    token_response(issued)
                }
                _ => {
                    tracing::warn!(%client_id, "token refused: account can no longer publish");
                    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant")
                }
            }
        }
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}

// --- bearer ---------------------------------------------------------------

/// The account behind an access token, if the token is live and the account
/// is still active. What the bearer middleware asks for anything that is not
/// a static token; `/mcp` then insists on an admin, `/me/mcp` does not.
/// With it, the resource the token was issued for: `None` for a token that
/// named none. Each endpoint asks `token_fits`
/// whether that resource lets the token in there.
pub(crate) async fn token_user_for(config: &Arc<Config>, token: &str) -> Option<(User, Option<String>)> {
    let (user_id, _client, resource) = store::of(config).access_token_grant(token).await?;
    publishing_user(config, &user_id).await.map(|user| (user, resource))
}

/// A resource as a token records it: no trailing slash.
pub(crate) fn normalise_resource(resource: &str) -> String {
    resource.trim().trim_end_matches('/').to_string()
}

/// Where a token may be used, by the resource it was issued for (RFC 8707).
/// A token for the publishing connector reaches everything a person can; a
/// token for a person's own connector reaches that and any app's tools,
/// which it can already call there; a token for one app's tools reaches
/// that app's connector and nothing else. A token that named no resource
/// keeps working everywhere, as tokens did before resources were recorded.
pub(crate) fn token_fits(config: &Config, resource: Option<&str>, endpoint_path: &str) -> bool {
    let Some(resource) = resource else {
        return true;
    };
    let Some(base) = config.base_url.as_deref() else {
        return true;
    };
    let connector = crate::platform::app_tools::connector_app(endpoint_path);
    if resource == format!("{base}/mcp") {
        return true;
    }
    if resource == format!("{base}/me/mcp") {
        return endpoint_path == "/me/mcp" || connector.is_some();
    }
    // An app's connector, as it is reached: on the app's host in subdomain
    // mode, so a token bound to the path-mode address is not taken there.
    connector.is_some_and(|app| resource == crate::platform::app_tools::connector_url(config, app))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_may_go_over_https_or_to_this_machine_and_nowhere_else() {
        assert!(redirect_uri_allowed("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_uri_allowed("http://localhost:52341/callback"));
        assert!(redirect_uri_allowed("http://127.0.0.1:8/cb"));
        assert!(redirect_uri_allowed("http://[::1]:3000/cb"));
        assert!(!redirect_uri_allowed("http://example.com/cb"), "plaintext off-machine");
        assert!(!redirect_uri_allowed("http://localhost.evil.com/cb"));
        assert!(!redirect_uri_allowed("claude://callback"), "custom scheme");
        assert!(!redirect_uri_allowed("javascript:alert(1)"));
        assert!(!redirect_uri_allowed("/relative"));
        assert!(!redirect_uri_allowed(""));
    }

    #[test]
    fn a_loopback_redirect_may_change_port_but_nothing_else() {
        assert!(redirect_uri_matches("http://localhost:1234/cb", "http://localhost:9999/cb"));
        assert!(redirect_uri_matches("http://127.0.0.1/cb", "http://127.0.0.1:4000/cb"));
        assert!(!redirect_uri_matches("http://localhost:1234/cb", "http://localhost:1234/other"));
        assert!(!redirect_uri_matches("http://localhost:1234/cb", "http://127.0.0.1:1234/cb"));
        assert!(!redirect_uri_matches("http://localhost:1234/cb", "https://localhost:1234/cb"));
        // Off the machine, the port is part of the identity.
        assert!(!redirect_uri_matches("https://c.test/cb", "https://c.test:8443/cb"));
        assert!(!redirect_uri_matches("https://c.test/cb", "https://c.test/cb?x=1"));
        assert!(redirect_uri_matches("https://c.test/cb", "https://c.test/cb"));
    }

    #[test]
    fn pkce_is_the_rfc_7636_test_vector() {
        assert!(pkce_matches(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!pkce_matches("wrong", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
    }
}
