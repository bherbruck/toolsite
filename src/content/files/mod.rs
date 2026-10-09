//! Published bytes: pages, bundles, icons, stored sources and handlers, and
//! the pieces of an upload still arriving.
//!
//! Every file is named by a key, the path it has always had under
//! `DATA_DIR`: `<slug>.html`, `<app>/<path>`, `<slug>.icon` or
//! `<slug>/index.icon`, `<app>.source`, `<app>/handler.wasm`. A key is a
//! bundle asset path (`slug::valid_asset_path`), the same rule a blob key
//! keeps, so no segment of one starts with a dot and no key can name the
//! platform's own places: `.site/`, `.trash/`, `.tmp/` on the volume, or
//! `.toolsite/` in the bucket.
//!
//! Two implementations. `local` is today's layout under `DATA_DIR`, byte
//! for byte, for a site on files. `bucket` keeps every key at
//! `.toolsite/content/<key>` in the site's bucket, for a site on Postgres,
//! so a publish through one runner is there for every other.
//!
//! Readers ask for a key as of its app's generation, which the catalog
//! counts and every publish moves on, from one sequence that never repeats
//! a number. The bucket keeps what it fetched on this runner's disk under
//! that generation, by the digest of the key, so a publish on another runner
//! makes everything cached here unreachable at the next read, with no
//! message between them. Generation 0 is an app nothing was published to:
//! the bucket answers nothing for it without asking.
//!
//! Writers take the app's turn first (a lock in this process, and on
//! Postgres an advisory lock every runner shares), write, count the new
//! generation and only then let the next writer in. Two deploys at once
//! land one after the other, never interleaved, and no reader sees the new
//! generation before its files are there. A bundle still overlays: it
//! replaces the files it carries and keeps the rest.

pub mod bucket;
pub mod local;

#[cfg(test)]
mod conformance;

use crate::{
    config::Config,
    content::{
        bundle::Unpacked,
        slug::{valid_asset_path, valid_segment},
    },
    state::Backend,
};
use async_trait::async_trait;
use bytes::Bytes;
use std::{path::PathBuf, sync::Arc, time::Duration};

/// Longer than any key the platform makes: a slug, a bundle path and an
/// extension.
const MAX_KEY_LEN: usize = 1024;

/// Whether `key` may name a published file. The whole traversal defence for
/// keys: checked by every store before a key touches a path or an object.
pub fn valid_key(key: &str) -> bool {
    key.len() <= MAX_KEY_LEN && valid_asset_path(key)
}

pub(crate) fn check_key(key: &str) -> Result<(), String> {
    if valid_key(key) {
        Ok(())
    } else {
        Err(format!("{key:?} cannot name a published file"))
    }
}

/// A name inside a trash entry: a file, or a path within the app directory
/// the entry keeps. The same rule as a key.
pub(crate) fn check_trash_name(entry: &str, name: &str) -> Result<(), String> {
    if !valid_segment(entry) {
        return Err(format!("{entry:?} is not a trash entry"));
    }
    if !valid_key(name) {
        return Err(format!("{name:?} cannot name a file in the trash"));
    }
    Ok(())
}

/// The app a key belongs to: its first segment, without the extension of a
/// page or sidecar beside the apps (`shop.html`, `shop.source`).
pub fn app_of(key: &str) -> &str {
    match key.split_once('/') {
        Some((app, _)) => app,
        None => key.rsplit_once('.').map_or(key, |(stem, _)| stem),
    }
}

/// One name in a listing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
}

/// An app's turn at writing its files. Let go when released, or when
/// dropped, however the writer ends.
#[async_trait]
pub trait Turn: Send {
    async fn release(self: Box<Self>);
    /// `release` for a synchronous caller.
    fn release_blocking(self: Box<Self>);
}

/// Where published bytes live. Keys arrive as callers build them and are
/// checked here again; nothing below trusts them.
#[async_trait]
pub trait Files: Send + Sync {
    /// Whether reads depend on the app's generation. The volume answers
    /// from the file itself; the bucket from a cache keyed by it.
    fn by_generation(&self) -> bool;

