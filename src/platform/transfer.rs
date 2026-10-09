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
//! through one is left out, with a warning, as if it were not there.

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
                if kind.is_symlink() {
                    found.push(rel);
                } else if kind.is_dir() {
                    dirs.push(rel);
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
/// go to the bucket.
fn read(config: &Config, links: &Links, blobs: bool, notes: &mut Vec<String>) -> Result<Snapshot, String> {
    let on_files = !config.stores.is_postgres();
    let mut snapshot = Snapshot::default();
    let skip = |what: &str, notes: &mut Vec<String>| {
        tracing::warn!(path = what, "a symbolic link is not followed; what it points at is not copied");
        notes.push(format!("skipped {what}: a symbolic link, never followed"));
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
                let bytes = wait(store.fetch(&key))?.ok_or_else(|| format!("{key} went while it was read"))?;
                snapshot.content.insert(key, Digest::of(&bytes));
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
                if kind.is_symlink() {
                    notes.push(format!("skipped {base}/{key}: a symbolic link, never followed"));
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
    if let Ok(text) = std::fs::read_to_string(marker(&from.data_dir)) {
        verify_only = true;
        let at = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|marker| marker["at"].as_u64());
        report.notes.push(format!(
            "this directory was migrated already{}: compared only, nothing copied. Copying again could \
             undo what changed on Postgres since; remove .site/{} first to copy anyway",
            at.map(|at| format!(" (at {at}, Unix time)")).unwrap_or_default(),
            state::MARKER
        ));
    }
    let links = Links::scan(&from.data_dir);
    for linked in [".site/secret.key", ".site/relocating.json"] {
        if links.crosses(linked) {
            return Err(format!("{linked} is a symbolic link, which this never follows: put the file itself there"));
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
    let here = read(from, &links, true, &mut report.notes)?;
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
    Ok(Prepared { here, report, verify_only })
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
pub fn migrate(prepared: Prepared, from: &Config, to: &Config, options: &Options) -> Result<Report, String> {
    let Prepared { here, mut report, verify_only } = prepared;
    let mut there = read(to, &Links::none(), false, &mut Vec::new())?;
    there.blobs = bucket_blobs(to, &here.blobs)?;
    if !verify_only {
        let mut budget = Budget { written: 0, fail_after: options.fail_after };
        let copied = copy(from, &here, to, &there, &mut budget);
        report.written = budget.written;
        copied?;
        there = read(to, &Links::none(), false, &mut Vec::new())?;
        there.blobs = bucket_blobs(to, &here.blobs)?;
    }
    report.lines = compare_all(&here, &there, true);
    report.notes.push("apps' SQLite databases stay where they are, in each app's directory on this volume".into());
    if !verify_only && report.differences() == 0 {
        let counts: BTreeMap<&str, usize> = report.lines.iter().map(|line| (line.name.as_str(), line.here)).collect();
        let body = serde_json::json!({ "at": unix_now(), "from": "toolsite migrate-to-postgres", "counts": counts });
        crate::content::catalog::files::write_aside(&marker(&from.data_dir), format!("{body}\n").as_bytes())?;
    }
    Ok(report)
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
    let mut report = Report { sides: ("postgres".into(), "files".into()), ..Report::default() };
    let site = to.data_dir.join(".site");
    let held = ["auth.db", "oauth.db"].iter().any(|name| site.join(name).exists());
    let marked = std::fs::read_to_string(marker(&to.data_dir)).ok();
    let resuming = marked.as_deref().is_some_and(|text| text.contains(EXPORTING));
    if !options.verify_only && held && !resuming {
        return Err(match marked {
            // A copy over what was left behind would keep every account,
            // grant and token removed on Postgres since, since a copy never
            // removes: they would come back to life.
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
        });
    }
    if let Some(moving) = catalog::of(from).relocation_blocking()? {
        return Err(format!(
            "a project move from {} to {} is unfinished: start the server once on Postgres so it finishes, \
             then stop it and run this again",
            moving.from, moving.to
        ));
    }
    let from_links = if from.stores.is_postgres() { Links::none() } else { Links::scan(&from.data_dir) };
    let here = read(from, &from_links, false, &mut report.notes)?;
    let links = Links::scan(&to.data_dir);
    let mut there = read(to, &links, false, &mut Vec::new())?;
    if !options.verify_only {
        // Until the copy is whole the directory is not a file-mode site:
        // the marker keeps a server off it, and lets a stopped run resume.
        if !marker(&to.data_dir).exists() {
            let body = serde_json::json!({ "at": unix_now(), "from": EXPORTING });
            crate::content::catalog::files::write_aside(&marker(&to.data_dir), format!("{body}\n").as_bytes())?;
        }
        let mut budget = Budget { written: 0, fail_after: options.fail_after };
        let copied = copy(from, &here, to, &there, &mut budget).and_then(|()| databases(&from.data_dir, &to.data_dir, &mut budget));
        report.written = budget.written;
        copied?;
        there = read(to, &links, false, &mut Vec::new())?;
    }
    report.lines = compare_all(&here, &there, false);
    report.notes.push(
        "apps' files stay in the bucket: keep TOOLSITE_BLOB_S3_* set for file mode. Sealed values were copied \
         sealed: keep TOOLSITE_SECRET_KEY set too"
            .into(),
    );
    if !options.verify_only && report.differences() == 0 {
        match std::fs::remove_file(marker(&to.data_dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("could not remove the marker: {e}")),
            _ => {}
        }
    }
    Ok(report)
}

/// Copies apps' SQLite databases, and those the trash keeps, from the
/// site's volume to another directory; never over a file that is there.
fn databases(volume: &Path, to: &Path, budget: &mut Budget) -> Result<(), String> {
    if volume == to {
        return Ok(());
    }
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
        for entry in std::fs::read_dir(volume.join(&dir)).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) || !(name == "data.db" || name.starts_with("data.db-")) {
                continue;
            }
            let target = to.join(&dir).join(&name);
            if target.exists() {
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
}
