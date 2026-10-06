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
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
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
    /// Who may reach this app: "public", "authenticated", or "restricted"
    /// (once called "granted", still accepted).
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
    /// The project folder this app sits in, as a path like `ops/yard`.
    /// Absent means the root. Folders are a tree the platform keeps; the
    /// URL of an app does not change when it moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The account that first published it, when one was signed in. An
    /// editor may remove what it created and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
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
    /// A random part of every generated inner view's name. The scoped
    /// authorizer trusts a base-table read only when it comes through one of
    /// those names, and a person can name a CTE after anything they can
    /// guess, so the name they would have to guess is not guessable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_salt: Option<String>,
    /// Paths inside the app that accept a WebSocket, from `[[socket]]` in
    /// toolsite.toml. An upgrade anywhere else is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sockets: Vec<String>,
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

/// The access levels an app, a rule or the site default may name, as
/// toolsite stores and reports them.
pub const GATES: [&str; 3] = ["public", "authenticated", "restricted"];

/// The stored word for an access level, taking the old and the plain names
/// too: `granted` is what `restricted` was called before, and `signed-in`
/// is how a person says `authenticated`. Nothing for a word that is none.
pub fn normalise_gate(word: &str) -> Option<&'static str> {
    match word.trim().to_ascii_lowercase().as_str() {
        "public" => Some("public"),
        "authenticated" | "signed-in" | "signed_in" => Some("authenticated"),
        "restricted" | "granted" => Some("restricted"),
        _ => None,
    }
}

/// Old metas say `granted`; a meta read from disk speaks the current words,
/// so the next write stores them. An unknown word is left alone, and the
/// gate treats it as closed.
fn current_words(mut meta: PageMeta) -> PageMeta {
    if let Some(gate) = meta.gate.as_deref().and_then(normalise_gate) {
        meta.gate = Some(gate.to_string());
    }
    for rule in &mut meta.rules {
        if let Some(gate) = normalise_gate(&rule.gate) {
            rule.gate = gate.to_string();
        }
    }
    meta
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
            gate: None,
            allow_http: Vec::new(),
            rules: Vec::new(),
            project: None,
            created_by: None,
            roles: Vec::new(),
            queryable: Vec::new(),
            policies: Vec::new(),
            generated: Vec::new(),
            access_salt: None,
            sockets: Vec::new(),
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
            return current_words(serde_json::from_str(&text).unwrap_or_default());
        }
    }
    PageMeta::default()
}

pub async fn read_meta(config: &Config, slug: &str) -> PageMeta {
    let path = meta_path(config, slug).await;
    match fs::read_to_string(&path).await {
        Ok(text) => current_words(serde_json::from_str(&text).unwrap_or_default()),
        Err(_) => PageMeta::default(),
    }
}

pub async fn write_meta(config: &Config, slug: &str, meta: &PageMeta) -> std::io::Result<()> {
    let meta = &current_words(meta.clone());
    let path = meta_path(config, slug).await;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string(meta).map_err(std::io::Error::other)?;
    fs::write(&path, json).await?;
    close_if_hidden(config, slug, meta);
    Ok(())
}

/// A hidden app keeps no live connection open: retraction takes effect on
/// the sockets now, not at their next check.
fn close_if_hidden(config: &Config, slug: &str, meta: &PageMeta) {
    if meta.hidden {
        config.connections.close_app(slug.split('/').next().unwrap_or(slug));
    }
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
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    close_if_hidden(config, slug, meta);
    Ok(())
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

    generated_icon(config, slug).await
}

/// The badge an app without an icon gets, from its name. One rule for the
/// index, the browser and the favicon, so the tab and the list match.
pub(crate) async fn generated_icon(config: &Config, slug: &str) -> Icon {
    let title = match page_path(config, slug).await {
        Some(path) => page_title(&path).await,
        None => None,
    };
    Icon::Generated(badge_initials(slug, title.as_deref()), slug_hue(slug))
}

