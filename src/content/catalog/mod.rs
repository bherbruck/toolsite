//! The catalog: what is published, where, and how. Each page's and app's
//! `PageMeta`, its notes, its generation, and which slugs exist.
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
//! The catalog never reaches the sockets or instances a change affects:
//! `update_meta` here tells `config.app_events()`, which closes them.

pub mod files;
pub mod postgres;

#[cfg(test)]
mod conformance;

use crate::{
    config::Config,
    content::store::PageMeta,
    state::{
        events::{AppChange, AppEvents},
        Backend,
    },
};
use async_trait::async_trait;
use std::sync::Arc;

/// A change to one meta, run while the catalog holds it. An error leaves
/// the stored meta as it was and is returned.
pub type MetaEdit<'a> = Box<dyn FnOnce(&mut PageMeta) -> Result<(), String> + Send + 'a>;

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
    /// How many times the app's content has been published, as this catalog
    /// counts. Caches key on it, so a publish makes their old entries
    /// unreachable rather than needing a message to every runner.
    async fn generation(&self, app: &str) -> Result<u64, String>;
    /// Counts one more publish and returns the new generation.
    async fn bump_generation(&self, app: &str) -> Result<u64, String>;
    /// What the last agent left for the next one, if anything.
    async fn notes(&self, slug: &str) -> Result<Option<String>, String>;
    async fn set_notes(&self, slug: &str, notes: &str) -> Result<(), String>;
    /// Every published slug: loose pages, and each app by its root only.
    async fn slugs(&self) -> Result<Vec<String>, String>;
    /// Every app, by its top-level name, sorted.
    async fn apps(&self) -> Result<Vec<String>, String>;
    /// Takes the metas and notes of `slug` and everything under it out of
    /// the catalog, as a removal does, and answers what they were. Nothing
    /// is destroyed: the files backend leaves its sidecars for the trash to
    /// move, Postgres keeps the rows in `platform.removed_pages`.
    fn retire_blocking(&self, slug: &str, at: u64) -> Result<Retired, String>;
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

/// Every published slug; empty, logged, when the catalog cannot be read.
pub async fn slugs(config: &Config) -> Vec<String> {
    of(config).slugs().await.unwrap_or_else(|why| {
        tracing::warn!(%why, "the published slugs could not be listed");
        Vec::new()
    })
}

/// Every app by its top-level name; empty, logged, when the catalog cannot
/// be read.
pub async fn apps(config: &Config) -> Vec<String> {
    of(config).apps().await.unwrap_or_else(|why| {
        tracing::warn!(%why, "the apps could not be listed");
        Vec::new()
    })
}

/// The top-level names among `slugs`, sorted, each once.
pub(crate) fn apps_among(slugs: Vec<String>) -> Vec<String> {
    let mut apps: Vec<String> = slugs.into_iter().map(|slug| app_of(&slug).to_string()).collect();
    apps.sort();
    apps.dedup();
    apps
}