    /// A file on this runner's disk holding what is stored at `key`, as of
    /// the app's `generation`; nothing when nothing is stored there.
    async fn local(&self, key: &str, generation: u64) -> Result<Option<PathBuf>, String>;
    /// `local` for a synchronous caller.
    fn local_blocking(&self, key: &str, generation: u64) -> Result<Option<PathBuf>, String>;
    /// Stores `bytes` at `key`, replacing what was there.
    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), String>;
    /// Removes what is at `key`; whether anything was.
    async fn delete(&self, key: &str) -> Result<bool, String>;
    /// Unpacks a bundle at `slug`, through `bundle`'s checks, over what is
    /// there. Blocking: decompression is.
    fn put_bundle_blocking(&self, slug: &str, body: &[u8]) -> Result<Unpacked, String>;
    /// The names directly inside `dir` (`""` for the top), platform names
    /// never among them.
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, String>;
    /// `list` for a synchronous caller.
    fn list_blocking(&self, dir: &str) -> Result<Vec<Entry>, String>;
    /// Whether anything was published at `app`, as of `generation`.
    async fn exists(&self, app: &str, generation: u64) -> Result<bool, String>;
    /// `exists` for a synchronous caller.
    fn exists_blocking(&self, app: &str, generation: u64) -> Result<bool, String>;
    /// Takes `app` for whoever published first, when that publish wrote no
    /// file of its own (a manifest, migrations).
    async fn hold_name(&self, app: &str) -> Result<(), String>;
    /// Waits for `app`'s turn at writing, here and on every runner.
    async fn take_turn(&self, app: &str) -> Result<Box<dyn Turn>, String>;
    /// `take_turn` for a synchronous caller, on a blocking thread.
    fn take_turn_blocking(&self, app: &str) -> Result<Box<dyn Turn>, String>;

    /// A new trash entry for `slug`, removed at `at`. Never one in use.
    fn trash_entry_blocking(&self, slug: &str, at: u64) -> Result<String, String>;
    /// Writes a copy of something taken from elsewhere (a record, a meta)
    /// into the entry.
    fn trash_write_blocking(&self, entry: &str, name: &str, bytes: &[u8]) -> Result<(), String>;
    /// Moves what is at `key` (a file, or everything under `key/`) into the
    /// entry as `name`; whether anything moved.
    fn trash_move_blocking(&self, entry: &str, key: &str, name: &str) -> Result<bool, String>;
    fn trash_read_blocking(&self, entry: &str, name: &str) -> Result<Option<Vec<u8>>, String>;
    /// Moves `name` out of the entry back to `key`; whether it was there.
    fn untrash_blocking(&self, entry: &str, name: &str, key: &str) -> Result<bool, String>;
    /// Every entry, oldest first.
    fn trash_entries_blocking(&self) -> Result<Vec<String>, String>;
    /// Lets go of an entry nothing was put in.
    fn trash_discard_blocking(&self, entry: &str);

    /// Stores one piece of an inline upload, named by the upload's digest.
    fn chunk_put_blocking(&self, upload: &str, index: u32, bytes: &[u8]) -> Result<(), String>;
    fn chunk_read_blocking(&self, upload: &str, index: u32) -> Result<Option<Vec<u8>>, String>;
    /// Removes every piece of an upload.
    fn chunks_clear_blocking(&self, upload: &str);
    /// Removes the pieces of every upload older than `age`.
    fn chunks_sweep_blocking(&self, age: Duration);
}

/// The store this site's backend keeps published files in. Cheap: it
/// holds a path or a bucket handle and opens nothing until a method runs.
/// A Postgres site with no bucket gets a store that refuses everything;
/// the boot guard keeps a real one from starting that way.
pub fn of(config: &Config) -> Arc<dyn Files> {
    match &config.stores.backend {
        Backend::Files => Arc::new(local::Local::new(config.data_dir.clone())),
        Backend::Postgres(postgres) => {
            let s3 = match &config.blobs.backend {
                crate::runtime::blobs::Backend::S3(s3) => Some(s3.clone()),
                crate::runtime::blobs::Backend::Local => None,
            };
            Arc::new(bucket::Bucket::new(s3, config.data_dir.clone(), Some(postgres.pool.clone())))
        }
    }
}

