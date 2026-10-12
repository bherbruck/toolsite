//! Moving a site's platform state between files and Postgres: `toolsite
//! migrate-to-postgres`, and `toolsite export-to-files` to come back.
//!
//! Both directions are one mapping, read through the same store traits the
//! server uses. A side is a `Config` whose backend says where it lives; a
//! `Snapshot` is everything a side holds, read through its stores and keyed
//! the same way on either backend. A copy reads both sides, writes to the
//! target only what differs (through the target's stores), reads the
//! target again and compares. `--verify-only` is the compare alone. So
//! the mapping cannot drift between directions, and a second run, or a run
//! after one that stopped part way, writes only what is still missing.
//!
//! What crosses: every account and OAuth row as stored (sessions,
//! invitations, MFA secrets sealed and recovery digests keyed exactly as
//! they were, so nobody signs in again and every token keeps working);
//! metas, notes and generations; the project tree, host labels and
//! markers; settings (still sealed), tools, migration ladders, repository
//! links, jobs and installations; export, deploy and device tokens by
//! digest; every published file; the trash; and, going to Postgres, apps'
//! files kept on the volume, into the bucket.
//!
//! What stays: each app's SQLite database is a file on the volume in both
//! modes, and is never touched. Tickets (upload links, sign-ins in flight)
//! live minutes and are not copied. A job's last fired turn on Postgres is
//! not copied back: files keep none.
//!
//! On the volume a symbolic link is never followed: a path that passes
//! through one is left out, with a warning, as if it were not there. So is
//! anything that is neither a file nor a directory (a FIFO, a device). Where
//! leaving a path out would loosen what the site enforces, the run is
//! refused instead: anything under `.site/` (accounts, projects, labels, the
//! marker that grants were adopted), and any meta a page or app takes its
//! gate from. A copy that cannot carry a gate must not carry the page.
//!
//! Nothing runs beside a copy. A database that has been copied into says
//! so in `platform.site_flags` (`state::TRANSFER_FLAG`), and no runner
//! starts on it until the copy is done; a database that already holds a
//! site is not copied into; and the source is read again after the copy,
//! and the runner registry too, so a server that wrote during the copy is
//! found before the run says it finished.

use crate::{
    config::Config,
    content::{
        catalog,
        files,
        slug::valid_slug,
        store::{Folder, PageMeta},
    },
    platform::{records, tokens},
    state::{self, rows::Row},
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// How a run goes.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// Compare both sides and write nothing.
    pub verify_only: bool,
    /// Stops the copy with an error after this many writes, as a crash
    /// would: for the tests that prove a stopped run resumes.
    pub fail_after: Option<usize>,
}

/// One category compared: how many items each side holds, and what differs.
#[derive(Debug, Clone)]
pub struct Line {
    pub name: String,
    pub here: usize,
    pub there: usize,
    /// "missing x", "different x", "extra x": what the target lacks,
    /// holds otherwise, or holds beyond the source.
    pub differences: Vec<String>,
}

/// What a run did and found.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// The names of the two sides, source first.
    pub sides: (String, String),
    pub lines: Vec<Line>,
    /// Writes made to the target.
    pub written: usize,
    /// What the person running it should know: links skipped, what stays.
    pub notes: Vec<String>,
    /// Whether the run only compared, asked to or because the target says
    /// it was copied into already.
    pub compared_only: bool,
}

impl Report {
    pub fn differences(&self) -> usize {
        self.lines.iter().map(|line| line.differences.len()).sum()
    }

    /// The table a person reads: a row per category, then what differs.
    pub fn table(&self) -> String {
        let mut out = format!("{:<28} {:>10} {:>10}  {}\n", "", self.sides.0, self.sides.1, "differences");
        for line in &self.lines {
            out.push_str(&format!("{:<28} {:>10} {:>10}  {}\n", line.name, line.here, line.there, line.differences.len()));
        }
        for line in self.lines.iter().filter(|line| !line.differences.is_empty()) {
            out.push_str(&format!("\n{}:\n", line.name));
            for difference in line.differences.iter().take(20) {
                out.push_str(&format!("  {difference}\n"));
            }
            if line.differences.len() > 20 {
                out.push_str(&format!("  ... and {} more\n", line.differences.len() - 20));
            }
        }
        out
    }
}

/// Size and SHA-256 of some bytes, as hex: how files are compared.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Digest(String);

impl Digest {
    fn of(bytes: &[u8]) -> Digest {
        Digest(format!("{}:{}", bytes.len(), data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))))
    }
}

/// A JSON record: the text to write, compared by what it means, so pretty
/// and compact spellings of one record are the same record.
#[derive(Debug, Clone)]
struct Doc {
    text: String,
    value: serde_json::Value,
}

impl Doc {
    fn new(text: String) -> Doc {
        let value = serde_json::from_str(&text).unwrap_or_else(|_| serde_json::Value::String(text.clone()));
        Doc { text, value }
    }
}

impl PartialEq for Doc {
    fn eq(&self, other: &Doc) -> bool {
        self.value == other.value
    }
}

/// A file in the trash: where it is read from on its side, and what it holds.
#[derive(Debug, Clone)]
struct Kept {
    from: String,
    digest: Digest,
}

impl PartialEq for Kept {
    fn eq(&self, other: &Kept) -> bool {
        self.digest == other.digest
    }
}

/// An app's file on the volume, going to the bucket.
#[derive(Debug, Clone, PartialEq)]
struct Blob {
    content_type: String,
    digest: Digest,
}

/// Everything one side holds, keyed the same on either backend.
#[derive(Default)]
struct Snapshot {
    /// Per table of each store, rows by their key.
    accounts: Vec<BTreeMap<String, Row>>,
    oauth: Vec<BTreeMap<String, Row>>,
    /// Slugs with a meta other than the default or notes.
    pages: BTreeMap<String, (serde_json::Value, Option<String>)>,
    folders: Option<Vec<Folder>>,
    labels: BTreeMap<String, String>,
    flags: BTreeMap<String, String>,
    settings: BTreeMap<String, String>,
    tools: BTreeMap<String, Doc>,
    migrations: BTreeMap<String, Doc>,
    links: BTreeMap<String, Doc>,
    installations: BTreeMap<String, Doc>,
    jobs: BTreeMap<String, Doc>,
    tokens: BTreeMap<String, (tokens::Token, String)>,
    apps: BTreeSet<String>,
    content: BTreeMap<String, Digest>,
    trash: BTreeMap<String, Kept>,
    blobs: BTreeMap<String, Blob>,
}

/// A two-part key, `a` then `b`, unambiguous since neither part can hold
/// a NUL.
fn pair(a: &str, b: &str) -> String {
    format!("{a}\u{0}{b}")
}

fn split(key: &str) -> (&str, &str) {
    key.split_once('\u{0}').unwrap_or((key, ""))
}

fn show(key: &str) -> String {
    key.replace('\u{0}', " ")
}

/// The links under a data directory, found once by a walk that follows
/// none, so every later read can ask whether its path passes through one.
/// Anything that is neither a file nor a directory counts as one: a FIFO
/// read would hang, a device would answer with whatever it holds.
struct Links {
    found: Vec<String>,
}

impl Links {
    fn none() -> Links {
        Links { found: Vec::new() }
    }

    fn scan(root: &Path) -> Links {
        let mut found = Vec::new();
        let mut dirs = vec![String::new()];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(root.join(&dir)) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
                // Scratch is never read.
                if rel == ".tmp" {
                    continue;
                }
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_dir() {
                    dirs.push(rel);
                } else if !kind.is_file() {
                    found.push(rel);
                }
            }
        }
        found.sort();
        Links { found }
    }

    /// Whether `rel` is a link or lies under one.
    fn crosses(&self, rel: &str) -> bool {
        self.found.iter().any(|link| rel == link || rel.starts_with(&format!("{link}/")))
    }

    /// The links that cannot be left out without loosening the site: under
    /// `.site/`; a meta anywhere, the trash's included, and an app the trash
    /// keeps; and what a published page or app would read its meta through.
    /// Left out, each would come back as a default: an account store with
    /// no accounts, grants adopted again, a gated app public.
    fn governing(&self, published: &Snapshot) -> Vec<String> {
        let mut slugs = BTreeSet::new();
        for key in published.content.keys() {
            slugs.insert(files::app_of(key).to_string());
            for suffix in ["/index.html", ".html"] {
                if let Some(slug) = key.strip_suffix(suffix) {
                    slugs.insert(slug.to_string());
                }
            }
        }
        slugs.extend(published.apps.iter().cloned());
        slugs.extend(published.pages.keys().cloned());
        let mut out: Vec<String> = self
            .found
            .iter()
            .filter(|link| {
                let trashed_app = link.strip_prefix(".trash/").and_then(|rest| rest.split_once('/')).is_some_and(|(_, name)| name == "app");
                *link == ".site" || link.starts_with(".site/") || link.ends_with(".meta") || trashed_app
            })
            .cloned()
            .collect();
        for slug in slugs {
            for meta in [format!("{slug}.meta"), format!("{slug}/index.meta")] {
                if self.crosses(&meta) && !out.contains(&meta) {
                    out.push(meta);
                }
            }
        }
        out.sort();
        out
    }
}

/// Whether a published file is one the bucket keeps: anything a key can
/// name but the records and catalog sidecars (rows on Postgres) and an
/// app's SQLite database (on the volume in both modes).
fn is_content(key: &str) -> bool {
    if !files::valid_key(key) {
        return false;
    }
    let name = key.rsplit('/').next().unwrap_or(key);
    let record = name.rsplit_once('.').is_some_and(|(_, extension)| {
        crate::content::store::SIDECAR_EXTENSIONS.contains(&extension) && !["icon", "source"].contains(&extension)
    });
    let database = key.split('/').count() == 2 && (name == "data.db" || name.starts_with("data.db-"));
    !record && !database
}

/// A trash name as Postgres keeps it: a meta or notes that was a file
/// inside the removed app is kept beside it, named as a removal on
/// Postgres names it (`slug.meta`, `slug-page.meta`), since a restore reads
/// it there; an app's database stays on the volume; the rest as it is.
fn trash_name(name: &str) -> Option<String> {
    let Some(inside) = name.strip_prefix("app/") else { return Some(name.to_string()) };
    if !inside.contains('/') && (inside == "data.db" || inside.starts_with("data.db-")) {
        return None;
    }
    for extension in ["meta", "notes"] {
        if let Some(stem) = inside.strip_suffix(&format!(".{extension}")) {
            let page = stem.strip_suffix("index").map(|rest| rest.trim_end_matches('/')).unwrap_or(stem);
            return Some(if page.is_empty() {
                format!("slug.{extension}")
            } else {
                format!("slug-{}.{extension}", page.replace('/', "-"))
            });
        }
    }
    Some(name.to_string())
}

fn wait<F: std::future::Future>(future: F) -> F::Output {
    state::wait(future)
}

