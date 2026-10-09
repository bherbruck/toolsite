use crate::{
    config::Config,
    state::tickets::Kind,
    content::{
        bundle::unpack_bundle,
        slug::valid_slug,
        store::page_url,
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
use std::time::Duration;
use tokio::fs;

/// A short-lived, single-slug write capability handed to an agent so it can
/// `curl -T file.html <url>` instead of pasting page HTML through a tool call.
/// Kept in `state::Tickets`, which holds its expiry; reusable until then.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UploadTicket {
    pub slug: String,
    /// The account that asked for it, when one was signed in. Checked again
    /// when the file arrives, so a scope revoked in between still counts.
    pub user: Option<String>,
    /// The folder a new app lands in on its first publish.
    pub project: Option<String>,
}

pub(crate) const UPLOAD_TTL: Duration = Duration::from_secs(900);

pub(crate) const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const MAX_ICON_BYTES: usize = 1024 * 1024;

/// Mints an upload ticket good for `ttl` and returns its id, the credential
/// the upload URL carries.
pub async fn issue_ticket(config: &Config, ticket: &UploadTicket, ttl: Duration) -> Result<String, String> {
    config.stores.tickets.put(Kind::Upload, ttl, ticket).await
}

/// The live upload ticket behind `id`. A store that cannot answer is logged
/// and treated as no ticket: the caller is asked to mint a fresh one.
async fn live_ticket(config: &Config, id: &str) -> Option<UploadTicket> {
    match config.stores.tickets.get(Kind::Upload, id).await {
        Ok(ticket) => ticket,
        Err(why) => {
            tracing::error!(%why, "upload ticket could not be read");
            None
        }
    }
}

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
    /// One of the app's files, stored under the key given as the value.
    pub(crate) blob: Option<String>,
    /// With `?source`: the commit message for the push to a linked
    /// repository. Also taken from the `X-Toolsite-Message` header.
    pub(crate) message: Option<String>,
    /// The commit the published build came from, so the Repo tab can say
    /// whether the live app is the repository's head. Also the
    /// `X-Toolsite-Commit` header.
    pub(crate) commit: Option<String>,
}

/// What a source upload knows about where it came from. A ticket or MCP
/// upload is the publisher's act and pushes to a linked repository; a
/// deploy-token upload comes from a pipeline and never pushes back.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SourceMeta {
    pub(crate) push: bool,
    pub(crate) message: Option<String>,
    pub(crate) commit: Option<String>,
}