/// Two letters for a badge: the first letters of the title's first two
/// words when there is a title, else the first two letters or digits of the
/// slug's last segment. Uppercase; "?" when there is nothing to use.
pub(crate) fn badge_initials(slug: &str, title: Option<&str>) -> String {
    let from_title: String = title
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .collect();
    let initials = if from_title.is_empty() {
        slug.rsplit('/')
            .next()
            .unwrap_or(slug)
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(2)
            .collect()
    } else {
        from_title
    };
    if initials.is_empty() {
        "?".to_string()
    } else {
        initials.to_uppercase()
    }
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

// --- projects ----------------------------------------------------------------
//
// A project is a folder in a tree the platform keeps, and an app belongs to
// one. The tree is logical: an app's URL is its slug whatever folder it is
// in, so moving an app changes who may manage it, not where it lives. The
// tree is kept under `.site/`, which no slug can name.

#[derive(Debug, Clone, serde::Serialize, Deserialize, PartialEq)]
pub struct Folder {
    /// `ops`, `ops/yard`. Segments follow the slug rules.
    pub path: String,
    pub name: String,
    pub created_at: u64,
    /// Locked: only the permissions set on this project and above it apply
    /// to what is inside. Rows set inside are kept, but ignored while it is
    /// locked. Customizable (false) is the default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    /// Paths this project had before a rename or a move, newest last, so a
    /// link to the old place still arrives. An alias goes when another
    /// project takes that path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed_from: Vec<String>,
    /// General access for everything inside that does not set its own:
    /// public, authenticated or restricted. Unset follows the project above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
}

/// Aliases kept per project. Enough for a few renames in a row.
const MAX_ALIASES: usize = 5;

fn projects_path(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("projects.json")
}

pub async fn list_folders(config: &Config) -> Vec<Folder> {
    match fs::read_to_string(projects_path(config)).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// One writer at a time for the project tree. Every change reads the whole
/// file, edits it and writes it back; two at once would lose one of them,
/// and a lost lock or general access setting opens what it closed.
static FOLDERS_WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Writes the tree whole, through a temporary file and a rename, so a crash
/// leaves the old file or the new one and never half of one. A torn file
/// would read as "no projects": every lock and project setting gone.
async fn write_folders(config: &Config, folders: &[Folder]) -> std::io::Result<()> {
    let path = projects_path(config);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(folders).map_err(std::io::Error::other)?;
    let temp = path.with_extension("json.part");
    fs::write(&temp, json).await?;
    fs::rename(&temp, &path).await
}

/// The projects that are locked, for the permission rule. Read in a blocking
/// context because every scope check runs in one.
pub fn locked_prefixes_blocking(config: &Config) -> Vec<String> {
    let mut locked: Vec<String> = std::fs::read_to_string(projects_path(config))
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Folder>>(&text).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|folder| folder.locked)
        .map(|folder| folder.path)
        .collect();
    // While a project move is unfinished, its access rows may still sit at
    // the old path after the tree has moved on. A lock that moved with the
    // tree is held at the old path too until the move is done, so rows it
    // ignored stay ignored in between.
    if let Some((from, to)) = relocation_in_progress(config) {
        let extra: Vec<String> = locked
            .iter()
            .filter_map(|path| {
                if *path == to {
                    Some(from.clone())
                } else {
                    path.strip_prefix(&format!("{to}/")).map(|rest| format!("{from}/{rest}"))
                }
            })
            .collect();
        locked.extend(extra);
    }
    locked
}

/// Where the record of an unfinished project move lives.
pub fn relocation_journal(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("relocating.json")
}

/// The `(from, to)` of an unfinished project move, if one is recorded.
pub fn relocation_in_progress(config: &Config) -> Option<(String, String)> {
    let text = std::fs::read_to_string(relocation_journal(config)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((value.get("from")?.as_str()?.to_string(), value.get("to")?.as_str()?.to_string()))
}

/// Whether one project is locked.
pub async fn folder_locked(config: &Config, path: &str) -> bool {
    list_folders(config).await.iter().any(|folder| folder.path == path && folder.locked)
}

/// Locks or unlocks a project. The top level has no row and cannot be locked.
pub async fn set_locked(config: &Config, path: &str, locked: bool) -> Result<(), String> {
    let _one_writer = FOLDERS_WRITE.lock().await;
    let mut folders = list_folders(config).await;
    let Some(folder) = folders.iter_mut().find(|folder| folder.path == path) else {
        return Err(format!("there is no project '{path}'"));
    };
    folder.locked = locked;
    write_folders(config, &folders).await.map_err(|e| e.to_string())
}

/// Sets or clears a project's general access. The top level has no row; its
/// access is the site default.
pub async fn set_folder_gate(config: &Config, path: &str, gate: Option<&str>) -> Result<(), String> {
    let gate = match gate {
        Some(word) => Some(normalise_gate(word).ok_or_else(|| format!("'{word}' is not public, authenticated or restricted"))?.to_string()),
        None => None,
    };
    let _one_writer = FOLDERS_WRITE.lock().await;
    let mut folders = list_folders(config).await;
    let Some(folder) = folders.iter_mut().find(|folder| folder.path == path) else {
        return Err(format!("there is no project '{path}'"));
    };
    folder.gate = gate;
    write_folders(config, &folders).await.map_err(|e| e.to_string())
}

/// Where an app's general access comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum GateSource {
    /// A route rule in the app, by prefix.
    Rule(String),
    /// The app's own setting.
    App,
    /// A project at or above the app, by path.
    Project(String),
    /// The site default, `TOOLSITE_DEFAULT_ACCESS`.
    Site,
}

/// The general access that applies to one path of an app, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveGate {
    pub gate: String,
    pub source: GateSource,
    /// The outermost locked project the app sits in, when there is one: then
    /// the app's own setting, its route rules and any project inside the
    /// lock are ignored.
    pub locked_by: Option<String>,
}

