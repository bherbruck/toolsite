//! An app doing heavy work: starting its own jobs (`jobs.run`), asking for
//! more time, fuel, rows and memory (`[limits]`), and writing many rows in
//! one transaction (`db.batch`). Driven through the committed guest fixture,
//! so each property is proven on the path a real handler takes.
//!
//! Every scenario runs on the backend `TOOLSITE_TEST_BACKEND` names: files
//! by default, Postgres when it is `postgres` (`scripts/test-postgres.sh
//! --full` sets it), where job records, slots and start rates are the
//! database's. `tests/jobs_runners.rs` holds what only two runners show.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use toolsite::{
    platform::schedule,
    runtime::{db, limits, wasm::Runtime},
    AppState, Config,
};

mod common;
use common::blocking;

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

struct Site {
    _dir: TempDir,
    config: Arc<Config>,
    runtime: Arc<Runtime>,
    database: Option<common::Database>,
}

impl Site {
    fn state(&self) -> AppState {
        AppState { config: self.config.clone(), runtime: self.runtime.clone() }
    }
}

/// The database goes with the site, on a thread of its own: a test that
/// panics still drops it, and runs it started end with it.
impl Drop for Site {
    fn drop(&mut self) {
        if let Some(database) = self.database.take() {
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(database.drop())
            })
            .join()
            .unwrap();
        }
    }
}

async fn site_with(config: impl FnOnce(Config) -> Config) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let local = Config::local(dir.path().to_path_buf(), "test-token");
    let (local, database) = if common::wants_postgres() {
        let database = common::Database::new().await;
        (Config { stores: database.stores(), ..local }, Some(database))
    } else {
        (local, None)
    };
    let config = Arc::new(config(local));
    let runtime = Runtime::new().unwrap();
    // What the router does: jobs an app starts run on this runtime.
    config.jobs.attach(&runtime);
    Site { _dir: dir, config, runtime, database }
}

async fn site() -> Site {
    site_with(|config| config).await
}

fn install(site: &Site, app: &str) {
    std::fs::create_dir_all(site.config.data_dir.join(app)).unwrap();
    std::fs::write(site.config.data_dir.join(app).join("handler.wasm"), HANDLER).unwrap();
}

fn request(method: &str, path: &str, query: &str, body: &str) -> toolsite::runtime::wasm::Request {
    toolsite::runtime::wasm::Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
    }
}

/// One request through the handler, as `user`, under the app's own limits.
async fn call_as(
    site: &Site,
    app: &str,
    user: Option<toolsite::runtime::wasm::User>,
    request: toolsite::runtime::wasm::Request,
) -> (u16, String) {
    let guards = limits::of(&site.config, app).await.request;
    let (runtime, config, app) = (site.runtime.clone(), site.config.clone(), app.to_string());
    let response = tokio::task::spawn_blocking(move || runtime.handle(config, &app, HANDLER, user, request, guards))
        .await
        .unwrap()
        .unwrap();
    (response.status, String::from_utf8_lossy(&response.body).to_string())
}

async fn call(site: &Site, app: &str, request: toolsite::runtime::wasm::Request) -> (u16, String) {
    call_as(site, app, None, request).await
}

async fn run_now(site: &Site, app: &str, name: &str) -> (u16, String) {
    call(site, app, request("GET", "/api/jobs-run", &format!("name={name}"), "")).await
}

fn scalar(config: &Config, app: &str, sql: &str) -> serde_json::Value {
    db::run(config, app, sql, &[]).map(|out| out.rows[0][0].clone()).unwrap_or(serde_json::Value::Null)
}

fn marks(config: &Config, app: &str, what: &str) -> i64 {
    scalar(config, app, &format!("select count(*) from marks where what = '{what}'")).as_i64().unwrap_or(0)
}

/// Waits, a little at a time, for `done`, up to `limit`.
async fn until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    done()
}

const NEVER: &str = "0 0 0 1 1 *";

