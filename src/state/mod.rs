//! Where platform state lives: files under `DATA_DIR`, or Postgres when
//! `DATABASE_URL` is set.
//!
//! The bottom layer, beside `config`: `runtime` and `accounts` reach their
//! stores through here, so nothing in this module reaches up into HTTP,
//! `platform` or `Config`. It decides the backend once, at boot, and refuses
//! to start when the choice would split a site's state in two.

pub mod pg;
pub mod runners;

use std::{
    future::Future,
    path::Path,
    sync::{Arc, OnceLock},
};

/// Under `.site/`: this data directory's platform state belongs to Postgres
/// now. Written by the migration command, or by the first Postgres boot on a
/// directory that had nothing to migrate.
pub const MARKER: &str = "migrated-to-postgres";

/// The backend a site's platform state lives in, chosen once in `main`.
#[derive(Clone, Default)]
pub enum Backend {
    /// Today's storage, everything under `DATA_DIR`.
    #[default]
    Files,
    Postgres(Arc<pg::Postgres>),
}

/// The store handles every layer reaches platform state through. Empty
/// apart from the backend for now: each store joins as it moves behind its
/// trait, an `Arc<dyn Trait>` chosen here from the backend.
#[derive(Clone, Default)]
pub struct Stores {
    pub backend: Backend,
    /// This process in the runner registry, when it serves on Postgres.
    pub runner: Option<Arc<runners::Runner>>,
}

impl Stores {
    pub fn is_postgres(&self) -> bool {
        matches!(self.backend, Backend::Postgres(_))
    }

    /// Why an app's SQLite database may not be opened now, if it may not.
    /// App databases are files on one volume until they move to Postgres,
    /// and two runners writing one SQLite file over a shared volume is a
    /// split brain; a loud refusal beats a quiet corruption.
    pub fn sqlite_refusal(&self) -> Option<String> {
        let runner = self.runner.as_ref()?;
        let peers = runner.peers();
        (!peers.is_empty()).then(|| {
            format!(
                "another toolsite runner is live on this database ({}); app databases are \
                 SQLite files on one volume, so they are refused until this runner is alone",
                peers.join(", ")
            )
        })
    }
}

/// What the environment says about the backend. Read once in `main`, and
/// by the `toolsite user` commands, through the same function.
pub struct Settings {
    pub database_url: Option<String>,
    /// `TOOLSITE_SECRET_KEY` as it was given. Checked here, used by `seal`.
    pub secret_key: Option<String>,
    /// Whether a bucket is configured for apps' files.
    pub bucket: bool,
    /// Connections the pool may hold, from `TOOLSITE_DATABASE_POOL`.
    pub pool_size: usize,
}

impl Settings {
    pub fn from_env(read: impl Fn(&str) -> Option<String>, bucket: bool) -> Result<Settings, String> {
        let pool_size = match read("TOOLSITE_DATABASE_POOL") {
            Some(value) => value
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|size| *size > 0)
                .ok_or_else(|| format!("TOOLSITE_DATABASE_POOL must be a whole number above 0, not {value:?}"))?,
            None => pg::DEFAULT_POOL_SIZE,
        };
        Ok(Settings {
            // The unprefixed name is what a platform's database injects.
            database_url: read("TOOLSITE_DATABASE_URL").or_else(|| read("DATABASE_URL")),
            secret_key: read("TOOLSITE_SECRET_KEY"),
            bucket,
            pool_size,
        })
    }
}

