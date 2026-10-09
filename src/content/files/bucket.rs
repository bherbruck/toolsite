//! Published files in the site's bucket, for a site on Postgres: every
//! runner reads and writes the same objects, so none needs a volume of its
//! own for them.
//!
//! ```text
//! .toolsite/content/<key>             a page, bundle file, icon, source or handler
//! .toolsite/trash/<entry>/<name>      a removal's files and copies
//! .toolsite/tmp/inline/<digest>/<n>   an inline upload's pieces
//! ```
//!
//! An app's own files sit at `<app>/<key>` beside these. No slug, bundle
//! path or blob key can start with a dot, so none can reach `.toolsite/`.
//!
//! What a runner fetched it keeps under `DATA_DIR/.tmp/content/<app>/
//! <generation>/`, named by the SHA-256 of the key. A key that was not
//! there is remembered in memory, a bounded number of them, never on disk:
//! a visitor can ask for any number of names that do not exist, and each
//! would otherwise leave a file behind. A read at a newer generation never
//! looks at an older one's files, and one at generation 0, an app with
//! nothing published or taken away, never looks at all. The names are
//! digests, so nothing a visitor sends can name a cached file, and `.tmp`
//! is never served.

use super::{
    app_of, check_key, check_trash_name,
    local::{lock_blocking, turn_lock, LocalTurn},
    Entry, Files, Turn,
};
use crate::{content::bundle::Unpacked, runtime::blobs::S3, state::pg::LOCK_PUBLISH};
use async_trait::async_trait;
use bytes::Bytes;
use deadpool_postgres::Pool;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Mutex},
    time::{Duration, SystemTime},
};
use tokio::io::AsyncWriteExt;

const CONTENT: &str = ".toolsite/content/";
const TRASH: &str = ".toolsite/trash/";
const INLINE: &str = ".toolsite/tmp/inline/";
/// Objects sent at once when a bundle goes up or the trash moves an app.
const AT_ONCE: usize = 16;
/// Bytes of a bundle held in memory at once on their way up.
const HELD_BYTES: usize = 16 * 1024 * 1024;
/// An older generation's cache is left this long after a newer one is
/// first read, for any request still reading from it.
const KEEP_OLD: Duration = Duration::from_secs(60);

pub struct Bucket {
    s3: Option<S3>,
    data_dir: PathBuf,
    pool: Option<Pool>,
}

/// Turns held on Postgres at once in this process. Each holds a pooled
/// connection for its lock and needs another to count its generation, so
/// without a bound enough publishes of different apps at once would hold
/// every connection and wait for one more.
pub(super) const MAX_TURNS: usize = 4;
static TURNS: LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_TURNS)));
/// Between tries at a lock another runner holds: short at first, since most
/// publishes take moments, then no longer than this.
const FIRST_PAUSE: Duration = Duration::from_millis(10);
const LAST_PAUSE: Duration = Duration::from_millis(250);

/// The newest generation each app's cache was swept below, by cache path.
static SWEPT: LazyLock<Mutex<HashMap<PathBuf, u64>>> = LazyLock::new(Default::default);

/// Keys found absent, by the path their file would have had in the cache,
/// which names the generation too. Forgotten all at once when full: a miss
/// forgotten costs one more request to the bucket, never a wrong answer.
const MAX_ABSENT: usize = 50_000;
static ABSENT: LazyLock<Mutex<std::collections::HashSet<PathBuf>>> = LazyLock::new(Default::default);

fn absent(path: &std::path::Path) -> bool {
    ABSENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(path)
}

fn remember_absent(path: PathBuf) {
    let mut known = ABSENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if known.len() >= MAX_ABSENT {
        known.clear();
    }
    known.insert(path);
}

fn digest(key: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(key.as_bytes()))
}

/// A turn held in this process and, through a connection kept for it, on
/// every runner: an advisory lock on the app. Released, the lock is let go
/// and the connection pooled again; dropped, the connection is closed,
/// which lets the lock go with it.
struct BucketTurn {
    client: Option<deadpool_postgres::Client>,
    _here: tokio::sync::OwnedMutexGuard<()>,
    _place: tokio::sync::OwnedSemaphorePermit,
    app: String,
}

