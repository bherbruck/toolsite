//! What an integration test needs to run one scenario on either backend:
//! files by default, Postgres when `TOOLSITE_TEST_BACKEND=postgres`
//! (`scripts/test-postgres.sh --full` sets it), or Postgres always for the
//! `_on_postgres` twins that `scripts/test-postgres.sh` alone runs.
//!
//! Included with `mod common;` by the files that use it; not every file uses
//! every function.
#![allow(dead_code)]

use std::sync::Arc;
use toolsite::state::{pg, Backend, Stores};

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

/// A database of its own on the test server, every ladder applied.
pub struct Database {
    pub postgres: Arc<pg::Postgres>,
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
        Database { postgres, name }
    }

    /// The stores a Postgres site keeps on this database.
    pub fn stores(&self) -> Stores {
        Stores::new(Backend::Postgres(self.postgres.clone()), None, Some(&secret_key())).unwrap()
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
