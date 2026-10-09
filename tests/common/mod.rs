//! What an integration test needs to run one scenario on either backend:
//! files by default, Postgres when `TOOLSITE_TEST_BACKEND=postgres`
//! (`scripts/test-postgres.sh --full` sets it), or Postgres always for the
//! `_on_postgres` twins that `scripts/test-postgres.sh` alone runs.
//!
//! Included with `mod common;` by the files that use it; not every file uses
//! every function.
#![allow(dead_code)]

use std::sync::Arc;
use toolsite::{
    runtime::blobs::{Backend as BlobBackend, Blobs, S3},
    state::{pg, Backend, Stores},
};

pub const NEEDS: &str = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one";
/// A key made at random once, for sealing on a test site, when the
/// environment gives none.
pub const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

/// The site key. Sealing reads it from the environment, as on a real
/// Postgres site; `scripts/test-postgres.sh` exports one for the whole run,
/// so no test sees it change. Run another way, it is set here, once.
pub fn secret_key() -> String {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var("TOOLSITE_SECRET_KEY").is_err() {
            unsafe { std::env::set_var("TOOLSITE_SECRET_KEY", KEY) };
        }
    });
    std::env::var("TOOLSITE_SECRET_KEY").unwrap()
}

pub fn wants_postgres() -> bool {
    std::env::var("TOOLSITE_TEST_BACKEND").is_ok_and(|backend| backend == "postgres")
}

/// A bucket of its own on the test MinIO, made now. A Postgres site keeps
/// its published files and its apps' files there, as a real one must.
pub async fn bucket() -> S3 {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} unset; scripts/test-postgres.sh sets it"));
    let name = format!("t-{}", toolsite::content::slug::random_token(12));
    let s3 = S3::new(
        &var("TOOLSITE_TEST_S3_ENDPOINT"),
        &name,
        "us-east-1",
        &var("TOOLSITE_TEST_S3_ACCESS_KEY_ID"),
        &var("TOOLSITE_TEST_S3_SECRET_ACCESS_KEY"),
        true,
    )
    .unwrap();
    s3.ensure_bucket().await.unwrap();
    s3
}

/// The bucket as a site's file settings, with the default ceilings.
pub fn blobs_in(s3: &S3) -> Blobs {
    Blobs {
        backend: BlobBackend::S3(s3.clone()),
        max_bytes: toolsite::config::DEFAULT_MAX_BLOB_BYTES,
        max_write_bytes: toolsite::config::DEFAULT_MAX_BLOB_BYTES,
    }
}

/// A database of its own on the test server, every ladder applied, and a
/// bucket of its own beside it.
pub struct Database {
    pub postgres: Arc<pg::Postgres>,
    pub bucket: S3,
    name: String,
}

impl Database {
    pub async fn new() -> Database {
        let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
        let name = format!("t_{}", toolsite::content::slug::random_token(12).to_lowercase());
        let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client.batch_execute(&format!("create database {name}")).await.unwrap();
        let mut url = url::Url::parse(&server).unwrap();
        url.set_path(&name);
        let postgres = Arc::new(pg::connect(url.as_str(), 8).await.unwrap());
        pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap();
        Database { postgres, bucket: bucket().await, name }
    }

    /// The stores a Postgres site keeps on this database.
    pub fn stores(&self) -> Stores {
        Stores::new(Backend::Postgres(self.postgres.clone()), None, Some(&secret_key())).unwrap()
    }

    /// Where a Postgres site on this database keeps files: its bucket.
    pub fn blobs(&self) -> Blobs {
        blobs_in(&self.bucket)
    }

    /// Drops the database. A scenario that fails leaves its database for
    /// the container to take away.
    pub async fn drop(self) {
        self.postgres.pool.close();
        let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
        let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client.batch_execute(&format!("drop database if exists {} with (force)", self.name)).await.unwrap();
    }
}

/// Runs a synchronous account call from a test. On Postgres such a call
/// waits on the runtime, which an async worker may not do, so on a
/// multi-threaded runtime the thread is handed over to blocking first. On a
/// single-threaded one (a files-only test) it runs as it is.
pub fn blocking<T>(call: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(call),
        _ => call(),
    }
}

/// Publishes `bytes` at `key` the way an upload does, into whichever store
/// the site keeps published files in: a fixture written straight to the
/// volume is invisible to a Postgres site, whose files are in its bucket.
/// On files it is the plain write the fixtures always made.
pub fn publish(config: &toolsite::Config, key: &str, bytes: impl Into<axum::body::Bytes>) {
    let bytes = bytes.into();
    if !config.stores.is_postgres() {
        let path = config.data_dir.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, &bytes).unwrap();
        return;
    }
    let handle = tokio::runtime::Handle::current();
    blocking(|| handle.block_on(toolsite::content::files::publish(config, key, bytes))).unwrap();
}

/// Every copy of `name` the trash keeps, oldest entry first, read through
/// whichever store keeps the site's trash: `.trash/` on files, the bucket
/// on Postgres.
pub fn trash_files(config: &toolsite::Config, name: &str) -> Vec<String> {
    blocking(|| {
        let files = toolsite::content::files::of(config);
        files
            .trash_entries_blocking()
            .unwrap()
            .iter()
            .filter_map(|entry| files.trash_read_blocking(entry, name).unwrap())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect()
    })
}
