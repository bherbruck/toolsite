pub mod auth;
pub mod bundle;
pub mod config;
pub mod db;
pub mod mcp;
pub mod oauth;
pub mod slug;
pub mod store;
pub mod upload;
pub mod wasm;
pub mod web;

pub use config::Config;

use crate::{
    auth::require_bearer,
    mcp::PageHost,
    oauth::{
        authorize, oauth_authorization_server_metadata, oauth_protected_resource_metadata,
        token_endpoint,
    },
    upload::{upload_root, upload_sub, MAX_UPLOAD_BYTES},
    web::{index, serve_icon, serve_page},
};
use crate::wasm::Runtime;
use axum::{
    extract::{DefaultBodyLimit, FromRef},
    http::Uri,
    middleware,
    routing::{any, get, post, put},
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::sync::Arc;

/// Shared by every handler. Split from `Config` so the data layer never has
/// to know the wasm runtime exists; `FromRef` lets handlers that only want
/// config keep asking for exactly that.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub runtime: Arc<Runtime>,
}

impl FromRef<AppState> for Arc<Config> {
    fn from_ref(state: &AppState) -> Self {
        state.config.clone()
    }
}

/// Assembles every route. Kept out of `main` so tests can drive the whole
/// surface in-process instead of over a socket.
pub fn build_router(config: Arc<Config>, runtime: Arc<Runtime>) -> Router {
    // rmcp's Streamable HTTP transport validates the inbound `Host` header
    // (DNS-rebinding protection) against an allowlist that defaults to
    // localhost only. Deployed behind a real domain, that must include the
    // public host or every request 403s before auth even runs.
    let host_config = match config
        .base_url
        .as_deref()
        .and_then(|b| b.parse::<Uri>().ok())
        .and_then(|u| u.authority().map(|a| a.as_str().to_string()))
    {
        Some(authority) => StreamableHttpServerConfig::default().with_allowed_hosts([
            authority,
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ]),
        None => StreamableHttpServerConfig::default().disable_allowed_hosts(),
    };

    let mcp_config = config.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(PageHost::new(mcp_config.clone())),
        LocalSessionManager::default().into(),
        host_config,
    );

    let mcp_router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(config.clone(), require_bearer));

    let mut public_router = Router::new()
        .route("/", get(index))
        .route("/p/{*slug}", any(serve_page))
        .route("/icon/{*slug}", get(serve_icon))
        .route("/upload/{ticket}", put(upload_root).post(upload_root))
        .route("/upload/{ticket}/{*sub}", put(upload_sub).post(upload_sub))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES));

    if config.oauth.is_some() {
        public_router = public_router
            .route(
                "/.well-known/oauth-protected-resource",
                get(oauth_protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(oauth_protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(oauth_authorization_server_metadata),
            )
            .route("/authorize", get(authorize))
            .route("/token", post(token_endpoint));
    }

    let public_router = public_router.with_state(AppState {
        config: config.clone(),
        runtime,
    });

    Router::new().merge(mcp_router).merge(public_router)
}
