//! Published files on the volume, where they have always been: each key is
//! the path under `DATA_DIR`. The trash is `.trash/<entry>/` and an inline
//! upload's pieces are `.tmp/inline/<digest>/<index>.part`, both under a
//! dot no key can name.
//!
//! Generations mean nothing here: a file is read as it is now, and there
//! is one process, whose own compiled handlers are forgotten on a publish.

use super::{check_key, check_trash_name, Entry, Files, Turn};
use crate::content::bundle::Unpacked;
use async_trait::async_trait;
use bytes::Bytes;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

pub struct Local {
    data_dir: PathBuf,
}

/// Each app's turn at writing, by `DATA_DIR` and app.
static TURNS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

pub(super) fn turn_lock(root: &Path, app: &str) -> Arc<tokio::sync::Mutex<()>> {
    TURNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(root.join(app))
        .or_default()
        .clone()
}

/// Waits for `lock` on whatever thread this is: a blocking thread, a thread
/// of no runtime, or (in a test) a runtime's own, where tokio's blocking
/// wait would refuse. A turn is held for a handful of writes, so a short
/// sleep between tries costs little.
pub(super) fn lock_blocking(lock: Arc<tokio::sync::Mutex<()>>) -> tokio::sync::OwnedMutexGuard<()> {
    loop {
        if let Ok(guard) = lock.clone().try_lock_owned() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A turn held by this process alone.
pub(super) struct LocalTurn(#[allow(dead_code)] pub(super) tokio::sync::OwnedMutexGuard<()>);

#[async_trait]
impl Turn for LocalTurn {
    async fn release(self: Box<Self>) {}
    fn release_blocking(self: Box<Self>) {}
}

fn io(what: &str) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("could not {what}: {e}")
}

impl Local {
    pub fn new(data_dir: PathBuf) -> Local {
        Local { data_dir }
    }

    fn path(&self, key: &str) -> Result<PathBuf, String> {
        check_key(key)?;
        Ok(self.data_dir.join(key))
    }

    fn trash_root(&self) -> PathBuf {
        self.data_dir.join(".trash")
    }

    fn trash_path(&self, entry: &str, name: &str) -> Result<PathBuf, String> {
        check_trash_name(entry, name)?;
        Ok(self.trash_root().join(entry).join(name))
    }

    fn spool(&self, upload: &str) -> Result<PathBuf, String> {
        if upload.is_empty() || !upload.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{upload:?} does not name an upload"));
        }
        Ok(self.data_dir.join(".tmp").join("inline").join(upload))
    }

    fn found(path: PathBuf) -> Option<PathBuf> {
        path.is_file().then_some(path)
    }

    fn listing(&self, dir: &str) -> Result<Vec<Entry>, String> {
        let path = if dir.is_empty() { self.data_dir.clone() } else { self.path(dir)? };
        let Ok(entries) = std::fs::read_dir(&path) else { return Ok(Vec::new()) };
        let mut out: Vec<Entry> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                // .site, .trash, .tmp and an app's .blobs are the platform's.
                (!name.starts_with('.')).then(|| Entry { name, dir: entry.path().is_dir() })
            })
            .collect();
        out.sort();
        Ok(out)
    }

    fn present(&self, app: &str) -> bool {
        self.data_dir.join(app).is_dir() || self.data_dir.join(format!("{app}.html")).is_file()
    }
}

#[async_trait]
impl Files for Local {
    fn by_generation(&self) -> bool {
        false
    }

