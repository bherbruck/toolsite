//! Where a browser's file lands: `PUT /blob/<ticket>`.
//!
//! A handler that wants a file from a visitor does not read it — it asks the
//! host for an upload URL (`blobs.upload-url`) and hands that to the page,
//! which PUTs the file straight here. The ticket says which app and which key,
//! is good once, and expires in minutes, so the URL can be given to a browser
//! without giving it anything else. The body is streamed to storage and never
//! held whole in memory, which is what lets a blob be gigabytes when a
//! handler's request body may not be.

use crate::{runtime::blobs, AppState};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

pub(crate) async fn receive(
    State(state): State<AppState>,
    host: Option<axum::Extension<crate::content::origins::AppHost>>,
    Path(ticket): Path<String>,
    request: Request<Body>,
) -> Response {
    let config = &state.config;
    let Some(ticket) = blobs::take_upload(config, &ticket) else {
        tracing::warn!("blob upload refused: ticket unknown, expired or already used");
        return (
            StatusCode::UNAUTHORIZED,
            "upload URL unknown, expired or already used; ask the app for a new one\n",
        )
            .into_response();
    };
    // On an app host, only that app's files.
    if let Some(axum::Extension(host)) = &host
        && host.0 != ticket.app
    {
        tracing::warn!(host = %host.0, ticket_for = %ticket.app, "blob upload refused: a ticket for another app");
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }

    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    // A declared length over the ceiling is refused before a byte arrives;
    // an undeclared or dishonest one is caught as it streams.
    if let Some(declared) = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && ticket.max_bytes > 0
        && declared > ticket.max_bytes
    {
        return too_large(ticket.max_bytes);
    }

    let stream = request.into_body().into_data_stream();
    match blobs::receive(
        config,
        &ticket.app,
        &ticket.key,
        &content_type,
        ticket.max_bytes,
        stream,
    )
    .await
    {
        Ok(size) => {
            tracing::info!(app = %ticket.app, key = %ticket.key, size, "blob stored");
            (StatusCode::CREATED, format!("stored {} bytes as {}\n", size, ticket.key)).into_response()
        }
        Err(blobs::Error::TooLarge(_)) => too_large(ticket.max_bytes),
        Err(blobs::Error::InvalidKey(why)) => {
            (StatusCode::BAD_REQUEST, format!("{why}\n")).into_response()
        }
        Err(error) => {
            tracing::warn!(app = %ticket.app, key = %ticket.key, %error, "blob upload failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "could not store the file\n").into_response()
        }
    }
}

fn too_large(max_bytes: u64) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("file is over this upload's limit of {} MB\n", max_bytes / 1024 / 1024),
    )
        .into_response()
}
