mod auth;
mod bundle;
mod config;
mod db;
mod mcp;
mod oauth;
mod slug;
mod store;
mod upload;
mod web;

use crate::{
    auth::require_bearer,
    config::Config,
    mcp::PageHost,
    oauth::{
        authorize, oauth_authorization_server_metadata, oauth_protected_resource_metadata,
        token_endpoint, OAuth,
    },
    upload::{upload_root, upload_sub, MAX_UPLOAD_BYTES},
    web::{index, serve_icon, serve_page},
};
use axum::{
    extract::DefaultBodyLimit,
    http::Uri,
    middleware,
    routing::{get, post, put},
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::fs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Local dev convenience; in a container the env is set directly.
    dotenvy::dotenv().ok();

    // Without this the default filter drops everything, so a deployed instance
    // looks silent even while it's rejecting requests.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    fs::create_dir_all(&data_dir).await?;

    let bearer_token = std::env::var("BEARER_TOKEN").ok().filter(|s| !s.is_empty());
    let oauth_client_id = std::env::var("OAUTH_CLIENT_ID").ok().filter(|s| !s.is_empty());
    let oauth_client_secret = std::env::var("OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|s| !s.is_empty());

    let oauth = match (oauth_client_id, oauth_client_secret) {
        (Some(client_id), Some(client_secret)) => Some(OAuth {
            client_id,
            client_secret,
            auth_codes: Mutex::new(HashMap::new()),
        }),
        (None, None) => None,
        _ => panic!("set both OAUTH_CLIENT_ID and OAUTH_CLIENT_SECRET together, or neither"),
    };

    if bearer_token.is_none() && oauth.is_none() {
        panic!("set BEARER_TOKEN, or OAUTH_CLIENT_ID + OAUTH_CLIENT_SECRET (or both)");
    }

    // A bare host is the natural thing to paste in, but every URL built from
    // this needs a scheme to be usable, so supply one rather than emitting
    // href-less strings like "example.com/p/slug".
    let base_url = std::env::var("PUBLIC_BASE_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.contains("://") {
                s
            } else {
                format!("https://{s}")
            }
        });
    if oauth.is_some() && base_url.is_none() {
        panic!(
            "PUBLIC_BASE_URL is required when OAUTH_CLIENT_ID/OAUTH_CLIENT_SECRET are set \
             (OAuth discovery metadata needs absolute URLs)"
        );
    }

    let mut valid_tokens = Vec::new();
    if let Some(t) = &bearer_token {
        valid_tokens.push(t.clone());
    }
    if let Some(o) = &oauth {
        valid_tokens.push(o.client_secret.clone());
    }

    let databases = std::env::var("DATABASES")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"))
        .unwrap_or(false);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");

    // Printed at boot so a misconfigured deploy is obvious from the logs
    // rather than only from a client's opaque "can't connect".
    tracing::info!(
        bearer_auth = bearer_token.is_some(),
        oauth_auth = oauth.is_some(),
        base_url = base_url.as_deref().unwrap_or("<unset>"),
        databases,
        "auth configuration"
    );

    let oauth_enabled = oauth.is_some();
    let config = Arc::new(Config {
        data_dir,
        base_url,
        local_base: format!("http://localhost:{port}"),
        valid_tokens,
        oauth,
        databases,
        uploads: Mutex::new(HashMap::new()),
    });

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
        .route("/p/{*slug}", get(serve_page))
        .route("/icon/{*slug}", get(serve_icon))
        .route("/upload/{ticket}", put(upload_root).post(upload_root))
        .route("/upload/{ticket}/{*sub}", put(upload_sub).post(upload_sub))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES));

    if oauth_enabled {
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

    let public_router = public_router.with_state(config.clone());

    let app = Router::new().merge(mcp_router).merge(public_router);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