/// Reads everything a side holds. `links` is the volume's, or none for
/// Postgres; with `blobs`, apps' own files on the volume are read too, to
/// go to the bucket. Without `content`, published files are listed but not
/// read: for asking whether the platform's state moved.
fn read(config: &Config, links: &Links, blobs: bool, content: bool, notes: &mut Vec<String>) -> Result<Snapshot, String> {
    let on_files = !config.stores.is_postgres();
    let mut snapshot = Snapshot::default();
    let skip = |what: &str, notes: &mut Vec<String>| {
        tracing::warn!(path = what, "a symbolic link or special file is not followed; what it points at is not copied");
        notes.push(format!("skipped {what}: a symbolic link or special file, never followed"));
    };
    let site = |name: &str| format!(".site/{name}");

    // Accounts and OAuth: a file-mode site that never made the file has no
    // rows, and reading must not make one.
    let has = |name: &str| !on_files || config.data_dir.join(".site").join(name).is_file();
    if on_files && links.crosses(&site("auth.db")) {
        skip(&site("auth.db"), notes);
        snapshot.accounts = crate::accounts::store::TABLES.iter().map(|_| BTreeMap::new()).collect();
    } else {
        let store = crate::accounts::store::of(config);
        for table in crate::accounts::store::TABLES {
            let rows = if has("auth.db") { store.rows(table)? } else { Vec::new() };
            snapshot.accounts.push(keyed(table, rows));
        }
    }
    if on_files && links.crosses(&site("oauth.db")) {
        skip(&site("oauth.db"), notes);
        snapshot.oauth = crate::platform::oauth_store::TABLES.iter().map(|_| BTreeMap::new()).collect();
    } else {
        let store = crate::platform::oauth_store::of(config);
        for table in crate::platform::oauth_store::TABLES {
            let rows = if has("oauth.db") { wait(store.rows(table))? } else { Vec::new() };
            snapshot.oauth.push(keyed(table, rows));
        }
    }

    // The catalog.
    let catalog = catalog::of(config);
    let default_meta = serde_json::to_value(PageMeta::default()).map_err(|e| e.to_string())?;
    for slug in wait(catalog.pages())? {
        let sidecars = [
            format!("{slug}.meta"),
            format!("{slug}/index.meta"),
            format!("{slug}.notes"),
            format!("{slug}/index.notes"),
        ];
        if let Some(linked) = sidecars.iter().find(|path| on_files && links.crosses(path)) {
            skip(linked, notes);
            continue;
        }
        let meta = serde_json::to_value(wait(catalog.meta(&slug))?).map_err(|e| e.to_string())?;
        let page_notes = wait(catalog.notes(&slug))?;
        if meta != default_meta || page_notes.is_some() {
            snapshot.pages.insert(slug, (meta, page_notes));
        }
    }
    if on_files && links.crosses(&site("projects.json")) {
        skip(&site("projects.json"), notes);
    } else {
        snapshot.folders = Some(wait(catalog.folders())?);
    }
    if on_files && links.crosses(&site("labels.json")) {
        skip(&site("labels.json"), notes);
    } else {
        snapshot.labels = wait(catalog.labels())?;
    }
    for flag in [crate::platform::permissions::GRANTS_ADOPTED] {
        if on_files && links.crosses(&site(flag)) {
            skip(&site(flag), notes);
        } else if let Some(value) = wait(catalog.flag(flag))? {
            snapshot.flags.insert(flag.to_string(), value);
        }
    }

    // Records.
    let records = records::of(config);
    for app in wait(records.apps())? {
        let linked = |extension: &str| on_files && links.crosses(&format!("{app}.{extension}"));
        for extension in records::EXTENSIONS {
            if linked(extension) {
                skip(&format!("{app}.{extension}"), notes);
            }
        }
        if !linked("secrets") {
            for (name, sealed) in wait(records.settings(&app))? {
                snapshot.settings.insert(pair(&app, &name), sealed);
            }
        }
        if !linked("tools")
            && let Some(text) = wait(records.tools(&app))?
        {
            snapshot.tools.insert(app.clone(), Doc::new(text));
        }
        if !linked("migrations")
            && let Some(text) = records.migrations_blocking(&app)?
        {
            snapshot.migrations.insert(app.clone(), Doc::new(text));
        }
        if !linked("repo")
            && let Some(text) = wait(records.repo_link(&app))?
        {
            snapshot.links.insert(app.clone(), Doc::new(text));
        }
        if !linked("jobs") {
            for (name, text) in wait(records.jobs(&app))? {
                snapshot.jobs.insert(pair(&app, &name), Doc::new(text));
            }
        }
    }
    if on_files && links.crosses(&site("github.json")) {
        skip(&site("github.json"), notes);
    } else if let Some(text) = wait(records.installations())? {
        snapshot.installations.insert("installations".into(), Doc::new(text));
    }

    // Tokens.
    let kept = tokens::of(config);
    for kind in tokens::Kind::ALL {
        for (app, token, hash) in wait(kept.dump(kind))? {
            if on_files && links.crosses(&format!("{app}.{}", kind.extension())) {
                continue;
            }
            snapshot.tokens.insert(format!("{app}\u{0}{}\u{0}{}", kind.name(), token.id), (token, hash));
        }
    }

    // Published files, and which apps exist.
    let store = files::of(config);
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for entry in wait(store.list(&dir))? {
            let key = if dir.is_empty() { entry.name.clone() } else { format!("{dir}/{}", entry.name) };
            if on_files && links.crosses(&key) {
                skip(&key, notes);
                continue;
            }
            if !files::valid_key(&key) {
                continue;
            }
            if dir.is_empty() && on_files && (entry.dir || key.ends_with(".html")) {
                snapshot.apps.insert(files::app_of(&key).to_string());
            }
            if entry.dir {
                dirs.push(key);
            } else if is_content(&key) {
                let digest = if content {
                    Digest::of(&wait(store.fetch(&key))?.ok_or_else(|| format!("{key} went while it was read"))?)
                } else {
                    Digest(String::new())
                };
                snapshot.content.insert(key, digest);
            }
        }
    }
    if !on_files {
        let mut names: BTreeSet<String> = snapshot.content.keys().map(|key| files::app_of(key).to_string()).collect();
        names.extend(wait(catalog.pages())?.iter().map(|slug| files::app_of(slug).to_string()));
        for app in names {
            if wait(catalog.generation(&app))? > 0 {
                snapshot.apps.insert(app);
            }
        }
    }

    // The trash.
    if on_files && links.crosses(".trash") {
        skip(".trash", notes);
    } else {
        for entry in store.trash_entries_blocking()? {
            if on_files && links.crosses(&format!(".trash/{entry}")) {
                skip(&format!(".trash/{entry}"), notes);
                continue;
            }
            for name in store.trash_names_blocking(&entry)? {
                let Some(kept) = (if on_files { trash_name(&name) } else { Some(name.clone()) }) else { continue };
                let key = pair(&entry, &kept);
                // A meta kept beside the app wins over one found inside it.
                if snapshot.trash.contains_key(&key) && kept != name {
                    continue;
                }
                let bytes = store.trash_read_blocking(&entry, &name)?.ok_or_else(|| format!("{entry}/{name} went while it was read"))?;
                snapshot.trash.insert(key, Kept { from: name, digest: Digest::of(&bytes) });
            }
        }
    }

    if blobs {
        snapshot.blobs = volume_blobs(&config.data_dir, links, notes)?;
    }
    Ok(snapshot)
}

fn keyed(table: &state::rows::Table, rows: Vec<Row>) -> BTreeMap<String, Row> {
    let at: Vec<usize> = if table.key.is_empty() {
        (0..table.columns.len()).collect()
    } else {
        table.key.iter().map(|key| table.columns.iter().position(|(name, _)| name == key).expect("a key is a column")).collect()
    };
    rows.into_iter()
        .map(|row| {
            let key: Vec<&state::rows::Cell> = at.iter().map(|n| &row[*n]).collect();
            (serde_json::to_string(&key).unwrap_or_default(), row)
        })
        .collect()
}

/// Apps' own files on the volume, `<app>/.blobs/data/<key>`, with the type
/// kept beside each.
fn volume_blobs(root: &Path, links: &Links, notes: &mut Vec<String>) -> Result<BTreeMap<String, Blob>, String> {
    let mut out = BTreeMap::new();
    let Ok(apps) = std::fs::read_dir(root) else { return Ok(out) };
    for app in apps.flatten() {
        let name = app.file_name().to_string_lossy().into_owned();
        if !app.file_type().is_ok_and(|kind| kind.is_dir()) || !tokens::valid_app(&name) {
            continue;
        }
        let base = format!("{name}/.blobs/data");
        if links.crosses(&base) {
            notes.push(format!("skipped {base}: a symbolic link, never followed"));
            continue;
        }
        let mut dirs = vec![String::new()];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(root.join(&base).join(&dir)) else { continue };
            for entry in entries.flatten() {
                let file = entry.file_name().to_string_lossy().into_owned();
                let key = if dir.is_empty() { file } else { format!("{dir}/{file}") };
                let Ok(kind) = entry.file_type() else { continue };
                if !kind.is_dir() && !kind.is_file() {
                    notes.push(format!("skipped {base}/{key}: a symbolic link or special file, never followed"));
                    continue;
                }
                if crate::runtime::blobs::valid_key(&key).is_err() {
                    continue;
                }
                if kind.is_dir() {
                    dirs.push(key);
                } else if kind.is_file() {
                    let bytes = std::fs::read(entry.path()).map_err(|e| format!("{base}/{key} could not be read: {e}"))?;
                    let meta = format!("{name}/.blobs/meta/{key}");
                    let content_type = (!links.crosses(&meta))
                        .then(|| std::fs::read_to_string(root.join(&meta)).ok())
                        .flatten()
                        .map(|text| crate::runtime::blobs::clean_content_type(&text))
                        .unwrap_or_else(|| "application/octet-stream".into());
                    out.insert(pair(&name, &key), Blob { content_type, digest: Digest::of(&bytes) });
                }
            }
        }
    }
    Ok(out)
}

/// The bucket's copy of each of `wanted`, for comparing: an app's files
/// already in the bucket for other reasons are not this run's business.
fn bucket_blobs(config: &Config, wanted: &BTreeMap<String, Blob>) -> Result<BTreeMap<String, Blob>, String> {
    let crate::runtime::blobs::Backend::S3(s3) = &config.blobs.backend else { return Ok(BTreeMap::new()) };
    let mut out = BTreeMap::new();
    for key in wanted.keys() {
        let (app, name) = split(key);
        let Some(entry) = crate::runtime::blobs::stat(config, app, name).map_err(|e| e.to_string())? else { continue };
        let Some(bytes) = wait(s3.object_read(&format!("{app}/{name}")))? else { continue };
        out.insert(key.clone(), Blob { content_type: entry.content_type, digest: Digest::of(&bytes) });
    }
    Ok(out)
}

fn compare<V: PartialEq>(name: &str, here: &BTreeMap<String, V>, there: &BTreeMap<String, V>, extras: bool) -> Line {
    let mut differences = Vec::new();
    for (key, value) in here {
        match there.get(key) {
            None => differences.push(format!("missing {}", show(key))),
            Some(other) if other != value => differences.push(format!("different {}", show(key))),
            Some(_) => {}
        }
    }
    if extras {
        differences.extend(there.keys().filter(|key| !here.contains_key(*key)).map(|key| format!("extra {}", show(key))));
    }
    Line { name: name.to_string(), here: here.len(), there: there.len(), differences }
}

