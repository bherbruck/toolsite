//! The conformance suite on both backends. SQLite always runs; Postgres is
//! ignored unless asked for (`scripts/test-postgres.sh`), and each of its
//! tests works in a database of its own, migrated by the real ladders and
//! dropped at the end.

use super::{PostgresOAuth, SqliteOAuth};
use crate::config::Config;
use std::sync::Arc;

enum Guard {
    Dir(#[allow(dead_code)] tempfile::TempDir),
    Database(String),
}

impl Guard {
    async fn finish(self) {
        if let Guard::Database(name) = self {
            let (client, connection) = tokio_postgres::connect(&server(), tokio_postgres::NoTls).await.unwrap();
            tokio::spawn(connection);
            client
                .batch_execute(&format!("drop database if exists {name} with (force)"))
                .await
                .unwrap();
        }
    }
}

async fn sqlite() -> (Arc<SqliteOAuth>, Guard) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteOAuth::of(&Config::local(dir.path().to_path_buf(), "t"));
    (Arc::new(store), Guard::Dir(dir))
}

const NEEDS: &str = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one";

fn server() -> String {
    std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS)
}

async fn postgres() -> (Arc<PostgresOAuth>, Guard) {
    let name = format!("t_{}", crate::content::slug::random_token(12).to_lowercase());
    let (client, connection) = tokio_postgres::connect(&server(), tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("create database {name}")).await.unwrap();
    let mut url = url::Url::parse(&server()).unwrap();
    url.set_path(&name);
    let postgres = crate::state::pg::connect(url.as_str(), 16).await.unwrap();
    crate::state::pg::migrate(&postgres.pool, crate::state::pg::LADDERS).await.unwrap();
    (Arc::new(PostgresOAuth::new(postgres.pool.clone())), Guard::Database(name))
}

/// One test per property, for one backend.
macro_rules! suite {
    ($fixture:ident, #[$attr:meta]) => {
        suite!(@each $fixture, #[$attr],
            a_code_is_redeemed_exactly_once,
            an_expired_code_is_refused_and_spent,
            tokens_are_stored_hashed,
            an_expired_access_token_stops_working,
            an_expired_refresh_token_is_refused_and_spent,
            a_refresh_token_works_once_and_only_for_its_client,
            an_access_token_and_a_refresh_token_are_not_interchangeable,
            an_idle_registration_is_swept_but_a_connected_one_stays,
            tokens_stay_bound_to_their_resource,
            revoking_an_account_ends_its_tokens_and_codes_and_no_one_elses,
            an_unknown_client_is_nothing,
            hostile_values_are_kept_exactly_as_given,
            one_code_redeemed_by_many_at_once_yields_one_token,
            one_refresh_rotated_by_many_at_once_yields_one_new_refresh,
            a_rotation_racing_a_revocation_leaves_nothing_live,
            an_exchange_spends_its_code_and_cannot_outlive_a_revocation,
        );
    };
    (@each $fixture:ident, #[$attr:meta], $($name:ident),* $(,)?) => {
        $(
            #[$attr]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                let (store, guard) = super::$fixture().await;
                super::super::conformance::$name(store).await;
                guard.finish().await;
            }
        )*
    };
}

mod on_sqlite {
    suite!(sqlite, #[cfg(test)]);
}

mod on_postgres {
    suite!(postgres, #[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]);
}

/// The free functions are the trait on a blocking thread: in file mode they
/// reach the same `.site/oauth.db` the async store does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_blocking_calls_and_the_store_see_the_same_rows_in_file_mode() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), "t"));
    let worker = config.clone();
    let (client, issued) = tokio::task::spawn_blocking(move || {
        let client = super::register_client(&worker, Some("t"), &["https://c.test/cb".into()]).unwrap();
        let issued = super::issue_tokens(&worker, &client.id, "u1", None).unwrap();
        (client, issued)
    })
    .await
    .unwrap();
    let store = super::of(&config);
    assert_eq!(store.client(&client.id).await, Some(client.clone()));
    assert_eq!(
        store.access_token_grant(&issued.access_token).await,
        Some(("u1".to_string(), client.id.clone(), None))
    );
    assert!(dir.path().join(".site").join("oauth.db").exists());
}
