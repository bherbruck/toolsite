//! The catalog: what is published, where, and how. Each page's and app's
//! `PageMeta`, its notes and its generation; the project
//! tree and the record of a move under way; every host label issued; and
//! the site's one-time markers.
//!
//! Two implementations: `files`, today's sidecars under `DATA_DIR` (the
//! default), and `postgres`, schema `platform`, when the site runs on
//! `DATABASE_URL`. The rules about what a meta means stay with their
//! modules; the catalog only keeps it.
//!
//! A change to a meta is one call, `update_meta(slug, edit)`: the edit runs
//! with the stored meta held, so two changes at once both land. On files the
//! hold is a lock per slug in this process and the write is a temporary file
//! and a rename; on Postgres it is `select ... for update` in a transaction.
//! Reading a meta, changing it and writing it back as three steps loses
//! whichever of two concurrent changes wrote first, and a lost `hidden` or
//! `gate` opens what was closed.
//!
//! The project tree changes the same way, as one `update_folders` call
//! (one writer at a time: a lock and a rename on files, an advisory lock
//! and the difference written in one transaction on Postgres). A project
//! move holds `hold_relocations` from its first check to its journal being
//! cleared, so two moves, or two runners resuming one, take turns. A host
//! label is chosen and issued under a lock too, and on Postgres the label's
//! primary key refuses a second app besides.
//!
//! The catalog never reaches the sockets or instances a change affects:
//! `update_meta` here tells `config.app_events()`, which closes them.

pub mod files;
pub mod postgres;

#[cfg(test)]
mod conformance;

use crate::{
    config::Config,
    content::store::{Folder, PageMeta},
    state::{
        events::{AppChange, AppEvents},
        Backend,
    },
};
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};

/// A change to one meta, run while the catalog holds it. An error leaves
/// the stored meta as it was and is returned.
pub type MetaEdit<'a> = Box<dyn FnOnce(&mut PageMeta) -> Result<(), String> + Send + 'a>;

/// A change to the whole project tree, run while the catalog holds it. An
/// error leaves the tree as it was and is returned.
pub type FoldersEdit<'a> = Box<dyn FnOnce(&mut Vec<Folder>) -> Result<(), String> + Send + 'a>;

/// Chooses an app's host label from every label issued so far (label to
/// app), while the catalog holds the list. Pure: it runs under a lock, so
/// it reads nothing else.
pub type LabelChoice<'a> = Box<dyn FnOnce(&BTreeMap<String, String>) -> String + Send + 'a>;

/// A project move that has begun and not finished.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Relocation {
    pub from: String,
    pub to: String,
}

/// Project moves held: no other move, here or on another runner, starts or
/// resumes until this is released or dropped.
#[async_trait]
pub trait Held: Send {
    /// Lets the next move in. A hold that cannot be released cleanly is
    /// let go some other way (its connection closed), never kept.
    async fn release(self: Box<Self>);
}

/// Project moves, app moves and project creations and removals that may
/// wait for their turn in one process at once. Each is an admin's request;
/// past this many, the next is refused rather than queued, so a burst of
/// them cannot pile up requests (or, on Postgres, connections) without end.
pub const MAX_WAITING_MOVES: usize = 32;

/// One site's queue of moves in this process: its turn, and how many wait.
#[derive(Default)]
struct Turns {
    lock: Arc<tokio::sync::Mutex<()>>,
    waiting: std::sync::atomic::AtomicUsize,
}

/// The queues, by `DATA_DIR`: one per site, which in a server is one.
static TURNS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Arc<Turns>>>> =
    std::sync::LazyLock::new(Default::default);

/// This process's turn at moving `data_dir`'s projects, waiting behind at
/// most `MAX_WAITING_MOVES` others.
async fn take_turn(data_dir: &std::path::Path) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
    use std::sync::atomic::Ordering;
    let turns = TURNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(data_dir.to_path_buf())
        .or_default()
        .clone();
    if let Ok(turn) = turns.lock.clone().try_lock_owned() {
        return Ok(turn);
    }
    /// Counts one waiter for as long as it waits, however the wait ends.
    struct Waiting(Arc<Turns>);
    impl Drop for Waiting {
        fn drop(&mut self) {
            self.0.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }
    if turns.waiting.fetch_add(1, Ordering::SeqCst) >= MAX_WAITING_MOVES {
        turns.waiting.fetch_sub(1, Ordering::SeqCst);
        return Err("too many project changes are waiting for one another; try again in a moment".into());
    }
    let waiting = Waiting(turns.clone());
    let turn = turns.lock.clone().lock_owned().await;
    drop(waiting);
    Ok(turn)
}