/// The one rule for general access, used by every place that asks who may
/// open an app:
///
/// 1. A route rule for the path, then the app's own setting.
/// 2. Else the nearest project at or above the app with a setting.
/// 3. Else the site default.
///
/// Under a locked project, step 1 and every project inside the lock are
/// skipped: the locked project's setting, or what it inherits, applies.
pub async fn effective_gate(config: &Config, app: &str, within: &str) -> EffectiveGate {
    let app = app.split('/').next().unwrap_or(app);
    let meta = read_meta(config, app).await;
    let folders = list_folders(config).await;
    resolve_gate(&meta, &folders, &config.default_gate, within)
}

/// The resolution itself, for a caller that already holds the meta and the
/// tree, such as a page showing what an app would get with no setting.
pub fn resolve_gate_public(meta: &PageMeta, folders: &[Folder], default: &str, within: &str) -> EffectiveGate {
    resolve_gate(meta, folders, default, within)
}

fn resolve_gate(meta: &PageMeta, folders: &[Folder], default: &str, within: &str) -> EffectiveGate {
    let project = meta.project.clone().unwrap_or_default();
    // Outermost first: `ops`, `ops/yard`.
    let mut chain = folder_chain(&format!("{project}/x"));
    if project.is_empty() {
        chain.clear();
    }
    let node = |path: &str| folders.iter().find(|folder| folder.path == path);
    let locked_by = chain.iter().find(|path| node(path).is_some_and(|folder| folder.locked)).cloned();
    let site = || EffectiveGate { gate: normalise_gate(default).unwrap_or("public").to_string(), source: GateSource::Site, locked_by: locked_by.clone() };

    // An app that names a project the tree does not hold is between two
    // states: a move that stopped halfway, or a hand edit. Whatever locks
    // and settings belonged to that project are not visible from here, so
    // the app is closed rather than left to its own say.
    if !project.is_empty() && node(&project).is_none() {
        return EffectiveGate { gate: "restricted".to_string(), source: GateSource::Site, locked_by };
    }

    // Under a lock, nothing set inside it counts.
    let projects: Vec<&String> = match &locked_by {
        Some(lock) => chain.iter().filter(|path| path.len() <= lock.len()).collect(),
        None => {
            if let Some(rule) = meta
                .rules
                .iter()
                .filter(|rule| within.starts_with(&rule.prefix))
                .max_by_key(|rule| rule.prefix.len())
            {
                return EffectiveGate { gate: rule.gate.clone(), source: GateSource::Rule(rule.prefix.clone()), locked_by: None };
            }
            if let Some(gate) = &meta.gate {
                return EffectiveGate { gate: gate.clone(), source: GateSource::App, locked_by: None };
            }
            chain.iter().collect()
        }
    };
    for path in projects.iter().rev() {
        if let Some(gate) = node(path).and_then(|folder| folder.gate.as_deref()).and_then(normalise_gate) {
            return EffectiveGate { gate: gate.to_string(), source: GateSource::Project((*path).clone()), locked_by };
        }
    }
    site()
}