#[async_trait]
impl Turn for BucketTurn {
    async fn release(mut self: Box<Self>) {
        let Some(client) = self.client.take() else { return };
        let released = client
            .execute("select pg_advisory_unlock($1::int4, hashtext($2))", &[&LOCK_PUBLISH, &self.app])
            .await;
        if let Err(e) = released {
            tracing::warn!(app = %self.app, why = %crate::state::pg::chain(&e), "an app's publish lock did not end cleanly; its connection is closed instead");
            drop(deadpool_postgres::Object::take(client));
        }
    }

    fn release_blocking(self: Box<Self>) {
        crate::state::wait(self.release());
    }
}

impl Drop for BucketTurn {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            drop(deadpool_postgres::Object::take(client));
        }
    }
}

impl Bucket {
    pub fn new(s3: Option<S3>, data_dir: PathBuf, pool: Option<Pool>) -> Bucket {
        Bucket { s3, data_dir, pool }
    }

    fn s3(&self) -> Result<&S3, String> {
        self.s3.as_ref().ok_or_else(|| {
            "a Postgres site keeps its published files in its bucket, and none is configured \
             (TOOLSITE_BLOB_S3_ENDPOINT and TOOLSITE_BLOB_S3_BUCKET)"
                .to_string()
        })
    }

    fn cache_root(&self) -> PathBuf {
        self.data_dir.join(".tmp").join("content")
    }

    fn trash_object(entry: &str, name: &str) -> Result<String, String> {
        check_trash_name(entry, name)?;
        Ok(format!("{TRASH}{entry}/{name}"))
    }