// --- jobs.run ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn jobs_run_starts_the_job_at_once_and_it_runs_with_no_identity() {
    let site = site().await;
    install(&site, "app");
    schedule::set_job(&site.config, "app", "mark", NEVER, "/api/job-mark").await.unwrap();

    // Asked by a signed-in visitor; the job must not inherit them.
    let alice = toolsite::runtime::wasm::User { id: "u1".into(), email: "alice@example.com".into() };
    let started = Instant::now();
    let (status, body) = call_as(&site, "app", Some(alice), request("GET", "/api/jobs-run", "name=mark", "")).await;
    assert_eq!((status, body.as_str()), (200, "started"));
    assert!(until(Duration::from_secs(20), || marks(&site.config, "app", "mark") == 1).await, "the job never ran");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(scalar(&site.config, "app", "select who from marks"), serde_json::json!("anonymous:none"));

    // Recorded as a run, with when it started and finished and how long.
    assert!(until(Duration::from_secs(5), || !schedule::is_running(&site.config, "app", "mark")).await);
    let job = &schedule::read_jobs(&site.config, "app")["mark"];
    assert_eq!(job.last_status.as_deref(), Some("200"));
    assert!(job.last_started_at.is_some() && job.last_finished_at.is_some() && job.last_duration_ms.is_some(), "{job:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_asked_for_while_one_runs_is_queued_once_and_never_runs_alongside() {
    let site = site().await;
    install(&site, "app");
    schedule::set_job(&site.config, "app", "nap", NEVER, "/api/job-nap").await.unwrap();

    assert_eq!(run_now(&site, "app", "nap").await, (200, "started".into()));
    assert!(schedule::is_running(&site.config, "app", "nap"));
    // Asked twice more while it naps: one more run, not two.
    assert_eq!(run_now(&site, "app", "nap").await, (200, "queued".into()));
    assert_eq!(run_now(&site, "app", "nap").await, (200, "queued".into()));
    // A person asking gets the same answer rather than a second run.
    assert_eq!(schedule::run_job(&site.state(), "app", "nap").await.unwrap(), schedule::Ran::Queued);

    assert!(until(Duration::from_secs(30), || !schedule::is_running(&site.config, "app", "nap")).await, "still running");
    assert_eq!(marks(&site.config, "app", "nap"), 2, "the queued asks were not one run");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scheduler_skips_a_job_that_is_still_running_and_records_the_skip() {
    let site = site().await;
    install(&site, "app");
    schedule::set_job(&site.config, "app", "nap", "* * * * * *", "/api/job-nap").await.unwrap();
    assert_eq!(run_now(&site, "app", "nap").await, (200, "started".into()));

    let scheduler = schedule::Scheduler::new(site.state());
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 1;
    assert!(scheduler.tick(at).await.is_empty(), "a second run started beside the first");
    assert_eq!(schedule::read_jobs(&site.config, "app")["nap"].last_skipped_at, Some(at));

    assert!(until(Duration::from_secs(30), || !schedule::is_running(&site.config, "app", "nap")).await);
    assert_eq!(marks(&site.config, "app", "nap"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_apps_starts_past_the_sites_rate_are_refused() {
    let site = site_with(|config| Config { jobs: Arc::new(schedule::Jobs::new(2)), ..config }).await;
    install(&site, "app");
    for name in ["one", "two", "three"] {
        schedule::set_job(&site.config, "app", name, NEVER, "/api/job-mark").await.unwrap();
    }
    assert_eq!(run_now(&site, "app", "one").await.0, 200);
    assert_eq!(run_now(&site, "app", "two").await.0, 200);
    let (status, body) = run_now(&site, "app", "three").await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("limit"), "{body}");
    assert!(until(Duration::from_secs(20), || marks(&site.config, "app", "mark") == 2).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_run_reaches_only_the_apps_own_declared_jobs() {
    let site = site().await;
    install(&site, "one");
    install(&site, "two");
    schedule::set_job(&site.config, "one", "mark", NEVER, "/api/job-mark").await.unwrap();

    // Another app's job, by name: not this app's to start.
    let (status, body) = run_now(&site, "two", "mark").await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("declares no job"), "{body}");
    // A name nothing declares, or one shaped like a path.
    for name in ["nothing", "../one/mark", "one/mark"] {
        assert_eq!(run_now(&site, "one", name).await.0, 409, "{name}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(marks(&site.config, "one", "mark"), 0);
    assert!(!schedule::is_running(&site.config, "one", "mark"));
}

/// The production case: a job that works in stages, each asking for the
/// next. With a fixed 30-second tick that was thirty seconds a stage; queued
/// behind itself, the next stage starts the moment the last one ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_chains_itself_twenty_stages_back_to_back() {
    let site = site().await;
    install(&site, "app");
    schedule::set_job(&site.config, "app", "chain", NEVER, "/api/chain").await.unwrap();

    let started = Instant::now();
    assert_eq!(run_now(&site, "app", "chain").await, (200, "started".into()));
    assert!(
        until(Duration::from_secs(60), || marks(&site.config, "app", "stage") >= 20).await,
        "only {} stages",
        marks(&site.config, "app", "stage")
    );
    let took = started.elapsed();
    assert!(took < Duration::from_secs(30), "twenty stages took {took:?}");
    assert!(until(Duration::from_secs(10), || !schedule::is_running(&site.config, "app", "chain")).await);
    assert_eq!(marks(&site.config, "app", "stage"), 20, "the chain ran past its own end");
}

/// The scheduler sleeps until the next due time rather than a fixed tick, so
/// a schedule of every second runs every second.
#[tokio::test(flavor = "multi_thread")]
async fn the_scheduler_wakes_for_each_second_of_a_one_second_schedule() {
    let site = site().await;
    install(&site, "app");
    schedule::set_job(&site.config, "app", "mark", "* * * * * *", "/api/job-mark").await.unwrap();
    // Compiled before the clock starts, so a loaded machine's first compile
    // is not counted against the schedule.
    assert_eq!(call(&site, "app", request("GET", "/api/echo", "", "")).await.0, 200);
    schedule::Scheduler::new(site.state()).spawn();
    // Three runs within ten seconds: a 30-second tick could not do one. A
    // turn that comes while the last run still goes is skipped, so on a
    // loaded machine not every second gets its run.
    assert!(
        until(Duration::from_secs(10), || marks(&site.config, "app", "mark") >= 3).await,
        "ran {} times",
        marks(&site.config, "app", "mark")
    );
}

// --- [limits] ---------------------------------------------------------------

async fn declare(site: &Site, app: &str, toml: &str) -> Vec<String> {
    toolsite::platform::manifest::apply(&site.config, app, toml).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn without_limits_an_app_runs_on_the_defaults_it_always_had() {
    let site = site().await;
    install(&site, "app");
    let effective = limits::of(&site.config, "app").await;
    assert_eq!(effective.request, toolsite::runtime::wasm::Guards::default());
    assert_eq!(effective.job.wall_clock, Duration::from_secs(60));
    let many = "q=with%20recursive%20n(i)%20as%20(select%201%20union%20all%20select%20i%2B1%20from%20n%20where%20i%20%3C%2020000)%20select%20i%20from%20n";
    assert_eq!(call(&site, "app", request("GET", "/api/sql-count", many, "")).await, (200, "1000:true".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn asked_limits_apply_and_values_past_a_ceiling_are_clamped_and_said() {
    let site = site().await;
    install(&site, "app");
    let notes = declare(
        &site,
        "app",
        "[limits]\nrequest_seconds = 30\njob_seconds = 86400\njob_fuel = 9000000000\nquery_rows = 5000\nmemory_mb = 256\n",
    )
    .await;
    let said = notes.join("\n");
    assert!(said.contains("job_seconds: asked for 86400, the site allows 900"), "{said}");
    assert!(!said.contains("request_seconds: asked"), "a value under its ceiling was reported as clamped: {said}");

    let effective = limits::of(&site.config, "app").await;
    assert_eq!(effective.request.wall_clock, Duration::from_secs(30));
    assert_eq!(effective.job.wall_clock, Duration::from_secs(900));
    assert_eq!(effective.job.fuel, Some(9_000_000_000));
    assert_eq!(effective.request.memory_bytes, 256 * 1024 * 1024);
    assert_eq!(effective.job.query_rows, 5000);

    // A site with a lower ceiling clamps the same ask lower.
    let tight = site_with(|config| Config {
        limits: limits::Ceilings { query_rows: 2000, ..Default::default() },
        ..config
    }).await;
    let notes = declare(&tight, "app", "[limits]\nquery_rows = 5000\n").await;
    assert!(notes.join("\n").contains("query_rows: asked for 5000, the site allows 2000"), "{notes:?}");
    assert_eq!(limits::of(&tight.config, "app").await.request.query_rows, 2000);

    // Withdrawing the block goes back to the defaults; a typo is refused.
    declare(&site, "app", "spa = false\n").await;
    assert_eq!(limits::of(&site.config, "app").await.request, toolsite::runtime::wasm::Guards::default());
    assert!(toolsite::platform::manifest::apply(&site.config, "app", "[limits]\nquery_row = 5\n").await.is_err());
    assert!(toolsite::platform::manifest::apply(&site.config, "app", "[limits]\nquery_rows = 0\n").await.is_err());
}

const ORDERS: &str = "create table orders (id integer primary key, owner_id text, total real);";
const ORDERS_ACCESS: &str = r#"
[access]
[[access.table]]
table = "orders"
where = "owner_id = current_user()"
owner = "owner_id"
write = true
"#;

async fn orders(site: &Site, app: &str, extra: &str) {
    toolsite::runtime::migrate::store(&site.config, app, vec![("001.sql".to_string(), ORDERS.to_string())]).unwrap();
    toolsite::runtime::migrate::apply(&site.config, app).unwrap();
    declare(site, app, &format!("{extra}\n{ORDERS_ACCESS}")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn query_rows_applies_to_query_and_query_scoped_alike() {
    let site = site().await;
    install(&site, "app");
    orders(&site, "app", "[limits]\nquery_rows = 3000").await;
    let user = blocking(|| toolsite::accounts::users::sign_up(&site.config, "alice@example.com", "correct horse battery")).unwrap();
    db::run(
        &site.config,
        "app",
        &format!(
            "with recursive n(i) as (select 1 union all select i+1 from n where i < 4000) \
             insert into orders (owner_id, total) select '{}', i from n",
            user.id
        ),
        &[],
    )
    .unwrap();
    let alice = Some(toolsite::runtime::wasm::User { id: user.id.clone(), email: user.email.clone() });

    assert_eq!(
        call(&site, "app", request("GET", "/api/sql-count", "q=select%20*%20from%20orders", "")).await,
        (200, "3000:true".into())
    );
    assert_eq!(
        call_as(&site, "app", alice.clone(), request("GET", "/api/scoped-count", "q=select%20*%20from%20my_orders", "")).await,
        (200, "3000:true".into())
    );
    // Under the cap, nothing is cut.
    assert_eq!(
        call_as(&site, "app", alice, request("GET", "/api/scoped-count", "q=select%20*%20from%20my_orders%20limit%2010", "")).await,
        (200, "10:false".into())
    );
}

// --- db.batch ---------------------------------------------------------------

async fn batch(site: &Site, app: &str, statements: &[&str]) -> (u16, String) {
    call(site, app, request("POST", "/api/batch", "", &statements.join("\n"))).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_answers_rows_changed_per_statement_and_a_failure_undoes_all_of_it() {
    let site = site().await;
    install(&site, "app");
    db::run(&site.config, "app", "create table t (id integer primary key, v text not null)", &[]).unwrap();

    let (status, body) = batch(
        &site,
        "app",
        &["insert into t (v) values (?)\ta", "insert into t (v) values (?), (?)\tb\tc", "update t set v = 'z'", "select * from t"],
    )
    .await;
    assert_eq!((status, body.as_str()), (200, "ok:1,2,3,0"));

    // The third statement breaks a constraint: the first two are undone.
    let (status, body) = batch(
        &site,
        "app",
        &["insert into t (v) values ('d')", "delete from t where id = 1", "insert into t (v) values (?)\tn", "insert into t (v) values ('e')"],
    )
    .await;
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("statement 3"), "the failing statement is not named: {body}");
    assert_eq!(scalar(&site.config, "app", "select count(*) from t"), serde_json::json!(3));
    assert_eq!(scalar(&site.config, "app", "select count(*) from t where id = 1"), serde_json::json!(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn parameters_in_a_batch_are_bound_so_text_cannot_become_sql() {
    let site = site().await;
    install(&site, "app");
    db::run(&site.config, "app", "create table t (v text)", &[]).unwrap();
    let payload = "x'); drop table t; --";
    let (status, body) = batch(&site, "app", &[&format!("insert into t values (?)\t{payload}")]).await;
    assert_eq!((status, body.as_str()), (200, "ok:1"));
    assert_eq!(scalar(&site.config, "app", "select v from t"), serde_json::json!(payload));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_cannot_attach_reach_another_app_or_end_the_hosts_transaction() {
    let site = site().await;
    install(&site, "app");
    install(&site, "victim");
    db::run(&site.config, "victim", "create table secrets (v text); insert into secrets values ('mine')", &[]).unwrap();
    db::run(&site.config, "app", "create table t (v text)", &[]).unwrap();

    for statements in [
        vec!["insert into t values ('a')", "attach database '../victim/data.db' as v"],
        vec!["insert into t values ('a')", "pragma writable_schema = on"],
        vec!["insert into t values ('a')", "commit", "insert into t values ('b')", "select * from nowhere"],
        vec!["insert into t values ('a')", "savepoint s", "release s"],
        vec!["insert into t values ('a')", "begin"],
    ] {
        let (status, body) = batch(&site, "app", &statements).await;
        assert_ne!(status, 200, "{statements:?} went through: {body}");
        assert_eq!(scalar(&site.config, "app", "select count(*) from t"), serde_json::json!(0), "{statements:?} left rows");
    }
    let (status, body) = batch(&site, "app", &["attach database '../victim/data.db' as v"]).await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("not authorized"), "{body}");
    // Two statements in one entry would slip the second past the count.
    assert_ne!(batch(&site, "app", &["insert into t values ('a'); insert into t values ('b')"]).await.0, 200);
    assert_eq!(scalar(&site.config, "victim", "select v from secrets"), serde_json::json!("mine"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_batch_stops_at_the_calls_time_limit() {
    let site = site().await;
    install(&site, "app");
    declare(&site, "app", "[limits]\nrequest_seconds = 1\n").await;
    db::run(&site.config, "app", "create table t (v integer)", &[]).unwrap();
    let started = Instant::now();
    let body = "insert into t values (1)\n\
                with recursive n(i) as (select 1 union all select i+1 from n) insert into t select i from n";
    let guards = limits::of(&site.config, "app").await.request;
    let (runtime, config) = (site.runtime.clone(), site.config.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        runtime.handle(config, "app", HANDLER, None, request("POST", "/api/batch", "", body), guards)
    })
    .await
    .unwrap();
    // Stopped either way: the statement interrupted, or the guest after it.
    if let Ok(response) = outcome {
        assert_ne!(response.status, 200, "{}", String::from_utf8_lossy(&response.body));
    }
    assert!(started.elapsed() < Duration::from_secs(10), "ran for {:?}", started.elapsed());
    assert_eq!(scalar(&site.config, "app", "select count(*) from t"), serde_json::json!(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_past_its_size_caps_is_refused_before_anything_runs() {
    let site = site().await;
    install(&site, "app");
    db::run(&site.config, "app", "create table t (v integer)", &[]).unwrap();
    let many: Vec<&str> = std::iter::repeat_n("insert into t values (1)", db::MAX_BATCH_STATEMENTS + 1).collect();
    let (status, body) = batch(&site, "app", &many).await;
    assert_eq!(status, 500);
    assert!(body.contains("at most 10000 statements"), "{body}");

    let long = format!("insert into t values (1) -- {}", "x".repeat(db::MAX_BATCH_SQL_BYTES));
    let (status, body) = batch(&site, "app", &[&long]).await;
    assert_eq!(status, 500);
    assert!(body.contains("bytes of SQL"), "{body}");
    assert_eq!(scalar(&site.config, "app", "select count(*) from t"), serde_json::json!(0));

    // Exactly at the statement cap is fine.
    let full: Vec<&str> = std::iter::repeat_n("insert into t values (1)", db::MAX_BATCH_STATEMENTS).collect();
    assert_eq!(batch(&site, "app", &full).await.0, 200);
    assert_eq!(scalar(&site.config, "app", "select count(*) from t"), serde_json::json!(10_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_scoped_holds_every_statement_to_the_row_level_policy() {
    let site = site().await;
    install(&site, "app");
    orders(&site, "app", "").await;
    let alice = blocking(|| toolsite::accounts::users::sign_up(&site.config, "alice@example.com", "correct horse battery")).unwrap();
    let bob = blocking(|| toolsite::accounts::users::sign_up(&site.config, "bob@example.com", "correct horse battery")).unwrap();
    db::run(&site.config, "app", &format!("insert into orders (id, owner_id, total) values (1, '{}', 10)", bob.id), &[]).unwrap();
    let as_alice = Some(toolsite::runtime::wasm::User { id: alice.id.clone(), email: alice.email.clone() });
    let scoped = |statements: &[&str]| request("POST", "/api/batch-scoped", "", &statements.join("\n"));

    // Her own rows, through the view: fine, and invisible rows are untouched.
    let (status, body) = call_as(
        &site,
        "app",
        as_alice.clone(),
        scoped(&["insert into my_orders (total) values (?)\ti:5", "update my_orders set total = 0 where id = 1", "delete from my_orders where id = 1"]),
    )
    .await;
    assert_eq!((status, body.as_str()), (200, "ok:1,0,0"));
    assert_eq!(scalar(&site.config, "app", "select total from orders where id = 1"), serde_json::json!(10.0));
    let mine = format!("select count(*) from orders where owner_id = '{}'", alice.id);
    assert_eq!(scalar(&site.config, "app", &mine), serde_json::json!(1));

    // The base table, or a row in someone else's name: the whole batch fails.
    for statements in [
        vec!["insert into my_orders (total) values (6)", "delete from orders"],
        vec!["insert into my_orders (total) values (6)", "update orders set total = 0"],
        vec!["insert into my_orders (total) values (6)", &*format!("insert into my_orders (owner_id, total) values ('{}', 9)", bob.id)],
    ] {
        let (status, body) = call_as(&site, "app", as_alice.clone(), scoped(&statements)).await;
        assert_ne!(status, 200, "{statements:?} went through: {body}");
        assert_eq!(scalar(&site.config, "app", &mine), serde_json::json!(1), "{statements:?} left alice's insert behind");
    }
    assert_eq!(scalar(&site.config, "app", "select count(*) from orders"), serde_json::json!(2));

    // Nobody signed in reaches nobody's rows.
    let (status, body) = call(&site, "app", scoped(&["delete from my_orders"])).await;
    assert_eq!((status, body.as_str()), (200, "ok:0"));
    assert_eq!(scalar(&site.config, "app", "select count(*) from orders"), serde_json::json!(2));
}

// --- adversarial: an app that tries to take more than its share ---------------

/// An app that declares many jobs and starts them all together gets a few
/// at once, by `jobs.run`, by a person and by the schedule alike; the rest
/// are refused or skipped, and another app is not held up.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_runs_only_a_few_jobs_at_once_however_they_are_started() {
    let site = site_with(|config| Config { jobs: Arc::new(schedule::Jobs::new(600).with_running_per_app(2)), ..config }).await;
    install(&site, "greedy");
    install(&site, "other");
    for n in 0..6 {
        schedule::set_job(&site.config, "greedy", &format!("nap{n}"), NEVER, "/api/job-nap").await.unwrap();
    }
    schedule::set_job(&site.config, "other", "nap", NEVER, "/api/job-nap").await.unwrap();

    assert_eq!(run_now(&site, "greedy", "nap0").await, (200, "started".into()));
    assert_eq!(run_now(&site, "greedy", "nap1").await, (200, "started".into()));
    let (status, body) = run_now(&site, "greedy", "nap2").await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("at once"), "{body}");
    // Asking again for one that runs still queues it: no new slot needed.
    assert_eq!(run_now(&site, "greedy", "nap0").await, (200, "queued".into()));
    // A person asking is refused the same way.
    let refused = schedule::run_job(&site.state(), "greedy", "nap3").await.unwrap_err();
    assert!(refused.contains("at once"), "{refused}");
    // The schedule too: every job due, two already running, none started.
    let scheduler = schedule::Scheduler::new(site.state());
    for n in 0..6 {
        schedule::set_job(&site.config, "greedy", &format!("nap{n}"), "* * * * * *", "/api/job-nap").await.unwrap();
    }
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 1;
    let started = scheduler.tick(at).await;
    assert!(started.is_empty(), "{} more runs started beside the two", started.len());
    assert_eq!(schedule::read_jobs(&site.config, "greedy")["nap4"].last_skipped_at, Some(at));
    // Another app starts at once.
    assert_eq!(run_now(&site, "other", "nap").await, (200, "started".into()));

    // Two runs, and nap0's one queued rerun, never more than two at a time.
    // Watched until all three have marked, rather than until nothing runs:
    // a slot can be free a moment before the run that held it has written.
    let mut most = 0;
    let all_marked = until(Duration::from_secs(60), || {
        most = most.max((0..6).filter(|n| schedule::is_running(&site.config, "greedy", &format!("nap{n}"))).count());
        marks(&site.config, "greedy", "nap") >= 3
    })
    .await;
    let jobs = schedule::read_jobs(&site.config, "greedy");
    assert!(all_marked, "{} marks: {:?}", marks(&site.config, "greedy", "nap"), (jobs.get("nap0"), jobs.get("nap1")));
    assert!(most <= 2, "{most} of the app's jobs ran at once");
    assert!(until(Duration::from_secs(60), || !(0..6).any(|n| schedule::is_running(&site.config, "greedy", &format!("nap{n}")))).await);
    assert_eq!(marks(&site.config, "greedy", "nap"), 3, "a run beyond the two and the queued one");
    // With the slots free again, the next asks start.
    assert_eq!(run_now(&site, "greedy", "nap2").await, (200, "started".into()));
}

/// A visitor hammering a public route that starts a job: the job runs once
/// at a time with one rerun queued, and the app's rate holds however fast
/// the visitor asks.
#[tokio::test(flavor = "multi_thread")]
async fn a_visitor_looping_on_a_route_that_starts_a_job_cannot_queue_more_than_one_rerun() {
    let site = site_with(|config| Config { jobs: Arc::new(schedule::Jobs::new(5)), ..config }).await;
    install(&site, "app");
    schedule::set_job(&site.config, "app", "nap", NEVER, "/api/job-nap").await.unwrap();
    schedule::set_job(&site.config, "app", "mark", NEVER, "/api/job-mark").await.unwrap();
    let mut answers = Vec::new();
    for _ in 0..50 {
        answers.push(run_now(&site, "app", "nap").await.1);
    }
    assert_eq!(answers.iter().filter(|a| *a == "started").count(), 1, "{answers:?}");
    assert!(until(Duration::from_secs(30), || !schedule::is_running(&site.config, "app", "nap")).await);
    assert_eq!(marks(&site.config, "app", "nap"), 2, "fifty asks were more than one run and one rerun");
    // Starts and reruns counted: two of the five a minute are spent, and
    // three more starts of anything use up the rest.
    for _ in 0..3 {
        assert_eq!(run_now(&site, "app", "mark").await.0, 200);
        assert!(until(Duration::from_secs(20), || !schedule::is_running(&site.config, "app", "mark")).await);
    }
    let (status, body) = run_now(&site, "app", "mark").await;
    assert_eq!(status, 409, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_declaring_more_jobs_than_it_may_is_refused_whole() {
    let site = site().await;
    install(&site, "app");
    let toml: String = (0..=schedule::MAX_JOBS_PER_APP)
        .map(|n| format!("[[job]]\nname = \"j{n}\"\nschedule = \"0 0 3 * * *\"\npath = \"/api/x\"\n"))
        .collect();
    let error = toolsite::platform::manifest::apply(&site.config, "app", &toml).await.unwrap_err();
    assert!(error.contains("at most"), "{error}");
    assert!(schedule::read_jobs(&site.config, "app").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_that_are_not_whole_positive_numbers_are_refused_and_huge_ones_clamped() {
    let site = site().await;
    install(&site, "app");
    for bad in [
        "request_seconds = -1",
        "request_seconds = 1.5",
        "job_fuel = \"none\"",
        "memory_mb = 99999999999999999999999",
        "query_rows = 0",
        "query_rows = true",
    ] {
        let outcome = toolsite::platform::manifest::apply(&site.config, "app", &format!("[limits]\n{bad}\n")).await;
        assert!(outcome.is_err(), "{bad} was taken: {outcome:?}");
    }
    assert_eq!(limits::of(&site.config, "app").await.request, toolsite::runtime::wasm::Guards::default());

    // The largest number TOML has: clamped to each ceiling, said so.
    let notes = declare(
        &site,
        "app",
        "[limits]\nrequest_seconds = 9223372036854775807\nmemory_mb = 9223372036854775807\nquery_rows = 9223372036854775807\njob_fuel = 9223372036854775807\n",
    )
    .await;
    assert!(notes.join("\n").contains("memory_mb: asked for"), "{notes:?}");
    let effective = limits::of(&site.config, "app").await;
    let ceilings = limits::Ceilings::default();
    assert_eq!(effective.request.wall_clock, Duration::from_secs(ceilings.request_seconds));
    assert_eq!(effective.job.memory_bytes as u64, ceilings.memory_mb * 1024 * 1024);
    assert_eq!(effective.request.query_rows as u64, ceilings.query_rows);
    assert_eq!(effective.job.fuel, ceilings.job_fuel);
    // And a call runs under them.
    assert_eq!(call(&site, "app", request("GET", "/api/echo", "", "")).await.0, 200);
}

/// A request and a job of one app run under their own limits: a long
/// job's time is not a request's, and a short request's is not a job's.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_never_gets_a_jobs_time_nor_a_job_a_requests() {
    let site = site().await;
    install(&site, "app");
    declare(&site, "app", "[limits]\nrequest_seconds = 1\njob_seconds = 10\n").await;
    schedule::set_job(&site.config, "app", "nap", NEVER, "/api/job-nap").await.unwrap();

    // A request that naps past its one second is stopped there. Compiled
    // first, so the clock below is the call's alone.
    assert_eq!(call(&site, "app", request("GET", "/api/echo", "", "")).await.0, 200);
    let guards = limits::of(&site.config, "app").await.request;
    let (runtime, config) = (site.runtime.clone(), site.config.clone());
    let started = Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        runtime.handle(config, "app", HANDLER, None, request("GET", "/api/job-nap", "ms=3000", ""), guards)
    })
    .await
    .unwrap();
    assert!(outcome.is_err() || outcome.as_ref().unwrap().status != 200, "a request napped three seconds on a one-second limit");
    assert!(started.elapsed() < Duration::from_millis(2500), "{:?}", started.elapsed());
    assert_eq!(marks(&site.config, "app", "nap"), 0);

    // The same nap as a job, started from that request's app, has ten.
    assert_eq!(run_now(&site, "app", "nap").await, (200, "started".into()));
    assert!(until(Duration::from_secs(30), || !schedule::is_running(&site.config, "app", "nap")).await);
    assert_eq!(schedule::read_jobs(&site.config, "app")["nap"].last_status.as_deref(), Some("200"));

    // And a job of a site whose job clock is short is stopped on it, even
    // though requests there may run longer.
    let short = site_with(|config| Config {
        limits: limits::Ceilings { job_seconds: 1, request_seconds: 60, ..Default::default() },
        ..config
    }).await;
    install(&short, "app");
    declare(&short, "app", "[limits]\nrequest_seconds = 30\n").await;
    schedule::set_job(&short.config, "app", "nap", NEVER, "/api/job-nap").await.unwrap();
    match schedule::run_job(&short.state(), "app", "nap").await.unwrap() {
        schedule::Ran::Finished(status) => assert!(status.starts_with("failed"), "{status}"),
        other => panic!("{other:?}"),
    }
}

/// A scoped batch has the deadline a scoped query has: a recursive CTE that
/// reads nothing but itself still stops.
#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_batch_cannot_outrun_the_call_with_a_recursive_cte() {
    let site = site().await;
    install(&site, "app");
    orders(&site, "app", "[limits]\nrequest_seconds = 1").await;
    let user = blocking(|| toolsite::accounts::users::sign_up(&site.config, "alice@example.com", "correct horse battery")).unwrap();
    let alice = Some(toolsite::runtime::wasm::User { id: user.id.clone(), email: user.email.clone() });
    let body = "insert into my_orders (total) values (1)\n\
                with recursive n(i) as (select 1 union all select i+1 from n) select count(*) from n";
    let guards = limits::of(&site.config, "app").await.request;
    let (runtime, config) = (site.runtime.clone(), site.config.clone());
    let started = Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        runtime.handle(config, "app", HANDLER, alice, request("POST", "/api/batch-scoped", "", body), guards)
    })
    .await
    .unwrap();
    if let Ok(response) = outcome {
        assert_ne!(response.status, 200, "{}", String::from_utf8_lossy(&response.body));
    }
    assert!(started.elapsed() < Duration::from_secs(5), "ran for {:?}", started.elapsed());
    assert_eq!(scalar(&site.config, "app", "select count(*) from orders"), serde_json::json!(0));
}

/// What a scoped batch reports when it fails is about the statement, not
/// about rows the person cannot see.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_scoped_batch_says_nothing_of_rows_the_person_cannot_see() {
    let site = site().await;
    install(&site, "app");
    orders(&site, "app", "").await;
    let alice = blocking(|| toolsite::accounts::users::sign_up(&site.config, "alice@example.com", "correct horse battery")).unwrap();
    let bob = blocking(|| toolsite::accounts::users::sign_up(&site.config, "bob@example.com", "correct horse battery")).unwrap();
    db::run(&site.config, "app", &format!("insert into orders (id, owner_id, total) values (77, '{}', 4411.22)", bob.id), &[]).unwrap();
    let as_alice = Some(toolsite::runtime::wasm::User { id: alice.id.clone(), email: alice.email.clone() });
    for statements in [
        vec!["insert into my_orders (id, total) values (77, 1)"],
        vec!["insert into my_orders (id, total) values (5, 1)", "update my_orders set id = 77 where id = 5"],
        vec!["select total from orders"],
        vec!["select * from my_orders where total / 0 = 1 or (select total from orders where id = 77) > 0"],
    ] {
        let (status, body) = call_as(&site, "app", as_alice.clone(), request("POST", "/api/batch-scoped", "", &statements.join("\n"))).await;
        assert_ne!(status, 200, "{statements:?}: {body}");
        assert!(!body.contains("4411") && !body.contains(&bob.id) && !body.contains("bob@"), "{statements:?} told: {body}");
    }
    assert_eq!(scalar(&site.config, "app", "select total from orders where id = 77"), serde_json::json!(4411.22));
}
