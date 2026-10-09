//! One suite every records store must pass, run on files always and on
//! Postgres when `TOOLSITE_TEST_DATABASE_URL` names a server
//! (`scripts/test-postgres.sh` starts one). It pins what the owners rely on:
//! a record comes back exactly as it went in; one app's records never answer
//! for another's; an edit that fails changes nothing; of many changes to one
//! repository link at once, every one lands; and a removal takes an app's
//! records and hands them back as the sidecars they are on files.

use super::{files::Files, postgres::Postgres, AppRecords};
use crate::accounts::store::conformance::{drop_postgres_database, postgres_database};
use std::sync::Arc;

/// Values that have broken stores before: quotes and SQL, a NUL, Unicode
/// lookalikes, and a long string.
fn hostile() -> Vec<String> {
    vec![
        "'); drop table platform.app_tools; --".to_string(),
        "nul\u{0}inside".to_string(),
        "\u{2215}etc\u{2215}passwd \u{0430}dmin \u{1F512}".to_string(),
        "x".repeat(64 * 1024),
    ]
}

/// A record as an owner writes one: pretty JSON holding every hostile value.
fn document() -> String {
    super::pretty(&serde_json::json!({ "values": hostile(), "n": 1 })).unwrap()
}

/// A sealed value is the sealer's output, never a NUL, which Postgres
/// text cannot hold; every other hostile value is fair.
fn sealed_values() -> Vec<String> {
    hostile().into_iter().filter(|value| !value.contains('\0')).collect()
}

async fn settings_round_trip(records: &dyn AppRecords) {
    assert!(records.settings("crm").await.unwrap().is_empty());
    for (n, value) in sealed_values().iter().enumerate() {
        assert!(!records.set_setting("crm", &format!("K{n}"), Some(value)).await.unwrap(), "a new setting said it replaced one");
    }
    assert!(records.set_setting("crm", "K0", Some("second")).await.unwrap(), "a replaced setting said it was new");
    let stored = records.settings("crm").await.unwrap();
    assert_eq!(stored.len(), sealed_values().len());
    assert_eq!(stored["K0"], "second");
    assert_eq!(stored["K2"], sealed_values()[2]);
    assert_eq!(records.settings_blocking("crm").unwrap(), stored);

    // Apart: another app, and an app whose name only starts the same.
    assert!(records.settings("crm2").await.unwrap().is_empty());
    assert!(records.settings("cr").await.unwrap().is_empty());

    assert!(records.set_setting("crm", "K1", None).await.unwrap());
    assert!(!records.set_setting("crm", "K1", None).await.unwrap(), "a removal of nothing said it removed something");
    assert!(!records.settings("crm").await.unwrap().contains_key("K1"));
}

async fn documents_round_trip(records: &dyn AppRecords) {
    let text = document();
    assert_eq!(records.tools("shop").await.unwrap(), None);
    records.set_tools("shop", Some(&text)).await.unwrap();
    records.set_tools("zoo", Some("[]")).await.unwrap();
    records.set_tools("alpha", Some("[1]")).await.unwrap();
    assert_eq!(records.tools("shop").await.unwrap().as_deref(), Some(text.as_str()));
    assert_eq!(records.tools("shop2").await.unwrap(), None);
    assert_eq!(records.apps_with_tools().await.unwrap(), ["alpha", "shop", "zoo"]);
    records.set_tools("zoo", None).await.unwrap();
    records.set_tools("zoo", None).await.unwrap();
    assert_eq!(records.apps_with_tools().await.unwrap(), ["alpha", "shop"]);

    assert_eq!(records.migrations_blocking("shop").unwrap(), None);
    records.set_migrations_blocking("shop", &text).unwrap();
    records.set_migrations_blocking("shop", &text).unwrap();
    assert_eq!(records.migrations_blocking("shop").unwrap().as_deref(), Some(text.as_str()));
    assert_eq!(records.migrations_blocking("other").unwrap(), None);

    assert_eq!(records.repo_link("shop").await.unwrap(), None);
    let stored = records.update_repo_link("shop", Box::new(|current| {
        assert_eq!(current, None);
        Ok(document())
    }))
    .await
    .unwrap();
    assert_eq!(stored, text);
    assert_eq!(records.repo_link("shop").await.unwrap().as_deref(), Some(text.as_str()));
    records.update_repo_link("alpha", Box::new(|_| Ok("{\"a\":1}".into()))).await.unwrap();
    assert_eq!(
        records.repo_links().await.unwrap(),
        vec![("alpha".to_string(), "{\"a\":1}".to_string()), ("shop".to_string(), text.clone())]
    );

    assert_eq!(records.installations().await.unwrap(), None);
    records.set_installations(&text).await.unwrap();
    records.set_installations("[]").await.unwrap();
    assert_eq!(records.installations().await.unwrap().as_deref(), Some("[]"));
    records.set_installations(&text).await.unwrap();
    assert_eq!(records.installations().await.unwrap().as_deref(), Some(text.as_str()));
}

