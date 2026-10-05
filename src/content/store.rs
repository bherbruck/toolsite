use crate::config::Config;
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
pub struct PageMeta {
    /// Shown on the site index.
    #[serde(default = "yes")]
    pub listed: bool,
    /// Soft delete: the URL 404s, but the file is untouched and unhiding
    /// brings it straight back.
    #[serde(default)]
    pub hidden: bool,
    /// Client-routed bundle: unknown paths under the app fall back to its
    /// index.html instead of 404ing.
    #[serde(default)]
    pub spa: bool,
    /// Who may reach this app: "public", "authenticated", or "granted".
    /// Absent means the site's default (`TOOLSITE_DEFAULT_ACCESS`), so an
    /// internal deployment can be gated everywhere without touching apps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// Hosts this app's handler may reach. Empty means none, which is the
    /// default: a capability nobody asked for is not granted.
    #[serde(default)]
    pub allow_http: Vec<String>,
    /// Exceptions, by path prefix. A public app with a private corner and a
    /// private app with a public front page are the same feature, so both are
    /// this. Longest matching prefix wins.
    #[serde(default)]
    pub rules: Vec<PathRule>,
    /// Roles the app's handler checks, declared in toolsite.toml so whoever
    /// grants access can pick the right word. A hint only: any role may be
    /// granted, and the platform never interprets one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Hand-written views a person may query from outside the app, read
    /// only. Declared as `[access] views = [...]` in toolsite.toml.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queryable: Vec<String>,
    /// Row-level access policies, declared as `[[access.table]]`. The
    /// platform generates a view per policy, and triggers when it may write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<Policy>,
    /// Names of the views and triggers the platform generated from the
    /// policies, so a policy that is removed takes its objects with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated: Vec<String>,
}

/// One row-level access policy: who may see, and perhaps change, which rows
/// of one table. `where_` is SQL written against the app's own tables and the
/// identity functions `current_user()`, `current_email()`, `current_role()`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
pub struct Policy {
    pub table: String,
    /// The view people query. Defaults to `my_<table>`.
    pub view: String,
    #[serde(rename = "where")]
    pub where_: String,
    /// A column filled with `current_user()` on insert when left NULL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Whether inserts, updates and deletes go through the view too.
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct PathRule {
    /// Matched against the path within the app, e.g. "/admin" or "/api/all".
    pub prefix: String,
    pub gate: String,
}

impl PageMeta {
    /// The app's own gate, or the site's default when it has not said.
    pub fn gate<'a>(&'a self, default: &'a str) -> &'a str {
        self.gate.as_deref().unwrap_or(default)
    }

    /// The gate that applies to one path within this app.
    pub fn gate_for<'a>(&'a self, path: &str, default: &'a str) -> &'a str {
        self.rules
            .iter()
            .filter(|rule| path.starts_with(&rule.prefix))
            // Longest prefix wins, so /admin/reports can be stricter than
            // /admin without ordering mattering.
            .max_by_key(|rule| rule.prefix.len())
            .map(|rule| rule.gate.as_str())
            .unwrap_or_else(|| self.gate(default))
    }
}

/// The gates an app, a rule or the site default may name.
pub const GATES: [&str; 3] = ["public", "authenticated", "granted"];

pub(crate) fn yes() -> bool {
    true
}

impl Default for PageMeta {
    fn default() -> Self {
        Self {
            listed: true,
            hidden: false,
            spa: false,
            gate: None,
            allow_http: Vec::new(),
            rules: Vec::new(),
            roles: Vec::new(),
            queryable: Vec::new(),
            policies: Vec::new(),
            generated: Vec::new(),
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

/// The same read, without an async runtime. Host functions a guest calls run
/// inside a blocking task, where awaiting is the wrong tool.
pub fn read_meta_blocking(config: &Config, slug: &str) -> PageMeta {
    for candidate in [
        config.data_dir.join(format!("{slug}.meta")),
        config.data_dir.join(format!("{slug}/index.meta")),
    ] {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            return serde_json::from_str(&text).unwrap_or_default();
        }
    }
    PageMeta::default()
}

pub async fn read_meta(config: &Config, slug: &str) -> PageMeta {
    let path = meta_path(config, slug).await;
    match fs::read_to_string(&path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => PageMeta::default(),
    }
}

pub async fn write_meta(config: &Config, slug: &str, meta: &PageMeta) -> std::io::Result<()> {
    let path = meta_path(config, slug).await;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string(meta).map_err(std::io::Error::other)?;
    fs::write(&path, json).await
}

/// `write_meta` for a blocking task, which is where migrations run. Writes
/// to the same file `read_meta_blocking` reads.
pub fn write_meta_blocking(config: &Config, slug: &str, meta: &PageMeta) -> Result<(), String> {
    let direct = config.data_dir.join(format!("{slug}.meta"));
    let inner = config.data_dir.join(format!("{slug}/index.meta"));
    let path = if direct.is_file() || !inner.is_file() { direct } else { inner };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string(meta).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
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

/// Whatever the last agent wanted the next one to know: schema, decisions,
/// what is half-finished. Kept beside the app rather than inside the bundle,
/// so it is not served to visitors and does not need a place in the build.
pub(crate) async fn notes_path(config: &Config, slug: &str) -> PathBuf {
    let direct = config.data_dir.join(format!("{slug}.notes"));
    if fs::metadata(&direct).await.is_ok() {
        return direct;
    }
    let inner = config.data_dir.join(format!("{slug}/index.notes"));
    if fs::metadata(&inner).await.is_ok() {
        return inner;
    }
    if fs::metadata(config.data_dir.join(format!("{slug}.html")))
        .await
        .is_ok()
    {
        direct
    } else {
        inner
    }
}

pub async fn read_notes(config: &Config, slug: &str) -> Option<String> {
    fs::read_to_string(notes_path(config, slug).await).await.ok()
}

pub async fn write_notes(config: &Config, slug: &str, notes: &str) -> std::io::Result<()> {
    let path = notes_path(config, slug).await;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, notes).await
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
    (!title.is_empty()).then(|| title.to_string())
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
                    return Icon::Src(text.to_string());
                }
                // Short, non-markup text is an emoji or a letter or two.
                if !text.is_empty() && !text.starts_with('<') && text.chars().count() <= 4 {
                    return Icon::Text(text.to_string());
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
            // A page of the same name may also exist; one slug, one entry.
            if !out.contains(&prefix) {
                out.push(prefix);
            }
            return;
        }
        let Ok(mut entries) = fs::read_dir(dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // .trash and .site are the platform's, not anybody's app.
            if name.starts_with('.') {
                continue;
            }
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
                    if !out.contains(&slug) {
                        out.push(slug);
                    }
                }
            }
        }
    })
}
