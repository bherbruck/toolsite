use crate::{config::Config, slug::escape_html};
use serde::Deserialize;
use std::{path::PathBuf, time::SystemTime};
use tokio::fs;

/// Enough of a page to find its <title> without reading whole artifacts.
pub(crate) const TITLE_SCAN_BYTES: u64 = 8 * 1024;

pub(crate) fn page_url(config: &Config, slug: &str) -> String {
    match &config.base_url {
        Some(base) => format!("{base}/p/{slug}"),
        None => format!("/p/{slug}"),
    }
}

/// The file backing a slug: either the page itself or, for an app root, that
/// app's index page.
pub(crate) async fn page_path(config: &Config, slug: &str) -> Option<PathBuf> {
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
pub(crate) async fn icon_path(config: &Config, slug: &str) -> Option<PathBuf> {
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
pub(crate) struct PageMeta {
    /// Shown on the site index.
    #[serde(default = "yes")]
    pub(crate) listed: bool,
    /// Soft delete: the URL 404s, but the file is untouched and unhiding
    /// brings it straight back.
    #[serde(default)]
    pub(crate) hidden: bool,
    /// Client-routed bundle: unknown paths under the app fall back to its
    /// index.html instead of 404ing.
    #[serde(default)]
    pub(crate) spa: bool,
}

pub(crate) fn yes() -> bool {
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
pub(crate) async fn meta_path(config: &Config, slug: &str) -> PathBuf {
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

pub(crate) async fn read_meta(config: &Config, slug: &str) -> PageMeta {
    let path = meta_path(config, slug).await;
    match fs::read_to_string(&path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => PageMeta::default(),
    }
}

pub(crate) async fn write_meta(config: &Config, slug: &str, meta: &PageMeta) -> std::io::Result<()> {
    let path = meta_path(config, slug).await;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string(meta).map_err(std::io::Error::other)?;
    fs::write(&path, json).await
}

/// Coarse "when did this change" for the index; exact timestamps aren't worth
/// a date-formatting dependency here.
pub(crate) fn relative_time(then: SystemTime) -> String {
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

pub(crate) async fn page_title(path: &std::path::Path) -> Option<String> {
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

pub(crate) enum Icon {
    /// An emoji or other short scrap of text, drawn inline.
    Text(String),
    /// Anything with an image URL: an uploaded file, or a data: URI.
    Src(String),
    /// Fallback: initials on a slug-derived colour.
    Generated(String, u16),
}

/// Stable per-slug hue so a page keeps the same generated colour forever.
pub(crate) fn slug_hue(slug: &str) -> u16 {
    let mut hash: u32 = 2_166_136_261;
    for b in slug.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    (hash % 360) as u16
}

pub(crate) async fn page_icon(config: &Config, slug: &str) -> Icon {
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

/// True if the page or any app above it has been hidden.
pub(crate) async fn is_hidden(config: &Config, slug: &str) -> bool {
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

pub(crate) fn collect_slugs<'a>(
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