async fn a_failed_edit_changes_nothing(records: &dyn AppRecords) {
    records.update_repo_link("kept", Box::new(|_| Ok("{\"v\":1}".into()))).await.unwrap();
    let refused = records.update_repo_link("kept", Box::new(|_| Err("no".into()))).await;
    assert_eq!(refused.unwrap_err(), "no");
    assert_eq!(records.repo_link("kept").await.unwrap().as_deref(), Some("{\"v\":1}"));
    assert!(records.update_repo_link("never", Box::new(|_| Err("no".into()))).await.is_err());
    assert_eq!(records.repo_link("never").await.unwrap(), None);
}

async fn concurrent_link_changes_all_land(records: Arc<dyn AppRecords>) {
    let tasks: Vec<_> = (0..32)
        .map(|n| {
            let records = records.clone();
            tokio::spawn(async move {
                records
                    .update_repo_link("busy", Box::new(move |current| {
                        let mut seen: Vec<u32> = current.map(|t| serde_json::from_str(t).unwrap()).unwrap_or_default();
                        seen.push(n);
                        Ok(serde_json::to_string(&seen).unwrap())
                    }))
                    .await
                    .unwrap()
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let mut seen: Vec<u32> = serde_json::from_str(&records.repo_link("busy").await.unwrap().unwrap()).unwrap();
    seen.sort();
    assert_eq!(seen, (0..32).collect::<Vec<_>>(), "a change to the link was lost");
}

/// Concurrent settings for one app: every name lands.
async fn concurrent_settings_all_land(records: Arc<dyn AppRecords>) {
    let tasks: Vec<_> = (0..32)
        .map(|n| {
            let records = records.clone();
            tokio::spawn(async move { records.set_setting("many", &format!("S{n}"), Some("v")).await.unwrap() })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(records.settings("many").await.unwrap().len(), 32, "a setting was lost");
}

/// A job as the scheduler writes one, holding a hostile value.
fn job(path: &str) -> String {
    serde_json::json!({ "schedule": "* * * * * *", "path": path, "last_status": hostile()[0] }).to_string()
}

fn parsed(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

async fn jobs_round_trip(records: &dyn AppRecords) {
    assert!(records.jobs("farm").await.unwrap().is_empty());
    assert_eq!(records.set_job("farm", "nightly", &job("/a"), 3).await.unwrap(), Ok(()));
    assert_eq!(records.set_job("farm", "hourly", &job("/b"), 3).await.unwrap(), Ok(()));
    assert_eq!(records.set_job("farm2", "hourly", &job("/c"), 3).await.unwrap(), Ok(()));
    let stored = records.jobs("farm").await.unwrap();
    assert_eq!(stored.keys().collect::<Vec<_>>(), ["hourly", "nightly"]);
    assert_eq!(parsed(&stored["nightly"]), parsed(&job("/a")));
    assert_eq!(records.jobs_blocking("farm").unwrap(), stored);
    // Apart: an app whose name only starts the same.
    assert!(records.jobs("far").await.unwrap().is_empty());

    // The ceiling counts new names only.
    assert_eq!(records.set_job("farm", "third", &job("/d"), 3).await.unwrap(), Ok(()));
    assert_eq!(records.set_job("farm", "fourth", &job("/e"), 3).await.unwrap(), Err(3));
    assert_eq!(records.set_job("farm", "nightly", &job("/z"), 3).await.unwrap(), Ok(()));
    assert_eq!(parsed(&records.jobs("farm").await.unwrap()["nightly"]), parsed(&job("/z")));

    // Every job on the site in one scan, sorted.
    let all: Vec<(String, String)> = records.all_jobs().await.unwrap().into_iter().map(|(app, name, _)| (app, name)).collect();
    let want = [("farm", "hourly"), ("farm", "nightly"), ("farm", "third"), ("farm2", "hourly")];
    assert_eq!(all, want.map(|(a, n)| (a.to_string(), n.to_string())));

    // An edit sees the job and lands; a job that is not there runs no edit.
    let changed = records
        .update_job("farm", "hourly", Box::new(|current| {
            let mut job = parsed(current.unwrap());
            job["last_run"] = 7.into();
            Ok(job.to_string())
        }))
        .await
        .unwrap();
    assert!(changed);
    assert_eq!(parsed(&records.jobs("farm").await.unwrap()["hourly"])["last_run"], 7);
    assert!(!records.update_job("farm", "gone", Box::new(|_| panic!("an edit ran on no job"))).await.unwrap());
    assert!(records.update_job("farm", "hourly", Box::new(|_| Err("no".into()))).await.is_err());
    assert_eq!(parsed(&records.jobs("farm").await.unwrap()["hourly"])["last_run"], 7);

    assert!(records.remove_job("farm", "third").await.unwrap());
    assert!(!records.remove_job("farm", "third").await.unwrap());
    assert!(!records.remove_job("farm2", "nightly").await.unwrap(), "removed another app's job");
    assert_eq!(records.jobs("farm").await.unwrap().len(), 2);
}

/// Runs finishing together each write their job's record: every change
/// lands, none written over another.
async fn concurrent_job_updates_all_land(records: Arc<dyn AppRecords>) {
    records.set_job("busy", "work", &job("/w"), 100).await.unwrap().unwrap();
    let tasks: Vec<_> = (0..32)
        .map(|n| {
            let records = records.clone();
            tokio::spawn(async move {
                let edit = Box::new(move |current: Option<&str>| {
                    let mut job = parsed(current.unwrap());
                    job[format!("k{n}")] = n.into();
                    Ok(job.to_string())
                });
                if n % 2 == 0 {
                    records.update_job("busy", "work", edit).await.unwrap()
                } else {
                    tokio::task::spawn_blocking(move || records.update_job_blocking("busy", "work", edit).unwrap()).await.unwrap()
                }
            })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap());
    }
    let landed = parsed(&records.jobs("busy").await.unwrap()["work"]);
    for n in 0..32 {
        assert_eq!(landed[format!("k{n}")], n, "a change to the job was lost");
    }
    // Many new jobs at once still stop at the ceiling.
    let tasks: Vec<_> = (0..20)
        .map(|n| {
            let records = records.clone();
            tokio::spawn(async move { records.set_job("crowd", &format!("j{n}"), &job("/x"), 5).await.unwrap() })
        })
        .collect();
    let mut stored = 0;
    for task in tasks {
        stored += task.await.unwrap().is_ok() as usize;
    }
    assert_eq!(stored, 5);
    assert_eq!(records.jobs("crowd").await.unwrap().len(), 5);
}

async fn run(records: Arc<dyn AppRecords>) {
    jobs_round_trip(&*records).await;
    concurrent_job_updates_all_land(records.clone()).await;
    settings_round_trip(&*records).await;
    documents_round_trip(&*records).await;
    a_failed_edit_changes_nothing(&*records).await;
    concurrent_link_changes_all_land(records.clone()).await;
    concurrent_settings_all_land(records).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_files_records_conform() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".site")).unwrap();
    run(Arc::new(Files::new(dir.path().to_path_buf()))).await;
}

/// The sidecars are where they always were, in the shape they always had,
/// and no temporary file is left beside them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_files_records_write_the_same_sidecars_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let records = Files::new(dir.path().to_path_buf());
    records.set_setting("crm", "B", Some("sealed-b")).await.unwrap();
    records.set_setting("crm", "A", Some("sealed-a")).await.unwrap();
    let mut expected = std::collections::BTreeMap::new();
    expected.insert("A", "sealed-a");
    expected.insert("B", "sealed-b");
    assert_eq!(std::fs::read_to_string(dir.path().join("crm.secrets")).unwrap(), serde_json::to_string_pretty(&expected).unwrap());
    // The last setting gone leaves an empty map, as it always did.
    records.set_setting("crm", "A", None).await.unwrap();
    records.set_setting("crm", "B", None).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("crm.secrets")).unwrap(), "{}");

    records.set_tools("crm", Some("[1]")).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("crm.tools")).unwrap(), "[1]");
    records.set_tools("crm", None).await.unwrap();
    assert!(!dir.path().join("crm.tools").exists(), "no tools left a file");
    records.set_migrations_blocking("crm", "[]").unwrap();
    assert!(dir.path().join("crm.migrations").is_file());
    records.update_repo_link("crm", Box::new(|_| Ok("{}".into()))).await.unwrap();
    assert!(dir.path().join("crm.repo").is_file());
    records.set_installations("[]").await.unwrap();
    assert!(dir.path().join(".site/github.json").is_file());
    // Jobs are one map per app, by name, as the scheduler always wrote them.
    records.set_job("crm", "b", "{\"path\":\"/b\",\"schedule\":\"* * * * * *\"}", 100).await.unwrap().unwrap();
    records.set_job("crm", "a", "{\"path\":\"/a\",\"schedule\":\"* * * * * *\"}", 100).await.unwrap().unwrap();
    let jobs: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("crm.jobs")).unwrap()).unwrap();
    assert_eq!(jobs, serde_json::json!({ "a": { "path": "/a", "schedule": "* * * * * *" }, "b": { "path": "/b", "schedule": "* * * * * *" } }));
    assert!(records.fire("crm", "a", 10).await.unwrap() && records.fire("crm", "a", 10).await.unwrap(), "files have one scheduler");

    let left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.') && name != ".site")
        .collect();
    assert!(left.is_empty(), "temporary files left behind: {left:?}");

    // A settings file that does not parse is an error, not an empty map a
    // change would be written over.
    std::fs::write(dir.path().join("torn.secrets"), "{not json").unwrap();
    assert!(records.settings("torn").await.is_err());
    assert!(records.set_setting("torn", "K", Some("v")).await.is_err());
    assert_eq!(std::fs::read_to_string(dir.path().join("torn.secrets")).unwrap(), "{not json");
    // A jobs file too: refused, not replaced by the one job being set.
    std::fs::write(dir.path().join("torn.jobs"), "{not json").unwrap();
    assert!(records.jobs("torn").await.is_err());
    assert!(records.set_job("torn", "j", "{}", 100).await.is_err());
    assert_eq!(std::fs::read_to_string(dir.path().join("torn.jobs")).unwrap(), "{not json");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_postgres_records_conform() {
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    let records = Arc::new(Postgres::new(pool.clone()));
    run(records.clone()).await;

    // A setting is kept as it was handed over (sealed by the caller): the
    // row holds that text and nothing else.
    records.set_setting("vault", "API_KEY", Some("sealed:abc")).await.unwrap();
    let client = pool.get().await.unwrap();
    let rows = client.query("select sealed from platform.app_settings where app = 'vault'", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "sealed:abc");
    drop(client);

    // A turn is claimed once however many schedulers ask at once; another
    // turn, or another job's, is its own.
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let records = records.clone();
            tokio::spawn(async move { records.fire("farm", "hourly", 1_000_000_000).await.unwrap() })
        })
        .collect();
    let mut won = 0;
    for task in tasks {
        won += task.await.unwrap() as usize;
    }
    assert_eq!(won, 1);
    assert!(records.fire("farm", "hourly", 1_000_000_002).await.unwrap());
    assert!(records.fire("farm", "nightly", 1_000_000_000).await.unwrap());
    assert!(records.fire("farm2", "hourly", 1_000_000_000).await.unwrap());
    // Fires a day older than the latest are pruned as it is claimed.
    assert!(records.fire("farm", "hourly", 1_000_200_000).await.unwrap());
    let kept: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one("select count(*) from platform.job_fires where app = 'farm' and name = 'hourly'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, 1);
    drop(records);
    std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
}

