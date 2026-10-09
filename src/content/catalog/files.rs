//! The catalog as files under `DATA_DIR`: `<slug>.meta` and `<slug>.notes`
//! beside a page, or `index.meta` and `index.notes` inside an app's
//! directory, as they always were.
//!
//! A meta changes under a lock per slug held by this process, and is written
//! to a temporary file and renamed into place, so a reader sees the old meta
//! or the new one and a crash never leaves half of one. The temporary file's
//! name starts with a dot: no slug or bundle path can name it, so it is
//! never served even if a crash leaves it behind.
//!
//! The site-wide records live under `.site/`, which no slug can name:
//! `projects.json` (the tree), `relocating.json` (a move under way),
//! `labels.json` (every host label issued) and one file per marker. Each is
//! changed under a lock of its own in this process and written the same way.

use super::{Catalog, FoldersEdit, Held, LabelChoice, MetaEdit, Relocation, Retired};
use crate::content::store::{current_words, Folder, PageMeta};
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, HashMap},
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

    pub(super) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn lock_for(&self, slug: &str) -> Arc<Mutex<()>> {
        held(&LOCKS).entry(self.data_dir.join(slug)).or_default().clone()
    }

    /// A file under `.site/`. Its lock shares the map with the slugs': a
    /// slug never starts with a dot, so the two never meet.
    fn site_file(&self, name: &str) -> PathBuf {
        self.data_dir.join(".site").join(name)
    }

    /// The tree, empty when there is no file yet. A file that cannot be read
    /// or parsed is an error, not an empty tree: an empty tree forgets every
    /// lock, which opens the rows a lock was ignoring, and writing an edit
    /// over it would lose every project.
    fn read_folders(&self) -> Result<Vec<Folder>, String> {
        read_site_json(&self.site_file("projects.json"), "the project tree")
    }

    fn change_folders(&self, edit: FoldersEdit<'_>) -> Result<Vec<Folder>, String> {
        let path = self.site_file("projects.json");
        let lock = held(&LOCKS).entry(path.clone()).or_default().clone();
        let _one_writer = held(&lock);
        let mut folders = self.read_folders()?;
        edit(&mut folders)?;
        let json = serde_json::to_string_pretty(&folders).map_err(|e| e.to_string())?;
        write_aside(&path, json.as_bytes())?;
        Ok(folders)
    }

    fn read_relocation(&self) -> Result<Option<Relocation>, String> {
        match std::fs::read_to_string(self.site_file("relocating.json")) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|_| "a project move record could not be read".to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("a project move record could not be read: {e}")),
        }
    }

    /// Every label issued, by label; none when there is no file yet. A file
    /// that cannot be read is an error: a removed app's label is kept only
    /// here, so reading it as empty would issue that label again, and
    /// writing the next label over it would forget every one.
    fn read_labels(&self) -> Result<BTreeMap<String, String>, String> {
        read_site_json(&self.site_file("labels.json"), "the issued host labels")
    }

    /// A marker's file: its name is a constant of the caller's, checked
    /// here anyway, since it is joined to a path.
    fn flag_file(&self, name: &str) -> Result<PathBuf, String> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            return Err(format!("{name:?} is not a marker name"));
        }
        Ok(self.site_file(name))
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

/// A JSON record under `.site/`: its default when the file is not there,
/// an error when it is there and cannot be read or parsed.
fn read_site_json<T: serde::de::DeserializeOwned + Default>(path: &Path, what: &str) -> Result<T, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{what} could not be read: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("{what} could not be read: {e}")),
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

    async fn folders(&self) -> Result<Vec<Folder>, String> {
        self.read_folders()
    }

    fn folders_blocking(&self) -> Result<Vec<Folder>, String> {
        self.read_folders()
    }

    async fn update_folders(&self, edit: FoldersEdit<'_>) -> Result<Vec<Folder>, String> {
        self.change_folders(edit)
    }

    async fn relocation(&self) -> Result<Option<Relocation>, String> {
        self.read_relocation()
    }

    fn relocation_blocking(&self) -> Result<Option<Relocation>, String> {
        self.read_relocation()
    }

    async fn begin_relocation(&self, from: &str, to: &str) -> Result<(), String> {
        let json = serde_json::to_string(&Relocation { from: from.to_string(), to: to.to_string() })
            .map_err(|e| e.to_string())?;
        write_aside(&self.site_file("relocating.json"), json.as_bytes())
    }

    async fn end_relocation(&self) -> Result<(), String> {
        match std::fs::remove_file(self.site_file("relocating.json")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }

    async fn hold_relocations(&self) -> Result<Box<dyn Held>, String> {
        Ok(Box::new(MoveHold(super::take_turn(&self.data_dir).await?)))
    }

    fn label_owner_blocking(&self, label: &str) -> Result<Option<String>, String> {
        Ok(self.read_labels()?.remove(label))
    }

    /// A label issued to another app is refused, as Postgres's key refuses
    /// it. A list that cannot be written is logged and the label still
    /// given, as it always was: the app's meta keeps it, and the meta is
    /// what later assignments read besides the list.
    fn assign_label_blocking(&self, app: &str, record: bool, choose: LabelChoice<'_>) -> Result<String, String> {
        let path = self.site_file("labels.json");
        let lock = held(&LOCKS).entry(path.clone()).or_default().clone();
        let _one_writer = held(&lock);
        let mut labels = self.read_labels()?;
        let label = choose(&labels);
        if let Some(owner) = labels.get(&label).filter(|owner| *owner != app)
            && record
        {
            return Err(format!("the host label {label} was issued to {owner} already"));
        }
        if record && !labels.contains_key(&label) {
            labels.insert(label.clone(), app.to_string());
            let written = serde_json::to_string_pretty(&labels)
                .map_err(|e| e.to_string())
                .and_then(|json| write_aside(&path, json.as_bytes()));
            if let Err(why) = written {
                tracing::warn!(app, %why, "the list of issued host labels could not be written");
            }
        }
        Ok(label)
    }

    async fn flag(&self, name: &str) -> Result<Option<String>, String> {
        Ok(tokio::fs::read_to_string(self.flag_file(name)?).await.ok())
    }

    async fn set_flag(&self, name: &str, value: &str) -> Result<(), String> {
        write_aside(&self.flag_file(name)?, value.as_bytes())
    }
}

/// A hold on project moves in this process.
struct MoveHold(#[allow(dead_code)] tokio::sync::OwnedMutexGuard<()>);

#[async_trait]
impl Held for MoveHold {
    async fn release(self: Box<Self>) {}
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
