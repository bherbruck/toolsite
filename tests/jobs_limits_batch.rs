//! An app doing heavy work: starting its own jobs (`jobs.run`), asking for
//! more time, fuel, rows and memory (`[limits]`), and writing many rows in
//! one transaction (`db.batch`). Driven through the committed guest fixture,
//! so each property is proven on the path a real handler takes.

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

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

struct Site {
    _dir: TempDir,
    config: Arc<Config>,
    runtime: Arc<Runtime>,
}

impl Site {
    fn state(&self) -> AppState {
        AppState { config: self.config.clone(), runtime: self.runtime.clone() }
    }
}

fn site_with(config: impl FnOnce(Config) -> Config) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(config(Config::local(dir.path().to_path_buf(), "test-token")));
    let runtime = Runtime::new().unwrap();
    // What the router does: jobs an app starts run on this runtime.
    config.jobs.attach(&runtime);
    Site { _dir: dir, config, runtime }
}

fn site() -> Site {
    site_with(|config| config)
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
    let site = site();
    install(&site, "app");
    schedule::set_job(&site.config, "app", "mark", NEVER, "/api/job-mark").unwrap();

    // Asked by a signed-in visitor; the job must not inherit them.
    let alice = toolsite::runtime::wasm::User { id: "u1".into(), email: "alice@example.com".into() };
    let started = Instant::now();
    let (status, body) = call_as(&site, "app", Some(alice), request("GET", "/api/jobs-run", "name=mark", "")).await;
    assert_eq!((status, body.as_str()), (200, "started"));
    assert!(until(Duration::from_secs(20), || marks(&site.config, "app", "mark") == 1).await, "the job never ran");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(scalar(&site.config, "app", "select who from marks"), serde_json::json!("anonymous:none"));

    // Recorded as a run, with when it started and finished and how long.
    assert!(until(Duration::from_secs(5), || !site.config.jobs.is_running("app", "mark")).await);
    let job = &schedule::read_jobs(&site.config, "app")["mark"];
    assert_eq!(job.last_status.as_deref(), Some("200"));
    assert!(job.last_started_at.is_some() && job.last_finished_at.is_some() && job.last_duration_ms.is_some(), "{job:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_asked_for_while_one_runs_is_queued_once_and_never_runs_alongside() {
    let site = site();
    install(&site, "app");
    schedule::set_job(&site.config, "app", "nap", NEVER, "/api/job-nap").unwrap();

    assert_eq!(run_now(&site, "app", "nap").await, (200, "started".into()));
    assert!(site.config.jobs.is_running("app", "nap"));
    // Asked twice more while it naps: one more run, not two.
    assert_eq!(run_now(&site, "app", "nap").await, (200, "queued".into()));
    assert_eq!(run_now(&site, "app", "nap").await, (200, "queued".into()));
    // A person asking gets the same answer rather than a second run.
    assert_eq!(schedule::run_job(&site.state(), "app", "nap").await.unwrap(), schedule::Ran::Queued);

    assert!(until(Duration::from_secs(30), || !site.config.jobs.is_running("app", "nap")).await, "still running");
    assert_eq!(marks(&site.config, "app", "nap"), 2, "the queued asks were not one run");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scheduler_skips_a_job_that_is_still_running_and_records_the_skip() {
    let site = site();
    install(&site, "app");
    schedule::set_job(&site.config, "app", "nap", "* * * * * *", "/api/job-nap").unwrap();
    assert_eq!(run_now(&site, "app", "nap").await, (200, "started".into()));

    let scheduler = schedule::Scheduler::new(site.state());
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 1;
    assert!(scheduler.tick(at).await.is_empty(), "a second run started beside the first");
    assert_eq!(schedule::read_jobs(&site.config, "app")["nap"].last_skipped_at, Some(at));

    assert!(until(Duration::from_secs(30), || !site.config.jobs.is_running("app", "nap")).await);
    assert_eq!(marks(&site.config, "app", "nap"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_apps_starts_past_the_sites_rate_are_refused() {
    let site = site_with(|config| Config { jobs: Arc::new(schedule::Jobs::new(2)), ..config });
    install(&site, "app");
    for name in ["one", "two", "three"] {
        schedule::set_job(&site.config, "app", name, NEVER, "/api/job-mark").unwrap();
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
    let site = site();
    install(&site, "one");
    install(&site, "two");
    schedule::set_job(&site.config, "one", "mark", NEVER, "/api/job-mark").unwrap();

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
    assert!(!site.config.jobs.is_running("one", "mark"));
}

/// The production case: a job that works in stages, each asking for the
/// next. With a fixed 30-second tick that was thirty seconds a stage; queued
/// behind itself, the next stage starts the moment the last one ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_chains_itself_twenty_stages_back_to_back() {
    let site = site();
    install(&site, "app");
    schedule::set_job(&site.config, "app", "chain", NEVER, "/api/chain").unwrap();

    let started = Instant::now();
    assert_eq!(run_now(&site, "app", "chain").await, (200, "started".into()));
    assert!(
        until(Duration::from_secs(60), || marks(&site.config, "app", "stage") >= 20).await,
        "only {} stages",
        marks(&site.config, "app", "stage")
    );
    let took = started.elapsed();
    assert!(took < Duration::from_secs(30), "twenty stages took {took:?}");
    assert!(until(Duration::from_secs(10), || !site.config.jobs.is_running("app", "chain")).await);
    assert_eq!(marks(&site.config, "app", "stage"), 20, "the chain ran past its own end");
}

/// The scheduler sleeps until the next due time rather than a fixed tick, so
/// a schedule of every second runs every second.
#[tokio::test(flavor = "multi_thread")]
async fn the_scheduler_wakes_for_each_second_of_a_one_second_schedule() {
    let site = site();
    install(&site, "app");
    schedule::set_job(&site.config, "app", "mark", "* * * * * *", "/api/job-mark").unwrap();
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

#[tokio::test]
async fn without_limits_an_app_runs_on_the_defaults_it_always_had() {
    let site = site();
    install(&site, "app");
    let effective = limits::of(&site.config, "app").await;
    assert_eq!(effective.request, toolsite::runtime::wasm::Guards::default());
    assert_eq!(effective.job.wall_clock, Duration::from_secs(60));
    let many = "q=with%20recursive%20n(i)%20as%20(select%201%20union%20all%20select%20i%2B1%20from%20n%20where%20i%20%3C%2020000)%20select%20i%20from%20n";
    assert_eq!(call(&site, "app", request("GET", "/api/sql-count", many, "")).await, (200, "1000:true".into()));
}

#[tokio::test]
async fn asked_limits_apply_and_values_past_a_ceiling_are_clamped_and_said() {
    let site = site();
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
    });
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

#[tokio::test]
async fn query_rows_applies_to_query_and_query_scoped_alike() {
    let site = site();
    install(&site, "app");
    orders(&site, "app", "[limits]\nquery_rows = 3000").await;
    let user = toolsite::accounts::users::sign_up(&site.config, "alice@example.com", "correct horse battery").unwrap();
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

#[tokio::test]
async fn a_batch_answers_rows_changed_per_statement_and_a_failure_undoes_all_of_it() {
    let site = site();
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

#[tokio::test]
async fn parameters_in_a_batch_are_bound_so_text_cannot_become_sql() {
    let site = site();
    install(&site, "app");
    db::run(&site.config, "app", "create table t (v text)", &[]).unwrap();
    let payload = "x'); drop table t; --";
    let (status, body) = batch(&site, "app", &[&format!("insert into t values (?)\t{payload}")]).await;
    assert_eq!((status, body.as_str()), (200, "ok:1"));
    assert_eq!(scalar(&site.config, "app", "select v from t"), serde_json::json!(payload));
}

#[tokio::test]
async fn a_batch_cannot_attach_reach_another_app_or_end_the_hosts_transaction() {
    let site = site();
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

#[tokio::test]
async fn a_long_batch_stops_at_the_calls_time_limit() {
    let site = site();
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

#[tokio::test]
async fn a_batch_past_its_size_caps_is_refused_before_anything_runs() {
    let site = site();
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

#[tokio::test]
async fn batch_scoped_holds_every_statement_to_the_row_level_policy() {
    let site = site();
    install(&site, "app");
    orders(&site, "app", "").await;
    let alice = toolsite::accounts::users::sign_up(&site.config, "alice@example.com", "correct horse battery").unwrap();
    let bob = toolsite::accounts::users::sign_up(&site.config, "bob@example.com", "correct horse battery").unwrap();
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