/// The boot guards. Each refusal says what to do next and never repeats a
/// secret: the caller logs it as it is.
///
/// On Postgres every runner must open every sealed value, so the key comes
/// from the environment and never from a file one runner made; apps' files
/// must be in a bucket, since a volume-local store is one runner's; and a
/// directory still holding file-mode state has to be migrated first, or the
/// site would come up empty beside its own data.
///
/// In file mode, a directory that moved to Postgres is refused, so nobody
/// writes to the old files after cutover by accident.
pub fn check(settings: &Settings, data_dir: &Path) -> Result<(), String> {
    let site = data_dir.join(".site");
    let migrated = site.join(MARKER).exists();
    if settings.database_url.is_none() {
        if migrated {
            return Err(format!(
                "this data directory moved to Postgres (.site/{MARKER} is present): set DATABASE_URL \
                 to serve it, or run `toolsite export-to-files` to bring its state back to files"
            ));
        }
        return Ok(());
    }

    let Some(key) = &settings.secret_key else {
        return Err("DATABASE_URL is set but TOOLSITE_SECRET_KEY is not: on Postgres every runner \
                    must open the same sealed values, so the key comes from the environment. Set it \
                    to 32 random bytes in base64 (`openssl rand -base64 32`)"
            .into());
    };
    crate::seal::parse_key(key)?;
    if !settings.bucket {
        return Err("DATABASE_URL is set but no bucket is: on Postgres apps' files must be shared \
                    between runners, so set TOOLSITE_BLOB_S3_ENDPOINT and TOOLSITE_BLOB_S3_BUCKET"
            .into());
    }
    if !migrated && site.join("auth.db").exists() {
        return Err(format!(
            "DATABASE_URL is set but this data directory still holds file-mode state (.site/auth.db) \
             and no .site/{MARKER}: run `toolsite migrate-to-postgres` with the server stopped, \
             or unset DATABASE_URL to keep serving from files"
        ));
    }
    Ok(())
}

/// Chooses the backend: the guards, then for Postgres the pool and every
/// ladder. Logs what it chose, the database by host, port and name only.
pub async fn open(settings: &Settings, data_dir: &Path) -> Result<Backend, String> {
    check(settings, data_dir)?;
    let Some(url) = &settings.database_url else {
        tracing::info!(backend = "files", data_dir = %data_dir.display(), "platform state");
        return Ok(Backend::Files);
    };
    let postgres = pg::connect(url, settings.pool_size).await?;
    let applied = pg::migrate(&postgres.pool, pg::LADDERS).await?;
    claim(data_dir)?;
    tracing::info!(
        backend = "postgres",
        database = %postgres.target,
        tls = postgres.tls,
        pool = settings.pool_size,
        applied = ?applied,
        "platform state"
    );
    Ok(Backend::Postgres(Arc::new(postgres)))
}