/// What a project's own apps get when they set nothing: the nearest project
/// at or above it with a setting, else the site default. For showing.
pub async fn project_gate(config: &Config, path: &str) -> (String, GateSource) {
    let folders = list_folders(config).await;
    let mut chain = folder_chain(&format!("{path}/x"));
    if path.is_empty() {
        chain.clear();
    }
    for at in chain.iter().rev() {
        if let Some(gate) = folders.iter().find(|f| &f.path == at).and_then(|f| f.gate.as_deref()).and_then(normalise_gate) {
            return (gate.to_string(), GateSource::Project(at.clone()));
        }
    }
    (normalise_gate(&config.default_gate).unwrap_or("public").to_string(), GateSource::Site)
}

/// Whether `path` is a folder. The root always is and has no row.
pub async fn folder_exists(config: &Config, path: &str) -> bool {
    path.is_empty() || list_folders(config).await.iter().any(|folder| folder.path == path)
}

/// Whether some app already sits at `path` in the tree (its project plus its
/// slug). Project paths and app paths share one namespace, because a
/// permission row is keyed by path alone: a project and an app at the same
/// path would share every row, so access given on one would open the other.
pub async fn app_at_path(config: &Config, path: &str) -> Option<String> {
    apps_with_folders(config)
        .await
        .into_iter()
        .find(|(app, folder)| {
            let at = if folder.is_empty() { app.clone() } else { format!("{folder}/{app}") };
            at == path
        })
        .map(|(app, _)| app)
}

/// Whether a project has this path. See [`app_at_path`] for why the two
/// must never meet.
pub async fn project_at_path(config: &Config, path: &str) -> bool {
    !path.is_empty() && list_folders(config).await.iter().any(|folder| folder.path == path)
}

/// Creates `parent/name`. The parent must exist, or be the root.
pub async fn create_folder(config: &Config, parent: &str, name: &str) -> Result<Folder, String> {
    if !crate::content::slug::valid_segment(name) {
        return Err("a folder name is letters, numbers, '-' or '_'".into());
    }
    if !(parent.is_empty() || crate::content::slug::valid_slug(parent)) {
        return Err("invalid parent folder".into());
    }
    let _one_writer = FOLDERS_WRITE.lock().await;
    let mut folders = list_folders(config).await;
    if !parent.is_empty() && !folders.iter().any(|folder| folder.path == parent) {
        return Err(format!("there is no folder '{parent}'"));
    }
    let path = if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") };
    if folders.iter().any(|folder| folder.path == path) {
        return Err(format!("there is already a folder '{path}'"));
    }
    if let Some(app) = app_at_path(config, &path).await {
        return Err(format!(
            "the app {app} is at {path}; a project cannot share an app's path, because access on one would open the other"
        ));
    }
    let folder = Folder {
        path,
        name: name.to_string(),
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        locked: false,
        renamed_from: Vec::new(),
        gate: None,
    };
    drop_alias(&mut folders, &folder.path);
    folders.push(folder.clone());
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    write_folders(config, &folders).await.map_err(|e| e.to_string())?;
    Ok(folder)
}

/// A path is now a real project, so no other project may still answer to it
/// as an old name.
fn drop_alias(folders: &mut [Folder], path: &str) {
    for folder in folders.iter_mut() {
        folder.renamed_from.retain(|old| old != path && !old.starts_with(&format!("{path}/")));
    }
}