    async fn local(&self, key: &str, _generation: u64) -> Result<Option<PathBuf>, String> {
        let path = self.path(key)?;
        Ok(tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file()).then_some(path))
    }

    fn local_blocking(&self, key: &str, _generation: u64) -> Result<Option<PathBuf>, String> {
        Ok(Self::found(self.path(key)?))
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), String> {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io("make a directory"))?;
        }
        tokio::fs::write(&path, &bytes).await.map_err(io("write a file"))
    }

    async fn delete(&self, key: &str) -> Result<bool, String> {
        match tokio::fs::remove_file(self.path(key)?).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("could not remove a file: {e}")),
        }
    }

    fn put_bundle_blocking(&self, slug: &str, body: &[u8]) -> Result<Unpacked, String> {
        let dest = self.path(slug)?;
        std::fs::create_dir_all(&dest).map_err(io("make the app's directory"))?;
        crate::content::bundle::unpack_bundle(body, &dest, slug)
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>, String> {
        self.listing(dir)
    }

    fn list_blocking(&self, dir: &str) -> Result<Vec<Entry>, String> {
        self.listing(dir)
    }

    async fn exists(&self, app: &str, _generation: u64) -> Result<bool, String> {
        check_key(app)?;
        Ok(self.present(app))
    }

    fn exists_blocking(&self, app: &str, _generation: u64) -> Result<bool, String> {
        check_key(app)?;
        Ok(self.present(app))
    }

    async fn hold_name(&self, app: &str) -> Result<(), String> {
        tokio::fs::create_dir_all(self.path(app)?).await.map_err(io("make the app's directory"))
    }

    async fn take_turn(&self, app: &str) -> Result<Box<dyn Turn>, String> {
        Ok(Box::new(LocalTurn(turn_lock(&self.data_dir, app).lock_owned().await)))
    }

    fn take_turn_blocking(&self, app: &str) -> Result<Box<dyn Turn>, String> {
        Ok(Box::new(LocalTurn(lock_blocking(turn_lock(&self.data_dir, app)))))
    }

    /// Timestamped, so removing the same slug twice never writes over the
    /// first removal, which would be destroying data by another route; made
    /// here, so two removals in one second get a place each.
    fn trash_entry_blocking(&self, slug: &str, at: u64) -> Result<String, String> {
        let base = format!("{at}-{}", slug.replace('/', "-"));
        std::fs::create_dir_all(self.trash_root()).map_err(io("make the trash"))?;
        for n in 1.. {
            let entry = if n == 1 { base.clone() } else { format!("{base}-{n}") };
            match std::fs::create_dir(self.trash_root().join(&entry)) {
                Ok(()) => return Ok(entry),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("could not make a trash entry: {e}")),
            }
        }
        unreachable!("an unbounded search ends")
    }

    fn trash_write_blocking(&self, entry: &str, name: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.trash_path(entry, name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io("make a trash entry"))?;
        }
        std::fs::write(path, bytes).map_err(io("write into the trash"))
    }

    fn trash_move_blocking(&self, entry: &str, key: &str, name: &str) -> Result<bool, String> {
        let from = self.path(key)?;
        let to = self.trash_path(entry, name)?;
        if !from.is_dir() && !from.is_file() {
            return Ok(false);
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io("make a trash entry"))?;
        }
        std::fs::rename(&from, &to).map_err(io("move into the trash"))?;
        Ok(true)
    }

    fn trash_read_blocking(&self, entry: &str, name: &str) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(self.trash_path(entry, name)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("could not read from the trash: {e}")),
        }
    }

    fn untrash_blocking(&self, entry: &str, name: &str, key: &str) -> Result<bool, String> {
        let from = self.trash_path(entry, name)?;
        let to = self.path(key)?;
        if !from.exists() {
            return Ok(false);
        }
        // A directory a publish left empty holds nothing to put back over.
        let emptied = to.is_dir() && std::fs::remove_dir(&to).is_ok();
        if !emptied && to.exists() {
            return Err(format!("{key} is in use; nothing was put back over it"));
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io("make a directory"))?;
        }
        std::fs::rename(&from, &to).map_err(io("put a file back"))?;
        Ok(true)
    }

    fn trash_entries_blocking(&self) -> Result<Vec<String>, String> {
        let Ok(entries) = std::fs::read_dir(self.trash_root()) else { return Ok(Vec::new()) };
        let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        Ok(names)
    }

    fn trash_discard_blocking(&self, entry: &str) {
        if crate::content::slug::valid_segment(entry) {
            // Only an empty directory goes: anything in it stays.
            let _ = std::fs::remove_dir(self.trash_root().join(entry));
        }
    }

    fn chunk_put_blocking(&self, upload: &str, index: u32, bytes: &[u8]) -> Result<(), String> {
        let dir = self.spool(upload)?;
        std::fs::create_dir_all(&dir).map_err(io("spool the chunk"))?;
        std::fs::write(dir.join(format!("{index}.part")), bytes).map_err(io("spool the chunk"))
    }

    fn chunk_read_blocking(&self, upload: &str, index: u32) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(self.spool(upload)?.join(format!("{index}.part"))) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("chunk {index} could not be read back: {e}")),
        }
    }

    fn chunks_clear_blocking(&self, upload: &str) {
        if let Ok(dir) = self.spool(upload) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A spool is made when its upload's first chunk arrives, which is
    /// after its ticket's clock started, so one this old belongs to an
    /// upload that has expired, here or before a restart.
    fn chunks_sweep_blocking(&self, age: Duration) {
        let Ok(entries) = std::fs::read_dir(self.data_dir.join(".tmp").join("inline")) else { return };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|elapsed| elapsed > age);
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}
