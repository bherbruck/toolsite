//! The state foundation against a real Postgres. Every test here is ignored
//! unless asked for: `scripts/test-postgres.sh` starts a database, sets
//! TOOLSITE_TEST_DATABASE_URL and runs them. Each test works in a database
//! of its own, created here and dropped at the end, so advisory locks and
//! the runner registry of one test never meet another's.

use std::sync::Arc;
use toolsite::state::{
    self,
    pg::{self, Ladder},
    runners::Runner,
    Backend, Settings, Stores,
};

const NEEDS: &str = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one";
const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

/// A database for one test: its URL, and its name for `drop_database`.
async fn fresh_database() -> (String, String) {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
    let name = format!("t_{}", toolsite::content::slug::random_token(12));
    let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("create database {name}")).await.unwrap();
    let mut url = url::Url::parse(&server).unwrap();
    url.set_path(&name);
    (url.to_string(), name)
}

async fn drop_database(name: &str) {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
    let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    client.batch_execute(&format!("drop database if exists {name} with (force)")).await.unwrap();
}

/// The real ladders and a probe whose first step cannot run twice: a
/// second `create schema` fails, so a step applied twice fails the test
/// even before the counts are compared. The sleep holds the lock long
/// enough that the other runners are certainly waiting on it.
const PROBED: &[Ladder] = &[
    Ladder { store: "state", steps: pg::LADDERS[0].steps },
    Ladder {
        store: "probe",
        steps: &[
            "create schema probe; create table probe.once (n int); insert into probe.once values (1); select pg_sleep(0.3);",
            "insert into probe.once values (2)",
        ],
    },
];

#[tokio::test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_ladder_applies_once_when_three_runners_boot_together_on_postgres() {
    let (url, name) = fresh_database().await;

    // Three runners, each with its own pool, all connected before any
    // migrates, then released at once.
    let start = Arc::new(tokio::sync::Barrier::new(3));
    let mut boots = Vec::new();
    for _ in 0..3 {
        let url = url.clone();
        let start = start.clone();
        boots.push(tokio::spawn(async move {
            let postgres = pg::connect(&url, 2).await.unwrap();
            start.wait().await;
            pg::migrate(&postgres.pool, PROBED).await
        }));
    }
    let mut applied = Vec::new();
    for boot in boots {
        applied.extend(boot.await.unwrap().expect("a runner failed to migrate"));
    }
    applied.sort();
    assert_eq!(applied, ["probe/001", "probe/002", "state/001", "state/002"], "steps applied across the three runners");

    let postgres = pg::connect(&url, 1).await.unwrap();
    let client = postgres.pool.get().await.unwrap();
    let probes: i64 = client.query_one("select count(*) from probe.once", &[]).await.unwrap().get(0);
    assert_eq!(probes, 2);
    let recorded: i64 = client.query_one("select count(*) from state.migrations", &[]).await.unwrap().get(0);
    assert_eq!(recorded, 4);
    drop(client);

    // A fourth boot later finds nothing to do.
    assert!(pg::migrate(&postgres.pool, PROBED).await.unwrap().is_empty());
    drop(postgres);
    drop_database(&name).await;
}

#[tokio::test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_database_a_newer_build_migrated_is_refused_on_postgres() {
    let (url, name) = fresh_database().await;
    let postgres = pg::connect(&url, 1).await.unwrap();
    pg::migrate(&postgres.pool, PROBED).await.unwrap();

    // A build that has never heard of `probe`.
    let why = pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap_err();
    assert!(why.contains("probe") && why.contains("newer toolsite"), "{why}");

    // A build that knows only the first step of it.
    const OLDER: &[Ladder] = &[
        Ladder { store: "state", steps: pg::LADDERS[0].steps },
        Ladder { store: "probe", steps: &["create schema probe"] },
    ];
    let why = pg::migrate(&postgres.pool, OLDER).await.unwrap_err();
    assert!(why.contains("version 2"), "{why}");

    // A refusal still let go of the lock: the next migration gets it.
    assert!(pg::migrate(&postgres.pool, PROBED).await.unwrap().is_empty());
    drop(postgres);
    drop_database(&name).await;
}