impl SourceMeta {
    pub(crate) fn from_request(query: &UploadQuery, headers: &axum::http::HeaderMap, push: bool) -> Self {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let clean = |value: Option<String>| value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Self {
            push,
            message: clean(query.message.clone()).or_else(|| header("x-toolsite-message")),
            commit: clean(query.commit.clone()).or_else(|| header("x-toolsite-commit")),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum UploadKind {
    Page,
    Icon,
    Bundle { spa: bool },
    Handler,
    Source,
    Manifest,
    Migrations,
    Blob(String),
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
    meta: SourceMeta,
) -> Response {
    let Some(UploadTicket { slug, user, project }) = live_ticket(config, ticket).await else {
        return (
            StatusCode::UNAUTHORIZED,
            "upload ticket unknown or expired; call create_upload again\n",
        )
            .into_response();
    };

    let slug = match sub {
        Some(sub) => format!("{slug}/{}", sub.trim_end_matches(".html")),
        None => slug,
    };
    store_for_publisher(config, runtime, slug, kind, body, meta, user, project).await
}

/// Writes `body` as `kind` at `slug` for a publisher the platform already
/// identified: the account behind an upload ticket, or an inline upload over
/// MCP. Both paths end here, so the slug rules, the editor check at arrival,
/// the store itself and the first-publish stamp are one rule.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store_for_publisher(
    config: &Config,
    runtime: &Runtime,
    slug: String,
    kind: UploadKind,
    body: Bytes,
    meta: SourceMeta,
    user: Option<String>,
    project: Option<String>,
) -> Response {
    if !valid_slug(&slug) {
        return (
            StatusCode::BAD_REQUEST,
            "page name must be path segments of letters, numbers, '-' or '_'\n",
        )
            .into_response();
    }

    // The ticket says who asked. Their editor scope has to hold now, not
    // only when the ticket was minted.
    let app = slug.split('/').next().unwrap_or(&slug).to_string();
    if let Some(user_id) = &user {
        // A new app is judged by the folder it is about to land in.
        let folder = if crate::content::store::app_exists(config, &app).await {
            crate::content::store::app_folder(config, &app).await
        } else {
            project.clone().unwrap_or_default()
        };
        let path = if folder.is_empty() { app.clone() } else { format!("{folder}/{app}") };
        let (check, who, in_folder, which) = (config.clone_for_task(), user_id.clone(), folder, app.clone());
        let held = tokio::task::spawn_blocking(move || {
            crate::accounts::users::user_by_id(&check, &who)
                .and_then(|user| {
                    let locks = crate::content::store::locked_prefixes_blocking(&check);
                    crate::accounts::users::app_scope(&check, &user, &in_folder, &which, &locks)
                })
        })
        .await
        .ok()
        .flatten();
        if !held.is_some_and(|held| held >= crate::accounts::users::Scope::Editor) {
            tracing::warn!(app = %app, path = %path, "upload refused: the account no longer has editor access");
            return (
                StatusCode::FORBIDDEN,
                format!("the account behind this upload needs editor access at {path}\n"),
            )
                .into_response();
        }
    }

    let is_new = !crate::content::store::app_exists(config, &app).await;
    if is_new {
        forget_stale_meta(config, &app).await;
    }
    let response = store_for_slug(config, runtime, slug, kind, body, meta).await;
    if response.status().is_success() {
        if is_new {
            forget_stale_access(config, &app, project.as_deref()).await;
            hold_the_name(config, &app).await;
        }
        stamp_new_app(config, &app, user.as_deref(), project.as_deref()).await;
    }
    response
}

/// A new app starts from the default meta. One left at its name with no
/// files (a removal whose catalog step failed, on Postgres) would otherwise
/// hand the newcomer its gate, its project and its creator, and with them
/// whoever holds access there. Logged, like stale access, and not kept: an
/// app that never existed has no trash entry to keep it in.
async fn forget_stale_meta(config: &Config, app: &str) {
    let stored = crate::content::catalog::meta(config, app).await;
    let as_json = |meta: &crate::content::store::PageMeta| serde_json::to_value(meta).unwrap_or_default();
    if as_json(&stored) == as_json(&crate::content::store::PageMeta::default()) {
        return;
    }
    tracing::warn!(app, stale = %as_json(&stored), "a new app had a meta waiting at its name; it starts from the default");
    if let Err(why) = crate::content::catalog::update_meta(config, app, |meta| {
        *meta = crate::content::store::PageMeta::default();
        Ok(())
    })
    .await
    {
        tracing::warn!(app, %why, "a stale meta could not be cleared");
    }
}

/// Makes the app's directory, if its first publish wrote none, so the name
/// is taken by whatever was published first on either backend. On files a
/// manifest or a source alone made one, by writing a sidecar inside it; on
/// Postgres the meta is a row, and without this the next publisher at the
/// name, in another project, would count as new and inherit that row.
async fn hold_the_name(config: &Config, app: &str) {
    if !crate::content::store::app_exists(config, app).await
        && let Err(e) = fs::create_dir_all(config.data_dir.join(app)).await
    {
        tracing::warn!(app, error = %e, "a new app's directory could not be made");
    }
}

/// A new app starts with no one holding anything on it. Rows or grants left
/// at its path, set before it existed or by a different app that once lived
/// there, would otherwise open it to whoever holds them. They go to the log,
/// not to the bin: an app that never existed has no trash entry to keep them in.
pub(crate) async fn forget_stale_access(config: &Config, app: &str, project: Option<&str>) {
    // Nor does it inherit a socket or a resident instance a former app at
    // this name left running.
    config.connections.close_app(app);
    config.residents.stop(app);
    let folder = project.filter(|p| !p.is_empty()).unwrap_or("");
    let path = if folder.is_empty() { app.to_string() } else { format!("{folder}/{app}") };
    let (cfg, app_owned, path_owned) = (config.clone_for_task(), app.to_string(), path.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        crate::accounts::users::forget_app(&cfg, &app_owned, &path_owned)
    })
    .await;
    if let Ok(Ok(dropped)) = outcome {
        let empty = dropped.get("access").and_then(|v| v.as_array()).is_none_or(|a| a.is_empty())
            && dropped.get("grants").and_then(|v| v.as_array()).is_none_or(|a| a.is_empty());
        if !empty {
            tracing::warn!(app = %app, path = %path, dropped = %dropped, "a new app had access rows waiting at its path; they were removed");
        }
    }
}

/// Records, once, who first published an app and the folder it landed in.
/// An editor may later remove what it created and nothing else; the folder
/// decides who may manage it from here on.
pub(crate) async fn stamp_new_app(config: &Config, app: &str, user_id: Option<&str>, project: Option<&str>) {
    let meta = crate::content::catalog::meta(config, app).await;
    let user_id = user_id.filter(|_| meta.created_by.is_none()).map(str::to_string);
    let project = project.filter(|p| !p.is_empty() && meta.project.is_none()).map(str::to_string);
    if user_id.is_none() && project.is_none() {
        return;
    }
    // Checked again while held: only the first publish stamps.
    let _ = crate::content::catalog::update_meta(config, app, move |meta| {
        if meta.created_by.is_none() {
            meta.created_by = user_id;
        }
        if meta.project.is_none() {
            meta.project = project;
        }
        Ok(())
    })
    .await;
}

/// Writes `body` as `kind` at `slug`, for a caller that has already decided
/// the writer may: an upload ticket, or a deploy token for the app. The slug
/// is validated and may include a page name.
pub(crate) async fn store_for_slug(
    config: &Config,
    runtime: &Runtime,
    slug: String,
    kind: UploadKind,
    body: Bytes,
    meta: SourceMeta,
) -> Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "body is empty\n").into_response();
    }
    // Whoever publishes may say which commit this came from; the Repo tab
    // compares it with the branch head. Nothing else depends on it.
    if let Some(sha) = &meta.commit {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        if !crate::platform::github::record_deployed(config, &app, sha).await {
            tracing::info!(app = %app, "commit named on upload was not recorded: no live link, or not a sha");
        }
    }

    // A file for the app to serve or read: seed images, a dataset, a model.
    // The type comes from the key's extension, since this path carries no
    // headers; a handler storing its own can say what it likes.
    if let UploadKind::Blob(key) = kind {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        let content_type = crate::content::serve::content_type_for(&key).to_string();
        let size = body.len();
        let owned = (config.clone_for_task(), app.clone(), key.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            crate::runtime::blobs::put(&owned.0, &owned.1, &owned.2, &content_type, &body)
        })
        .await;
        return match outcome {
            Ok(Ok(())) => {
                tracing::info!(app = %app, key = %key, size, "blob stored by upload");
                (StatusCode::OK, format!("stored {size} bytes as {key} for {app}\n")).into_response()
            }
            Ok(Err(crate::runtime::blobs::Error::InvalidKey(why))) => {
                (StatusCode::BAD_REQUEST, format!("{why}\n")).into_response()
            }
            Ok(Err(crate::runtime::blobs::Error::TooLarge(_))) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "over the blob size limit\n").into_response()
            }
            Ok(Err(error)) => {
                tracing::warn!(app = %app, key = %key, %error, "blob upload failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "could not store the file\n").into_response()
            }
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not store the file\n").into_response(),
        };
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
            Ok(Ok((version, ran, notes))) => {
                tracing::info!(app = %app, version, ran, "schema migrated");
                let mut text = format!("{app}: {count} migration(s) stored, {ran} applied, now at version {version}\n");
                for note in notes {
                    text.push_str(&note);
                    text.push('\n');
                }
                (StatusCode::OK, text).into_response()
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
        return match crate::platform::manifest::apply_checked(config, runtime, &app, &text).await {
            Ok(changed) => {
                let mut reply = if changed.is_empty() {
                    format!("{app} already matches its manifest\n")
                } else {
                    tracing::info!(app = %app, changed = ?changed, "manifest applied");
                    format!("{app}: applied {}\n", changed.join(", "))
                };
                // Where people connect to what was just declared.
                if !crate::platform::app_tools::read(config, &app).await.is_empty() {
                    reply.push_str(&format!(
                        "Tools are live at {}. Add it as a connector in Claude or ChatGPT; people sign in with their toolsite account.\n",
                        crate::platform::app_tools::connector_url(config, &app)
                    ));
                }
                (StatusCode::OK, reply).into_response()
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
        let mut text = format!(
            "stored {} bytes of source for {app}; fetch it back with GET on this \
             same URL with ?source\n",
            body.len()
        );
        // A linked repository gets the same archive as a commit. Reported,
        // never fatal: the upload itself has already succeeded.
        if meta.push && let Some(link) = crate::platform::github::link(config, &app).await {
            match crate::platform::github::push_source(config, &app, &body, meta.message.as_deref()).await {
                Ok(crate::platform::github::SourcePush::Pushed(sha)) => {
                    text.push_str(&format!("pushed to {}@{} as {}\n", link.full_name(), link.branch, &sha[..sha.len().min(7)]));
                }
                Ok(crate::platform::github::SourcePush::Unchanged) => {
                    text.push_str(&format!("repository {} unchanged: the branch already has these files\n", link.full_name()));
                }
                Err(why) => {
                    tracing::warn!(app = %app, repo = %link.full_name(), %why, "source stored but not pushed");
                    text.push_str(&format!("not pushed to {}: {why}\n", link.full_name()));
                }
            }
        }
        return (StatusCode::OK, text).into_response();
    }

    if let UploadKind::Handler = kind {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        let takes_connections = match runtime.validate(&body) {
            Ok(takes) => takes,
            Err(error) => {
                tracing::warn!(app = %app, error = %error, "handler rejected");
                return (
                    StatusCode::BAD_REQUEST,
                    format!("not a valid handler component: {error}\n"),
                )
                    .into_response();
            }
        };
        // Held until the handler is written, so a manifest declaring
        // [resident] cannot be checked against the handler this replaces.
        let _declaring = config.residents.declaring().await;
        if !takes_connections && crate::content::catalog::meta(config, &app).await.resident.is_some() {
            tracing::warn!(app = %app, "handler rejected: the app runs resident and the handler takes no connections");
            return (
                StatusCode::BAD_REQUEST,
                "the app's toolsite.toml declares [resident], which needs a handler that exports on-connection \
                 (the app-with-connections or app-resident world); this one does not\n",
            )
                .into_response();
        }

        let dir = config.data_dir.join(&app);
        if fs::create_dir_all(&dir).await.is_err()
            || fs::write(dir.join("handler.wasm"), &body).await.is_err()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response();
        }
        runtime.forget(&app);
        // A resident instance runs the code that was replaced.
        config.residents.stop(&app);
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
        let (unpack_dest, unpack_slug) = (dest.clone(), slug.clone());
        let result =
            tokio::task::spawn_blocking(move || unpack_bundle(&body, &unpack_dest, &unpack_slug)).await;
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
            let _ = crate::content::catalog::update_meta(config, &slug, |meta| {
                meta.spa = true;
                Ok(())
            })
            .await;
        }

        // A single page at the same slug would shadow this app for good:
        // serving prefers <slug>.html, so the bundle would be unreachable and
        // the index would list the slug twice. Republishing a slug replaces
        // what was there, which is what this is.
        let shadow = config.data_dir.join(format!("{slug}.html"));
        let replaced_page = fs::remove_file(&shadow).await.is_ok();

        let has_index = unpacked.files.iter().any(|f| f == "index.html");
        let mut body = format!(
            "unpacked {} files to {}\n",
            unpacked.files.len(),
            page_url(config, &slug)
        );
        if replaced_page {
            body.push_str("replaced the single page that was at this slug\n");
        }
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
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = refuse_unknown_flags(&uri) {
        return response;
    }
    let meta = SourceMeta::from_request(&query, &headers, true);
    store_upload(
        &state.config,
        &state.runtime,
        &ticket,
        None,
        upload_kind(&query),
        body,
        meta,
    )
    .await
}

