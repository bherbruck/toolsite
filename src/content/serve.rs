use crate::{
    config::Config,
    content::{
        slug::{valid_asset_path, valid_slug},
        store::{
            collect_slugs, icon_path, is_hidden, page_icon, page_path, page_title, read_meta,
            relative_time, Icon,
        },
    },
    runtime::wasm::{Guards, Request as WasmRequest},
    AppState,
};
use maud::{html, Markup, PreEscaped};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, Request, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};

/// Ceiling on a request body handed to a guest.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
use std::{sync::Arc, time::SystemTime};
use tokio::fs;

pub(crate) fn content_type_for(path: &str) -> &'static str {
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


/// Escaping is maud's job here, not the caller's: values arrive raw from the
/// store and get escaped on the way out, so a new field cannot silently skip
/// the step the way a hand-written `format!` could.
pub(crate) fn icon_markup(icon: &Icon) -> Markup {
    match icon {
        Icon::Text(text) => html! { span."icon"."icon-text" { (text) } },
        Icon::Src(src) => html! { span."icon" { img src=(src) alt=""; } },
        Icon::Generated(initials, hue) => html! {
            span."icon"."icon-gen" style={ "--h:" (hue) } { (initials) }
        },
    }
}

pub(crate) fn sniff_image_type(bytes: &[u8]) -> &'static str {
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