/// Moves the project at `from`, and every project below it, to `to`. Only
/// the tree changes here; the caller moves what points into it first.
pub async fn relocate_folder(config: &Config, from: &str, to: &str) -> Result<(), String> {
    let _one_writer = FOLDERS_WRITE.lock().await;
    let mut folders = list_folders(config).await;
    if !folders.iter().any(|folder| folder.path == from) {
        return Err(format!("there is no project '{from}'"));
    }
    let under = format!("{from}/");
    for folder in folders.iter_mut() {
        if folder.path == from {
            folder.path = to.to_string();
            folder.name = to.rsplit('/').next().unwrap_or(to).to_string();
            folder.renamed_from.retain(|old| old != to);
            folder.renamed_from.push(from.to_string());
            if folder.renamed_from.len() > MAX_ALIASES {
                let excess = folder.renamed_from.len() - MAX_ALIASES;
                folder.renamed_from.drain(..excess);
            }
        } else if let Some(rest) = folder.path.strip_prefix(&under) {
            folder.path = format!("{to}/{rest}");
        }
    }
    // Every new path is real now; no alias elsewhere may claim it.
    let new_paths: Vec<String> = folders
        .iter()
        .filter(|folder| folder.path == to || folder.path.starts_with(&format!("{to}/")))
        .map(|folder| folder.path.clone())
        .collect();
    for path in new_paths {
        for folder in folders.iter_mut() {
            if folder.path != to {
                folder.renamed_from.retain(|old| *old != path);
            }
        }
    }
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    write_folders(config, &folders).await.map_err(|e| e.to_string())
}

/// Removes the project row at `path`. The caller has checked it is empty.
pub async fn remove_folder(config: &Config, path: &str) -> Result<(), String> {
    let _one_writer = FOLDERS_WRITE.lock().await;
    let mut folders = list_folders(config).await;
    let before = folders.len();
    folders.retain(|folder| folder.path != path);
    if folders.len() == before {
        return Err(format!("there is no project '{path}'"));
    }
    write_folders(config, &folders).await.map_err(|e| e.to_string())
}

/// Where an old project path now lives, if a project was renamed or moved
/// away from it: `ops/yard` after `ops` became `site` gives `site/yard`.
pub async fn renamed_path(config: &Config, path: &str) -> Option<String> {
    let folders = list_folders(config).await;
    if folders.iter().any(|folder| folder.path == path) {
        return None;
    }
    folders.iter().find_map(|folder| {
        folder.renamed_from.iter().rev().find_map(|old| {
            if path == old {
                Some(folder.path.clone())
            } else {
                path.strip_prefix(&format!("{old}/")).map(|rest| format!("{}/{rest}", folder.path))
            }
        })
    })
}

/// The folders directly inside `parent`.
pub async fn subfolders(config: &Config, parent: &str) -> Vec<Folder> {
    list_folders(config)
        .await
        .into_iter()
        .filter(|folder| match folder.path.rsplit_once('/') {
            Some((above, _)) => above == parent,
            None => parent.is_empty(),
        })
        .collect()
}

/// The folder chain above a path, outermost first: `ops/yard/x` gives
/// `ops`, `ops/yard`.
pub fn folder_chain(path: &str) -> Vec<String> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    (1..segments.len()).map(|n| segments[..n].join("/")).collect()
}

/// Whether anything has been published at `app` yet: its directory or a
/// single page. A new app is placed in a folder on its first publish.
pub async fn app_exists(config: &Config, app: &str) -> bool {
    fs::metadata(config.data_dir.join(app)).await.is_ok()
        || fs::metadata(config.data_dir.join(format!("{app}.html"))).await.is_ok()
}

/// The folder an app sits in, empty for the root.
pub async fn app_folder(config: &Config, app: &str) -> String {
    read_meta(config, app).await.project.unwrap_or_default()
}

/// Where an app sits in the tree: its folder path plus its slug, for
/// display and for the scope rows an admin gives on the app itself.
pub async fn logical_path(config: &Config, app: &str) -> String {
    match read_meta(config, app).await.project {
        Some(folder) if !folder.is_empty() => format!("{folder}/{app}"),
        _ => app.to_string(),
    }
}

/// Every app's slug with the folder it is in.
pub async fn apps_with_folders(config: &Config) -> Vec<(String, String)> {
    let mut slugs = Vec::new();
    collect_slugs(&config.data_dir, String::new(), &mut slugs).await;
    let mut apps: Vec<String> = slugs
        .into_iter()
        .map(|slug| slug.split('/').next().unwrap_or(&slug).to_string())
        .collect();
    apps.sort();
    apps.dedup();
    let mut out = Vec::with_capacity(apps.len());
    for app in apps {
        let folder = read_meta(config, &app).await.project.unwrap_or_default();
        out.push((app, folder));
    }
    out
}
