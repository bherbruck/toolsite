use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::fs;
use rmcp::ServiceExt;
use toolsite::{
    build_router,
    config::Config,
    platform::{client_oauth::OAuth, mcp::PageHost},
    runtime::wasm::Runtime,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Local dev convenience; in a container the env is set directly.
    dotenvy::dotenv().ok();

    // Speaking MCP over stdio makes stdout the protocol channel, so every log
    // line has to go to stderr or it corrupts the stream.
    let stdio = std::env::args().any(|arg| arg == "--stdio")
        || std::env::var("MCP_STDIO").is_ok_and(|v| v != "0");

    // Without this the default filter drops everything, so a deployed instance
    // looks silent even while it's rejecting requests.
    let logs = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
    );
    if stdio {
        logs.with_writer(std::io::stderr).init();
    } else {
        logs.init();
    }

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    fs::create_dir_all(&data_dir).await?;

    // MCP_TOKEN was this variable's first name. A deployment still carrying
    // it would otherwise start with no bearer auth at all and 401 everything,
    // with nothing in the logs pointing at the cause.
    let bearer_token = std::env::var("BEARER_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let legacy = std::env::var("MCP_TOKEN").ok().filter(|s| !s.is_empty());
            if legacy.is_some() {
                tracing::warn!(
                    "MCP_TOKEN is the old name for BEARER_TOKEN and still works; \
                     rename it to stop this warning"
                );
            }
            legacy
        });
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

    // Over stdio the client already owns the process, so there is nothing for
    // a token to protect; HTTP still refuses everything without one.
    if !stdio && bearer_token.is_none() && oauth.is_none() {
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

    let config = Arc::new(Config {
        data_dir,
        base_url,
        local_base: format!("http://localhost:{port}"),
        valid_tokens,
        oauth,
        databases,
        uploads: Mutex::new(HashMap::new()),
    });

    let app = build_router(config.clone(), Runtime::new()?);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");

    if !stdio {
        axum::serve(listener, app).await?;
        return Ok(());
    }

    // The web server keeps running alongside: an agent talks MCP over stdio
    // but still needs somewhere to curl uploads to, and somewhere to view the
    // published page.
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!(%error, "web server stopped");
        }
    });

    tracing::info!("serving MCP on stdio");
    let service = PageHost::new(config).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
