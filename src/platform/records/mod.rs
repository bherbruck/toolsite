//! Small per-app records that are not credentials: an app's settings (sealed
//! by the caller before they arrive here), the tools it declares, its own
//! migration ladder, the repository it mirrors to, and the site's GitHub App
//! installations.
//!
//! Two implementations: `files`, today's sidecars under `DATA_DIR`
//! (`<app>.secrets`, `<app>.tools`, `<app>.migrations`, `<app>.repo` and
//! `.site/github.json`, the default), and `postgres`, schema `platform`,
//! when the site runs on `DATABASE_URL`. The records are kept as their
//! owners shape them: settings as name to sealed value, the rest as the JSON
//! text their module writes. What a record means stays with its module.
//!
//! Every change is one held step. On files that is a lock per sidecar in this
//! process and a temporary file renamed into place, so a reader sees the old
//! record or the new one and never half of one; on Postgres it is a row per
//! setting, or `select ... for update` on the record's row.
//!
//! A removal takes an app's records out with `retire_blocking`, which hands
//! back each as the sidecar file it would have been, for the trash to keep.

pub mod files;
pub mod postgres;

#[cfg(test)]
mod conformance;

use crate::{config::Config, state::Backend};
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};

/// A change to one record, run while it is held: given the stored text, if
/// any, answers the text to store. An error leaves the record as it was and
/// is returned.
pub type DocEdit<'a> = Box<dyn FnOnce(Option<&str>) -> Result<String, String> + Send + 'a>;

/// Where per-app records live. App names arrive validated by their owner
/// module; the store does not judge them.
#[async_trait]
pub trait AppRecords: Send + Sync {
    /// An app's settings: name to sealed value.
    async fn settings(&self, app: &str) -> Result<BTreeMap<String, String>, String>;
    /// `settings` for a handler's host call, on a blocking thread.
    fn settings_blocking(&self, app: &str) -> Result<BTreeMap<String, String>, String>;
    /// Stores a sealed value under `name`, replacing any, or removes it with
    /// none. Answers whether there was a value before.
    async fn set_setting(&self, app: &str, name: &str, sealed: Option<&str>) -> Result<bool, String>;

    /// The tools an app declares, as JSON text, if it declares any.
    async fn tools(&self, app: &str) -> Result<Option<String>, String>;
    /// Replaces an app's tools; none removes them.
    async fn set_tools(&self, app: &str, tools: Option<&str>) -> Result<(), String>;
    /// Every app with tools, sorted.
    async fn apps_with_tools(&self) -> Result<Vec<String>, String>;

    /// The app's migration ladder as JSON text, if it has one. Blocking:
    /// migrations run on a blocking thread, beside the app's database.
    fn migrations_blocking(&self, app: &str) -> Result<Option<String>, String>;
    fn set_migrations_blocking(&self, app: &str, ladder: &str) -> Result<(), String>;

    /// The app's repository link as JSON text, live or disconnected.
    async fn repo_link(&self, app: &str) -> Result<Option<String>, String>;
    /// Runs `edit` on the stored link with it held and stores what it
    /// answers. Of any number of changes at once, every one lands.
    async fn update_repo_link(&self, app: &str, edit: DocEdit<'_>) -> Result<String, String>;
    /// Every app's link, by app, sorted.
    async fn repo_links(&self) -> Result<Vec<(String, String)>, String>;

    /// The GitHub App's installations as JSON text, if any were fetched.
    async fn installations(&self) -> Result<Option<String>, String>;
    async fn set_installations(&self, installations: &str) -> Result<(), String>;

    /// Takes every record of `app` out of the store, as a removal does, and
    /// answers each as the sidecar it is on files: (extension, contents).
    /// Nothing is destroyed: files leave their sidecars for the trash to
    /// move; Postgres keeps the rows in `platform.removed_records` besides.
    fn retire_blocking(&self, app: &str, at: u64) -> Result<Vec<(&'static str, String)>, String>;
}

/// The records this site's backend keeps. Cheap: a path or a pool handle.
pub fn of(config: &Config) -> Arc<dyn AppRecords> {
    match &config.stores.backend {
        Backend::Files => Arc::new(files::Files::new(config.data_dir.clone())),
        Backend::Postgres(postgres) => Arc::new(postgres::Postgres::new(postgres.pool.clone())),
    }
}

/// Pretty JSON, as every sidecar has always been written.
pub(crate) fn pretty<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}