/// An app's generation; 0, logged, when the catalog cannot say, so an
/// outage reads as nothing published rather than as an old cache.
pub async fn generation(config: &Config, app: &str) -> u64 {
    crate::content::catalog::of(config).generation(app).await.unwrap_or_else(|why| {
        tracing::warn!(app, %why, "an app's generation could not be read; its files are treated as absent");
        0
    })
}

/// The generation this site's store reads `app` at: the app's own where
/// the store keys on it, else 0 without asking the catalog.
pub async fn reading(config: &Config, app: &str) -> u64 {
    generation_for(config, &*of(config), app).await
}

async fn generation_for(config: &Config, files: &dyn Files, app: &str) -> u64 {
    if files.by_generation() { generation(config, app).await } else { 0 }
}

fn generation_for_blocking(config: &Config, files: &dyn Files, app: &str) -> u64 {
    if files.by_generation() { crate::state::wait_in_place(generation(config, app)) } else { 0 }
}

/// A local file holding `key`'s bytes as published now; nothing when there
/// are none or the store cannot say (logged).
pub async fn path(config: &Config, key: &str) -> Option<PathBuf> {
    let files = of(config);
    let generation = generation_for(config, &*files, app_of(key)).await;
    answer(key, files.local(key, generation).await)
}

/// `path` as of a generation already read, so one request reads one.
pub async fn path_at(config: &Config, generation: u64, key: &str) -> Option<PathBuf> {
    answer(key, of(config).local(key, generation).await)
}

/// `path` for a synchronous caller.
pub fn path_blocking(config: &Config, key: &str) -> Option<PathBuf> {
    let files = of(config);
    let generation = generation_for_blocking(config, &*files, app_of(key));
    answer(key, files.local_blocking(key, generation))
}

fn answer(key: &str, found: Result<Option<PathBuf>, String>) -> Option<PathBuf> {
    found.unwrap_or_else(|why| {
        tracing::warn!(key, %why, "a published file could not be read");
        None
    })
}

/// The whole file at `key`, for something small enough to hold.
pub async fn read(config: &Config, key: &str) -> Option<Vec<u8>> {
    tokio::fs::read(path(config, key).await?).await.ok()
}

/// `read` for a synchronous caller.
pub fn read_blocking(config: &Config, key: &str) -> Option<Vec<u8>> {
    std::fs::read(path_blocking(config, key)?).ok()
}

/// Counts a publish of `app`, under its turn.
async fn count(config: &Config, app: &str) -> Result<u64, String> {
    crate::content::catalog::of(config).bump_generation(app).await
}

/// Stores `bytes` at `key` as a publish of its app: in the app's turn,
/// then counted.
pub async fn publish(config: &Config, key: &str, bytes: Bytes) -> Result<(), String> {
    check_key(key)?;
    let files = of(config);
    let app = app_of(key);
    let turn = files.take_turn(app).await?;
    let stored = files.put(key, bytes).await;
    let counted = count(config, app).await;
    turn.release().await;
    stored?;
    counted.map(|_| ())
}

/// Unpacks a bundle at `slug` as a publish of its app.
pub async fn publish_bundle(config: &Config, slug: &str, body: Bytes) -> Result<Unpacked, String> {
    check_key(slug)?;
    let files = of(config);
    let app = app_of(slug).to_string();
    let turn = files.take_turn(&app).await?;
    let (unpacking, owned) = (files.clone(), slug.to_string());
    let unpacked = tokio::task::spawn_blocking(move || unpacking.put_bundle_blocking(&owned, &body))
        .await
        .unwrap_or_else(|e| Err(format!("unpacking failed: {e}")));
    // Counted even when the bundle was refused part way: whatever it wrote
    // before the refusal is published now, and no cache may hide it.
    let counted = count(config, &app).await;
    turn.release().await;
    let unpacked = unpacked?;
    counted.map(|_| unpacked)
}