/// Marks a directory that had nothing to migrate as Postgres's, so the
/// files this build still writes there (accounts, until they move) are not
/// later mistaken for a file-mode site. Never replaces a marker the
/// migration command wrote.
fn claim(data_dir: &Path) -> Result<(), String> {
    let site = data_dir.join(".site");
    std::fs::create_dir_all(&site).map_err(|e| format!("could not create .site: {e}"))?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let body = serde_json::json!({ "at": at, "from": "an empty data directory: nothing to migrate" });
    match std::fs::OpenOptions::new().write(true).create_new(true).open(site.join(MARKER)) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(format!("{body}\n").as_bytes())
                .map_err(|e| format!("could not write .site/{MARKER}: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("could not write .site/{MARKER}: {e}")),
    }
}

static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Keeps the server's runtime for `wait` on threads that did not come from
/// it. Called once from `main`.
pub fn keep_runtime(handle: tokio::runtime::Handle) {
    let _ = RUNTIME.set(handle);
}

/// Runs an async store call to completion from synchronous code: a wasm
/// host function or an account function, both already on a blocking thread.
///
/// Blocking an async worker thread would stall every task queued behind it,
/// so that is a panic, with a message naming the mistake, rather than a
/// slowdown nobody can trace.
pub fn wait<F: Future>(future: F) -> F::Output {
    let handle = tokio::runtime::Handle::try_current()
        .ok()
        .or_else(|| RUNTIME.get().cloned())
        .expect("state::wait needs a tokio runtime: none is entered here and none was kept");
    // tokio refuses to block a thread that drives tasks. Asking it with a
    // future that is already done finds out before the real call starts, so
    // the panic below can say what went wrong instead of tokio's general one.
    let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.block_on(async {})));
    if probe.is_err() {
        panic!(
            "state::wait was called on an async worker thread: a store call blocks its thread, \
             so call it from spawn_blocking or await the store directly"
        );
    }
    handle.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn postgres(key: Option<&str>, bucket: bool) -> Settings {
        Settings {
            database_url: Some("postgres://toolsite:hunter2@db.internal:5432/site".into()),
            secret_key: key.map(str::to_string),
            bucket,
            pool_size: 4,
        }
    }

    fn files() -> Settings {
        Settings { database_url: None, secret_key: None, bucket: false, pool_size: 4 }
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join(".site")).unwrap();
        std::fs::write(dir.join(".site").join(name), b"x").unwrap();
    }

    #[test]
    fn file_mode_starts_on_an_empty_or_a_file_mode_directory() {
        let dir = tempfile::tempdir().unwrap();
        check(&files(), dir.path()).unwrap();
        touch(dir.path(), "auth.db");
        check(&files(), dir.path()).unwrap();
    }

    #[test]
    fn file_mode_refuses_a_directory_that_moved_to_postgres() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), MARKER);
        let why = check(&files(), dir.path()).unwrap_err();
        assert!(why.contains("DATABASE_URL") && why.contains("toolsite export-to-files"), "{why}");
    }

    #[test]
    fn postgres_mode_refuses_to_start_without_the_secret_key() {
        let dir = tempfile::tempdir().unwrap();
        let why = check(&postgres(None, true), dir.path()).unwrap_err();
        assert!(why.contains("TOOLSITE_SECRET_KEY"), "{why}");
        assert!(!why.contains("hunter2"), "the refusal repeated the database password: {why}");
    }

    #[test]
    fn postgres_mode_refuses_a_secret_key_that_is_not_32_bytes_without_repeating_it() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["not base64 at all!", "c2hvcnQ=", "s3cr3t-value-that-should-not-be-logged"] {
            let why = check(&postgres(Some(bad), true), dir.path()).unwrap_err();
            assert!(why.contains("TOOLSITE_SECRET_KEY"), "{why}");
            assert!(!why.contains(bad), "the refusal repeated the key: {why}");
        }
    }

    #[test]
    fn postgres_mode_refuses_to_start_without_a_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let why = check(&postgres(Some(KEY), false), dir.path()).unwrap_err();
        assert!(why.contains("TOOLSITE_BLOB_S3_BUCKET"), "{why}");
        assert!(!why.contains(KEY), "{why}");
    }

    #[test]
    fn postgres_mode_refuses_file_mode_state_that_was_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "auth.db");
        let why = check(&postgres(Some(KEY), true), dir.path()).unwrap_err();
        assert!(why.contains("toolsite migrate-to-postgres"), "{why}");
    }

    #[test]
    fn postgres_mode_starts_on_an_empty_or_a_migrated_directory() {
        let dir = tempfile::tempdir().unwrap();
        check(&postgres(Some(KEY), true), dir.path()).unwrap();
        touch(dir.path(), "auth.db");
        touch(dir.path(), MARKER);
        check(&postgres(Some(KEY), true), dir.path()).unwrap();
    }

    #[test]
    fn a_claim_never_replaces_the_marker_the_migration_wrote() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".site")).unwrap();
        std::fs::write(dir.path().join(".site").join(MARKER), b"migrated: 12 accounts").unwrap();
        claim(dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join(".site").join(MARKER)).unwrap(), b"migrated: 12 accounts");

        let fresh = tempfile::tempdir().unwrap();
        claim(fresh.path()).unwrap();
        // The directory is Postgres's now: file mode refuses it.
        assert!(check(&files(), fresh.path()).is_err());
    }

    #[test]
    fn the_pool_size_must_be_a_positive_number() {
        let read = |value: &'static str| move |name: &str| (name == "TOOLSITE_DATABASE_POOL").then(|| value.to_string());
        assert_eq!(Settings::from_env(read("4"), false).unwrap().pool_size, 4);
        assert!(Settings::from_env(read("0"), false).is_err());
        assert!(Settings::from_env(read("many"), false).is_err());
        assert_eq!(Settings::from_env(|_| None, false).unwrap().pool_size, pg::DEFAULT_POOL_SIZE);
    }

    #[test]
    #[should_panic(expected = "async worker thread")]
    fn wait_panics_on_an_async_worker_thread() {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap();
        runtime.block_on(async {
            tokio::spawn(async { wait(async { 1 }) }).await.map_err(|e| std::panic::resume_unwind(e.into_panic())).unwrap()
        });
    }

    #[tokio::test]
    async fn wait_runs_a_store_call_on_a_blocking_thread() {
        let got = tokio::task::spawn_blocking(|| {
            wait(async {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                41 + 1
            })
        })
        .await
        .unwrap();
        assert_eq!(got, 42);
    }
}