pub(crate) async fn upload_sub(
    State(state): State<AppState>,
    Path((ticket, sub)): Path<(String, String)>,
    Query(query): Query<UploadQuery>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = refuse_unknown_flags(&uri) {
        return response;
    }
    let meta = SourceMeta::from_request(&query, &headers, true);
    store_upload(
        &state.config,
        &state.runtime,
        &ticket,
        Some(sub),
        upload_kind(&query),
        body,
        meta,
    )
    .await
}

/// Every flag this endpoint understands. Anything else is a mistake worth
/// reporting: an unknown flag used to fall through to "publish the body as a
/// page", so probing for a flag that does not exist would overwrite the app's
/// front page with whatever was being probed.
pub(crate) const KNOWN_FLAGS: [&str; 9] = [
    "icon", "bundle", "spa", "handler", "source", "manifest", "blob", "message", "commit",
];

pub(crate) fn unknown_flags(uri: &axum::http::Uri) -> Vec<String> {
    let Some(query) = uri.query() else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split('=').next().unwrap_or_default().to_string())
        .filter(|name| !name.is_empty() && !KNOWN_FLAGS.contains(&name.as_str()) && name != "migrations")
        .collect()
}

/// A flag nobody recognises is a question, not an instruction. Answering it
/// by publishing the body as a page is how probing for `?config` replaced an
/// app's front page with a TOML file.
fn refuse_unknown_flags(uri: &axum::http::Uri) -> Option<Response> {
    let unknown = unknown_flags(uri);
    if unknown.is_empty() {
        return None;
    }
    Some(
        (
            StatusCode::BAD_REQUEST,
            format!(
                "unknown upload flag(s): {}. This endpoint understands {}, \
                 and no flag at all means publish the body as a page.\n",
                unknown.join(", "),
                [KNOWN_FLAGS.as_slice(), ["migrations"].as_slice()]
                    .concat()
                    .join(", ")
            ),
        )
            .into_response(),
    )
}

pub(crate) fn upload_kind(query: &UploadQuery) -> UploadKind {
    if let Some(key) = &query.blob {
        UploadKind::Blob(key.clone())
    } else if query.migrations.is_some() {
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
    let Some(slug) = live_ticket(config, &ticket).await.map(|t| t.slug) else {
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