/// A removal takes every record of the app out, keeps them in
/// `removed_records`, hands them back as sidecars, and leaves an app whose
/// name only starts the same alone: an app published again at the name
/// starts with nothing of the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_removal_retires_the_records_and_keeps_them_on_postgres() {
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    let records: Arc<dyn AppRecords> = Arc::new(Postgres::new(pool.clone()));
    for app in ["shop", "shop_x"] {
        records.set_setting(app, "K", Some("sealed")).await.unwrap();
        records.set_tools(app, Some("[1]")).await.unwrap();
        records.set_migrations_blocking(app, "[[\"001.sql\",\"create table t (n)\"]]").unwrap();
        records.update_repo_link(app, Box::new(|_| Ok("{\"owner\":\"o\"}".into()))).await.unwrap();
        records.set_job(app, "nightly", "{\"path\":\"/n\",\"schedule\":\"0 0 3 * * *\"}", 100).await.unwrap().unwrap();
    }
    let retiring = records.clone();
    let retired = tokio::task::spawn_blocking(move || retiring.retire_blocking("shop", 77)).await.unwrap().unwrap();
    let kinds: Vec<&str> = retired.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, ["secrets", "tools", "migrations", "repo", "jobs"]);
    let jobs: serde_json::Value = serde_json::from_str(&retired[4].1).unwrap();
    assert_eq!(jobs, serde_json::json!({ "nightly": { "path": "/n", "schedule": "0 0 3 * * *" } }), "not the sidecar's shape");
    assert!(records.jobs("shop").await.unwrap().is_empty());
    assert_eq!(records.jobs("shop_x").await.unwrap().len(), 1);
    assert_eq!(retired[0].1, super::pretty(&std::collections::BTreeMap::from([("K", "sealed")])).unwrap());
    assert!(records.settings("shop").await.unwrap().is_empty());
    assert_eq!(records.tools("shop").await.unwrap(), None);
    assert_eq!(records.migrations_blocking("shop").unwrap(), None);
    assert_eq!(records.repo_link("shop").await.unwrap(), None);
    assert_eq!(records.settings("shop_x").await.unwrap().len(), 1);
    assert!(records.tools("shop_x").await.unwrap().is_some());
    let kept: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one("select count(*) from platform.removed_records where app = 'shop' and removed_at = 77", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, 5);
    drop(records);
    std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
}