fn compare_all(here: &Snapshot, there: &Snapshot, with_blobs: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    for (n, table) in crate::accounts::store::TABLES.iter().enumerate() {
        lines.push(compare(&format!("accounts.{}", table.name), &here.accounts[n], &there.accounts[n], true));
    }
    for (n, table) in crate::platform::oauth_store::TABLES.iter().enumerate() {
        lines.push(compare(&format!("oauth.{}", table.name), &here.oauth[n], &there.oauth[n], true));
    }
    let folders = |snapshot: &Snapshot| -> BTreeMap<String, Folder> {
        snapshot.folders.iter().flatten().map(|folder| (folder.path.clone(), folder.clone())).collect()
    };
    let set = |names: &BTreeSet<String>| -> BTreeMap<String, ()> { names.iter().map(|name| (name.clone(), ())).collect() };
    lines.push(compare("pages (meta, notes)", &here.pages, &there.pages, true));
    lines.push(compare("projects", &folders(here), &folders(there), true));
    lines.push(compare("host labels", &here.labels, &there.labels, true));
    lines.push(compare("markers", &here.flags, &there.flags, true));
    lines.push(compare("settings", &here.settings, &there.settings, true));
    lines.push(compare("tools", &here.tools, &there.tools, true));
    lines.push(compare("migration ladders", &here.migrations, &there.migrations, true));
    lines.push(compare("repository links", &here.links, &there.links, true));
    lines.push(compare("github installations", &here.installations, &there.installations, true));
    lines.push(compare("jobs", &here.jobs, &there.jobs, true));
    let tokens = |snapshot: &Snapshot| -> BTreeMap<String, String> {
        snapshot
            .tokens
            .iter()
            .map(|(key, (token, hash))| (key.clone(), serde_json::json!([token, hash]).to_string()))
            .collect()
    };
    lines.push(compare("tokens", &tokens(here), &tokens(there), true));
    lines.push(compare("apps", &set(&here.apps), &set(&there.apps), true));
    lines.push(compare("published files", &here.content, &there.content, true));
    lines.push(compare("trash", &here.trash, &there.trash, true));
    if with_blobs {
        lines.push(compare("apps' files (to the bucket)", &here.blobs, &there.blobs, false));
    }
    lines
}

/// Counts writes, and stops at the injected failure.
struct Budget {
    written: usize,
    fail_after: Option<usize>,
}

impl Budget {
    fn spend(&mut self) -> Result<(), String> {
        if self.fail_after.is_some_and(|most| self.written >= most) {
            return Err(format!("stopped after {} writes, as asked", self.written));
        }
        self.written += 1;
        Ok(())
    }
}

fn differs<V: PartialEq>(theirs: &BTreeMap<String, V>, key: &str, value: &V) -> bool {
    theirs.get(key) != Some(value)
}

/// Writes to `to` everything `here` holds that `there` (what `to` holds
/// now) lacks or holds otherwise. Never removes anything from `to`.
fn copy(from: &Config, here: &Snapshot, to: &Config, there: &Snapshot, budget: &mut Budget) -> Result<(), String> {
    const BATCH: usize = 500;
    let accounts = crate::accounts::store::of(to);
    for (n, table) in crate::accounts::store::TABLES.iter().enumerate() {
        let rows: Vec<Row> = here.accounts[n].iter().filter(|(key, row)| differs(&there.accounts[n], key, row)).map(|(_, row)| row.clone()).collect();
        for batch in rows.chunks(BATCH) {
            budget.spend()?;
            accounts.put_rows(table, batch)?;
        }
    }
    let oauth = crate::platform::oauth_store::of(to);
    for (n, table) in crate::platform::oauth_store::TABLES.iter().enumerate() {
        let rows: Vec<Row> = here.oauth[n].iter().filter(|(key, row)| differs(&there.oauth[n], key, row)).map(|(_, row)| row.clone()).collect();
        for batch in rows.chunks(BATCH) {
            budget.spend()?;
            wait(oauth.put_rows(table, batch.to_vec()))?;
        }
    }

    // Files before the catalog: on files a meta goes beside its page or
    // inside its app, whichever is there.
    let (source, target) = (files::of(from), files::of(to));
    let mut touched = BTreeSet::new();
    for (key, digest) in &here.content {
        if !differs(&there.content, key, digest) {
            continue;
        }
        let bytes = wait(source.fetch(key))?.ok_or_else(|| format!("{key} went while it was copied"))?;
        if Digest::of(&bytes) != *digest {
            return Err(format!("{key} changed while it was copied: is the server stopped?"));
        }
        budget.spend()?;
        wait(target.put(key, bytes))?;
        touched.insert(files::app_of(key).to_string());
    }
    let generations = to.stores.is_postgres();
    let catalog = catalog::of(to);
    for app in &here.apps {
        let missing = !there.apps.contains(app);
        if generations && (missing || touched.contains(app)) {
            // Moved on, so no runner's cache serves what was there before.
            budget.spend()?;
            wait(catalog.bump_generation(app))?;
        } else if !generations && missing {
            budget.spend()?;
            wait(target.hold_name(app))?;
        }
    }

    if let Some(folders) = &here.folders
        && there.folders.as_ref() != Some(folders)
    {
        budget.spend()?;
        let folders = folders.clone();
        wait(catalog.update_folders(Box::new(move |tree| {
            *tree = folders;
            Ok(())
        })))?;
    }
    for (label, app) in &here.labels {
        if differs(&there.labels, label, app) {
            budget.spend()?;
            let label = label.clone();
            catalog.assign_label_blocking(app, true, Box::new(move |_| label))?;
        }
    }
    for (name, value) in &here.flags {
        if differs(&there.flags, name, value) {
            budget.spend()?;
            wait(catalog.set_flag(name, value))?;
        }
    }
    for (slug, (meta, notes)) in &here.pages {
        let theirs = there.pages.get(slug);
        if theirs.map(|(meta, _)| meta) != Some(meta) {
            let meta: PageMeta = serde_json::from_value(meta.clone()).map_err(|e| format!("{slug}'s meta: {e}"))?;
            budget.spend()?;
            wait(catalog.update_meta(
                slug,
                Box::new(move |stored| {
                    *stored = meta;
                    Ok(())
                }),
            ))?;
        }
        if let Some(notes) = notes
            && theirs.and_then(|(_, notes)| notes.as_ref()) != Some(notes)
        {
            budget.spend()?;
            wait(catalog.set_notes(slug, notes))?;
        }
    }

    let records = records::of(to);
    for (key, sealed) in &here.settings {
        if differs(&there.settings, key, sealed) {
            let (app, name) = split(key);
            budget.spend()?;
            wait(records.set_setting(app, name, Some(sealed)))?;
        }
    }
    for (app, doc) in &here.tools {
        if differs(&there.tools, app, doc) {
            budget.spend()?;
            wait(records.set_tools(app, Some(&doc.text)))?;
        }
    }
    for (app, doc) in &here.migrations {
        if differs(&there.migrations, app, doc) {
            budget.spend()?;
            records.set_migrations_blocking(app, &doc.text)?;
        }
    }
    for (app, doc) in &here.links {
        if differs(&there.links, app, doc) {
            budget.spend()?;
            let text = doc.text.clone();
            wait(records.update_repo_link(app, Box::new(move |_| Ok(text))))?;
        }
    }
    for (key, doc) in &here.installations {
        if differs(&there.installations, key, doc) {
            budget.spend()?;
            wait(records.set_installations(&doc.text))?;
        }
    }
    for (key, doc) in &here.jobs {
        if differs(&there.jobs, key, doc) {
            let (app, name) = split(key);
            budget.spend()?;
            wait(records.set_job(app, name, &doc.text, usize::MAX))?.map_err(|count| format!("{app} has {count} jobs"))?;
        }
    }

    let kept = tokens::of(to);
    for (key, (token, hash)) in &here.tokens {
        if there.tokens.get(key).is_none_or(|(theirs, digest)| theirs != token || digest != hash) {
            let mut parts = key.split('\u{0}');
            let (app, kind) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
            let kind = tokens::Kind::ALL.into_iter().find(|k| k.name() == kind).ok_or("a token of no kind")?;
            budget.spend()?;
            wait(kept.restore(app, kind, token, hash))?;
        }
    }

    for (key, kept) in &here.trash {
        if differs(&there.trash, key, kept) {
            let (entry, name) = split(key);
            let bytes = source.trash_read_blocking(entry, &kept.from)?.ok_or_else(|| format!("{entry}/{name} went while it was copied"))?;
            budget.spend()?;
            target.trash_write_blocking(entry, name, &bytes)?;
        }
    }

    let blobs = Config { blobs: crate::runtime::blobs::Blobs { max_bytes: 0, max_write_bytes: 0, ..to.blobs.clone_settings() }, ..to.clone_for_task() };
    for (key, blob) in &here.blobs {
        if differs(&there.blobs, key, blob) {
            let (app, name) = split(key);
            let bytes = std::fs::read(from.data_dir.join(app).join(".blobs/data").join(name)).map_err(|e| format!("{app}'s file {name}: {e}"))?;
            budget.spend()?;
            crate::runtime::blobs::put(&blobs, app, name, &blob.content_type, &bytes).map_err(|e| format!("{app}'s file {name}: {e}"))?;
        }
    }
    Ok(())
}

/// Whether `key` opens every sealed value the snapshot holds: app settings
/// and two-step secrets. What does not open is named.
fn unopened(snapshot: &Snapshot, key: &[u8; 32]) -> Vec<String> {
    let mut out: Vec<String> = snapshot
        .settings
        .iter()
        .filter(|(_, sealed)| crate::seal::open_with(key, sealed).is_none())
        .map(|(name, _)| format!("setting {}", show(name)))
        .collect();
    let mfa = crate::accounts::store::TABLES.iter().position(|table| table.name == "mfa").expect("the mfa table");
    for row in snapshot.accounts[mfa].values() {
        if let (state::rows::Cell::Text(user), state::rows::Cell::Text(secret)) = (&row[0], &row[1])
            && crate::seal::open_with(key, secret).is_none()
        {
            out.push(format!("two-step secret of account {user}"));
        }
    }
    out
}

/// What the marker says while an export is under way.
const EXPORTING: &str = "toolsite export-to-files, not finished";