    fn chunk_prefix(upload: &str) -> Result<String, String> {
        if upload.is_empty() || !upload.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{upload:?} does not name an upload"));
        }
        Ok(format!("{INLINE}{upload}/"))
    }

    /// Removes older generations' caches of an app, once per newer one,
    /// past the time a request could still be reading them.
    fn sweep_older(app_dir: &std::path::Path, generation: u64) {
        {
            let mut swept = SWEPT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let seen = swept.entry(app_dir.to_path_buf()).or_default();
            if *seen >= generation {
                return;
            }
            *seen = generation;
        }
        Self::sweep_idle(app_dir, generation);
    }

    /// Fetches `key` into the cache for `generation`.
    async fn fetch(&self, key: &str, dir: &std::path::Path, name: &str) -> Result<Option<PathBuf>, String> {
        let io = |what: &'static str| move |e: std::io::Error| format!("could not {what} the cache: {e}");
        let Some((object, mut stream)) = self.s3()?.object_get(&format!("{CONTENT}{key}")).await? else {
            remember_absent(dir.join(name));
            return Ok(None);
        };
        tokio::fs::create_dir_all(dir).await.map_err(io("make"))?;
        // Written aside and renamed, so a reader beside this one sees the
        // whole file or none of it.
        let part = dir.join(format!(".{}.part", crate::content::slug::random_token(12)));
        let written = async {
            let mut file = tokio::fs::File::create(&part).await.map_err(io("write"))?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| format!("could not fetch {key}: {e}"))?;
                file.write_all(&chunk).await.map_err(io("write"))?;
            }
            file.flush().await.map_err(io("write"))?;
            let file = file.into_std().await;
            // The file's time is the object's, so whatever shows "updated"
            // shows when it was published, not when this runner first read it.
            if let Some(modified) = object.modified {
                let _ = file.set_modified(modified);
            }
            tokio::fs::rename(&part, dir.join(name)).await.map_err(io("write"))
        }
        .await;
        if let Err(why) = written {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(why);
        }
        Ok(Some(dir.join(name)))
    }

    /// Removes what is cached of an app that has nothing published now,
    /// removed since this runner read it: no newer generation will come to
    /// sweep it, unless the name is published again.
    async fn sweep_gone(app_dir: &std::path::Path) {
        if tokio::fs::metadata(app_dir).await.is_err() {
            return;
        }
        let app_dir = app_dir.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            Self::sweep_idle(&app_dir, u64::MAX);
            let _ = std::fs::remove_dir(&app_dir);
        })
        .await;
    }

    /// Removes the caches of generations below `generation` that nothing
    /// has been added to for a while.
    fn sweep_idle(app_dir: &std::path::Path, generation: u64) {
        let Ok(entries) = std::fs::read_dir(app_dir) else { return };
        for entry in entries.flatten() {
            let older = entry.file_name().to_str().and_then(|name| name.parse::<u64>().ok()).is_some_and(|g| g < generation);
            let idle = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|elapsed| elapsed > KEEP_OLD);
            if older && idle {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    async fn lookup(&self, key: &str, generation: u64) -> Result<Option<PathBuf>, String> {
        check_key(key)?;
        let app = app_of(key);
        let named = crate::content::slug::valid_segment(app);
        let app_dir = self.cache_root().join(app);
        if generation == 0 {
            if named {
                Self::sweep_gone(&app_dir).await;
            }
            return Ok(None);
        }
        if !named {
            return Err(format!("{key:?} belongs to no app"));
        }
        let dir = app_dir.join(generation.to_string());
        let name = digest(key);
        if tokio::fs::metadata(dir.join(&name)).await.is_ok_and(|m| m.is_file()) {
            return Ok(Some(dir.join(name)));
        }
        if absent(&dir.join(&name)) {
            return Ok(None);
        }
        let found = self.fetch(key, &dir, &name).await?;
        Self::sweep_older(&app_dir, generation);
        Ok(found)
    }

    async fn listing(&self, dir: &str) -> Result<Vec<Entry>, String> {
        let prefix = if dir.is_empty() {
            CONTENT.to_string()
        } else {
            check_key(dir)?;
            format!("{CONTENT}{dir}/")
        };
        let listing = self.s3()?.object_list(&prefix, true).await?;
        let mut out: Vec<Entry> = listing
            .objects
            .into_iter()
            .filter_map(|(key, _)| key.strip_prefix(&prefix).map(|name| Entry { name: name.to_string(), dir: false }))
            .chain(listing.prefixes.into_iter().filter_map(|key| {
                key.strip_prefix(&prefix)
                    .and_then(|name| name.strip_suffix('/'))
                    .map(|name| Entry { name: name.to_string(), dir: true })
            }))
            .filter(|entry| !entry.name.is_empty() && !entry.name.starts_with('.') && !entry.name.contains('/'))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn turn(&self, app: &str) -> Result<Box<dyn Turn>, String> {
        let here = turn_lock(&self.cache_root(), app).lock_owned().await;
        let mut pause = FIRST_PAUSE;
        loop {
            let place = TURNS.clone().acquire_owned().await.map_err(|e| e.to_string())?;
            if let Some(client) = self.try_everywhere(app).await? {
                return Ok(self.held(app, client, here, place));
            }
            drop(place);
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(LAST_PAUSE);
        }
    }

    /// The turn, held here and on every runner, or the local one alone on
    /// a store with no database.
    fn held(
        &self,
        app: &str,
        client: Option<deadpool_postgres::Client>,
        here: tokio::sync::OwnedMutexGuard<()>,
        place: tokio::sync::OwnedSemaphorePermit,
    ) -> Box<dyn Turn> {
        match client {
            Some(client) => Box::new(BucketTurn { client: Some(client), _here: here, _place: place, app: app.to_string() }),
            None => Box::new(LocalTurn(here)),
        }
    }

    /// One try at the app's lock on every runner: the connection that holds
    /// it, `Some(None)` on a store with no database, or nothing when another
    /// runner has it. A try never waits on Postgres, so a turn waiting for
    /// another runner holds neither a connection nor a place among the
    /// `TURNS`: one runner's slow publishes cannot leave every other app on
    /// this one waiting behind them.
    async fn try_everywhere(&self, app: &str) -> Result<Option<Option<deadpool_postgres::Client>>, String> {
        let Some(pool) = &self.pool else { return Ok(Some(None)) };
        let client = pool
            .get()
            .await
            .map_err(|e| format!("could not reach Postgres for an app's publish lock: {}", crate::state::pg::chain(&e)))?;
        let taken = client
            .query_one("select pg_try_advisory_lock($1::int4, hashtext($2))", &[&LOCK_PUBLISH, &app])
            .await;
        match taken {
            Ok(row) if row.get::<_, bool>(0) => Ok(Some(Some(client))),
            // Not taken: the connection holds nothing and goes back.
            Ok(_) => Ok(None),
            Err(e) => {
                // It may hold the lock or not; closed, it holds nothing.
                drop(deadpool_postgres::Object::take(client));
                Err(format!("could not take an app's publish lock: {}", crate::state::pg::chain(&e)))
            }
        }
    }

    /// Moves every object under `from` (a key, or everything under
    /// `from/`) to `to`; whether anything moved.
    async fn move_tree(&self, from: &str, to: &str) -> Result<bool, String> {
        let s3 = self.s3()?;
        let mut moved = s3.object_move(from, to).await?;
        let inside = format!("{from}/");
        let listing = s3.object_list(&inside, false).await?;
        let pairs: Vec<(String, String)> = listing
            .objects
            .into_iter()
            .filter_map(|(key, _)| key.strip_prefix(&inside).map(|rest| (key.clone(), format!("{to}/{rest}"))))
            .collect();
        for batch in pairs.chunks(AT_ONCE) {
            let results = futures_util::future::join_all(batch.iter().map(|(from, to)| s3.object_move(from, to))).await;
            for result in results {
                moved |= result?;
            }
        }
        Ok(moved)
    }

    async fn occupied(&self, key: &str) -> Result<bool, String> {
        let s3 = self.s3()?;
        let object = format!("{CONTENT}{key}");
        if s3.object_head(&object).await?.is_some() {
            return Ok(true);
        }
        Ok(!s3.object_list(&format!("{object}/"), false).await?.objects.is_empty())
    }

    async fn clear_prefix(&self, prefix: &str, older_than: Option<Duration>) -> Result<(), String> {
        let s3 = self.s3()?;
        let now = SystemTime::now();
        let doomed: Vec<String> = s3
            .object_list(prefix, false)
            .await?
            .objects
            .into_iter()
            .filter(|(_, object)| match older_than {
                None => true,
                Some(age) => object.modified.and_then(|m| now.duration_since(m).ok()).is_some_and(|elapsed| elapsed > age),
            })
            .map(|(key, _)| key)
            .collect();
        for batch in doomed.chunks(AT_ONCE) {
            for result in futures_util::future::join_all(batch.iter().map(|key| s3.object_delete(key))).await {
                result?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Files for Bucket {
    fn by_generation(&self) -> bool {
        true
    }

    async fn local(&self, key: &str, generation: u64) -> Result<Option<PathBuf>, String> {
        self.lookup(key, generation).await
    }

    fn local_blocking(&self, key: &str, generation: u64) -> Result<Option<PathBuf>, String> {
        crate::state::wait_in_place(self.lookup(key, generation))
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), String> {
        check_key(key)?;
        self.s3()?.object_put(&format!("{CONTENT}{key}"), bytes).await
    }

    async fn delete(&self, key: &str) -> Result<bool, String> {
        check_key(key)?;
        let s3 = self.s3()?;
        let object = format!("{CONTENT}{key}");
        if s3.object_head(&object).await?.is_none() {
            return Ok(false);
        }
        s3.object_delete(&object).await?;
        Ok(true)
    }

    /// Each file goes up as its own object, a few at once. One file is held
    /// in memory at a time per upload, never the unpacked bundle.
    fn put_bundle_blocking(&self, slug: &str, body: &[u8]) -> Result<Unpacked, String> {
        check_key(slug)?;
        let s3 = self.s3()?;
        let mut batch: Vec<(String, Bytes)> = Vec::new();
        let mut held = 0usize;
        let send = |batch: &mut Vec<(String, Bytes)>| -> Result<(), String> {
            let puts = batch.drain(..).map(|(key, bytes)| async move { s3.object_put(&key, bytes).await });
            crate::state::wait(futures_util::future::join_all(puts)).into_iter().collect::<Result<Vec<()>, String>>()?;
            Ok(())
        };
        let unpacked = crate::content::bundle::unpack_into(body, slug, &mut |rel, entry, size| {
            let mut bytes = Vec::with_capacity(size as usize);
            std::io::Read::read_to_end(entry, &mut bytes).map_err(|e| e.to_string())?;
            held += bytes.len();
            batch.push((format!("{CONTENT}{slug}/{rel}"), Bytes::from(bytes)));
            if batch.len() >= AT_ONCE || held >= HELD_BYTES {
                held = 0;
                send(&mut batch)?;
            }
            Ok(())
        })?;
        send(&mut batch)?;
        Ok(unpacked)
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>, String> {
        self.listing(dir).await
    }

    fn list_blocking(&self, dir: &str) -> Result<Vec<Entry>, String> {
        crate::state::wait_in_place(self.listing(dir))
    }

    /// Every publish counts a generation, the first one included, and a
    /// removal takes the app's out of the catalog: the generation says.
    async fn exists(&self, app: &str, generation: u64) -> Result<bool, String> {
        check_key(app)?;
        Ok(generation > 0)
    }

    fn exists_blocking(&self, app: &str, generation: u64) -> Result<bool, String> {
        check_key(app)?;
        Ok(generation > 0)
    }

    /// Nothing to write: the generation counted with it holds the name.
    async fn hold_name(&self, app: &str) -> Result<(), String> {
        check_key(app)
    }

    async fn take_turn(&self, app: &str) -> Result<Box<dyn Turn>, String> {
        self.turn(app).await
    }

    fn take_turn_blocking(&self, app: &str) -> Result<Box<dyn Turn>, String> {
        let here = lock_blocking(turn_lock(&self.cache_root(), app));
        let mut pause = FIRST_PAUSE;
        loop {
            let place = loop {
                if let Ok(place) = TURNS.clone().try_acquire_owned() {
                    break place;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            if let Some(client) = crate::state::wait(self.try_everywhere(app))? {
                return Ok(self.held(app, client, here, place));
            }
            drop(place);
            std::thread::sleep(pause);
            pause = (pause * 2).min(LAST_PAUSE);
        }
    }

    /// Named at random as well as by time, since two runners may remove
    /// the same slug in the same second and nothing here is made first.
    fn trash_entry_blocking(&self, slug: &str, at: u64) -> Result<String, String> {
        Ok(format!("{at}-{}-{}", slug.replace('/', "-"), crate::content::slug::random_token(8)))
    }

    fn trash_write_blocking(&self, entry: &str, name: &str, bytes: &[u8]) -> Result<(), String> {
        let object = Self::trash_object(entry, name)?;
        crate::state::wait(self.s3()?.object_put(&object, Bytes::copy_from_slice(bytes)))
    }

    fn trash_move_blocking(&self, entry: &str, key: &str, name: &str) -> Result<bool, String> {
        check_key(key)?;
        let to = Self::trash_object(entry, name)?;
        crate::state::wait(self.move_tree(&format!("{CONTENT}{key}"), &to))
    }

    fn trash_read_blocking(&self, entry: &str, name: &str) -> Result<Option<Vec<u8>>, String> {
        let object = Self::trash_object(entry, name)?;
        crate::state::wait(self.s3()?.object_read(&object))
    }

    fn untrash_blocking(&self, entry: &str, name: &str, key: &str) -> Result<bool, String> {
        check_key(key)?;
        let from = Self::trash_object(entry, name)?;
        crate::state::wait(async {
            let s3 = self.s3()?;
            let there = s3.object_head(&from).await?.is_some()
                || !s3.object_list(&format!("{from}/"), false).await?.objects.is_empty();
            if !there {
                return Ok(false);
            }
            if self.occupied(key).await? {
                return Err(format!("{key} is in use; nothing was put back over it"));
            }
            self.move_tree(&from, &format!("{CONTENT}{key}")).await
        })
    }

    fn trash_entries_blocking(&self) -> Result<Vec<String>, String> {
        let listing = crate::state::wait(self.s3()?.object_list(TRASH, true))?;
        let mut entries: Vec<String> = listing
            .prefixes
            .into_iter()
            .filter_map(|p| p.strip_prefix(TRASH).and_then(|p| p.strip_suffix('/')).map(str::to_string))
            .collect();
        entries.sort();
        Ok(entries)
    }

    /// An entry is its objects; one with none is not there to let go of.
    fn trash_discard_blocking(&self, _entry: &str) {}

    fn chunk_put_blocking(&self, upload: &str, index: u32, bytes: &[u8]) -> Result<(), String> {
        let object = format!("{}{index}", Self::chunk_prefix(upload)?);
        crate::state::wait(self.s3()?.object_put(&object, Bytes::copy_from_slice(bytes)))
    }

    fn chunk_read_blocking(&self, upload: &str, index: u32) -> Result<Option<Vec<u8>>, String> {
        let object = format!("{}{index}", Self::chunk_prefix(upload)?);
        crate::state::wait(self.s3()?.object_read(&object))
    }

    fn chunks_clear_blocking(&self, upload: &str) {
        let Ok(prefix) = Self::chunk_prefix(upload) else { return };
        if let Err(why) = crate::state::wait(self.clear_prefix(&prefix, None)) {
            tracing::warn!(%why, "an inline upload's pieces could not be cleared; the next sweep takes them");
        }
    }

    fn chunks_sweep_blocking(&self, age: Duration) {
        if let Err(why) = crate::state::wait(self.clear_prefix(INLINE, Some(age))) {
            tracing::warn!(%why, "old inline upload pieces could not be swept");
        }
    }
}
