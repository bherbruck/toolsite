//! The catalog as files under `DATA_DIR`: `<slug>.meta` and `<slug>.notes`
//! beside a page, or `index.meta` and `index.notes` inside an app's
//! directory, as they always were.
//!
//! A meta changes under a lock per slug held by this process, and is written
//! to a temporary file and renamed into place, so a reader sees the old meta
//! or the new one and a crash never leaves half of one. The temporary file's
//! name starts with a dot: no slug or bundle path can name it, so it is
//! never served even if a crash leaves it behind.

use super::{Catalog, MetaEdit, Retired};
use crate::content::store::{current_words, PageMeta};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

pub struct Files {
    data_dir: PathBuf,
}

/// One lock per meta this process has changed, by `DATA_DIR` and slug. A
/// lock per slug rather than a few shared ones, so an edit that reads
/// another app's meta can never wait on itself.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);

/// Generations, by `DATA_DIR` and app. In memory: on files there is one
/// process, and its caches start empty, so counting from zero at boot is
/// enough to tell its own publishes apart.
static GENERATIONS: LazyLock<Mutex<HashMap<PathBuf, u64>>> = LazyLock::new(Default::default);

fn held<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Files {
    pub fn new(data_dir: PathBuf) -> Files {
        Files { data_dir }
    }

    fn lock_for(&self, slug: &str) -> Arc<Mutex<()>> {
        held(&LOCKS).entry(self.data_dir.join(slug)).or_default().clone()
    }

    /// Where a slug's sidecar of this kind is, or would be written: beside
    /// the page, or inside the app's directory. An existing file wins; with
    /// none, it goes wherever the page itself lives.
    fn sidecar(&self, slug: &str, extension: &str) -> PathBuf {
        let direct = self.data_dir.join(format!("{slug}.{extension}"));
        if direct.exists() {
            return direct;
        }
        let inner = self.data_dir.join(format!("{slug}/index.{extension}"));
        if inner.exists() {
            return inner;
        }
        if self.data_dir.join(format!("{slug}.html")).exists() {
            direct
        } else {
            inner
        }
    }

    /// The same as `sidecar`, without blocking the runtime on the lookups.
    async fn sidecar_async(&self, slug: &str, extension: &str) -> PathBuf {
        let exists = |path: PathBuf| async move { tokio::fs::metadata(&path).await.is_ok() };
        let direct = self.data_dir.join(format!("{slug}.{extension}"));
        if exists(direct.clone()).await {
            return direct;
        }
        let inner = self.data_dir.join(format!("{slug}/index.{extension}"));
        if exists(inner.clone()).await {
            return inner;
        }
        if exists(self.data_dir.join(format!("{slug}.html"))).await {
            direct
        } else {
            inner
        }
    }

    fn parse(text: &str) -> PageMeta {
        current_words(serde_json::from_str(text).unwrap_or_default())
    }

    fn read(&self, slug: &str) -> PageMeta {
        for candidate in [
            self.data_dir.join(format!("{slug}.meta")),
            self.data_dir.join(format!("{slug}/index.meta")),
        ] {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                return Files::parse(&text);
            }
        }
        PageMeta::default()
    }

    /// The whole change, synchronously, under the slug's lock. Holds no
    /// lock across an await, so the async form runs it as it is.
    fn update(&self, slug: &str, edit: MetaEdit<'_>) -> Result<PageMeta, String> {
        let lock = self.lock_for(slug);
        let _one_writer = held(&lock);
        let mut meta = self.read(slug);
        edit(&mut meta)?;
        let meta = current_words(meta);
        let json = serde_json::to_string(&meta).map_err(|e| e.to_string())?;
        write_aside(&self.sidecar(slug, "meta"), json.as_bytes())?;
        Ok(meta)
    }
}

/// Writes `bytes` to a dotted temporary file beside `path` and renames it
/// into place.
fn write_aside(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("a sidecar has no directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or("a sidecar has no name")?;
    let temp = parent.join(format!(".{name}.{}.part", crate::content::slug::random_token(8)));
    if let Err(e) = std::fs::write(&temp, bytes) {
        let _ = std::fs::remove_file(&temp);
        return Err(e.to_string());
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })
}

#[async_trait]
impl Catalog for Files {
    async fn meta(&self, slug: &str) -> Result<PageMeta, String> {
        let path = self.sidecar_async(slug, "meta").await;
        Ok(match tokio::fs::read_to_string(&path).await {
            Ok(text) => Files::parse(&text),
            Err(_) => PageMeta::default(),
        })
    }

    fn meta_blocking(&self, slug: &str) -> Result<PageMeta, String> {
        Ok(self.read(slug))
    }

    async fn update_meta(&self, slug: &str, edit: MetaEdit<'_>) -> Result<PageMeta, String> {
        self.update(slug, edit)
    }

    fn update_meta_blocking(&self, slug: &str, edit: MetaEdit<'_>) -> Result<PageMeta, String> {
        self.update(slug, edit)
    }

    async fn generation(&self, app: &str) -> Result<u64, String> {
        Ok(held(&GENERATIONS).get(&self.data_dir.join(app)).copied().unwrap_or(0))
    }

    async fn bump_generation(&self, app: &str) -> Result<u64, String> {
        let mut generations = held(&GENERATIONS);
        let generation = generations.entry(self.data_dir.join(app)).or_default();
        *generation += 1;
        Ok(*generation)
    }

    async fn notes(&self, slug: &str) -> Result<Option<String>, String> {
        Ok(tokio::fs::read_to_string(self.sidecar_async(slug, "notes").await).await.ok())
    }

    async fn set_notes(&self, slug: &str, notes: &str) -> Result<(), String> {
        let path = self.sidecar_async(slug, "notes").await;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
        }
        tokio::fs::write(path, notes).await.map_err(|e| e.to_string())
    }

    async fn slugs(&self) -> Result<Vec<String>, String> {
        let mut slugs = Vec::new();
        collect_slugs(&self.data_dir, String::new(), &mut slugs).await;
        Ok(slugs)
    }

    async fn apps(&self) -> Result<Vec<String>, String> {
        Ok(super::apps_among(self.slugs().await?))
    }

    /// Nothing to take out: the sidecars are files, and the trash moves
    /// them with the rest.
    fn retire_blocking(&self, _slug: &str, _at: u64) -> Result<Retired, String> {
        Ok(Retired { pages: Vec::new() })
    }
}

/// Every published slug under `dir`, the way the index lists them.
pub(crate) fn collect_slugs<'a>(
    dir: &'a Path,
    prefix: String,
    out: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    use tokio::fs;
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
            } else if path.extension().and_then(|e| e.to_str()) == Some("html")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
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
    })
}