/// Removes what is at `key` as a publish of its app; whether anything was.
pub async fn unpublish(config: &Config, key: &str) -> Result<bool, String> {
    check_key(key)?;
    let files = of(config);
    let app = app_of(key);
    let turn = files.take_turn(app).await?;
    let removed = files.delete(key).await;
    let counted = match removed {
        Ok(true) => count(config, app).await.map(|_| ()),
        _ => Ok(()),
    };
    turn.release().await;
    let removed = removed?;
    counted.map(|()| removed)
}

/// Whether anything was published at `app`. A store that cannot say
/// answers no, logged.
pub async fn app_exists(config: &Config, app: &str) -> bool {
    let files = of(config);
    let generation = generation_for(config, &*files, app).await;
    files.exists(app, generation).await.unwrap_or_else(|why| {
        tracing::warn!(app, %why, "whether an app exists could not be read");
        false
    })
}

/// `app_exists` for a synchronous caller.
pub fn app_exists_blocking(config: &Config, app: &str) -> bool {
    let files = of(config);
    let generation = generation_for_blocking(config, &*files, app);
    files.exists_blocking(app, generation).unwrap_or_else(|why| {
        tracing::warn!(app, %why, "whether an app exists could not be read");
        false
    })
}

/// Takes `app` for its first publisher, when that publish wrote no file.
pub async fn hold_name(config: &Config, app: &str) -> Result<(), String> {
    let files = of(config);
    let turn = files.take_turn(app).await?;
    let held = files.hold_name(app).await;
    let counted = count(config, app).await;
    turn.release().await;
    held?;
    counted.map(|_| ())
}

/// Every top-level name that can be an app: a directory or a loose page.
fn names_from(entries: Vec<Entry>) -> Vec<String> {
    let mut names: Vec<String> = entries
        .into_iter()
        .filter_map(|entry| if entry.dir { Some(entry.name) } else { entry.name.strip_suffix(".html").map(str::to_string) })
        .filter(|name| valid_segment(name))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Every top-level name that can be an app; none, logged, when the store
/// cannot list them.
pub fn names_blocking(config: &Config) -> Vec<String> {
    match of(config).list_blocking("") {
        Ok(entries) => names_from(entries),
        Err(why) => {
            tracing::warn!(%why, "the published names could not be listed");
            Vec::new()
        }
    }
}

/// Every published slug, the way the index lists them: loose pages, and an
/// app by its root only, since its inner pages belong to its own
/// navigation. A directory with no index page is a group of pages, each
/// listed.
pub async fn slugs(config: &Config) -> Result<Vec<String>, String> {
    slugs_in(&*of(config)).await
}

async fn slugs_in(files: &dyn Files) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        let entries = files.list(&dir).await?;
        let join = |name: &str| if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
        if !dir.is_empty() && entries.iter().any(|entry| !entry.dir && entry.name == "index.html") {
            out.push(dir.clone());
            continue;
        }
        for entry in entries {
            if entry.dir {
                dirs.push(join(&entry.name));
            } else if let Some(stem) = entry.name.strip_suffix(".html")
                && valid_segment(stem)
            {
                out.push(join(stem));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|slug| seen.insert(slug.clone()));
    Ok(out)
}

/// The pages directly inside an app, by name, with a local file for each.
pub async fn pages_in(config: &Config, app: &str) -> Vec<(String, PathBuf)> {
    if !valid_segment(app) {
        return Vec::new();
    }
    let files = of(config);
    let generation = generation_for(config, &*files, app).await;
    let entries = files.list(app).await.unwrap_or_else(|why| {
        tracing::warn!(app, %why, "an app's pages could not be listed");
        Vec::new()
    });
    let mut pages = Vec::new();
    for entry in entries.into_iter().filter(|entry| !entry.dir) {
        let Some(stem) = entry.name.strip_suffix(".html") else { continue };
        if let Some(path) = answer(&entry.name, files.local(&format!("{app}/{}", entry.name), generation).await) {
            pages.push((stem.to_string(), path));
        }
    }
    pages
}
