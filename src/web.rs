use crate::{
    config::Config,
    slug::{valid_asset_path, valid_slug},
    store::{
        collect_slugs, icon_path, is_hidden, page_icon, page_path, page_title, read_meta,
        relative_time, Icon,
    },
};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode, Uri},
    response::{Html, IntoResponse, Redirect, Response},
};
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


pub(crate) fn icon_html(icon: &Icon) -> String {
    match icon {
        Icon::Text(text) => format!(r#"<span class="icon icon-text">{text}</span>"#),
        Icon::Src(src) => format!(r#"<span class="icon"><img src="{src}" alt=""></span>"#),
        Icon::Generated(initials, hue) => {
            format!(r#"<span class="icon icon-gen" style="--h:{hue}">{initials}</span>"#)
        }
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

pub(crate) async fn serve_page(
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

pub(crate) const INDEX_STYLE: &str = r#"
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

pub(crate) async fn index(State(config): State<Arc<Config>>) -> impl IntoResponse {
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