/// Under `.site/`: what the migration left behind.
fn marker(data_dir: &Path) -> PathBuf {
    data_dir.join(".site").join(state::MARKER)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// A file-mode site read and checked, ready to copy: everything it holds,
/// and what was found on the way.
pub struct Prepared {
    here: Snapshot,
    report: Report,
    verify_only: bool,
}

/// Reads a file-mode site (`from`, on the volume) for the migration and
/// checks it can go, before anything anywhere is written: `key` must be the
/// key in `.site/secret.key` if there is one, and must open every sealed
/// value; no project move may be unfinished. On a directory migrated
/// already the run only compares: copying again could undo what changed on
/// Postgres since. Runs on a blocking thread.
pub fn prepare(from: &Config, key: &[u8; 32], options: &Options) -> Result<Prepared, String> {
    let mut report = Report { sides: ("files".into(), "postgres".into()), ..Report::default() };
    let mut verify_only = options.verify_only;
    let links = Links::scan(&from.data_dir);
    // Before anything under .site is read, the key file included.
    let site = Links { found: links.found.iter().filter(|link| *link == ".site" || link.starts_with(".site/")).cloned().collect() };
    refuse_governing(&site.governing(&Snapshot::default()))?;
    if std::fs::symlink_metadata(marker(&from.data_dir)).is_ok() {
        let text = std::fs::read_to_string(marker(&from.data_dir)).unwrap_or_default();
        verify_only = true;
        let at = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|marker| marker["at"].as_u64());
        report.notes.push(format!(
            "this directory was migrated already{}: compared only, nothing copied. Copying again could \
             undo what changed on Postgres since; remove .site/{} first to copy anyway",
            at.map(|at| format!(" (at {at}, Unix time)")).unwrap_or_default(),
            state::MARKER
        ));
    }
    for name in ["auth.db", "oauth.db"] {
        if std::fs::symlink_metadata(from.data_dir.join(".site").join(name)).is_ok_and(|found| !found.is_file()) {
            return Err(format!(".site/{name} is not a file: this site's {name} cannot be read, and a copy without it would have none. Nothing was written"));
        }
    }
    if let Some(existing) = crate::seal::file_key(&from.data_dir)?
        && existing != *key
    {
        return Err("TOOLSITE_SECRET_KEY is not the key in .site/secret.key, which sealed this site's settings and \
                    two-step secrets: run `toolsite key show` and set TOOLSITE_SECRET_KEY to what it prints. \
                    Nothing was written"
            .into());
    }
    if let Some(moving) = catalog::of(from).relocation_blocking()? {
        return Err(format!(
            "a project move from {} to {} is unfinished (.site/relocating.json): start the server once in file mode \
             so it finishes, then stop it and run this again",
            moving.from, moving.to
        ));
    }
    let here = read(from, &links, true, true, &mut report.notes)?;
    refuse_governing(&links.governing(&here))?;
    let closed = unopened(&here, key);
    if !closed.is_empty() {
        return Err(format!(
            "TOOLSITE_SECRET_KEY does not open {} sealed value(s) ({}{}): it is not the key this site sealed with. \
             Nothing was written",
            closed.len(),
            closed.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            if closed.len() > 3 { ", ..." } else { "" }
        ));
    }
    if closed.is_empty() && crate::seal::file_key(&from.data_dir)?.is_none() && !sealed_anything(&here) {
        report.notes.push(
            "nothing on this site was sealed, so nothing proves TOOLSITE_SECRET_KEY: it seals what is sealed from now on, \
             so keep it set"
                .into(),
        );
    }
    Ok(Prepared { here, report, verify_only })
}

/// Whether a snapshot holds anything only the site key opens or checks:
/// settings, two-step secrets, recovery codes.
fn sealed_anything(snapshot: &Snapshot) -> bool {
    let table = |name: &str| crate::accounts::store::TABLES.iter().position(|table| table.name == name).expect("an account table");
    !snapshot.settings.is_empty() || !snapshot.accounts[table("mfa")].is_empty() || !snapshot.accounts[table("recovery_codes")].is_empty()
}

/// Refuses a run that would have to leave out what the site's access
/// rests on: see `Links::governing`.
fn refuse_governing(links: &[String]) -> Result<(), String> {
    if links.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} {} a symbolic link or not a plain file, which this never follows ({}{}): left out, what the site \
         enforces would come back as a default. Put the files themselves there. Nothing was written",
        links.len(),
        if links.len() == 1 { "path is" } else { "paths are" },
        links.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
        if links.len() > 5 { ", ..." } else { "" }
    ))
}

impl Prepared {
    /// Whether this run only compares.
    pub fn verify_only(&self) -> bool {
        self.verify_only
    }
}

/// Copies what `prepare` read to Postgres and the bucket (`to`), then
/// compares, and on no difference writes the marker that keeps file mode
/// off the directory. Runs on a blocking thread.
///
/// The database keeps `state::TRANSFER_FLAG` from before the first write:
/// `copying`, which keeps every runner off it and lets a stopped run resume,
/// then `done`, after which a run only compares, whatever the directory
/// says, since copying again would undo what changed on Postgres since. A
/// database that holds a site and no such row is not copied into.
pub fn migrate(prepared: Prepared, from: &Config, to: &Config, options: &Options) -> Result<Report, String> {
    let Prepared { here, mut report, mut verify_only } = prepared;
    let catalog = catalog::of(to);
    let recorded = wait(catalog.flag(state::TRANSFER_FLAG))?;
    let finished = recorded.as_deref().is_some_and(state::transfer_finished);
    if finished && !verify_only {
        verify_only = true;
        report.notes.push(format!(
            "this database was migrated into already: compared only, nothing copied. Copying again could undo \
             what changed on it since; delete its row {} in platform.site_flags first to copy anyway",
            state::TRANSFER_FLAG
        ));
    }
    let registry = registry(to)?;
    let mut there = read(to, &Links::none(), false, true, &mut Vec::new())?;
    there.blobs = bucket_blobs(to, &here.blobs)?;
    if !verify_only {
        if recorded.is_none() {
            let held = holding(&there);
            if !held.is_empty() {
                return Err(format!(
                    "this database already holds a site ({}): a copy never removes, so the two would be merged, \
                     and the copy would overwrite what that site changed. Migrate into an empty database. Nothing \
                     was written",
                    held.join(", ")
                ));
            }
            wait(catalog.set_flag(state::TRANSFER_FLAG, &state::transfer_flag(false, unix_now())))?;
        }
        let mut budget = Budget { written: 0, fail_after: options.fail_after };
        let copied = copy(from, &here, to, &there, &mut budget);
        report.written = budget.written;
        copied?;
        there = read(to, &Links::none(), false, true, &mut Vec::new())?;
        there.blobs = bucket_blobs(to, &here.blobs)?;
    }
    report.compared_only = verify_only;
    report.lines = compare_all(&here, &there, true);
    report.notes.push("apps' SQLite databases stay where they are, in each app's directory on this volume".into());
    // A run stopped between the row and the marker finishes here.
    let unmarked = finished && !options.verify_only && std::fs::symlink_metadata(marker(&from.data_dir)).is_err();
    if (!verify_only || unmarked) && report.differences() == 0 {
        unchanged(from, &here)?;
        still_alone(to, &registry)?;
        if !finished {
            wait(catalog.set_flag(state::TRANSFER_FLAG, &state::transfer_flag(true, unix_now())))?;
        }
        let counts: BTreeMap<&str, usize> = report.lines.iter().map(|line| (line.name.as_str(), line.here)).collect();
        let body = serde_json::json!({ "at": unix_now(), "from": "toolsite migrate-to-postgres", "counts": counts });
        crate::content::catalog::files::write_aside(&marker(&from.data_dir), format!("{body}\n").as_bytes())?;
    }
    Ok(report)
}

/// What a target holds that makes it a site already, by category. Host
/// markers and apps' names do not count: a runner sets the one on its first
/// start, and the other is any directory on the volume.
fn holding(snapshot: &Snapshot) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut held = |what: &'static str, any: bool| {
        if any {
            out.push(what);
        }
    };
    held("accounts", snapshot.accounts.iter().any(|rows| !rows.is_empty()));
    held("OAuth clients and tokens", snapshot.oauth.iter().any(|rows| !rows.is_empty()));
    held("metas", !snapshot.pages.is_empty());
    held("projects", snapshot.folders.as_ref().is_some_and(|folders| !folders.is_empty()));
    held("host labels", !snapshot.labels.is_empty());
    held("settings", !snapshot.settings.is_empty());
    held("tools", !snapshot.tools.is_empty());
    held("migration ladders", !snapshot.migrations.is_empty());
    held("repository links", !snapshot.links.is_empty());
    held("github installations", !snapshot.installations.is_empty());
    held("jobs", !snapshot.jobs.is_empty());
    held("tokens", !snapshot.tokens.is_empty());
    held("published files", !snapshot.content.is_empty());
    held("trash", !snapshot.trash.is_empty());
    out
}

/// The runner registry of a Postgres side, every row with its heartbeat;
/// none for files.
fn registry(config: &Config) -> Result<Vec<(String, i64)>, String> {
    match &config.stores.backend {
        state::Backend::Postgres(postgres) => wait(state::runners::Runner::registry(&postgres.pool)),
        state::Backend::Files => Ok(Vec::new()),
    }
}

/// Refuses to finish when a runner wrote a heartbeat since `before`: one
/// that started, or one whose heartbeat had stalled past the live window
/// and came back, served beside the copy.
fn still_alone(config: &Config, before: &[(String, i64)]) -> Result<(), String> {
    let beating: Vec<String> = registry(config)?.into_iter().filter(|row| !before.contains(row)).map(|(id, _)| id).collect();
    if beating.is_empty() {
        return Ok(());
    }
    Err(format!(
        "a toolsite server wrote its heartbeat to this database during the run (runner {}): stop every server, \
         then run this again. Nothing was marked done",
        beating.join(", ")
    ))
}

/// Refuses to finish when the source's platform state moved since it was
/// read: a server running on it wrote while the copy ran, and what it
/// wrote (an account turned off, a token revoked) may not have crossed.
fn unchanged(source: &Config, before: &Snapshot) -> Result<(), String> {
    let links = if source.stores.is_postgres() { Links::none() } else { Links::scan(&source.data_dir) };
    let now = read(source, &links, false, false, &mut Vec::new())?;
    let moved: Vec<String> = compare_all(before, &now, false)
        .into_iter()
        .filter(|line| line.name != "published files")
        .flat_map(|line| line.differences.into_iter().map(move |difference| format!("{}: {difference}", line.name)))
        .collect();
    if moved.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the source changed while it was copied ({}{}): is a server running on it? Stop it and run this again. \
         Nothing was marked done",
        moved.iter().take(3).cloned().collect::<Vec<_>>().join("; "),
        if moved.len() > 3 { "; ..." } else { "" }
    ))
}

/// A site's `Config` for one side of a run: on the volume at `data_dir`,
/// with `backend` for its platform state and `bucket` for files. Nothing
/// is served from it, so it accepts no token.
pub fn side(
    data_dir: PathBuf,
    backend: state::Backend,
    bucket: Option<crate::runtime::blobs::S3>,
    key: Option<&str>,
) -> Result<Config, String> {
    let blobs = match bucket {
        Some(s3) => crate::runtime::blobs::Blobs {
            backend: crate::runtime::blobs::Backend::S3(s3),
            max_bytes: crate::config::DEFAULT_MAX_BLOB_BYTES,
            max_write_bytes: crate::config::DEFAULT_MAX_BLOB_BYTES,
        },
        None => crate::runtime::blobs::Blobs::local(crate::config::DEFAULT_MAX_BLOB_BYTES),
    };
    Ok(Config {
        valid_tokens: Vec::new(),
        blobs,
        stores: state::Stores::new(backend, None, key)?,
        ..Config::local(data_dir, String::new())
    })
}

