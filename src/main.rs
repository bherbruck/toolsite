use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Form, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use rand::RngExt;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities,
        ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio::fs;

struct AuthCode {
    redirect_uri: String,
    code_challenge: Option<String>,
    expires_at: Instant,
}

/// Present only when OAUTH_CLIENT_ID + OAUTH_CLIENT_SECRET are configured.
/// Mounts the OAuth discovery/authorize/token routes; absent means the server
/// only does plain bearer-token auth (for clients that support that directly).
struct OAuth {
    client_id: String,
    client_secret: String,
    auth_codes: Mutex<HashMap<String, AuthCode>>,
}

/// A short-lived, single-slug write capability handed to an agent so it can
/// `curl -T file.html <url>` instead of pasting page HTML through a tool call.
struct UploadTicket {
    slug: String,
    expires_at: Instant,
}

const UPLOAD_TTL: Duration = Duration::from_secs(900);
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_ICON_BYTES: usize = 1024 * 1024;
/// Zip-bomb guards: a bundle is a built front-end, not an archive dump.
const MAX_BUNDLE_UNPACKED: u64 = 128 * 1024 * 1024;
const MAX_BUNDLE_ENTRIES: usize = 2_000;
/// Enough of a page to find its <title> without reading whole artifacts.
const TITLE_SCAN_BYTES: u64 = 8 * 1024;

struct Config {
    data_dir: PathBuf,
    base_url: Option<String>,
    /// Stand-in for `base_url` when it isn't configured, so upload URLs handed
    /// to an agent are still something it can actually curl.
    local_base: String,
    valid_tokens: Vec<String>,
    oauth: Option<OAuth>,
    uploads: Mutex<HashMap<String, UploadTicket>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PushPageRequest {
    #[schemars(description = "Full self-contained HTML document (inline any CSS/JS).")]
    html: String,
    #[schemars(
        description = "Optional URL slug. Random one is generated if omitted. Reusing a slug overwrites that page. May contain '/' to namespace it under an app, e.g. 'myapp/about'."
    )]
    slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PullPageRequest {
    #[schemars(description = "Slug of the page to fetch the current HTML for.")]
    slug: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PushAppRequest {
    #[schemars(
        description = "App namespace all pages are published under, e.g. 'myapp'. Letters, numbers, '-' and '_' only."
    )]
    app: String,
    #[schemars(
        description = "Map of page name to full HTML document, e.g. {\"index\": \"<html>...\", \"about\": \"<html>...\"}. A page named 'index' is also served at the app's own root URL."
    )]
    pages: HashMap<String, String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateUploadRequest {
    #[schemars(
        description = "Slug the upload writes to. Random one is generated if omitted. For a multi-page app pass the app name, then upload one file per page."
    )]
    slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetIconRequest {
    #[schemars(description = "Slug of the page to set the icon for.")]
    slug: String,
    #[schemars(
        description = "An emoji, a full inline <svg>...</svg>, or a data: URI. For a raster image file, use create_upload and PUT it to <upload-url>?icon instead."
    )]
    icon: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetVisibilityRequest {
    #[schemars(description = "Slug of the page to change.")]
    slug: String,
    #[schemars(
        description = "true takes the page down: its URL 404s and it leaves the index. Nothing is deleted — set false to bring it straight back."
    )]
    hidden: Option<bool>,
    #[schemars(
        description = "false keeps the page working at its URL but removes it from the site index. Use for scratch or link-only pages."
    )]
    listed: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListPagesRequest {
    #[schemars(
        description = "Include pages that are hidden or unlisted. Defaults to false, which lists only what a visitor would see."
    )]
    include_all: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PullAppRequest {
    #[schemars(description = "App namespace to fetch all pages for.")]
    app: String,
}

#[derive(Clone)]
struct PageHost {
    config: Arc<Config>,
    #[allow(dead_code)]
    tool_router: ToolRouter<PageHost>,
}