#[tokio::test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn two_live_runners_refuse_sqlite_apps_until_one_leaves_on_postgres() {
    let (url, name) = fresh_database().await;
    let postgres = pg::connect(&url, 2).await.unwrap();
    pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap();

    let first = Arc::new(Runner::new(&["control", "worker"], "default", Some("worker-1.railway.internal".into()), 8081));
    assert!(first.register(&postgres.pool).await.unwrap().is_empty());
    let stores = Stores { backend: Backend::Files, runner: Some(first.clone()) };
    assert!(stores.sqlite_refusal().is_none(), "alone, but refused");

    let second = Runner::new(&["worker"], "gpu", None, 9000);
    assert_eq!(second.register(&postgres.pool).await.unwrap(), vec![first.id.clone()]);
    assert_eq!(first.beat(&postgres.pool).await.unwrap(), vec![second.id.clone()]);
    let why = stores.sqlite_refusal().expect("two live runners, and SQLite apps still served");
    assert!(why.contains(&second.id), "{why}");

    // The registry holds what each said about itself.
    let client = postgres.pool.get().await.unwrap();
    let row = client
        .query_one(
            "select roles, pool, address, internal_port, draining from state.runners where id = $1",
            &[&first.id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, Vec<String>>(0), ["control", "worker"]);
    assert_eq!(row.get::<_, String>(1), "default");
    assert_eq!(row.get::<_, Option<String>>(2).as_deref(), Some("worker-1.railway.internal"));
    assert_eq!(row.get::<_, i32>(3), 8081);
    assert!(!row.get::<_, bool>(4));
    let row = client
        .query_one("select roles, pool, internal_port from state.runners where id = $1", &[&second.id])
        .await
        .unwrap();
    assert_eq!(row.get::<_, Vec<String>>(0), ["worker"]);
    assert_eq!(row.get::<_, String>(1), "gpu");
    assert_eq!(row.get::<_, i32>(2), 9000);

    // A runner whose heartbeat is older than LIVE_FOR no longer counts.
    client
        .execute("update state.runners set heartbeat_at = heartbeat_at - 31 where id = $1", &[&second.id])
        .await
        .unwrap();
    assert!(first.beat(&postgres.pool).await.unwrap().is_empty());
    assert!(stores.sqlite_refusal().is_none());

    // And one that left is gone at once.
    second.beat(&postgres.pool).await.unwrap();
    assert_eq!(first.beat(&postgres.pool).await.unwrap().len(), 1);
    second.leave(&postgres.pool).await;
    assert!(first.beat(&postgres.pool).await.unwrap().is_empty());
    drop(client);
    drop(postgres);
    drop_database(&name).await;
}

#[tokio::test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_database_from_the_first_release_climbs_to_the_runner_columns_on_postgres() {
    let (url, name) = fresh_database().await;
    let postgres = pg::connect(&url, 1).await.unwrap();
    const FIRST: &[Ladder] = &[Ladder { store: "state", steps: &[pg::LADDERS[0].steps[0]] }];
    assert_eq!(pg::migrate(&postgres.pool, FIRST).await.unwrap(), ["state/001"]);
    // Other stores' ladders may climb alongside; `state` takes only its second step.
    let climbed = pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap();
    let state: Vec<&String> = climbed.iter().filter(|step| step.starts_with("state/")).collect();
    assert_eq!(state, ["state/002"], "{climbed:?}");
    let runner = Runner::new(&["control", "worker"], "default", None, 8081);
    assert!(runner.register(&postgres.pool).await.unwrap().is_empty());
    drop(postgres);
    drop_database(&name).await;
}

#[tokio::test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_refused_password_is_reported_without_it_on_postgres() {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
    let mut url = url::Url::parse(&server).unwrap();
    url.set_password(Some("wr0ng-hunter2")).unwrap();
    let why = pg::connect(url.as_str(), 1).await.err().expect("a wrong password was accepted");
    assert!(!why.contains("wr0ng-hunter2"), "{why}");
    assert!(why.contains("password authentication failed"), "the cause was dropped: {why}");
}

#[tokio::test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_fresh_directory_opened_on_postgres_belongs_to_postgres_after() {
    let (url, name) = fresh_database().await;
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings { database_url: Some(url), secret_key: Some(KEY.into()), bucket: true, pool_size: 2 };
    let backend = state::open(&settings, dir.path()).await.unwrap();
    assert!(matches!(backend, Backend::Postgres(_)));
    assert!(dir.path().join(".site").join(state::MARKER).is_file());

    // Accounts live in Postgres now, but an auth.db left beside a claimed
    // directory (an old file, a copy) is not taken for unmigrated state,
    // and file mode still refuses to write beside a Postgres site.
    std::fs::write(dir.path().join(".site/auth.db"), b"").unwrap();
    state::open(&settings, dir.path()).await.unwrap();
    let files = Settings { database_url: None, secret_key: None, bucket: false, pool_size: 2 };
    assert!(state::open(&files, dir.path()).await.is_err());
    drop(backend);
    drop_database(&name).await;
}