pub(crate) async fn serve_icon(State(config): State<Arc<Config>>, Path(slug): Path<String>) -> Response {
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

/// Reserved prefix. A bundle that happens to ship `api/config.json` must not
/// be able to shadow its own handler, so this wins before any file lookup.
const API_PREFIX: &str = "api";

/// Serves one request for a published page, in a fixed order:
///
/// 1. `/p/<app>/api/...` — the app's wasm handler, always.
/// 2. an exact file on disk — static, no wasm involved.
/// 3. no file but a handler exists — the handler, so it can render routes.
/// 4. no file, no handler, `spa` set — the app's index.html.
/// 5. otherwise 404.
pub(crate) async fn serve_page(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let config = &state.config;
    let uri_path = request.uri().path().to_string();
    let Some(raw) = uri_path.strip_prefix("/p/") else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let had_trailing_slash = raw.ends_with('/');
    let slug = raw.trim_end_matches('/');
    if !valid_asset_path(slug) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // A hidden page is indistinguishable from one that never existed. Hiding
    // an app takes its assets down with it.
    if is_hidden(config, slug).await {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let (app, rest) = match slug.split_once('/') {
        Some((app, rest)) => (app, rest),
        None => (slug, ""),
    };
    let is_api = rest == API_PREFIX || rest.starts_with(&format!("{API_PREFIX}/"));

    // Only the cookie scoped to *this* app speaks for the visitor here. The
    // site cookie is sent to every path on the origin, so honouring it would
    // let any published app act as the visitor against every other one.
    let visitor = crate::accounts::users::current_app_user(config, app, request.headers()).await;
    let site_token = crate::accounts::users::token_from_cookies(
        request
            .headers()
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    );
    let may_hand_off = crate::accounts::users::is_visitor_navigation(request.headers());

    if let Some(denied) = gate_check(
        &state.config,
        app,
        visitor.as_ref(),
        site_token.as_deref(),
        &uri_path,
        is_api,
        may_hand_off,
    )
    .await
    {
        return denied;
    }

    if is_api {
        return match handler_wasm(config, app).await {
            Some(wasm) => run_handler(&state, app, &wasm, request, visitor).await,
            None => (StatusCode::NOT_FOUND, "this app has no handler").into_response(),
        };
    }

    // Sidecars sit beside the files they describe, so an exact-path lookup
    // would hand them out: .meta says whether a page is hidden and which gate
    // it is behind, and .notes is written for the next agent, not the public.
    if slug.rsplit('.').next().is_some_and(|extension| {
        matches!(extension, "meta" | "notes" | "icon" | "source" | "secrets" | "jobs")
    }) {
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
        if !had_trailing_slash {
            return Redirect::permanent(&format!("/p/{slug}/")).into_response();
        }
        return Html(html).into_response();
    }

    // Nothing on disk, but the app ships code: let it answer for its own
    // routes, which is what server-rendered pages need.
    if let Some(wasm) = handler_wasm(config, app).await {
        return run_handler(&state, app, &wasm, request, visitor).await;
    }

    // Client-routed bundle: /p/app/some/route is the app's own concern, so
    // hand back its index and let the router sort it out.
    if let Some(html) = spa_fallback(config, slug).await {
        return Html(html).into_response();
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

async fn handler_wasm(config: &Config, app: &str) -> Option<Vec<u8>> {
    if !valid_slug(app) {
        return None;
    }
    fs::read(config.data_dir.join(app).join("handler.wasm"))
        .await
        .ok()
}

/// Refuses a request the app's gate does not admit.
///
/// Only an app session opens a gate. A visitor holding a site session but not
/// yet a session for this app is sent through the handoff to earn one; a
/// visitor holding neither is sent to sign in. An API call gets a status
/// instead of either redirect, since sending a fetch to an HTML form only
/// produces a confusing parse error — and because an automatic handoff on a
/// background request is precisely the hole the app cookie exists to close.
async fn gate_check(
    config: &Arc<Config>,
    app: &str,
    visitor: Option<&crate::accounts::users::User>,
    site_token: Option<&str>,
    path: &str,
    is_api: bool,
    may_hand_off: bool,
) -> Option<Response> {
    let gate = read_meta(config, app).await.gate;
    if admits(config, &gate, app, visitor).await {
        return None;
    }

    // Resolved only once the app session has already failed, so a public app
    // costs no database work at all.
    let site_user = match site_token {
        Some(token) => {
            let (config, token) = (config.clone(), token.to_string());
            tokio::task::spawn_blocking(move || {
                crate::accounts::users::site_session_user(&config, &token)
            })
            .await
            .ok()
            .flatten()
        }
        None => None,
    };
    let next = app_scoped_next(app, path);

    // Worth a trip through the handoff only if the site session would in fact
    // satisfy this gate — otherwise the answer is already no, and minting a
    // session for the app would tell it about a visitor it just refused.
    if !is_api && may_hand_off && admits(config, &gate, app, site_user.as_ref()).await {
        return Some(
            Redirect::to(&format!(
                "/auth/handoff?app={app}&next={}",
                urlencoding::encode(&next)
            ))
            .into_response(),
        );
    }

    // Signing in again cannot help someone we have already identified, by
    // either tier, so say no rather than sending them round the loop.
    let known = visitor.is_some() || site_user.is_some();
    Some(if is_api {
        let status = if known {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        (status, "not permitted").into_response()
    } else if known {
        (StatusCode::FORBIDDEN, "you do not have access to this app").into_response()
    } else {
        Redirect::to(&format!("/auth/login?next={}", urlencoding::encode(&next))).into_response()
    })
}

/// Where to send a visitor so that the app's cookie will actually be sent
/// back. A cookie scoped `/p/<app>/` is not attached to a request for
/// `/p/<app>` — the path must match up to and including that slash — so the
/// bare app root would otherwise bounce between gate and handoff forever.
/// Both forms serve the same content.
fn app_scoped_next(app: &str, path: &str) -> String {
    if path == format!("/p/{app}") {
        format!("/p/{app}/")
    } else {
        path.to_string()
    }
}

/// Whether this gate admits this visitor. `None` is an anonymous request.
async fn admits(
    config: &Arc<Config>,
    gate: &str,
    app: &str,
    visitor: Option<&crate::accounts::users::User>,
) -> bool {
    match gate {
        "public" => true,
        "authenticated" => visitor.is_some(),
        "granted" => match visitor {
            Some(user) => {
                let (config, user, app) = (config.clone(), user.clone(), app.to_string());
                tokio::task::spawn_blocking(move || {
                    crate::accounts::users::has_grant(&config, &user, &app)
                })
                .await
                .unwrap_or(false)
            }
            None => false,
        },
        // An unknown gate is treated as closed rather than open.
        _ => false,
    }
}

async fn run_handler(
    state: &AppState,
    app: &str,
    wasm: &[u8],
    request: Request<Body>,
    visitor: Option<crate::accounts::users::User>,
) -> Response {
    let method = request.method().to_string();
    let uri = request.uri().clone();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();

    let body = match axum::body::to_bytes(request.into_body(), MAX_REQUEST_BYTES).await {
        Ok(body) => body.to_vec(),
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request too large").into_response(),
    };

    // The path the guest sees is relative to its own app, so a handler never
    // has to know where it is mounted.
    let path = uri
        .path()
        .strip_prefix(&format!("/p/{app}"))
        .unwrap_or(uri.path())
        .to_string();
    let guest_request = WasmRequest {
        method,
        path: if path.is_empty() { "/".into() } else { path },
        query: uri.query().unwrap_or_default().to_string(),
        headers,
        body,
    };

    let runtime = state.runtime.clone();
    let config = state.config.clone();
    let owned_app = app.to_string();
    let wasm = wasm.to_vec();
    // The guest's identity import is fed from a session scoped to this app,
    // never from anything the request claimed and never from a session that
    // belongs to a neighbour.
    let user = visitor.map(|user| crate::runtime::wasm::User {
        id: user.id,
        email: user.email,
    });
    // Guest execution is blocking and CPU-bound, and the database import
    // blocks too, so it must not run on an async worker.
    let outcome = tokio::task::spawn_blocking(move || {
        runtime.handle(
            config,
            &owned_app,
            &wasm,
            user,
            guest_request,
            Guards::default(),
        )
    })
    .await;

    match outcome {
        Ok(Ok(response)) => {
            let status =
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut builder = axum::response::Response::builder().status(status);
            for (name, value) in &response.headers {
                builder = builder.header(name, value);
            }
            builder
                .body(Body::from(response.body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        // A trap is the app's bug, not the site's: report it without leaking
        // the host's internals to a visitor.
        Ok(Err(error)) => {
            tracing::warn!(app, error = %error, "handler failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "handler error").into_response()
        }
        Err(error) => {
            tracing::error!(app, error = %error, "handler task panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, "handler error").into_response()
        }
    }
}

pub(crate) async fn spa_fallback(config: &Config, slug: &str) -> Option<String> {
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

pub(crate) const INDEX_SEARCH_SCRIPT: &str = r#"
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

pub(crate) struct PageCard {
    pub(crate) slug: String,
    pub(crate) title: Option<String>,
    pub(crate) icon: Icon,
    pub(crate) modified: Option<SystemTime>,
}

pub(crate) async fn index(
    State(config): State<Arc<Config>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Listing an app the viewer cannot open would leak its name and title,
    // and a title is exactly the sort of thing worth gating — "Salary Review
    // 2026" says plenty on its own.
    let viewer = crate::accounts::users::current_site_user(&config, &headers).await;
    let mut slugs = Vec::new();
    collect_slugs(&config.data_dir, String::new(), &mut slugs).await;

    let mut cards = Vec::with_capacity(slugs.len());
    for slug in &slugs {
        let meta = read_meta(&config, slug).await;
        if meta.hidden || !meta.listed {
            continue;
        }
        if !admits(&config, &meta.gate, slug, viewer.as_ref()).await {
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

    let body = html! {
        h1 { "Pages" }
        p."muted" { (count_label) }

        @if cards.is_empty() {
            p."empty" { "No pages yet. Push one to see it here." }
        } @else {
            input type="search" id="q" placeholder="Filter pages…" autocomplete="off";
            ul."stack" id="list" {
                @for card in &cards {
                    li data-slug=(card.slug.to_lowercase())
                       data-title=(card.title.as_deref().unwrap_or_default().to_lowercase()) {
                        a."card" href={ "/p/" (card.slug) } {
                            (icon_markup(&card.icon))
                            span."meta" {
                                // With a title the slug becomes the subtitle;
                                // without one it is all there is to show.
                                span."title" { (card.title.as_deref().unwrap_or(&card.slug)) }
                                span."slug" {
                                    @if card.title.is_some() { (card.slug) }
                                    @if let Some(modified) = card.modified {
                                        span."when" { (relative_time(modified)) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            p."no-match" id="no-match" { "No pages match that." }
        }
    };
    let markup = crate::ui::page(
        "Pages",
        body,
        (!cards.is_empty()).then_some(INDEX_SEARCH_SCRIPT),
    );
    Html(markup.into_string())
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