#[tool_router]
impl PageHost {
    fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Fallback publisher for when you cannot reach this host from a shell: pastes the page's HTML through this call. If you can run shell commands with network access, use create_upload instead. Call again with the same slug to update in place. Use a slug like 'myapp/about' to group pages under one app."
    )]
    async fn push_page(
        &self,
        Parameters(PushPageRequest { html, slug }): Parameters<PushPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = slug.unwrap_or_else(random_slug);
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }

        let path = self.config.data_dir.join(format!("{slug}.html"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        fs::write(&path, html)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let url = page_url(&self.config, &slug);
        Ok(CallToolResult::success(vec![ContentBlock::text(url)]))
    }

    #[tool(description = "Fetch the current HTML source of a previously pushed page by its slug, so it can be edited and pushed back.")]
    async fn pull_page(
        &self,
        Parameters(PullPageRequest { slug }): Parameters<PullPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let path = self.config.data_dir.join(format!("{slug}.html"));
        match fs::read_to_string(&path).await {
            Ok(html) => Ok(CallToolResult::success(vec![ContentBlock::text(html)])),
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no page found for slug '{slug}'"
            ))])),
        }
    }

    #[tool(
        description = "Fallback publisher for a whole multi-page app when you cannot reach this host from a shell: pastes every page's HTML through this call. If you can run shell commands with network access, use create_upload instead. A page named 'index' is also served at the app's own root URL. Returns each page's URL."
    )]
    async fn push_app(
        &self,
        Parameters(PushAppRequest { app, pages }): Parameters<PushAppRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_segment(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty and contain only letters, numbers, '-' or '_'",
            )]));
        }
        if pages.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "pages must not be empty",
            )]));
        }
        for name in pages.keys() {
            if !valid_segment(name) {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "page name '{name}' must be non-empty and contain only letters, numbers, '-' or '_'"
                ))]));
            }
        }

        let app_dir = self.config.data_dir.join(&app);
        fs::create_dir_all(&app_dir)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut urls = Vec::new();
        for (name, html) in &pages {
            let path = app_dir.join(format!("{name}.html"));
            fs::write(&path, html)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            // 'index' is what the app root serves, so report that URL for it.
            let slug = if name == "index" {
                app.clone()
            } else {
                format!("{app}/{name}")
            };
            urls.push(format!("{name}: {}", page_url(&self.config, &slug)));
        }
        urls.sort();
        Ok(CallToolResult::success(vec![ContentBlock::text(
            urls.join("\n"),
        )]))
    }

    #[tool(
        description = "The default way to publish. Returns a short-lived upload URL; write the HTML to a local file, then PUT the file to that URL with curl. Do not paste HTML into this call — the point is that the page never passes through the conversation. Works for a single page or a whole multi-page app. If the upload URL turns out to be unreachable from your sandbox, fall back to push_page/push_app."
    )]
    async fn create_upload(
        &self,
        Parameters(CreateUploadRequest { slug }): Parameters<CreateUploadRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = slug.unwrap_or_else(random_slug);
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }

        let ticket = random_token(32);
        {
            let now = Instant::now();
            let mut tickets = self.config.uploads.lock().unwrap();
            tickets.retain(|_, t| t.expires_at > now);
            tickets.insert(
                ticket.clone(),
                UploadTicket {
                    slug: slug.clone(),
                    expires_at: now + UPLOAD_TTL,
                },
            );
        }

        let upload = upload_url(&self.config, &ticket);
        let minutes = UPLOAD_TTL.as_secs() / 60;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Upload with (expires in {minutes} min, reusable until then):\n\
             \n  curl -fT <file.html> {upload}\n\
             \nMulti-page app — append the page name, one PUT per page:\n\
             \n  curl -fT index.html {upload}/index\n  curl -fT about.html {upload}/about\n\
             \nWhole built site (React/Vite/etc), gzipped tar of the dist folder:\n\
             \n  tar -czf - -C dist . | curl -f -T - '{upload}?bundle'\n\
             \nAdd &spa for a client-side router, so unknown paths serve index.html:\n\
             \n  tar -czf - -C dist . | curl -f -T - '{upload}?bundle&spa'\n\
             \nEach upload replies with the page's public URL. Single-file page lands at {page}",
            page = page_url(&self.config, &slug),
        ))]))
    }

    #[tool(
        description = "List published pages: slug, title, URL, when each was last changed, and its visibility. Call this to find out what already exists before editing or reusing a slug."
    )]
    async fn list_pages(
        &self,
        Parameters(ListPagesRequest { include_all }): Parameters<ListPagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let include_all = include_all.unwrap_or(false);
        let mut slugs = Vec::new();
        collect_slugs(&self.config.data_dir, String::new(), &mut slugs).await;

        let mut rows = Vec::new();
        for slug in slugs {
            let meta = read_meta(&self.config, &slug).await;
            if !include_all && (meta.hidden || !meta.listed) {
                continue;
            }
            let path = page_path(&self.config, &slug).await;
            let modified = match &path {
                Some(p) => fs::metadata(p).await.ok().and_then(|m| m.modified().ok()),
                None => None,
            };
            let title = match &path {
                Some(p) => page_title(p).await,
                None => None,
            };
            rows.push(serde_json::json!({
                "slug": slug,
                "title": title,
                "url": page_url(&self.config, &slug),
                "modified": modified.map(relative_time),
                "modified_epoch": modified
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                "listed": meta.listed,
                "hidden": meta.hidden,
            }));
        }
        // Most recently touched first: that's what a follow-up edit is after.
        rows.sort_by_key(|r| std::cmp::Reverse(r["modified_epoch"].as_u64().unwrap_or(0)));

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "no pages published yet",
            )]));
        }
        let json = serde_json::to_string(&rows)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Take a page down or restore it, and control whether it appears on the site index. Nothing is ever deleted, so this is the safe way to retract a page published by mistake."
    )]
    async fn set_visibility(
        &self,
        Parameters(SetVisibilityRequest {
            slug,
            hidden,
            listed,
        }): Parameters<SetVisibilityRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        if hidden.is_none() && listed.is_none() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "pass hidden, listed, or both",
            )]));
        }
        if page_path(&self.config, &slug).await.is_none() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no page found for slug '{slug}'"
            ))]));
        }

        let mut meta = read_meta(&self.config, &slug).await;
        if let Some(hidden) = hidden {
            meta.hidden = hidden;
        }
        if let Some(listed) = listed {
            meta.listed = listed;
        }
        write_meta(&self.config, &slug, &meta)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let state = match (meta.hidden, meta.listed) {
            (true, _) => "hidden (URL returns 404; set hidden=false to restore)".to_string(),
            (false, false) => format!("live but unlisted at {}", page_url(&self.config, &slug)),
            (false, true) => format!("live and listed at {}", page_url(&self.config, &slug)),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "{slug}: {state}"
        ))]))
    }

    #[tool(
        description = "Set the icon shown next to a page on the site index: an emoji, inline SVG, or data: URI. Pages without one get a generated icon, so this is optional."
    )]
    async fn set_icon(
        &self,
        Parameters(SetIconRequest { slug, icon }): Parameters<SetIconRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let icon = icon.trim();
        if icon.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "icon must not be empty",
            )]));
        }
        if icon.len() > MAX_ICON_BYTES {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "icon must be under {} KB",
                MAX_ICON_BYTES / 1024
            ))]));
        }
        if page_path(&self.config, &slug).await.is_none() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no page found for slug '{slug}'; publish the page first"
            ))]));
        }

        // Sits beside the page file, matching however that page was resolved.
        let path = match self.config.data_dir.join(format!("{slug}.html")) {
            p if p.exists() => self.config.data_dir.join(format!("{slug}.icon")),
            _ => self.config.data_dir.join(format!("{slug}/index.icon")),
        };
        fs::write(&path, icon)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "icon set for {}",
            page_url(&self.config, &slug)
        ))]))
    }

    #[tool(
        description = "Fetch the current HTML for every page in an app namespace, keyed by page name, so the app can be edited and pushed back with push_app."
    )]
    async fn pull_app(
        &self,
        Parameters(PullAppRequest { app }): Parameters<PullAppRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_segment(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty and contain only letters, numbers, '-' or '_'",
            )]));
        }
        let app_dir = self.config.data_dir.join(&app);
        let mut pages = HashMap::new();
        if let Ok(mut entries) = fs::read_dir(&app_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("html") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(html) = fs::read_to_string(&path).await {
                            pages.insert(stem.to_string(), html);
                        }
                    }
                }
            }
        }
        if pages.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no pages found for app '{app}'"
            ))]));
        }
        let json = serde_json::to_string(&pages)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for PageHost {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_server_info(Implementation::new("page-host", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Publishes self-contained HTML pages at public URLs.\n\
                 \n\
                 How to publish, in order of preference:\n\
                 1. If you can run shell commands: write the HTML to a file, call \
                 create_upload, then `curl -fT <file> <upload-url>`. Never read the file back \
                 into the conversation — that is the whole point. Emitting a page's HTML as \
                 tool-call arguments when you could have written a file is wasteful, so treat \
                 this as the default path.\n\
                 2. If curl fails because the sandbox has no network access to this host, fall \
                 back to push_page / push_app, which take the HTML inline.\n\
                 3. If there is no shell at all, use push_page / push_app directly.\n\
                 \n\
                 A built front-end (React, Vite, Svelte — anything with a dist folder) goes up \
                 whole: `tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle'`, adding \
                 &spa if it uses a client-side router.\n\
                 \n\
                 Call list_pages to see what already exists before picking a slug or editing \
                 something. To edit, fetch the page with `curl <page-url>` into a file (or \
                 pull_page / pull_app when you have no shell), edit it, then re-upload to the \
                 same slug. set_visibility retracts a page without deleting it — nothing here \
                 destroys data. Icons are optional: set_icon takes an emoji or SVG, an image \
                 file goes to `<upload-url>?icon`, and anything without one gets a generated \
                 badge.",
            )
    }
}

fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

fn random_slug() -> String {
    random_token(8)
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.split('/').all(valid_segment)
}

/// Looser than `valid_slug` because built bundles ship names like
/// `main.4f2a1c.js`. Dots are allowed inside a segment but a segment may not
/// start with one, which rules out `..` and dotfiles in a single stroke.
fn valid_asset_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('.')
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

fn content_type_for(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

fn page_url(config: &Config, slug: &str) -> String {
    match &config.base_url {
        Some(base) => format!("{base}/p/{slug}"),
        None => format!("/p/{slug}"),
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The file backing a slug: either the page itself or, for an app root, that
/// app's index page.
async fn page_path(config: &Config, slug: &str) -> Option<PathBuf> {
    let direct = config.data_dir.join(format!("{slug}.html"));
    if fs::metadata(&direct).await.is_ok() {
        return Some(direct);
    }
    let index = config.data_dir.join(format!("{slug}/index.html"));
    fs::metadata(&index).await.ok().map(|_| index)
}

/// Icons live next to their page as `<slug>.icon`. An app root accepts either
/// spelling, since a ticket upload writes the sibling form before the app
/// directory necessarily exists.
async fn icon_path(config: &Config, slug: &str) -> Option<PathBuf> {
    let direct = config.data_dir.join(format!("{slug}.icon"));
    if fs::metadata(&direct).await.is_ok() {
        return Some(direct);
    }
    let index = config.data_dir.join(format!("{slug}/index.icon"));
    fs::metadata(&index).await.ok().map(|_| index)
}

/// Per-page state kept in a `<slug>.meta` sidecar. Absent means "a normal,
/// visible page", so nothing has to be written on the common path.
#[derive(Debug, serde::Serialize, Deserialize)]
struct PageMeta {
    /// Shown on the site index.
    #[serde(default = "yes")]
    listed: bool,
    /// Soft delete: the URL 404s, but the file is untouched and unhiding
    /// brings it straight back.
    #[serde(default)]
    hidden: bool,
    /// Client-routed bundle: unknown paths under the app fall back to its
    /// index.html instead of 404ing.
    #[serde(default)]
    spa: bool,
}

fn yes() -> bool {
    true
}

impl Default for PageMeta {
    fn default() -> Self {
        Self {
            listed: true,
            hidden: false,
            spa: false,
        }
    }
}

/// Mirrors `icon_path`: a sidecar beside the page, or inside the app dir for
/// an app root.
async fn meta_path(config: &Config, slug: &str) -> PathBuf {
    let direct = config.data_dir.join(format!("{slug}.meta"));
    if fs::metadata(&direct).await.is_ok() {
        return direct;
    }
    let inner = config.data_dir.join(format!("{slug}/index.meta"));
    if fs::metadata(&inner).await.is_ok() {
        return inner;
    }
    // Nothing written yet: put it wherever the page itself lives.
    if fs::metadata(config.data_dir.join(format!("{slug}.html")))
        .await
        .is_ok()
    {
        direct
    } else {
        inner
    }
}

async fn read_meta(config: &Config, slug: &str) -> PageMeta {
    let path = meta_path(config, slug).await;
    match fs::read_to_string(&path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => PageMeta::default(),
    }
}

async fn write_meta(config: &Config, slug: &str, meta: &PageMeta) -> std::io::Result<()> {
    let path = meta_path(config, slug).await;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string(meta).map_err(std::io::Error::other)?;
    fs::write(&path, json).await
}

/// Coarse "when did this change" for the index; exact timestamps aren't worth
/// a date-formatting dependency here.
fn relative_time(then: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(then)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / 2_592_000),
    }
}

async fn page_title(path: &std::path::Path) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let file = fs::File::open(path).await.ok()?;
    let mut head = Vec::new();
    file.take(TITLE_SCAN_BYTES).read_to_end(&mut head).await.ok()?;
    let html = String::from_utf8_lossy(&head);

    let lower = html.to_lowercase();
    let open = lower.find("<title")?;
    let text_start = lower[open..].find('>')? + open + 1;
    let text_end = lower[text_start..].find("</title>")? + text_start;
    let title = html[text_start..text_end].trim();
    (!title.is_empty()).then(|| escape_html(title))
}

enum Icon {
    /// An emoji or other short scrap of text, drawn inline.
    Text(String),
    /// Anything with an image URL: an uploaded file, or a data: URI.
    Src(String),
    /// Fallback: initials on a slug-derived colour.
    Generated(String, u16),
}

/// Stable per-slug hue so a page keeps the same generated colour forever.
fn slug_hue(slug: &str) -> u16 {
    let mut hash: u32 = 2_166_136_261;
    for b in slug.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    (hash % 360) as u16
}

async fn page_icon(config: &Config, slug: &str) -> Icon {
    if let Some(path) = icon_path(config, slug).await {
        if let Ok(bytes) = fs::read(&path).await {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                let text = text.trim();
                if text.starts_with("data:") {
                    return Icon::Src(escape_html(text));
                }
                // Short, non-markup text is an emoji or a letter or two.
                if !text.is_empty() && !text.starts_with('<') && text.chars().count() <= 4 {
                    return Icon::Text(escape_html(text));
                }
            }
            if !bytes.is_empty() {
                return Icon::Src(format!("/icon/{slug}"));
            }
        }
    }

    let initials: String = slug
        .rsplit('/')
        .next()
        .unwrap_or(slug)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(2)
        .collect();
    let initials = if initials.is_empty() {
        "?".to_string()
    } else {
        initials.to_uppercase()
    };
    Icon::Generated(initials, slug_hue(slug))
}

fn icon_html(icon: &Icon) -> String {
    match icon {
        Icon::Text(text) => format!(r#"<span class="icon icon-text">{text}</span>"#),
        Icon::Src(src) => format!(r#"<span class="icon"><img src="{src}" alt=""></span>"#),
        Icon::Generated(initials, hue) => {
            format!(r#"<span class="icon icon-gen" style="--h:{hue}">{initials}</span>"#)
        }
    }
}

fn sniff_image_type(bytes: &[u8]) -> &'static str {
    let head = &bytes[..bytes.len().min(16)];
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]);
    let trimmed = text.trim_start();
    if trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") {
        "image/svg+xml"
    } else if head.starts_with(b"\x89PNG") {
        "image/png"
    } else if head.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if head.starts_with(b"GIF8") {
        "image/gif"
    } else if head.starts_with(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if head.starts_with(b"\x00\x00\x01\x00") {
        "image/x-icon"
    } else if std::str::from_utf8(bytes).is_ok() {
        // Emoji icons are stored as plain text.
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

async fn serve_icon(State(config): State<Arc<Config>>, Path(slug): Path<String>) -> Response {
    let slug = slug.trim_end_matches('/');
    if !valid_slug(slug) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let Some(path) = icon_path(&config, slug).await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Ok(bytes) = fs::read(&path).await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let content_type = sniff_image_type(&bytes);
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        bytes,
    )
        .into_response()
}

fn upload_url(config: &Config, ticket: &str) -> String {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    format!("{base}/upload/{ticket}")
}

/// Flags on an upload URL. Presence is what counts; any value works.
/// `?icon` stores the body as the page's icon, `?bundle` unpacks it as a
/// gzipped tar of a built site, and `?spa` marks that bundle client-routed.
#[derive(Deserialize)]
struct UploadQuery {
    icon: Option<String>,
    bundle: Option<String>,
    spa: Option<String>,
}

enum UploadKind {
    Page,
    Icon,
    Bundle { spa: bool },
}

/// What to do with one archive entry.
enum EntryVerdict {
    Take(String),
    /// Directories and archive metadata: every tarball has them and nothing is
    /// lost by not writing them.
    Ignore,
    /// Dotfiles and symlinks: harmless to leave out, but reported so an upload
    /// never silently ships less than it claims.
    Skip(&'static str),
    /// Traversal and absolute paths are attacks, not build-output quirks.
    Reject(String),
}

fn classify_entry(entry: &tar::Entry<'_, impl std::io::Read>) -> EntryVerdict {
    let entry_type = entry.header().entry_type();
    if entry_type.is_dir() || entry_type.is_pax_global_extensions() || entry_type.is_gnu_longname()
    {
        return EntryVerdict::Ignore;
    }
    // Links can point anywhere on the host filesystem.
    if !entry_type.is_file() {
        return EntryVerdict::Skip("symlinks and special files");
    }
    let raw = match entry.path() {
        Ok(path) => path.to_string_lossy().replace('\\', "/"),
        Err(e) => return EntryVerdict::Reject(format!("unreadable path in bundle: {e}")),
    };
    let rel = raw.trim_start_matches("./").to_string();
    if rel.is_empty() {
        return EntryVerdict::Ignore;
    }
    if rel.starts_with('/') || rel.split('/').any(|seg| seg == ".." || seg == ".") {
        return EntryVerdict::Reject(format!("unsafe path in bundle: {rel}"));
    }
    if rel.split('/').any(|seg| seg.starts_with('.')) {
        return EntryVerdict::Skip("dotfiles");
    }
    if !valid_asset_path(&rel) {
        return EntryVerdict::Reject(format!(
            "unsupported filename in bundle: {rel} (use letters, numbers, '.', '-', '_')"
        ));
    }
    EntryVerdict::Take(rel)
}

/// Entry paths as they should land on disk, or an error naming the offender.
/// Rejects anything that could escape the destination directory.
fn bundle_entry_paths(body: &[u8]) -> Result<Vec<String>, String> {
    let decoder = flate2::read::GzDecoder::new(body);
    let mut archive = tar::Archive::new(decoder);
    let mut paths = Vec::new();
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        match classify_entry(&entry) {
            EntryVerdict::Take(rel) => paths.push(rel),
            EntryVerdict::Ignore | EntryVerdict::Skip(_) => continue,
            EntryVerdict::Reject(message) => return Err(message),
        }
    }
    if paths.is_empty() {
        return Err("bundle contains no files".to_string());
    }
    if paths.len() > MAX_BUNDLE_ENTRIES {
        return Err(format!("bundle has more than {MAX_BUNDLE_ENTRIES} files"));
    }
    Ok(paths)
}

/// `tar -czf - dist` wraps everything in `dist/`, while `tar -czf - -C dist .`
/// does not. Strip a single shared top-level directory so both work.
fn bundle_strip_prefix(paths: &[String]) -> Option<String> {
    let first = paths.first()?.split('/').next()?.to_string();
    let all_share = paths
        .iter()
        .all(|p| p.starts_with(&format!("{first}/")));
    let root_has_index = paths.iter().any(|p| p == "index.html");
    (all_share && !root_has_index).then_some(first)
}

struct Unpacked {
    files: Vec<String>,
    skipped: Vec<&'static str>,
}

fn unpack_bundle(body: &[u8], dest: &std::path::Path) -> Result<Unpacked, String> {
    let paths = bundle_entry_paths(body)?;
    let strip = bundle_strip_prefix(&paths);

    let decoder = flate2::read::GzDecoder::new(body);
    let mut archive = tar::Archive::new(decoder);
    let mut written = Vec::new();
    let mut skipped: Vec<&'static str> = Vec::new();
    let mut total: u64 = 0;

    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let rel = match classify_entry(&entry) {
            EntryVerdict::Take(rel) => rel,
            EntryVerdict::Ignore => continue,
            EntryVerdict::Skip(reason) => {
                if !skipped.contains(&reason) {
                    skipped.push(reason);
                }
                continue;
            }
            EntryVerdict::Reject(message) => return Err(message),
        };
        let rel = match &strip {
            Some(prefix) => rel
                .strip_prefix(&format!("{prefix}/"))
                .unwrap_or(&rel)
                .to_string(),
            None => rel,
        };
        if rel.is_empty() {
            continue;
        }

        total += entry.header().size().unwrap_or(0);
        if total > MAX_BUNDLE_UNPACKED {
            return Err(format!(
                "bundle exceeds {} MB unpacked",
                MAX_BUNDLE_UNPACKED / 1024 / 1024
            ));
        }

        let out = dest.join(&rel);
        // Belt and braces: the path checks above should make this impossible,
        // but never write outside the destination.
        if !out.starts_with(dest) {
            return Err(format!("path escapes the app directory: {rel}"));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
        written.push(rel);
    }

    written.sort();
    Ok(Unpacked {
        files: written,
        skipped,
    })
}

/// Ticket-authenticated write. The ticket itself is the credential, so this
/// route sits outside the bearer middleware — an agent can upload a file
/// without ever being handed the server's real token.
async fn store_upload(
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

async fn upload_root(
    State(config): State<Arc<Config>>,
    Path(ticket): Path<String>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    store_upload(&config, &ticket, None, upload_kind(&query), body).await
}

async fn upload_sub(
    State(config): State<Arc<Config>>,
    Path((ticket, sub)): Path<(String, String)>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    store_upload(&config, &ticket, Some(sub), upload_kind(&query), body).await
}

fn upload_kind(query: &UploadQuery) -> UploadKind {
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

/// Clients disagree about how to present a static token: most send
/// `Authorization: Bearer <token>`, some send `x-api-key`. Accept either —
/// it's the same secret.
fn presented_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        })
    {
        return Some(bearer.trim());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

async fn require_bearer(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let presented = presented_token(&headers);
    if presented.is_some_and(|token| config.valid_tokens.iter().any(|v| v == token)) {
        return next.run(request).await;
    }

    // A rejected client usually reports nothing more than "can't connect", so
    // say here exactly what arrived. Never the token itself — only its shape.
    let scheme = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_whitespace().next())
        .unwrap_or("<none>");
    let header_names: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
    tracing::warn!(
        method = %request.method(),
        path = %request.uri().path(),
        user_agent = %headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>"),
        auth_scheme = %scheme,
        x_api_key = headers.contains_key("x-api-key"),
        token_presented = presented.is_some(),
        token_len = presented.map(str::len).unwrap_or(0),
        headers = ?header_names,
        "401: no valid token"
    );

    let mut response = StatusCode::UNAUTHORIZED.into_response();
    // Per MCP's auth spec, point OAuth-capable clients at the metadata rather
    // than leaving them to guess.
    if let Some(base) = config.base_url.as_deref() {
        if config.oauth.is_some() {
            if let Ok(value) = format!(
                r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource""#
            )
            .parse()
            {
                response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
            }
        }
    }
    response
}

async fn serve_page(
    State(config): State<Arc<Config>>,
    Path(slug): Path<String>,
    uri: Uri,
) -> impl IntoResponse {
    let slug = slug.trim_end_matches('/');
    if !valid_asset_path(slug) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // A hidden page is indistinguishable from one that never existed. Hiding
    // an app takes its assets down with it.
    if is_hidden(&config, slug).await {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    // A file inside a bundle: styles, scripts, images, fonts.
    let asset = config.data_dir.join(slug);
    if asset.is_file() {
        if let Ok(bytes) = fs::read(&asset).await {
            return ([(header::CONTENT_TYPE, content_type_for(slug))], bytes).into_response();
        }
    }

    let direct = config.data_dir.join(format!("{slug}.html"));
    if let Ok(html) = fs::read_to_string(&direct).await {
        return Html(html).into_response();
    }
    // App root without a filename: serve that app's 'index' page. Redirect to
    // the trailing-slash form first so relative links inside the app resolve
    // against the app directory rather than one level above it.
    let index = config.data_dir.join(format!("{slug}/index.html"));
    if let Ok(html) = fs::read_to_string(&index).await {
        if !uri.path().ends_with('/') {
            return Redirect::permanent(&format!("/p/{slug}/")).into_response();
        }
        return Html(html).into_response();
    }

    // Client-routed bundle: /p/app/some/route is the app's own concern, so
    // hand back its index and let the router sort it out.
    if let Some(html) = spa_fallback(&config, slug).await {
        return Html(html).into_response();
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// True if the page or any app above it has been hidden.
async fn is_hidden(config: &Config, slug: &str) -> bool {
    let mut prefix = String::new();
    for segment in slug.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        if read_meta(config, &prefix).await.hidden {
            return true;
        }
    }
    false
}

async fn spa_fallback(config: &Config, slug: &str) -> Option<String> {
    let segments: Vec<&str> = slug.split('/').collect();
    // Nearest enclosing app wins, so nested bundles behave sensibly.
    for depth in (1..segments.len()).rev() {
        let app = segments[..depth].join("/");
        if !read_meta(config, &app).await.spa {
            continue;
        }
        let index = config.data_dir.join(format!("{app}/index.html"));
        if let Ok(html) = fs::read_to_string(&index).await {
            return Some(html);
        }
    }
    None
}

const INDEX_STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f7f7f8;
  --fg: #1a1a1a;
  --muted: #6b7280;
  --card-bg: #ffffff;
  --border: #e5e7eb;
  --accent: #4f46e5;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #111114;
    --fg: #e8e8ea;
    --muted: #9198a1;
    --card-bg: #1a1a1f;
    --border: #2a2a30;
    --accent: #818cf8;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0;
  padding: 3rem 1.5rem;
  background: var(--bg);
  color: var(--fg);
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
}
.container { max-width: 640px; margin: 0 auto; }
h1 { font-size: 1.5rem; margin: 0 0 .25rem; }
.count { color: var(--muted); font-size: .9rem; margin: 0 0 1.5rem; }
input[type="search"] {
  width: 100%;
  padding: .6rem .8rem;
  border-radius: .5rem;
  border: 1px solid var(--border);
  background: var(--card-bg);
  color: var(--fg);
  font-size: .95rem;
  margin-bottom: 1.25rem;
}
input[type="search"]:focus { outline: 2px solid var(--accent); outline-offset: 1px; }
ul.pages { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .5rem; }
ul.pages li a {
  display: flex;
  align-items: center;
  gap: .75rem;
  padding: .7rem .9rem;
  border-radius: .5rem;
  border: 1px solid var(--border);
  background: var(--card-bg);
  color: var(--fg);
  text-decoration: none;
  font-size: .95rem;
  transition: border-color .15s ease;
}
ul.pages li a:hover { border-color: var(--accent); }
ul.pages li a::after { content: "\2192"; color: var(--muted); margin-left: auto; }
.icon {
  flex: 0 0 2.25rem;
  width: 2.25rem;
  height: 2.25rem;
  border-radius: .45rem;
  display: grid;
  place-items: center;
  overflow: hidden;
  background: var(--bg);
  border: 1px solid var(--border);
}
.icon img { width: 100%; height: 100%; object-fit: contain; }
.icon-text { font-size: 1.25rem; line-height: 1; border: none; background: none; }
.icon-gen {
  background: hsl(var(--h) 55% 45%);
  border-color: transparent;
  color: #fff;
  font-size: .8rem;
  font-weight: 600;
  letter-spacing: .02em;
}
.meta { display: flex; flex-direction: column; min-width: 0; }
.meta .title { font-weight: 500; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.meta .slug { color: var(--muted); font-size: .8rem; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.meta .when { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; }
.meta .when::before { content: "\00b7"; margin-right: .35rem; }
.empty, .no-match { color: var(--muted); text-align: center; padding: 2rem 0; }
.no-match { display: none; }
"#;

const INDEX_SEARCH_SCRIPT: &str = r#"
<script>
  const input = document.getElementById('q');
  const items = Array.from(document.getElementById('list').children);
  const noMatch = document.getElementById('no-match');
  input.addEventListener('input', () => {
    const q = input.value.trim().toLowerCase();
    let visible = 0;
    items.forEach((li) => {
      const match = (li.dataset.slug + ' ' + (li.dataset.title || '')).includes(q);
      li.style.display = match ? '' : 'none';
      if (match) visible++;
    });
    noMatch.style.display = (items.length > 0 && visible === 0 && q !== '') ? 'block' : 'none';
  });
</script>
"#;

fn collect_slugs<'a>(
    dir: &'a std::path::Path,
    prefix: String,
    out: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        // A directory with an index page is an app: list its root only. Its
        // inner pages belong to the app's own navigation, not this index.
        if !prefix.is_empty() && fs::metadata(dir.join("index.html")).await.is_ok() {
            out.push(prefix);
            return;
        }
        let Ok(mut entries) = fs::read_dir(dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                let child_prefix = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                collect_slugs(&path, child_prefix, out).await;
            } else if path.extension().and_then(|e| e.to_str()) == Some("html") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let slug = if prefix.is_empty() {
                        stem.to_string()
                    } else {
                        format!("{prefix}/{stem}")
                    };
                    out.push(slug);
                }
            }
        }
    })
}

struct PageCard {
    slug: String,
    title: Option<String>,
    icon: Icon,
    modified: Option<SystemTime>,
}

async fn index(State(config): State<Arc<Config>>) -> impl IntoResponse {
    let mut slugs = Vec::new();
    collect_slugs(&config.data_dir, String::new(), &mut slugs).await;

    let mut cards = Vec::with_capacity(slugs.len());
    for slug in &slugs {
        let meta = read_meta(&config, slug).await;
        if meta.hidden || !meta.listed {
            continue;
        }
        let path = page_path(&config, slug).await;
        let title = match &path {
            Some(path) => page_title(path).await,
            None => None,
        };
        let modified = match &path {
            Some(path) => fs::metadata(path).await.ok().and_then(|m| m.modified().ok()),
            None => None,
        };
        cards.push(PageCard {
            slug: slug.clone(),
            title,
            icon: page_icon(&config, slug).await,
            modified,
        });
    }
    // Newest first — the page you just pushed should be at the top.
    cards.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.slug.cmp(&b.slug))
    });

    let count_label = match cards.len() {
        0 => "No pages yet".to_string(),
        1 => "1 page".to_string(),
        n => format!("{n} pages"),
    };

    let (body, script) = if cards.is_empty() {
        (
            r#"<p class="empty">No pages yet. Push one to see it here.</p>"#.to_string(),
            "",
        )
    } else {
        let items: String = cards
            .iter()
            .map(|card| {
                let slug = &card.slug;
                let icon = icon_html(&card.icon);
                // With a title, the slug becomes the subtitle; without one the
                // slug is all there is to show.
                let when = card
                    .modified
                    .map(|t| format!(r#" <span class="when">{}</span>"#, relative_time(t)))
                    .unwrap_or_default();
                let meta = match &card.title {
                    Some(title) => format!(
                        r#"<span class="meta"><span class="title">{title}</span><span class="slug">{slug}{when}</span></span>"#
                    ),
                    None => format!(
                        r#"<span class="meta"><span class="title">{slug}</span><span class="slug">{when}</span></span>"#
                    ),
                };
                format!(
                    r#"<li data-slug="{slug_lower}" data-title="{title_lower}"><a href="/p/{slug}">{icon}{meta}</a></li>"#,
                    slug_lower = slug.to_lowercase(),
                    title_lower = card.title.as_deref().unwrap_or_default().to_lowercase(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let body = format!(
            r#"<input type="search" id="q" placeholder="Filter pages&hellip;" autocomplete="off">
<ul class="pages" id="list">
{items}
</ul>
<p class="no-match" id="no-match">No pages match that.</p>"#
        );
        (body, INDEX_SEARCH_SCRIPT)
    };

    let style = INDEX_STYLE;
    Html(format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pages</title>
<style>{style}</style>
</head>
<body>
<div class="container">
<h1>Pages</h1>
<p class="count">{count_label}</p>
{body}
</div>
{script}
</body>
</html>"#
    ))
}

// --- Minimal OAuth 2.1 shim (only mounted when OAuth is configured) -----
//
// claude.ai's custom connector flow (when the plain header-auth option isn't
// available) expects a real OAuth authorization server: it discovers
// endpoints via well-known metadata, redirects the user's browser through
// `/authorize`, then exchanges the resulting code at `/token`. There is only
// one user here, so `/authorize` auto-approves instead of showing a login
// screen. `/token` always hands back the configured client_secret as the
// access token, which is also what `require_bearer` accepts on `/mcp` — the
// client_secret is what actually gates the exchange.

async fn oauth_protected_resource_metadata(State(config): State<Arc<Config>>) -> impl IntoResponse {
    let base = config.base_url.as_deref().expect("base_url required in OAuth mode");
    Json(serde_json::json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
    }))
}

async fn oauth_authorization_server_metadata(
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

fn redirect_uri_allowed(uri: &str) -> bool {
    uri.parse::<Uri>()
        .ok()
        .and_then(|u| u.host().map(|h| h == "claude.ai" || h.ends_with(".claude.ai")))
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: Option<String>,
    #[allow(dead_code)]
    code_challenge_method: Option<String>,
}

async fn authorize(
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
struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    code_verifier: Option<String>,
}

fn oauth_error(status: StatusCode, error: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

fn client_credentials(headers: &HeaderMap, body: &TokenRequest) -> (Option<String>, Option<String>) {
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

async fn token_endpoint(
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

fn success_token(oauth: &OAuth) -> axum::response::Response {
    Json(serde_json::json!({
        "access_token": oauth.client_secret,
        "token_type": "Bearer",
        "expires_in": 31_536_000,
        "refresh_token": oauth.client_secret,
    }))
    .into_response()
}

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

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");

    // Printed at boot so a misconfigured deploy is obvious from the logs
    // rather than only from a client's opaque "can't connect".
    tracing::info!(
        bearer_auth = bearer_token.is_some(),
        oauth_auth = oauth.is_some(),
        base_url = base_url.as_deref().unwrap_or("<unset>"),
        "auth configuration"
    );

    let oauth_enabled = oauth.is_some();
    let config = Arc::new(Config {
        data_dir,
        base_url,
        local_base: format!("http://localhost:{port}"),
        valid_tokens,
        oauth,
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
