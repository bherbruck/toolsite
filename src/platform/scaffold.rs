//! What an agent needs to build a handler, over plain HTTP.
//!
//! Without this the contract is only in the repository, so an agent with a
//! shell and an upload URL still cannot build anything — it has to guess the
//! world's shape from rejection messages. Both routes are public: the WIT
//! describes an interface, and the scaffold is a template. Neither says
//! anything about what is published here.

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::{io::Write, sync::Arc};

use crate::{config::Config, content::slug::valid_slug};

const WIT: &str = include_str!("../../wit/toolsite.wit");
const GUIDE: &str = include_str!("../../templates/guide.md");
const HANDLER_CARGO: &str = include_str!("../../templates/handler/Cargo.toml");
const HANDLER_LIB: &str = include_str!("../../templates/handler/src/lib.rs");
const HANDLER_MIGRATION: &str =
    include_str!("../../templates/handler/migrations/001_initial.sql");

/// How the platform works, for an agent that would otherwise go looking in
/// somebody's app notes for it. This is the copy that stays current; notes
/// describe one app and go stale the moment the platform changes.
pub async fn guide() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        GUIDE,
    )
        .into_response()
}

/// The contract a handler compiles against, so `curl` is enough to start.
pub async fn wit() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        WIT,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ScaffoldQuery {
    /// Names the crate and, more usefully, is the slug the finished app will
    /// be served under.
    app: Option<String>,
}

/// A complete crate: the WIT vendored, Cargo.toml with the right crate type,
/// and a handler that already reads and writes its own database.
pub async fn handler_scaffold(
    State(config): State<Arc<Config>>,
    Query(query): Query<ScaffoldQuery>,
) -> Response {
    let app = query.app.unwrap_or_else(|| "myapp".to_string());
    if !valid_slug(&app) {
        return (
            StatusCode::BAD_REQUEST,
            "app must be letters, numbers, '-' or '_'\n",
        )
            .into_response();
    }

    let base = config
        .base_url
        .as_deref()
        .unwrap_or(&config.local_base)
        .to_string();
    let crate_name = format!("{app}_handler").replace('-', "_");
    let readme = format!(
        r#"# {app} handler

Build:

    rustup target add wasm32-wasip2
    cargo build --release --target wasm32-wasip2

Upload, using an upload URL from create_upload. Schema first, so the handler
is never live without its tables:

    tar -czf - -C migrations . | curl -f -T - '<upload-url>?migrations'
    curl -f -T target/wasm32-wasip2/release/{crate_name}.wasm '<upload-url>?handler'

Add migrations/002_*.sql for the next schema change rather than editing 001:
each file runs once, so an edited one never reaches a database that already
ran it.

It then answers every request under {base}/p/{app}/api/, and any route with no
file behind it.

The handler sees the path relative to this app, with /api still on it, and
gets exactly two capabilities: its own SQLite database and the identity of
whoever is signed in. No filesystem, no network, no environment.
"#
    );

    let files = [
        ("Cargo.toml", HANDLER_CARGO.replace("NAME", &app)),
        ("src/lib.rs", HANDLER_LIB.to_string()),
        ("migrations/001_initial.sql", HANDLER_MIGRATION.to_string()),
        ("wit/toolsite.wit", WIT.to_string()),
        ("README.md", readme),
    ];

    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in &files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        if builder
            .append_data(&mut header, format!("{app}-handler/{name}"), body.as_bytes())
            .is_err()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not build the scaffold\n")
                .into_response();
        }
    }
    let Ok(tar) = builder.into_inner() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not build the scaffold\n")
            .into_response();
    };

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    if encoder.write_all(&tar).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not build the scaffold\n")
            .into_response();
    }
    let Ok(gz) = encoder.finish() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not build the scaffold\n")
            .into_response();
    };

    (
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{app}-handler.tar.gz\""),
            ),
        ],
        gz,
    )
        .into_response()
}

/// `/scaffold/<app>` reads better in a tool description than a query string,
/// so both spellings work.
pub async fn handler_scaffold_named(
    state: State<Arc<Config>>,
    Path(app): Path<String>,
) -> Response {
    handler_scaffold(state, Query(ScaffoldQuery { app: Some(app) })).await
}