/// Copies a Postgres site (`from`) back into a data directory (`to`), then
/// compares, and on no difference removes the marker there so file mode
/// starts on it. The marker is there for the whole copy, so a server never
/// starts on half of one and a stopped run can resume. A directory holding
/// file-mode state is refused, the one the site was migrated from included:
/// a copy never removes, so what was removed on Postgres since would come
/// back. Runs on a blocking thread.
///
/// When `to` is another directory than the site's volume, each app's SQLite
/// database (and those the trash keeps) is copied there too, never over a
/// file already there. Apps' files stay in the bucket: file mode reads them
/// there with the same `TOOLSITE_BLOB_S3_*` settings.
pub fn export(from: &Config, to: &Config, options: &Options) -> Result<Report, String> {
    let mut report = Report { sides: ("postgres".into(), "files".into()), compared_only: options.verify_only, ..Report::default() };
    let site = to.data_dir.join(".site");
    let held = ["auth.db", "oauth.db"].iter().any(|name| std::fs::symlink_metadata(site.join(name)).is_ok());
    let marked = std::fs::symlink_metadata(marker(&to.data_dir))
        .is_ok()
        .then(|| std::fs::read_to_string(marker(&to.data_dir)).unwrap_or_default());
    let resuming = marked.as_deref().is_some_and(|text| text.contains(EXPORTING));
    // A copy over what was left behind would keep every account, grant and
    // token removed on Postgres since, since a copy never removes: they
    // would come back to life.
    let refused = |marked: &Option<String>| match marked {
        Some(_) => format!(
            "{} is the directory this site was migrated from, and still holds its file-mode state from \
             then: export into a new directory with --to (apps' databases are copied there from this one), \
             then serve from that",
            to.data_dir.display()
        ),
        None => format!(
            "{} holds a file-mode site of its own (.site/auth.db, and no .site/{}): export into a new directory",
            to.data_dir.display(),
            state::MARKER
        ),
    };
    if !options.verify_only && held && !resuming {
        return Err(refused(&marked));
    }
    if !options.verify_only {
        nested(&from.data_dir, &to.data_dir)?;
    }
    if let Some(moving) = catalog::of(from).relocation_blocking()? {
        return Err(format!(
            "a project move from {} to {} is unfinished: start the server once on Postgres so it finishes, \
             then stop it and run this again",
            moving.from, moving.to
        ));
    }
    if let Some(value) = wait(catalog::of(from).flag(state::TRANSFER_FLAG))?
        && !state::transfer_finished(&value)
    {
        return Err("toolsite migrate-to-postgres has not finished copying into this database, so it holds part of \
                    a site: finish that first"
            .into());
    }
    let registry = registry(from)?;
    let from_links = if from.stores.is_postgres() { Links::none() } else { Links::scan(&from.data_dir) };
    let here = read(from, &from_links, false, true, &mut report.notes)?;
    refuse_governing(&from_links.governing(&here))?;
    let links = Links::scan(&to.data_dir);
    let mut there = read(to, &links, false, true, &mut Vec::new())?;
    if !options.verify_only {
        // Writes through a link would land wherever it points.
        if !links.found.is_empty() {
            return Err(format!(
                "{} holds symbolic links or special files ({}): export into a new, empty directory",
                to.data_dir.display(),
                links.found.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !resuming && !holding(&there).is_empty() {
            return Err(refused(&marked));
        }
        // Until the copy is whole the directory is not a file-mode site:
        // the marker keeps a server off it, and lets a stopped run resume.
        if marked.is_none() {
            let body = serde_json::json!({ "at": unix_now(), "from": EXPORTING });
            crate::content::catalog::files::write_aside(&marker(&to.data_dir), format!("{body}\n").as_bytes())?;
        }
        let mut budget = Budget { written: 0, fail_after: options.fail_after };
        let copied = copy(from, &here, to, &there, &mut budget).and_then(|()| databases(&from.data_dir, &to.data_dir, &mut budget));
        report.written = budget.written;
        copied?;
        there = read(to, &links, false, true, &mut Vec::new())?;
    }
    report.lines = compare_all(&here, &there, false);
    report.notes.push(
        "apps' files stay in the bucket: keep TOOLSITE_BLOB_S3_* set for file mode. Sealed values were copied \
         sealed: keep TOOLSITE_SECRET_KEY set too"
            .into(),
    );
    if resuming && report.lines.iter().any(|line| line.differences.iter().any(|difference| difference.starts_with("extra "))) {
        report.notes.push(
            "the directory holds what the site no longer does: it changed since the run that stopped, and a copy \
             never removes. Export into a new directory"
                .into(),
        );
    }
    if !options.verify_only && report.differences() == 0 {
        unchanged(from, &here)?;
        still_alone(from, &registry)?;
        match std::fs::remove_file(marker(&to.data_dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("could not remove the marker: {e}")),
            _ => {}
        }
    }
    Ok(report)
}

/// Refuses a target inside the site's volume, or holding it: the export
/// would be read back as part of what it copies, or put inside an app.
fn nested(volume: &Path, to: &Path) -> Result<(), String> {
    let (Ok(volume), Ok(to)) = (volume.canonicalize(), to.canonicalize()) else { return Ok(()) };
    if volume != to && (to.starts_with(&volume) || volume.starts_with(&to)) {
        return Err(format!(
            "{} and the data directory {} are inside one another: export into a directory beside it",
            to.display(),
            volume.display()
        ));
    }
    Ok(())
}

/// Copies apps' SQLite databases, and those the trash keeps, from the
/// site's volume to another directory; never over a file that is there,
/// and never through a link on either side.
fn databases(volume: &Path, to: &Path, budget: &mut Budget) -> Result<(), String> {
    if volume == to {
        return Ok(());
    }
    let real_dir = |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|found| found.is_dir());
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(volume).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) && valid_slug(&name) {
            dirs.push(PathBuf::from(name));
        }
    }
    for entry in std::fs::read_dir(volume.join(".trash")).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            dirs.push(Path::new(".trash").join(entry.file_name()).join("app"));
        }
    }
    for dir in dirs {
        if !real_dir(&volume.join(&dir)) {
            continue;
        }
        for entry in std::fs::read_dir(volume.join(&dir)).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) || !(name == "data.db" || name.starts_with("data.db-")) {
                continue;
            }
            let target = to.join(&dir).join(&name);
            if std::fs::symlink_metadata(&target).is_ok() {
                continue;
            }
            budget.spend()?;
            std::fs::create_dir_all(to.join(&dir)).map_err(|e| e.to_string())?;
            std::fs::copy(entry.path(), &target).map_err(|e| format!("{}: {e}", dir.join(&name).display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn records_and_databases_are_not_published_files() {
        for key in ["shop.html", "shop.icon", "shop.source", "shop/index.html", "shop/handler.wasm", "shop/index.icon", "shop/a/data.db"] {
            assert!(is_content(key), "{key}");
        }
        for key in ["shop.meta", "shop/index.meta", "shop/page.notes", "shop.secrets", "shop.deploys", "shop/data.db", "shop/data.db-wal", ".site/auth.db", "shop/.blobs/data/x"] {
            assert!(!is_content(key), "{key}");
        }
    }

    #[test]
    fn a_meta_inside_a_removed_app_is_kept_where_a_restore_reads_it() {
        assert_eq!(trash_name("app/index.meta").as_deref(), Some("slug.meta"));
        assert_eq!(trash_name("app/index.notes").as_deref(), Some("slug.notes"));
        assert_eq!(trash_name("app/docs/index.meta").as_deref(), Some("slug-docs.meta"));
        assert_eq!(trash_name("app/page.meta").as_deref(), Some("slug-page.meta"));
        assert_eq!(trash_name("app/data.db"), None);
        assert_eq!(trash_name("app/data.db-wal"), None);
        assert_eq!(trash_name("app/sub/data.db").as_deref(), Some("app/sub/data.db"));
        assert_eq!(trash_name("slug.meta").as_deref(), Some("slug.meta"));
        assert_eq!(trash_name("app/index.html").as_deref(), Some("app/index.html"));
    }

    #[test]
    fn a_path_through_a_link_is_found() {
        let links = Links { found: vec!["shop".into(), ".site/labels.json".into()] };
        assert!(links.crosses("shop"));
        assert!(links.crosses("shop/index.html"));
        assert!(!links.crosses("shopping/index.html"));
        assert!(links.crosses(".site/labels.json"));
        assert!(!links.crosses(".site/projects.json"));
    }

    /// A file-mode site holding one of everything the transfer copies, a
    /// removed app in the trash, an app database, and links that point out.
    /// Run on a blocking thread.
    pub(crate) fn fixture(dir: &Path) -> Config {
        use crate::{accounts::users, platform::secrets};
        let config = side(dir.to_path_buf(), state::Backend::Files, None, None).unwrap();
        let store = files::of(&config);
        for (key, body) in [
            ("shop/index.html", &b"<h1>shop</h1>"[..]),
            ("shop/assets/app.js", b"console.log(1)"),
            ("shop/handler.wasm", b"\0asm\x01\0\0\0"),
            ("shop.source", b"source archive"),
            ("note.html", b"<h1>note</h1>"),
            ("note.icon", b"N"),
            ("docs/guide.html", b"<h1>guide</h1>"),
            ("gone/index.html", b"<h1>gone</h1>"),
        ] {
            wait(store.put(key, Bytes::copy_from_slice(body))).unwrap();
        }
        std::fs::write(dir.join("shop/data.db"), b"an app's own database").unwrap();
        std::fs::create_dir_all(dir.join("held")).unwrap();

        let catalog = catalog::of(&config);
        for (slug, hidden) in [("shop", false), ("note", true), ("gone", false)] {
            wait(catalog.update_meta(
                slug,
                Box::new(move |meta| {
                    meta.hidden = hidden;
                    meta.listed = false;
                    meta.gate = Some("restricted".into());
                    Ok(())
                }),
            ))
            .unwrap();
        }
        wait(catalog.set_notes("shop", "for the next agent")).unwrap();
        wait(catalog.update_folders(Box::new(|tree| {
            tree.push(Folder {
                path: "ops".into(),
                name: "Ops".into(),
                created_at: 1_700_000_000,
                locked: true,
                renamed_from: vec!["operations".into()],
                gate: Some("authenticated".into()),
            });
            Ok(())
        })))
        .unwrap();
        catalog.assign_label_blocking("shop", true, Box::new(|_| "shop-x1".into())).unwrap();
        wait(catalog.set_flag(crate::platform::permissions::GRANTS_ADOPTED, "1\n")).unwrap();

        wait(secrets::set(&config, "shop", "API_KEY", Some("s3cr3t"))).unwrap();
        let records = records::of(&config);
        wait(records.set_tools("shop", Some(r#"[{"name": "create_order"}]"#))).unwrap();
        records.set_migrations_blocking("shop", r#"[["001.sql", "create table t (n)"]]"#).unwrap();
        wait(records.update_repo_link("shop", Box::new(|_| Ok(r#"{"owner": "o", "repo": "r"}"#.into())))).unwrap();
        wait(records.set_installations("[]")).unwrap();
        wait(records.set_job("shop", "nightly", r#"{"schedule": "0 0 3 * * *", "path": "/api/nightly"}"#, 10)).unwrap().unwrap();
        for kind in tokens::Kind::ALL {
            wait(tokens::create(&config, "shop", kind, "holder", "who holds it")).unwrap();
        }

        let admin = users::sign_up_as(&config, "admin@example.com", "correct horse battery", true).unwrap();
        users::sign_up(&config, "viewer@example.com", "correct horse battery").unwrap();
        users::log_in(&config, "viewer@example.com", "correct horse battery").unwrap();
        users::grant(&config, "viewer@example.com", "shop", "manager").unwrap();
        users::grant_scope(&config, "viewer@example.com", "ops", users::Scope::Editor, Some("admin@example.com")).unwrap();
        users::set_pin(&config, &admin.id, "shop", true).unwrap();
        let accounts = crate::accounts::store::of(&config);
        accounts.begin_mfa(&admin.id, &crate::seal::seal(&config, "JBSWY3DPEHPK3PXP").unwrap(), "by-setup").unwrap();
        accounts.enable_mfa(&admin.id, 1_700_000_000).unwrap();
        accounts.replace_recovery_codes(&admin.id, &["digest-1".into(), "digest-2".into()]).unwrap();
        accounts.try_attempt(&admin.id, 1_700_000_000, 1_699_999_000, 5).unwrap();
        let client = crate::platform::oauth_store::register_client(&config, Some("Claude"), &["https://c.test/cb".into()]).unwrap();
        crate::platform::oauth_store::issue_tokens(&config, &client.id, &admin.id, None).unwrap();

        crate::platform::trash::remove(&config, "gone", 1_700_000_100).unwrap();

        // Links out of the directory: never followed, never copied.
        let outside = dir.parent().unwrap().join(format!("outside-{}", crate::content::slug::random_token(6)));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"not the site's").unwrap();
        std::fs::write(outside.join("settings.json"), br#"{"STOLEN": "x"}"#).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("evil")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("shop/leak.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("settings.json"), dir.join("linked.secrets")).unwrap();
        config
    }

    /// A data directory inside a temporary one, so the fixture's links have
    /// somewhere outside it to point that goes when the test does.
    fn nested() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("site");
        std::fs::create_dir(&dir).unwrap();
        (root, dir)
    }

    /// The key this process seals with on files: the environment's, else
    /// the one the site made.
    fn site_key(dir: &Path) -> [u8; 32] {
        match std::env::var("TOOLSITE_SECRET_KEY") {
            Ok(key) => crate::seal::parse_key(&key).unwrap(),
            Err(_) => crate::seal::file_key(dir).unwrap().unwrap(),
        }
    }

    fn on_blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        runtime.block_on(async { tokio::task::spawn_blocking(work).await.unwrap() })
    }

    /// The mapping run files to files: what the migration does, without a
    /// database, so the suite proves it hermetically.
    #[test]
    fn a_copy_holds_everything_the_original_did_and_nothing_a_link_pointed_at() {
        on_blocking(|| {
            let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
            let original = fixture(&a);
            let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            let report = export(&original, &copy, &Options::default()).unwrap();
            assert_eq!(report.differences(), 0, "{}", report.table());
            for name in [
                "accounts.users", "accounts.sessions", "accounts.grants", "accounts.scopes", "accounts.pins", "accounts.mfa",
                "accounts.recovery_codes", "accounts.mfa_failures", "oauth.clients", "oauth.tokens", "pages (meta, notes)",
                "projects", "host labels", "markers", "settings", "tools", "migration ladders", "repository links",
                "github installations", "jobs", "tokens", "apps", "published files", "trash",
            ] {
                let line = report.lines.iter().find(|line| line.name == name).unwrap_or_else(|| panic!("no line {name}"));
                assert!(line.here > 0, "nothing in {name}: {}", report.table());
            }

            for linked in ["evil", "shop/leak.txt", "linked.secrets"] {
                assert!(std::fs::symlink_metadata(b.path().join(linked)).is_err(), "{linked} was followed and copied");
                assert!(report.notes.iter().any(|note| note.contains(linked)), "no word of {linked}: {:?}", report.notes);
            }
            assert_eq!(std::fs::read(b.path().join("shop/data.db")).unwrap(), b"an app's own database");
            assert!(b.path().join("held").is_dir(), "an app with nothing published was dropped");
            assert_eq!(std::fs::read(b.path().join("shop/assets/app.js")).unwrap(), b"console.log(1)");

            // The copy is a file-mode site now: another copy is not put
            // over it, but comparing writes nothing and finds nothing.
            let refused = export(&original, &copy, &Options::default()).expect_err("copied over a file-mode site");
            assert!(refused.contains("file-mode site of its own"), "{refused}");
            let again = export(&original, &copy, &Options { verify_only: true, ..Options::default() }).unwrap();
            assert_eq!((again.written, again.differences()), (0, 0), "{}", again.table());
            assert!(!b.path().join(".site").join(state::MARKER).exists());

            // A difference planted on the copy is found, and only reported.
            std::fs::remove_file(b.path().join("shop/assets/app.js")).unwrap();
            let checked = export(&original, &copy, &Options { verify_only: true, ..Options::default() }).unwrap();
            assert_eq!(checked.written, 0);
            let found: Vec<&String> = checked.lines.iter().flat_map(|line| &line.differences).collect();
            assert_eq!(found, [&"missing shop/assets/app.js".to_string()], "{}", checked.table());
            assert!(!b.path().join("shop/assets/app.js").exists());

            // The removed app comes back from the copy's trash, meta and all.
            let entry = files::of(&copy).trash_entries_blocking().unwrap().remove(0);
            let restored = crate::platform::trash::restore(&copy, &entry).unwrap();
            assert!(restored.iter().any(|what| what == "gone/"), "{restored:?}");
            assert_eq!(catalog::meta_blocking(&copy, "gone").gate.as_deref(), Some("restricted"));
        });
    }

    /// A copy stopped part way, as a crash would stop it, leaves the marker
    /// so no server starts on half a site, and the next run finishes it.
    #[test]
    fn a_stopped_copy_resumes_and_finishes() {
        on_blocking(|| {
            let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
            let original = fixture(&a);
            let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            let mut stops = 0;
            for most in [3, 7, 9] {
                let stopped = export(&original, &copy, &Options { fail_after: Some(most), ..Options::default() });
                assert!(stopped.as_ref().err().is_some_and(|why| why.contains("stopped after")), "run {stops}: {:?}", stopped.map(|r| r.written));
                assert!(b.path().join(".site").join(state::MARKER).exists(), "a half copy had no marker");
                stops += 1;
            }
            let finished = export(&original, &copy, &Options::default()).unwrap();
            assert_eq!(finished.differences(), 0, "{}", finished.table());
            assert!(finished.written > 0 && stops == 3);
            assert!(!b.path().join(".site").join(state::MARKER).exists());
        });
    }

    #[test]
    fn a_key_that_does_not_open_the_sealed_values_is_refused() {
        on_blocking(|| {
            let (_root, dir) = nested();
            let site = fixture(&dir);
            let right = site_key(&dir);
            let wrong = [7u8; 32];
            let why = prepare(&site, &wrong, &Options::default()).err().expect("a wrong key was taken");
            assert!(why.contains("TOOLSITE_SECRET_KEY") && why.contains("Nothing was written"), "{why}");
            // Without the key file to compare, opening is the test.
            let _ = std::fs::remove_file(dir.join(".site/secret.key"));
            let why = prepare(&site, &wrong, &Options::default()).err().expect("a wrong key was taken");
            assert!(why.contains("does not open 2 sealed value(s)"), "{why}");
            assert!(!why.contains("s3cr3t") && !why.contains("JBSWY3DPEHPK3PXP"), "a sealed value was shown: {why}");
            assert!(prepare(&site, &right, &Options::default()).is_ok());
            assert!(!dir.join(".site").join(state::MARKER).exists());
        });
    }

    #[test]
    fn an_unfinished_project_move_is_refused() {
        on_blocking(|| {
            let (_root, dir) = nested();
            let site = fixture(&dir);
            wait(catalog::of(&site).begin_relocation("ops", "ops2")).unwrap();
            let why = prepare(&site, &site_key(&dir), &Options::default()).err().expect("a half-done move was taken");
            assert!(why.contains("unfinished") && why.contains("ops2"), "{why}");
        });
    }

    /// What each revoked, spent or retired thing on a site answers, asked
    /// the way the site asks it: a copy must answer the same.
    #[derive(Debug, Clone, PartialEq)]
    struct Revoked {
        disabled_password: bool,
        ended_session: bool,
        live_session: bool,
        dropped_grant: Option<String>,
        dropped_scope: bool,
        two_step: bool,
        recovery: (bool, bool, usize),
        revoked_token: bool,
        kept_token: bool,
        retired_token: bool,
        removed_app_token: bool,
        removed_app_setting: Option<String>,
        oauth_revoked: bool,
        refresh_spent: bool,
        locked: bool,
        hidden: bool,
        gated: Option<String>,
        statuses: Vec<(String, u16)>,
    }

    /// Every way a site was told no, set up on the fixture: an account
    /// turned off, a session ended, a grant and a scope taken back, a
    /// recovery code spent, a token revoked, tokens retired at a name and
    /// with a removed app, OAuth tokens revoked and a refresh spent.
    struct Revocations {
        ended: String,
        live: String,
        revoked: String,
        kept: String,
        retired: String,
        removed: String,
        admin: String,
        spent: String,
        unspent: String,
        access: String,
        refresh: (String, String),
    }

    fn revoke_everything(config: &Config) -> Revocations {
        use crate::{accounts::users, platform::oauth_store};
        users::sign_up(config, "gone@example.com", "correct horse battery").unwrap();
        users::log_in(config, "gone@example.com", "correct horse battery").unwrap();
        users::set_active(config, "gone@example.com", false).unwrap();
        let (_, ended) = users::log_in(config, "viewer@example.com", "correct horse battery").unwrap();
        users::log_out(config, &ended).unwrap();
        let (_, live) = users::log_in(config, "viewer@example.com", "correct horse battery").unwrap();
        users::grant(config, "viewer@example.com", "note", "editor").unwrap();
        users::revoke(config, "viewer@example.com", "note").unwrap();
        users::grant_scope(config, "viewer@example.com", "docs", users::Scope::Admin, None).unwrap();
        users::revoke_scope(config, "viewer@example.com", "docs").unwrap();

        let admin = users::user_by_email(config, "admin@example.com").unwrap();
        let (spent, unspent) = ("spent-digest".to_string(), "unspent-digest".to_string());
        let accounts = crate::accounts::store::of(config);
        accounts.replace_recovery_codes(&admin.id, &[spent.clone(), unspent.clone()]).unwrap();
        assert!(accounts.spend_recovery_code(&admin.id, &spent, 1_700_000_500).unwrap());

        let (revoked_token, revoked) = wait(tokens::create(config, "shop", tokens::Kind::Deploy, "old ci", "ci")).unwrap();
        wait(tokens::revoke(config, "shop", tokens::Kind::Deploy, &revoked_token.id)).unwrap();
        let (_, kept) = wait(tokens::create(config, "shop", tokens::Kind::Deploy, "new ci", "ci")).unwrap();
        let (_, retired) = wait(tokens::create(config, "fresh", tokens::Kind::Deploy, "early", "someone")).unwrap();
        crate::platform::trash::retire_tokens(config, "fresh", 1_700_000_600).unwrap();
        let (_, removed) = wait(tokens::create(config, "doomed", tokens::Kind::Export, "warehouse", "warehouse")).unwrap();
        wait(files::of(config).put("doomed/index.html", bytes::Bytes::from_static(b"<h1>doomed</h1>"))).unwrap();
        wait(crate::platform::secrets::set(config, "doomed", "API_KEY", Some("doomed-secret"))).unwrap();
        crate::platform::trash::remove(config, "doomed", 1_700_000_700).unwrap();

        let client = oauth_store::register_client(config, Some("Old"), &["https://o.test/cb".into()]).unwrap();
        let issued = oauth_store::issue_tokens(config, &client.id, &admin.id, None).unwrap();
        oauth_store::rotate_refresh(config, &client.id, &issued.refresh_token).unwrap();
        let gone = users::user_by_email(config, "viewer@example.com").unwrap();
        let theirs = oauth_store::issue_tokens(config, &client.id, &gone.id, None).unwrap();
        oauth_store::revoke_for_user(config, &gone.id).unwrap();
        Revocations {
            ended,
            live,
            revoked,
            kept,
            retired,
            removed,
            admin: admin.id,
            spent,
            unspent,
            access: theirs.access_token,
            refresh: (client.id, issued.refresh_token),
        }
    }

    /// Asks, without spending anything on `config` but the one recovery
    /// code it spends: on a copy made for asking.
    fn answers(config: &Config, r: &Revocations) -> Revoked {
        use crate::{accounts::users, platform::oauth_store};
        let accounts = crate::accounts::store::of(config);
        let viewer = users::user_by_email(config, "viewer@example.com").unwrap();
        let runtime = crate::runtime::wasm::Runtime::new().unwrap();
        let router = crate::build_router(std::sync::Arc::new(config.clone_for_task()), runtime);
        let statuses = ["/p/shop/", "/p/note", "/p/gone/", "/p/doomed/", "/p/docs/guide", "/p/held/", "/p/evil/"]
            .into_iter()
            .map(|path| {
                use tower::ServiceExt as _;
                let request = axum::http::Request::builder().uri(path).header("host", "site.test").body(axum::body::Body::empty()).unwrap();
                let status = wait(router.clone().oneshot(request)).unwrap().status().as_u16();
                (path.to_string(), status)
            })
            .collect();
        Revoked {
            disabled_password: users::check_password(config, "gone@example.com", "correct horse battery").is_ok(),
            ended_session: users::site_session_user(config, &r.ended).is_some(),
            live_session: users::site_session_user(config, &r.live).is_some(),
            dropped_grant: users::role_for(config, &viewer.id, "note"),
            dropped_scope: users::scopes_for(config, &viewer.id).iter().any(|(prefix, _)| prefix == "docs"),
            two_step: crate::accounts::mfa::is_enabled(config, &r.admin),
            recovery: (
                accounts.spend_recovery_code(&r.admin, &r.spent, 1_700_000_900).unwrap(),
                accounts.spend_recovery_code(&r.admin, &r.unspent, 1_700_000_900).unwrap(),
                accounts.recovery_left(&r.admin).unwrap(),
            ),
            revoked_token: tokens::check_blocking(config, "shop", tokens::Kind::Deploy, &r.revoked).is_some(),
            kept_token: tokens::check_blocking(config, "shop", tokens::Kind::Deploy, &r.kept).is_some(),
            retired_token: tokens::check_blocking(config, "fresh", tokens::Kind::Deploy, &r.retired).is_some(),
            removed_app_token: tokens::check_blocking(config, "doomed", tokens::Kind::Export, &r.removed).is_some(),
            removed_app_setting: wait(crate::platform::secrets::get(config, "doomed", "API_KEY")),
            oauth_revoked: oauth_store::access_token_holder(config, &r.access).is_some(),
            refresh_spent: oauth_store::rotate_refresh(config, &r.refresh.0, &r.refresh.1).is_some(),
            locked: catalog::folders_blocking(config).iter().any(|folder| folder.path == "ops" && folder.locked),
            hidden: catalog::meta_blocking(config, "note").hidden,
            gated: catalog::meta_blocking(config, "shop").gate,
            statuses,
        }
    }

    /// The answers a site that kept every revocation gives.
    fn assert_still_refused(answers: &Revoked) {
        let expected = Revoked {
            disabled_password: false,
            ended_session: false,
            live_session: true,
            dropped_grant: None,
            dropped_scope: false,
            two_step: true,
            recovery: (false, true, 0),
            revoked_token: false,
            kept_token: true,
            retired_token: false,
            removed_app_token: false,
            removed_app_setting: None,
            oauth_revoked: false,
            refresh_spent: false,
            locked: true,
            hidden: true,
            gated: Some("restricted".into()),
            statuses: answers.statuses.clone(),
        };
        assert_eq!(*answers, expected);
    }

    /// Every revocation on a site is one on its copy: asked the way the
    /// site asks, the copy refuses what the original refuses, and serves
    /// what it serves, with the same status.
    #[test]
    fn every_revocation_stays_revoked_in_a_copy() {
        on_blocking(|| {
            let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
            let original = fixture(&a);
            let revocations = revoke_everything(&original);
            let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            let report = export(&original, &copy, &Options::default()).unwrap();
            assert_eq!(report.differences(), 0, "{}", report.table());
            // Each side is asked once: asking spends a recovery code.
            let theirs = answers(&copy, &revocations);
            let ours = answers(&original, &revocations);
            assert_still_refused(&theirs);
            assert_eq!(ours, theirs);
            // Gated, hidden, removed, removed, public, nothing published, a link.
            let statuses: Vec<u16> = theirs.statuses.iter().map(|(_, status)| *status).collect();
            assert_eq!(statuses, [303, 404, 404, 404, 200, 404, 404], "{:?}", theirs.statuses);
        });
    }

    /// A link or special file that a gate, an account or a reservation is
    /// read through is never left out quietly: left out, it would come back
    /// as the default, and a gated app would be public.
    #[test]
    fn a_link_that_access_rests_on_refuses_the_run() {
        type Plant = fn(&Path, &Path);
        let plant: [(&str, Plant); 7] = [
            ("a meta that is a link", |dir, outside| {
                std::fs::remove_file(dir.join("shop/index.meta")).unwrap();
                std::fs::write(outside.join("gated.meta"), br#"{"gate": "restricted", "hidden": true}"#).unwrap();
                std::os::unix::fs::symlink(outside.join("gated.meta"), dir.join("shop/index.meta")).unwrap();
            }),
            ("a page's meta read through a linked directory", |dir, outside| {
                std::fs::create_dir_all(outside.join("pagedir")).unwrap();
                std::fs::write(outside.join("pagedir/index.meta"), br#"{"gate": "restricted"}"#).unwrap();
                std::fs::write(dir.join("solo.html"), b"<h1>solo</h1>").unwrap();
                std::os::unix::fs::symlink(outside.join("pagedir"), dir.join("solo")).unwrap();
            }),
            ("the account store", |dir, outside| {
                std::fs::rename(dir.join(".site/auth.db"), outside.join("auth.db")).unwrap();
                std::os::unix::fs::symlink(outside.join("auth.db"), dir.join(".site/auth.db")).unwrap();
            }),
            ("the host labels", |dir, outside| {
                std::fs::rename(dir.join(".site/labels.json"), outside.join("labels.json")).unwrap();
                std::os::unix::fs::symlink(outside.join("labels.json"), dir.join(".site/labels.json")).unwrap();
            }),
            ("the projects", |dir, outside| {
                std::fs::rename(dir.join(".site/projects.json"), outside.join("projects.json")).unwrap();
                std::os::unix::fs::symlink(outside.join("projects.json"), dir.join(".site/projects.json")).unwrap();
            }),
            ("the grants marker as a socket", |dir, _| {
                std::fs::remove_file(dir.join(".site").join(crate::platform::permissions::GRANTS_ADOPTED)).unwrap();
                let socket = std::os::unix::net::UnixListener::bind(dir.join(".site").join(crate::platform::permissions::GRANTS_ADOPTED)).unwrap();
                std::mem::forget(socket);
            }),
            ("an app the trash keeps", |dir, outside| {
                let entry = std::fs::read_dir(dir.join(".trash")).unwrap().flatten().next().unwrap().path();
                std::fs::rename(entry.join("app"), outside.join("trashed-app")).unwrap();
                std::os::unix::fs::symlink(outside.join("trashed-app"), entry.join("app")).unwrap();
            }),
        ];
        for (what, plant) in plant {
            on_blocking(move || {
                let (root, dir) = nested();
                let site = fixture(&dir);
                let outside = root.path().join("planted");
                std::fs::create_dir_all(&outside).unwrap();
                plant(&dir, &outside);
                let why = prepare(&site, &site_key(&dir), &Options::default()).err().unwrap_or_else(|| panic!("{what}: taken"));
                assert!(why.contains("symbolic link") && why.contains("Nothing was written"), "{what}: {why}");
                let copy = tempfile::tempdir().unwrap();
                let target = side(copy.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
                assert!(export(&site, &target, &Options::default()).is_err(), "{what}: exported");
            });
        }
    }

    /// A store file that is a directory is not an empty store: it is
    /// refused, not copied as no accounts at all.
    #[test]
    fn an_account_store_that_is_not_a_file_is_refused() {
        on_blocking(|| {
            let (_root, dir) = nested();
            let site = fixture(&dir);
            std::fs::remove_file(dir.join(".site/oauth.db")).unwrap();
            std::fs::create_dir(dir.join(".site/oauth.db")).unwrap();
            let why = prepare(&site, &site_key(&dir), &Options::default()).err().expect("taken");
            assert!(why.contains("oauth.db is not a file"), "{why}");
        });
    }

    /// A FIFO where a published file would be is never read, so the run
    /// neither hangs on it nor copies it.
    #[test]
    fn a_fifo_in_the_site_is_never_read() {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            on_blocking(move || {
                let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
                let original = fixture(&a);
                let made = std::process::Command::new("mkfifo").arg(a.join("shop/assets/pipe.js")).status().unwrap();
                assert!(made.success());
                let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
                let report = export(&original, &copy, &Options::default()).unwrap();
                assert_eq!(report.differences(), 0, "{}", report.table());
                assert!(report.notes.iter().any(|note| note.contains("shop/assets/pipe.js")), "{:?}", report.notes);
                assert!(std::fs::symlink_metadata(b.path().join("shop/assets/pipe.js")).is_err());
                done.send(()).unwrap();
            })
        });
        finished.recv_timeout(std::time::Duration::from_secs(60)).expect("the run hung on a FIFO, or failed");
    }

    /// Sidecars a publisher or a past bug could have left (fields twice,
    /// the wrong types, nesting past any parser's depth, a label for
    /// another app) are copied so that the copy serves them as the files
    /// did: what file mode made of each, the copy makes of it too.
    #[test]
    fn hostile_sidecars_mean_the_same_on_both_sides() {
        on_blocking(|| {
            let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
            let original = fixture(&a);
            let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
            let metas = [
                r#"{"gate": "restricted", "gate": "public"}"#.to_string(),
                r#"{"hidden": "yes", "gate": "restricted"}"#.to_string(),
                format!(r#"{{"gate": "restricted", "rules": {deep}}}"#),
                r#"{"hidden": true, "project": "ops", "created_by": "someone-else"}"#.to_string(),
                format!(r#"{{"hidden": true, "roles": [{}]}}"#, vec![r#""r""#; 20_000].join(",")),
                "not json at all".to_string(),
            ];
            for (n, meta) in metas.iter().enumerate() {
                std::fs::write(a.join(format!("crafted{n}.html")), b"<h1>crafted</h1>").unwrap();
                std::fs::write(a.join(format!("crafted{n}.meta")), meta).unwrap();
            }
            std::fs::write(a.join("shop.tools"), &deep).unwrap();
            // Names no slug rule passes: never read, never written.
            for name in ["new\nline.html", "new\nline.meta", "x..y.meta", "shop/..hidden.js", "shop/a\nb.js"] {
                std::fs::write(a.join(name), br#"{"gate": "public"}"#).unwrap();
            }
            std::fs::write(a.join("shop.secrets"), r#"{"API_KEY": "x", "API_KEY": "y"}"#).unwrap();
            std::fs::write(a.join(".site/labels.json"), r#"{"shop-x1": "shop", "note-x2": "shop"}"#).unwrap();
            let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            let report = export(&original, &copy, &Options::default()).unwrap();
            assert_eq!(report.differences(), 0, "{}", report.table());
            for n in 0..metas.len() {
                let slug = format!("crafted{n}");
                let (theirs, ours) = (catalog::meta_blocking(&copy, &slug), catalog::meta_blocking(&original, &slug));
                assert_eq!(serde_json::to_value(theirs).unwrap(), serde_json::to_value(ours).unwrap(), "{slug}");
            }
            let labels = |config: &Config| wait(catalog::of(config).labels()).unwrap();
            assert_eq!(labels(&copy), labels(&original));
            assert_eq!(wait(records::of(&copy).settings("shop")).unwrap(), wait(records::of(&original).settings("shop")).unwrap());
            assert_eq!(wait(records::of(&copy).tools("shop")).unwrap(), wait(records::of(&original).tools("shop")).unwrap());
            for name in ["new\nline.html", "new\nline.meta", "x..y.meta", "shop/..hidden.js", "shop/a\nb.js"] {
                assert!(std::fs::symlink_metadata(b.path().join(name)).is_err(), "{name:?} was copied");
            }
        });
    }

    /// An account store with tables and triggers of its own gives up only
    /// the rows of the tables the platform keeps: reading fires nothing,
    /// changes nothing in the source, and nothing but rows reaches the copy.
    #[test]
    fn a_planted_trigger_in_the_account_store_neither_fires_nor_crosses() {
        on_blocking(|| {
            let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
            let original = fixture(&a);
            {
                let conn = rusqlite::Connection::open(a.join(".site/auth.db")).unwrap();
                conn.execute_batch(
                    "create table stash (what text);
                     insert into stash values ('not the platform''s');
                     create trigger escalate after insert on sessions begin update users set is_admin = 1; end;
                     create trigger escalate_too after update on users begin update users set is_admin = 1; end;",
                )
                .unwrap();
            }
            let accounts = crate::accounts::store::of(&original);
            let users = &crate::accounts::store::TABLES[0];
            let before = accounts.rows(users).unwrap();
            let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            let report = export(&original, &copy, &Options::default()).unwrap();
            assert_eq!(report.differences(), 0, "{}", report.table());
            assert_eq!(accounts.rows(users).unwrap(), before, "reading the source changed it");
            assert!(
                !crate::accounts::users::user_by_email(&copy, "viewer@example.com").unwrap().is_admin,
                "a trigger made an admin"
            );
            let conn = rusqlite::Connection::open(b.path().join(".site/auth.db")).unwrap();
            let planted: i64 = conn
                .query_row("select count(*) from sqlite_master where name in ('stash', 'escalate', 'escalate_too')", [], |row| row.get(0))
                .unwrap();
            assert_eq!(planted, 0, "a table or trigger of the source's own crossed");
        });
    }

    /// A mounted volume's `lost+found`, at the root or inside an app, with
    /// what fsck leaves there and a part nobody but root may read, is no
    /// part of the site: both commands walk past it, read none of it and
    /// copy none of it.
    #[test]
    fn a_volumes_lost_and_found_is_walked_past() {
        use std::os::unix::fs::PermissionsExt as _;
        on_blocking(|| {
            let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
            let original = fixture(&a);
            for found in [a.join("lost+found"), a.join("shop/lost+found")] {
                std::fs::create_dir_all(found.join("#8811")).unwrap();
                std::fs::write(found.join("#4021"), br#"{"gate": "public"}"#).unwrap();
                std::fs::write(found.join("#8811/index.meta"), br#"{"gate": "public"}"#).unwrap();
                std::fs::create_dir(found.join("sealed")).unwrap();
                std::fs::set_permissions(found.join("sealed"), std::fs::Permissions::from_mode(0o000)).unwrap();
            }
            let prepared = prepare(&original, &site_key(&a), &Options::default()).unwrap();
            assert!(
                !prepared.here.content.keys().chain(prepared.here.pages.keys()).any(|key| key.contains("lost+found")),
                "lost+found was read as the site's"
            );
            let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            let report = export(&original, &copy, &Options::default()).unwrap();
            assert_eq!(report.differences(), 0, "{}", report.table());
            assert!(!b.path().join("lost+found").exists() && !b.path().join("shop/lost+found").exists(), "lost+found was copied");
            for found in [a.join("lost+found"), a.join("shop/lost+found")] {
                std::fs::set_permissions(found.join("sealed"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        });
    }

    /// While a copy runs, the source must stand still: a revocation made
    /// on it after it was read is found before the run says it finished.
    #[test]
    fn a_change_to_the_source_during_a_copy_is_found() {
        on_blocking(|| {
            let (_root, dir) = nested();
            let site = fixture(&dir);
            let before = read(&site, &Links::scan(&dir), false, true, &mut Vec::new()).unwrap();
            unchanged(&site, &before).unwrap();
            crate::accounts::users::set_active(&site, "viewer@example.com", false).unwrap();
            let why = unchanged(&site, &before).expect_err("a revocation during the copy went unseen");
            assert!(why.contains("accounts.users") && why.contains("server running"), "{why}");
            let before = read(&site, &Links::scan(&dir), false, true, &mut Vec::new()).unwrap();
            wait(tokens::revoke_all(&site, "shop", tokens::Kind::Deploy)).unwrap();
            let why = unchanged(&site, &before).expect_err("a revoked token during the copy went unseen");
            assert!(why.contains("tokens"), "{why}");
        });
    }

    /// An export never merges into a directory that holds any of a site's
    /// state, accounts or not, nor writes through a link, nor lands inside
    /// the volume it copies from.
    #[test]
    fn an_export_goes_only_into_a_directory_with_nothing_of_a_site() {
        on_blocking(|| {
            let (_a, a) = nested();
            let original = fixture(&a);
            type Plant = fn(&Path);
            let plants: [(&str, Plant); 4] = [
                ("a token list", |dir| std::fs::write(dir.join("shop.deploys"), br#"[{"id": "old", "label": "x", "hash": "h", "created_at": 1}]"#).unwrap()),
                ("a meta", |dir| std::fs::write(dir.join("shop.meta"), br#"{"gate": "public"}"#).unwrap()),
                ("a trash entry", |dir| {
                    std::fs::create_dir_all(dir.join(".trash/shop-1/")).unwrap();
                    std::fs::write(dir.join(".trash/shop-1/removed.json"), br#"{"slug": "shop"}"#).unwrap();
                }),
                ("a link", |dir| std::os::unix::fs::symlink("/tmp", dir.join("shop")).unwrap()),
            ];
            for (what, plant) in plants {
                let b = tempfile::tempdir().unwrap();
                plant(b.path());
                let target = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
                let why = export(&original, &target, &Options::default()).err().unwrap_or_else(|| panic!("exported over {what}"));
                assert!(why.contains("directory"), "{what}: {why}");
                assert!(!b.path().join(".site").join(state::MARKER).exists(), "{what}: marked");
            }
            let inside = a.join("rollback");
            std::fs::create_dir_all(&inside).unwrap();
            let target = side(inside.clone(), state::Backend::Files, None, None).unwrap();
            let why = export(&original, &target, &Options::default()).expect_err("exported inside the volume");
            assert!(why.contains("inside one another"), "{why}");
        });
    }

    /// An export stopped part way and resumed after the site revoked
    /// something never finishes: the copy never removes, so the directory
    /// still holds what was revoked, and it stays marked off file mode.
    #[test]
    fn an_export_resumed_after_a_revocation_never_finishes() {
        on_blocking(|| {
            let ((_a, a), b) = (nested(), tempfile::tempdir().unwrap());
            let original = fixture(&a);
            let (_, live) = crate::accounts::users::log_in(&original, "viewer@example.com", "correct horse battery").unwrap();
            let copy = side(b.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            let stopped = export(&original, &copy, &Options { fail_after: Some(4), ..Options::default() });
            assert!(stopped.is_err());
            assert!(crate::accounts::users::site_session_user(&copy, &live).is_some(), "the session had not crossed yet");
            crate::accounts::users::log_out(&original, &live).unwrap();
            let resumed = export(&original, &copy, &Options::default()).unwrap();
            assert!(resumed.differences() > 0, "{}", resumed.table());
            assert!(resumed.notes.iter().any(|note| note.contains("new directory")), "{:?}", resumed.notes);
            assert!(b.path().join(".site").join(state::MARKER).exists(), "a copy holding a revoked session was unmarked");
            let files = state::Settings { database_url: None, secret_key: None, bucket: false, pool_size: 1 };
            assert!(state::check(&files, b.path()).is_err(), "file mode would start on it");
        });
    }

    /// With nothing sealed and no key file, nothing proves a key: it is
    /// taken, and the run says so, since it seals what comes next.
    #[test]
    fn with_nothing_sealed_any_key_is_taken_and_the_run_says_so() {
        on_blocking(|| {
            let dir = tempfile::tempdir().unwrap();
            let site = side(dir.path().to_path_buf(), state::Backend::Files, None, None).unwrap();
            wait(files::of(&site).put("plain.html", bytes::Bytes::from_static(b"<h1>plain</h1>"))).unwrap();
            crate::accounts::users::sign_up(&site, "a@example.com", "correct horse battery").unwrap();
            let _ = std::fs::remove_file(dir.path().join(".site/secret.key"));
            let prepared = prepare(&site, &[9u8; 32], &Options::default()).unwrap();
            assert!(prepared.report.notes.iter().any(|note| note.contains("nothing proves")), "{:?}", prepared.report.notes);
        });
    }

    /// The key file is never read through a link: `key show` would print
    /// whatever it pointed at.
    #[test]
    fn a_key_file_that_is_a_link_is_never_read() {
        let (root, dir) = nested();
        std::fs::create_dir_all(dir.join(".site")).unwrap();
        std::fs::write(root.path().join("other.key"), [5u8; 32]).unwrap();
        std::os::unix::fs::symlink(root.path().join("other.key"), dir.join(".site/secret.key")).unwrap();
        let why = crate::seal::file_key(&dir).expect_err("read through a link");
        assert!(why.contains("symbolic link"), "{why}");
    }

    /// Apps' databases are copied from the volume, never through a link
    /// the trash holds where an app's directory was.
    #[test]
    fn a_database_is_never_copied_through_a_link() {
        let (root, volume) = nested();
        let to = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("elsewhere")).unwrap();
        std::fs::write(root.path().join("elsewhere/data.db"), b"another site's database").unwrap();
        std::fs::create_dir_all(volume.join(".trash/shop-1")).unwrap();
        std::os::unix::fs::symlink(root.path().join("elsewhere"), volume.join(".trash/shop-1/app")).unwrap();
        std::os::unix::fs::symlink(root.path().join("elsewhere"), volume.join("linked")).unwrap();
        std::fs::create_dir_all(volume.join("real")).unwrap();
        std::fs::write(volume.join("real/data.db"), b"ours").unwrap();
        databases(&volume, to.path(), &mut Budget { written: 0, fail_after: None }).unwrap();
        assert_eq!(std::fs::read(to.path().join("real/data.db")).unwrap(), b"ours");
        assert!(!to.path().join(".trash/shop-1/app/data.db").exists(), "copied through the trash's link");
        assert!(!to.path().join("linked").exists(), "copied through a linked app");
    }
}
