use crate::{config::Config, content::slug::random_token};
use axum::{
    extract::{Form, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect},
    Json,
};
use base64::{
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub struct AuthCode {
    pub(crate) redirect_uri: String,
    pub(crate) code_challenge: Option<String>,
    pub(crate) expires_at: Instant,
}

/// Present only when OAUTH_CLIENT_ID + OAUTH_CLIENT_SECRET are configured.
/// Mounts the OAuth discovery/authorize/token routes; absent means the server
/// only does plain bearer-token auth (for clients that support that directly).
pub struct OAuth {
    pub client_id: String,
    pub client_secret: String,
    pub auth_codes: Mutex<HashMap<String, AuthCode>>,
}

pub(crate) async fn oauth_protected_resource_metadata(State(config): State<Arc<Config>>) -> impl IntoResponse {
    let base = config.base_url.as_deref().expect("base_url required in OAuth mode");
    Json(serde_json::json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
    }))
}

pub(crate) async fn oauth_authorization_server_metadata(
    State(config): State<Arc<Config>>,
) -> impl IntoResponse {
    let base = config.base_url.as_deref().expect("base_url required in OAuth mode");
    Json(serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
    }))
}

pub(crate) fn redirect_uri_allowed(uri: &str) -> bool {
    uri.parse::<Uri>()
        .ok()
        .and_then(|u| u.host().map(|h| h == "claude.ai" || h.ends_with(".claude.ai")))
        .unwrap_or(false)
}

#[derive(Deserialize)]
pub(crate) struct AuthorizeParams {
    pub(crate) response_type: String,
    pub(crate) client_id: String,
    pub(crate) redirect_uri: String,
    pub(crate) state: Option<String>,
    pub(crate) code_challenge: Option<String>,
    #[allow(dead_code)]
    pub(crate) code_challenge_method: Option<String>,
}

pub(crate) async fn authorize(
    State(config): State<Arc<Config>>,
    Query(params): Query<AuthorizeParams>,
) -> impl IntoResponse {
    let oauth = config.oauth.as_ref().expect("/authorize only mounted when OAuth is configured");

    if !redirect_uri_allowed(&params.redirect_uri) {
        return (StatusCode::BAD_REQUEST, "redirect_uri not allowed").into_response();
    }
    if params.response_type != "code" || params.client_id != oauth.client_id {
        let mut url = format!("{}?error=invalid_request", params.redirect_uri);
        if let Some(state) = &params.state {
            url.push_str(&format!("&state={}", urlencoding::encode(state)));
        }
        return Redirect::to(&url).into_response();
    }

    let code = random_token(32);
    oauth.auth_codes.lock().unwrap().insert(
        code.clone(),
        AuthCode {
            redirect_uri: params.redirect_uri.clone(),
            code_challenge: params.code_challenge.clone(),
            expires_at: Instant::now() + Duration::from_secs(60),
        },
    );

    let mut url = format!("{}?code={}", params.redirect_uri, code);
    if let Some(state) = &params.state {
        url.push_str(&format!("&state={}", urlencoding::encode(state)));
    }
    Redirect::to(&url).into_response()
}

#[derive(Deserialize)]
pub(crate) struct TokenRequest {
    pub(crate) grant_type: String,
    pub(crate) code: Option<String>,
    pub(crate) redirect_uri: Option<String>,
    pub(crate) client_id: Option<String>,
    pub(crate) client_secret: Option<String>,
    pub(crate) code_verifier: Option<String>,
}

pub(crate) fn oauth_error(status: StatusCode, error: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

pub(crate) fn client_credentials(headers: &HeaderMap, body: &TokenRequest) -> (Option<String>, Option<String>) {
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(b64) = auth.strip_prefix("Basic ") {
            if let Ok(decoded) = BASE64_STANDARD.decode(b64) {
                if let Ok(s) = String::from_utf8(decoded) {
                    if let Some((id, secret)) = s.split_once(':') {
                        return (Some(id.to_string()), Some(secret.to_string()));
                    }
                }
            }
        }
    }
    (body.client_id.clone(), body.client_secret.clone())
}

pub(crate) async fn token_endpoint(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(body): Form<TokenRequest>,
) -> impl IntoResponse {
    let oauth = config.oauth.as_ref().expect("/token only mounted when OAuth is configured");

    let (client_id, client_secret) = client_credentials(&headers, &body);
    if client_id.as_deref() != Some(oauth.client_id.as_str())
        || client_secret.as_deref() != Some(oauth.client_secret.as_str())
    {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }

    match body.grant_type.as_str() {
        "authorization_code" => {
            let Some(code) = body.code.clone() else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            let entry = oauth.auth_codes.lock().unwrap().remove(&code);
            let Some(entry) = entry else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            };
            if entry.expires_at < Instant::now() {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            if body.redirect_uri.as_deref() != Some(entry.redirect_uri.as_str()) {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            if let Some(challenge) = &entry.code_challenge {
                let verifier = body.code_verifier.clone().unwrap_or_default();
                let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                if &computed != challenge {
                    return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
                }
            }
            success_token(oauth)
        }
        "refresh_token" => success_token(oauth),
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}

pub(crate) fn success_token(oauth: &OAuth) -> axum::response::Response {
    Json(serde_json::json!({
        "access_token": oauth.client_secret,
        "token_type": "Bearer",
        "expires_in": 31_536_000,
        "refresh_token": oauth.client_secret,
    }))
    .into_response()
}
