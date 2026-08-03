use crate::{
    config::Config,
    content::{
        bundle::unpack_bundle,
        slug::valid_slug,
        store::{page_url, read_meta, write_meta},
    },
    runtime::wasm::Runtime,
    AppState,
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
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
    pub(crate) handler: Option<String>,
    /// The project the bundle was built from, kept private.
    pub(crate) source: Option<String>,
    /// toolsite.toml: what the app needs, rather than a list of commands.
    pub(crate) manifest: Option<String>,
    /// A gzipped tar of migrations/*.sql — the app's own schema.
    pub(crate) migrations: Option<String>,
}

pub(crate) enum UploadKind {
    Page,
    Icon,
    Bundle { spa: bool },
    Handler,
    Source,
    Manifest,
    Migrations,
}

/// Ticket-authenticated write. The ticket itself is the credential, so this
/// route sits outside the bearer middleware — an agent can upload a file
/// without ever being handed the server's real token.
pub(crate) async fn store_upload(
    config: &Config,
    runtime: &Runtime,
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

    // The app's schema, applied before anything can ask for a table.
    if let UploadKind::Migrations = kind {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        let files = match crate::content::bundle::read_sql_files(&body) {
            Ok(files) => files,
            Err(message) => return (StatusCode::BAD_REQUEST, format!("{message}\n")).into_response(),
        };
        if files.is_empty() {
            return (StatusCode::BAD_REQUEST, "no .sql files in that archive\n").into_response();
        }

        let count = files.len();
        let owned_app = app.clone();
        let config_handle = config.clone_for_task();
        let outcome = tokio::task::spawn_blocking(move || {
            crate::runtime::migrate::store(&config_handle, &owned_app, files)?;
            crate::runtime::migrate::apply(&config_handle, &owned_app)
        })
        .await;

        return match outcome {
            Ok(Ok((version, ran))) => {
                tracing::info!(app = %app, version, ran, "schema migrated");
                (
                    StatusCode::OK,
                    format!("{app}: {count} migration(s) stored, {ran} applied, now at version {version}\n"),
                )
                    .into_response()
            }
            Ok(Err(message)) => (StatusCode::BAD_REQUEST, format!("{message}\n")).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "migration failed\n").into_response(),
        };
    }

    if let UploadKind::Manifest = kind {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        let Ok(text) = String::from_utf8(body.to_vec()) else {
            return (StatusCode::BAD_REQUEST, "toolsite.toml must be UTF-8\n").into_response();
        };
        return match crate::platform::manifest::apply(config, &app, &text).await {
            Ok(changed) if changed.is_empty() => {
                (StatusCode::OK, format!("{app} already matches its manifest\n")).into_response()
            }
            Ok(changed) => {
                tracing::info!(app = %app, changed = ?changed, "manifest applied");
                (
                    StatusCode::OK,
                    format!("{app}: applied {}\n", changed.join(", ")),
                )
                    .into_response()
            }
            Err(message) => (StatusCode::BAD_REQUEST, format!("{message}\n")).into_response(),
        };
    }

    // The project, not the output. Stored whole and never served: what a
    // visitor may see is exactly what the bundle contained, and a later
    // session needs the sources that produced it.
    if let UploadKind::Source = kind {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        if body.len() > MAX_UPLOAD_BYTES {
            return (StatusCode::PAYLOAD_TOO_LARGE, "source archive too large\n").into_response();
        }
        let path = config.data_dir.join(format!("{app}.source"));
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).await.is_err() {
                return (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response();
            }
        }
        if fs::write(&path, &body).await.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response();
        }
        tracing::info!(app = %app, bytes = body.len(), "source stored");
        return (
            StatusCode::OK,
            format!(
                "stored {} bytes of source for {app}; fetch it back with GET on this \
                 same URL with ?source\n",
                body.len()
            ),
        )
            .into_response();
    }

    if let UploadKind::Handler = kind {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        if let Err(error) = runtime.validate(&body) {
            tracing::warn!(app = %app, error = %error, "handler rejected");
            return (
                StatusCode::BAD_REQUEST,
                format!("not a valid handler component: {error}\n"),
            )
                .into_response();
        }

        let dir = config.data_dir.join(&app);
        if fs::create_dir_all(&dir).await.is_err()
            || fs::write(dir.join("handler.wasm"), &body).await.is_err()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response();
        }
        tracing::info!(app = %app, bytes = body.len(), "handler published");
        return (
            StatusCode::OK,
            format!("handler live at {}/api/\n", page_url(config, &app)),
        )
            .into_response();
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
    State(state): State<AppState>,
    Path(ticket): Path<String>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    store_upload(
        &state.config,
        &state.runtime,
        &ticket,
        None,
        upload_kind(&query),
        body,
    )
    .await
}

pub(crate) async fn upload_sub(
    State(state): State<AppState>,
    Path((ticket, sub)): Path<(String, String)>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    store_upload(
        &state.config,
        &state.runtime,
        &ticket,
        Some(sub),
        upload_kind(&query),
        body,
    )
    .await
}

pub(crate) fn upload_kind(query: &UploadQuery) -> UploadKind {
    if query.migrations.is_some() {
        UploadKind::Migrations
    } else if query.manifest.is_some() {
        UploadKind::Manifest
    } else if query.source.is_some() {
        UploadKind::Source
    } else if query.handler.is_some() {
        UploadKind::Handler
    } else if query.bundle.is_some() {
        UploadKind::Bundle {
            spa: query.spa.is_some(),
        }
    } else if query.icon.is_some() {
        UploadKind::Icon
    } else {
        UploadKind::Page
    }
}

/// The same ticket, read side. An agent that can write an app can fetch what
/// it needs to change it: the project it was built from, or the page as
/// served. Scoped to the ticket's own slug, like every write is.
pub(crate) async fn download(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
    Query(query): Query<UploadQuery>,
) -> Response {
    let config = &state.config;
    let Some(slug) = ticket_slug(config, &ticket) else {
        return (
            StatusCode::UNAUTHORIZED,
            "upload ticket unknown or expired; call create_upload again\n",
        )
            .into_response();
    };
    let app = slug.split('/').next().unwrap_or(&slug).to_string();

    if query.source.is_some() {
        return match fs::read(config.data_dir.join(format!("{app}.source"))).await {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, "application/gzip".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{app}-source.tar.gz\""),
                    ),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => (
                StatusCode::NOT_FOUND,
                format!(
                    "no source stored for {app}. Whoever published it did not upload one; \
                     send yours with PUT ?source so the next session has it.\n"
                ),
            )
                .into_response(),
        };
    }

    // Without a flag, hand back the page itself — the same thing a visitor
    // would get, but reachable when the app is gated.
    match fs::read_to_string(config.data_dir.join(format!("{slug}.html"))).await {
        Ok(html) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response(),
        Err(_) => match fs::read_to_string(config.data_dir.join(format!("{slug}/index.html"))).await
        {
            Ok(html) => {
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
            }
            Err(_) => (StatusCode::NOT_FOUND, "nothing published at that slug yet\n")
                .into_response(),
        },
    }
}

/// The slug a live ticket writes to, sweeping expired ones on the way.
fn ticket_slug(config: &Config, ticket: &str) -> Option<String> {
    let now = Instant::now();
    let mut tickets = config.uploads.lock().unwrap();
    tickets.retain(|_, t| t.expires_at > now);
    tickets.get(ticket).map(|t| t.slug.clone())
}