/// What a removal took out of the catalog, for the trash to keep beside the
/// files it moved.
#[derive(Debug, Clone, PartialEq)]
pub struct Retired {
    /// Each slug at or under the removed one that had a meta or notes, with
    /// the meta as stored (JSON) and the notes.
    pub pages: Vec<(String, Option<String>, Option<String>)>,
}

/// Where pages' and apps' metas, notes and generations live. Slugs arrive
/// validated (`slug::valid_slug`); the catalog does not judge them.
#[async_trait]
pub trait Catalog: Send + Sync {
    /// The stored meta, or the default for a slug with none.
    async fn meta(&self, slug: &str) -> Result<PageMeta, String>;
    /// `meta` for a synchronous caller: a wasm host call, a migration, a
    /// host label being read while a request is routed.
    fn meta_blocking(&self, slug: &str) -> Result<PageMeta, String>;
    /// Runs `edit` on the stored meta (the default when there is none) with
    /// it held, stores the result and returns it. Of any number of calls at
    /// once, every edit lands.
    async fn update_meta(&self, slug: &str, edit: MetaEdit<'_>) -> Result<PageMeta, String>;
    /// `update_meta` for a synchronous caller.
    fn update_meta_blocking(&self, slug: &str, edit: MetaEdit<'_>) -> Result<PageMeta, String>;
    /// Which publish of the app's content is current, 0 for none. Caches
    /// key on it, so a publish makes their old entries unreachable rather
    /// than needing a message to every runner.
    async fn generation(&self, app: &str) -> Result<u64, String>;
    /// Counts one more publish and returns the new generation: larger than
    /// any this catalog handed out for the name before, to an app since
    /// removed included, so a name published again never meets an old
    /// cache.
    async fn bump_generation(&self, app: &str) -> Result<u64, String>;
    /// What the last agent left for the next one, if anything.
    async fn notes(&self, slug: &str) -> Result<Option<String>, String>;
    async fn set_notes(&self, slug: &str, notes: &str) -> Result<(), String>;
    /// Takes the metas and notes of `slug` and everything under it out of
    /// the catalog, as a removal does, and answers what they were. Nothing
    /// is destroyed: the files backend leaves its sidecars for the trash to
    /// move, Postgres keeps the rows in `platform.removed_pages`.
    fn retire_blocking(&self, slug: &str, at: u64) -> Result<Retired, String>;

    /// The project tree, sorted by path.
    async fn folders(&self) -> Result<Vec<Folder>, String>;
    /// `folders` for a synchronous caller: every scope check reads the
    /// locked projects from one.
    fn folders_blocking(&self) -> Result<Vec<Folder>, String>;
    /// Runs `edit` on the whole tree with it held and stores the result.
    /// Of any number of calls at once, every edit lands.
    async fn update_folders(&self, edit: FoldersEdit<'_>) -> Result<Vec<Folder>, String>;

    /// The project move under way, if one is recorded. An unreadable record
    /// is an error, not "none": a move left half done must be noticed.
    async fn relocation(&self) -> Result<Option<Relocation>, String>;
    fn relocation_blocking(&self) -> Result<Option<Relocation>, String>;
    /// Records that `from` is about to become `to`, replacing any record.
    async fn begin_relocation(&self, from: &str, to: &str) -> Result<(), String>;
    /// Clears the record: the move is done.
    async fn end_relocation(&self) -> Result<(), String>;
    /// Waits until no other move is running, here or on any runner, and
    /// holds moves until the answer is released.
    async fn hold_relocations(&self) -> Result<Box<dyn Held>, String>;

    /// The app a host label was issued to, if it was.
    fn label_owner_blocking(&self, label: &str) -> Result<Option<String>, String>;
    /// With every label issued held, asks `choose` for `app`'s label and,
    /// when `record`, issues it to the app. A label already issued to
    /// another app is never issued again: on Postgres its primary key
    /// refuses the second app even if `choose` asked for it.
    fn assign_label_blocking(&self, app: &str, record: bool, choose: LabelChoice<'_>) -> Result<String, String>;

    /// A one-time marker's value, if it was set.
    async fn flag(&self, name: &str) -> Result<Option<String>, String>;
    async fn set_flag(&self, name: &str, value: &str) -> Result<(), String>;
}

/// The catalog this site's backend keeps. Cheap: it holds a path or a pool
/// handle, and opens nothing until a method runs.
pub fn of(config: &Config) -> Arc<dyn Catalog> {
    match &config.stores.backend {
        Backend::Files => Arc::new(files::Files::new(config.data_dir.clone())),
        Backend::Postgres(postgres) => Arc::new(postgres::Postgres::new(postgres.pool.clone(), config.data_dir.clone())),
    }
}

/// The app a slug belongs to: its first segment.
fn app_of(slug: &str) -> &str {
    slug.split('/').next().unwrap_or(slug)
}

/// A slug's meta. A catalog that cannot be read answers a closed meta, so
/// an outage hides an app rather than opening it. The files backend never
/// fails here: a missing or unreadable sidecar is the default, as always.
pub async fn meta(config: &Config, slug: &str) -> PageMeta {
    match of(config).meta(slug).await {
        Ok(meta) => meta,
        Err(why) => {
            tracing::warn!(slug, %why, "a meta could not be read; it is treated as hidden");
            PageMeta::closed()
        }
    }
}

/// `meta` for a synchronous caller.
pub fn meta_blocking(config: &Config, slug: &str) -> PageMeta {
    match of(config).meta_blocking(slug) {
        Ok(meta) => meta,
        Err(why) => {
            tracing::warn!(slug, %why, "a meta could not be read; it is treated as hidden");
            PageMeta::closed()
        }
    }
}

fn changed(config: &Config, slug: &str, meta: &PageMeta) {
    config
        .app_events()
        .app_changed(AppChange { app: app_of(slug), hidden: meta.hidden, removed: false });
}

/// Changes a slug's meta in one held step and tells the app's sockets and
/// instances. Answers the meta as stored.
pub async fn update_meta(
    config: &Config,
    slug: &str,
    edit: impl FnOnce(&mut PageMeta) -> Result<(), String> + Send,
) -> Result<PageMeta, String> {
    let meta = of(config).update_meta(slug, Box::new(edit)).await?;
    changed(config, slug, &meta);
    Ok(meta)
}

/// `update_meta` for a synchronous caller.
pub fn update_meta_blocking(
    config: &Config,
    slug: &str,
    edit: impl FnOnce(&mut PageMeta) -> Result<(), String> + Send,
) -> Result<PageMeta, String> {
    let meta = of(config).update_meta_blocking(slug, Box::new(edit))?;
    changed(config, slug, &meta);
    Ok(meta)
}

/// A slug's notes; none when there are none or they cannot be read.
pub async fn notes(config: &Config, slug: &str) -> Option<String> {
    match of(config).notes(slug).await {
        Ok(notes) => notes,
        Err(why) => {
            tracing::warn!(slug, %why, "notes could not be read");
            None
        }
    }
}

pub async fn set_notes(config: &Config, slug: &str, notes: &str) -> Result<(), String> {
    of(config).set_notes(slug, notes).await
}

/// Every published slug, from the files the site keeps; empty, logged,
/// when they cannot be listed.
pub async fn slugs(config: &Config) -> Vec<String> {
    crate::content::files::slugs(config).await.unwrap_or_else(|why| {
        tracing::warn!(%why, "the published slugs could not be listed");
        Vec::new()
    })
}

/// Every app by its top-level name; empty, logged, when the files cannot
/// be listed.
pub async fn apps(config: &Config) -> Vec<String> {
    apps_among(slugs(config).await)
}

/// The top-level names among `slugs`, sorted, each once.
fn apps_among(slugs: Vec<String>) -> Vec<String> {
    let mut apps: Vec<String> = slugs.into_iter().map(|slug| app_of(&slug).to_string()).collect();
    apps.sort();
    apps.dedup();
    apps
}

/// The project tree; empty, logged, when it cannot be read. Empty closes
/// rather than opens: an app whose project is not in the tree is restricted,
/// and a move or a new project inside one finds no parent.
pub async fn folders(config: &Config) -> Vec<Folder> {
    of(config).folders().await.unwrap_or_else(|why| {
        tracing::warn!(%why, "the project tree could not be read");
        Vec::new()
    })
}

/// `folders` for a synchronous caller.
pub fn folders_blocking(config: &Config) -> Vec<Folder> {
    of(config).folders_blocking().unwrap_or_else(|why| {
        tracing::warn!(%why, "the project tree could not be read");
        Vec::new()
    })
}

/// Changes the project tree in one held step.
pub async fn update_folders(
    config: &Config,
    edit: impl FnOnce(&mut Vec<Folder>) -> Result<(), String> + Send,
) -> Result<Vec<Folder>, String> {
    of(config).update_folders(Box::new(edit)).await
}
