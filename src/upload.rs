use crate::{
    bundle::unpack_bundle,
    config::Config,
    slug::valid_slug,
    store::{page_url, read_meta, write_meta},
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::fs;

/// A short-lived, single-slug write capability handed to an agent so it can
/// `curl -T file.html <url>` instead of pasting page HTML through a tool call.
pub struct UploadTicket {
    pub slug: String,
    pub expires_at: Instant,
}

pub(crate) const UPLOAD_TTL: Duration = Duration::from_secs(900);

pub(crate) const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const MAX_ICON_BYTES: usize = 1024 * 1024;

pub(crate) fn upload_url(config: &Config, ticket: &str) -> String {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    format!("{base}/upload/{ticket}")
}

/// Flags on an upload URL. Presence is what counts; any value works.
/// `?icon` stores the body as the page's icon, `?bundle` unpacks it as a
/// gzipped tar of a built site, and `?spa` marks that bundle client-routed.
#[derive(Deserialize)]
pub(crate) struct UploadQuery {
    pub(crate) icon: Option<String>,
    pub(crate) bundle: Option<String>,
    pub(crate) spa: Option<String>,
}

pub(crate) enum UploadKind {
    Page,
    Icon,
    Bundle { spa: bool },
}

/// Ticket-authenticated write. The ticket itself is the credential, so this
/// route sits outside the bearer middleware — an agent can upload a file
/// without ever being handed the server's real token.
pub(crate) async fn store_upload(
    config: &Config,
    ticket: &str,
    sub: Option<String>,
    kind: UploadKind,
    body: Bytes,
) -> Response {
    let slug = {
        let now = Instant::now();
        let mut tickets = config.uploads.lock().unwrap();
        tickets.retain(|_, t| t.expires_at > now);
        match tickets.get(ticket) {
            Some(t) => t.slug.clone(),
            None => {
                return (
                    StatusCode::UNAUTHORIZED,
                    "upload ticket unknown or expired; call create_upload again\n",
                )
                    .into_response()
            }
        }
    };

    let slug = match sub {
        Some(sub) => format!("{slug}/{}", sub.trim_end_matches(".html")),
        None => slug,
    };
    if !valid_slug(&slug) {
        return (
            StatusCode::BAD_REQUEST,
            "page name must be path segments of letters, numbers, '-' or '_'\n",
        )
            .into_response();
    }

    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "body is empty\n").into_response();
    }

    if let UploadKind::Bundle { spa } = kind {
        let dest = config.data_dir.join(&slug);
        if fs::create_dir_all(&dest).await.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response();
        }
        // Decompression is CPU-bound and blocking; keep it off the runtime.
        let unpack_dest = dest.clone();
        let result =
            tokio::task::spawn_blocking(move || unpack_bundle(&body, &unpack_dest)).await;
        let unpacked = match result {
            Ok(Ok(unpacked)) => unpacked,
            Ok(Err(message)) => {
                tracing::warn!(slug = %slug, error = %message, "bundle rejected");
                return (StatusCode::BAD_REQUEST, format!("{message}\n")).into_response();
            }
            Err(e) => {
                tracing::error!(slug = %slug, error = %e, "bundle unpack panicked");
                return (StatusCode::INTERNAL_SERVER_ERROR, "unpack failed\n").into_response();
            }
        };

        if spa {
            let mut meta = read_meta(config, &slug).await;
            meta.spa = true;
            let _ = write_meta(config, &slug, &meta).await;
        }

        let has_index = unpacked.files.iter().any(|f| f == "index.html");
        let mut body = format!(
            "unpacked {} files to {}\n",
            unpacked.files.len(),
            page_url(config, &slug)
        );
        if !unpacked.skipped.is_empty() {
            body.push_str(&format!("skipped {}\n", unpacked.skipped.join(", ")));
        }
        if !has_index {
            body.push_str(
                "warning: no index.html at the bundle root, so the app root will 404\n",
            );
        }
        tracing::info!(
            slug = %slug,
            files = unpacked.files.len(),
            skipped = ?unpacked.skipped,
            spa,
            "bundle published"
        );
        return (StatusCode::OK, body).into_response();
    }

    let as_icon = matches!(kind, UploadKind::Icon);
    if as_icon && body.len() > MAX_ICON_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("icon must be under {} KB\n", MAX_ICON_BYTES / 1024),
        )
            .into_response();
    }

    let bytes: Bytes = if as_icon {
        body
    } else {
        match String::from_utf8(body.to_vec()) {
            Ok(html) if !html.trim().is_empty() => Bytes::from(html),
            Ok(_) => return (StatusCode::BAD_REQUEST, "body is empty\n").into_response(),
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "body must be UTF-8 HTML\n").into_response()
            }
        }
    };

    let extension = if as_icon { "icon" } else { "html" };
    let path = config.data_dir.join(format!("{slug}.{extension}"));
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).await.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response();
        }
    }
    if fs::write(&path, &bytes).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response();
    }

    if as_icon {
        let public = slug.strip_suffix("/index").unwrap_or(&slug);
        return (
            StatusCode::OK,
            format!("icon set for {}\n", page_url(config, public)),
        )
            .into_response();
    }

    // An app's index page is reachable at the app root, which is the URL worth
    // handing back.
    let public = slug.strip_suffix("/index").unwrap_or(&slug);
    (StatusCode::OK, format!("{}\n", page_url(config, public))).into_response()
}

pub(crate) async fn upload_root(
    State(config): State<Arc<Config>>,
    Path(ticket): Path<String>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    store_upload(&config, &ticket, None, upload_kind(&query), body).await
}

pub(crate) async fn upload_sub(
    State(config): State<Arc<Config>>,
    Path((ticket, sub)): Path<(String, String)>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    store_upload(&config, &ticket, Some(sub), upload_kind(&query), body).await
}

pub(crate) fn upload_kind(query: &UploadQuery) -> UploadKind {
    if query.bundle.is_some() {
        UploadKind::Bundle {
            spa: query.spa.is_some(),
        }
    } else if query.icon.is_some() {
        UploadKind::Icon
    } else {
        UploadKind::Page
    }
}
