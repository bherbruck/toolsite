//! Jobs on a site with more than one runner: two toolsite processes on one
//! Postgres database, each with its own scheduler, its own memory and its
//! own wasm runtime, sharing the data directory as they would a volume.
//!
//! What has to hold is what one runner gave for free: a scheduled turn fires
//! once, a job runs once at a time, an app runs only its share of jobs and
//! starts them only at its rate, a rerun asked for anywhere runs, and a
//! runner that dies mid-run does not keep its job for ever. Each test is
//! ignored unless asked for, since it needs a database:
//! `scripts/test-postgres.sh` starts one.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use toolsite::{
    platform::schedule,
    runtime::{db, wasm::Runtime},
    state::leases::{Ended, Taken},
    AppState, Config,
};

mod common;

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");
const NEVER: &str = "0 0 0 1 1 *";

/// One toolsite process: its own config, stores and runtime.
struct Runner {
    config: Arc<Config>,
    runtime: Arc<Runtime>,
}

impl Runner {
    fn state(&self) -> AppState {
        AppState { config: self.config.clone(), runtime: self.runtime.clone() }
    }
}

/// A site with two runners on one database.
struct Site {
    _dir: tempfile::TempDir,
    a: Runner,
    b: Runner,
    database: Option<common::Database>,
}

impl Site {
    async fn new(jobs: impl Fn() -> schedule::Jobs) -> Site {
        let dir = tempfile::tempdir().unwrap();
        let database = common::Database::new().await;
        let runner = || {
            let local = Config::local(dir.path().to_path_buf(), "test-token");
            let config = Arc::new(Config { stores: database.stores(), jobs: Arc::new(jobs()), ..local });
            let runtime = Runtime::new().unwrap();
            config.jobs.attach(&runtime);
            Runner { config, runtime }
        };
        let (a, b) = (runner(), runner());
        Site { _dir: dir, a, b, database: Some(database) }
    }

    fn install(&self, app: &str) {
        let data_dir = &self.a.config.data_dir;
        std::fs::create_dir_all(data_dir.join(app)).unwrap();
        std::fs::write(data_dir.join(app).join("handler.wasm"), HANDLER).unwrap();
    }

    /// Marks the job routes wrote, read through runner A.
    fn marks(&self, app: &str, what: &str) -> i64 {
        db::run(&self.a.config, app, &format!("select count(*) from marks where what = '{what}'"), &[])
            .ok()
            .and_then(|out| out.rows[0][0].as_i64())
            .unwrap_or(0)
    }

    async fn rows(&self, sql: &str) -> i64 {
        let client = self.database.as_ref().unwrap().postgres.pool.get().await.unwrap();
        client.query_one(sql, &[]).await.unwrap().get(0)
    }
}

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

fn request(path: &str, query: &str) -> toolsite::runtime::wasm::Request {
    toolsite::runtime::wasm::Request {
        method: "GET".to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

/// `jobs.run` from a handler on `runner`, as an app's request would.
async fn run_now(runner: &Runner, app: &str, name: &str) -> (u16, String) {
    let guards = toolsite::runtime::limits::of(&runner.config, app).await.request;
    let (runtime, config, app, query) = (runner.runtime.clone(), runner.config.clone(), app.to_string(), format!("name={name}"));
    let response = tokio::task::spawn_blocking(move || runtime.handle(config, &app, HANDLER, None, request("/api/jobs-run", &query), guards))
        .await
        .unwrap()
        .unwrap();
    (response.status, String::from_utf8_lossy(&response.body).to_string())
}

fn is_running(runner: &Runner, app: &str, name: &str) -> bool {
    schedule::is_running(&runner.config, app, name)
}

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

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Two schedulers on one database, ticking the same second side by side for
/// a hundred turns of an every-second job: each turn runs once, never twice
/// and never not at all, and neither calls the other's turn a skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn two_schedulers_on_one_database_fire_each_turn_exactly_once_on_postgres() {
    let site = Site::new(schedule::Jobs::default).await;
    site.install("app");
    schedule::set_job(&site.a.config, "app", "mark", "* * * * * *", "/api/job-mark").await.unwrap();
    let (a, b) = (schedule::Scheduler::new(site.a.state()), schedule::Scheduler::new(site.b.state()));

    // Turns ahead of the clock, so a run's own finish time never counts as
    // a later turn taken.
    let base = now() + 10_000;
    let mut fired_by = (0, 0);
    for turn in base..base + 100 {
        let (on_a, on_b) = tokio::join!(a.tick(turn), b.tick(turn));
        fired_by.0 += on_a.len();
        fired_by.1 += on_b.len();
        for run in on_a.into_iter().chain(on_b) {
            run.await.unwrap();
        }
    }
    assert_eq!(fired_by.0 + fired_by.1, 100, "{fired_by:?}");
    assert_eq!(site.marks("app", "mark"), 100);
    // The claim is one row, the latest turn, however many turns fired.
    assert_eq!(site.rows("select due_at from platform.job_turns where app = 'app' and name = 'mark'").await, (base + 99) as i64);
    let job = &schedule::jobs(&site.b.config, "app").await["mark"];
    assert_eq!(job.last_skipped_at, None, "a turn the other scheduler took was recorded as skipped");
    assert_eq!(job.last_started_at, Some(base + 99));
}

/// A job running on runner A is running for runner B: B's "Run now" and
/// B's `jobs.run` queue one more run behind A's rather than a second beside
/// it, and A runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_job_running_on_one_runner_is_running_on_the_other_and_run_now_there_queues_on_postgres() {
    let site = Site::new(schedule::Jobs::default).await;
    site.install("app");
    schedule::set_job(&site.a.config, "app", "nap", NEVER, "/api/job-nap").await.unwrap();

    assert_eq!(run_now(&site.a, "app", "nap").await, (200, "started".into()));
    assert!(is_running(&site.b, "app", "nap"), "B does not see A's run");
    assert_eq!(schedule::running(&site.b.config, "app").await.into_iter().collect::<Vec<_>>(), ["nap"]);
    assert_eq!(schedule::run_job(&site.b.state(), "app", "nap").await.unwrap(), schedule::Ran::Queued);
    assert_eq!(run_now(&site.b, "app", "nap").await, (200, "queued".into()));

    assert!(until(Duration::from_secs(30), || !is_running(&site.a, "app", "nap")).await, "still running");
    assert_eq!(site.marks("app", "nap"), 2, "B's asks were not exactly one more run");
    assert_eq!(schedule::jobs(&site.a.config, "app").await["nap"].last_status.as_deref(), Some("200"));
}

/// A runner that dies mid-run stops renewing its slot. Until the slot runs
/// out the job is still running for everyone; after, it is free, the rerun
/// another runner queued on it is run there, and the dead runner's late
/// attempt to settle the slot is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_runner_that_dies_mid_job_lets_its_slot_go_and_its_queued_run_runs_elsewhere_on_postgres() {
    let site = Site::new(|| schedule::Jobs::default().with_slot_ttl(Duration::from_secs(2))).await;
    site.install("app");
    schedule::set_job(&site.a.config, "app", "mark", NEVER, "/api/job-mark").await.unwrap();

    // A takes the slot as a run does, and then is gone: it never renews,
    // never settles.
    let taken = site.a.config.stores.leases.acquire(&schedule::slot("app", "mark"), Some(("app", 4)), Duration::from_secs(2)).await.unwrap();
    let Taken::Lease(dead) = taken else { panic!("{taken:?}") };

    // B sees it running, and asks for one more run behind it.
    assert!(is_running(&site.b, "app", "mark"));
    assert_eq!(schedule::run_job(&site.b.state(), "app", "mark").await.unwrap(), schedule::Ran::Queued);
    let b = schedule::Scheduler::new(site.b.state());
    assert!(b.tick(now()).await.is_empty(), "a run was taken over while its slot was live");

    assert!(until(Duration::from_secs(10), || !is_running(&site.b, "app", "mark")).await, "the dead runner's slot never ran out");
    let runs = b.tick(now()).await;
    assert_eq!(runs.len(), 1, "the queued run was not taken over");
    for run in runs {
        run.await.unwrap();
    }
    assert_eq!(site.marks("app", "mark"), 1);
    // Taken over once: a second look finds nothing owed.
    assert!(b.tick(now()).await.is_empty());
    assert_eq!(site.marks("app", "mark"), 1);

    // The dead runner, back, cannot settle a slot that is not its own.
    assert_eq!(site.a.config.stores.leases.again_or_release(&dead, Duration::from_secs(2)).await.unwrap(), Ended::Lost);
    assert!(!site.a.config.stores.leases.release(&dead).await.unwrap());
    // And the job starts afresh from either runner.
    assert_eq!(run_now(&site.a, "app", "mark").await, (200, "started".into()));
    assert!(until(Duration::from_secs(20), || site.marks("app", "mark") == 2).await);
}

/// An app's ceiling on jobs at once is the site's, not each runner's: two
/// runs on two runners fill it, and a third is refused on either, by
/// `jobs.run`, by a person, and by the schedule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_per_app_running_cap_holds_across_two_runners_on_postgres() {
    let site = Site::new(|| schedule::Jobs::new(600).with_running_per_app(2)).await;
    site.install("greedy");
    site.install("other");
    for n in 0..5 {
        schedule::set_job(&site.a.config, "greedy", &format!("nap{n}"), NEVER, "/api/job-nap").await.unwrap();
    }
    schedule::set_job(&site.a.config, "other", "nap", NEVER, "/api/job-nap").await.unwrap();

    assert_eq!(run_now(&site.a, "greedy", "nap0").await, (200, "started".into()));
    assert_eq!(run_now(&site.b, "greedy", "nap1").await, (200, "started".into()));
    for runner in [&site.a, &site.b] {
        let (status, body) = run_now(runner, "greedy", "nap2").await;
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("at once"), "{body}");
    }
    let refused = schedule::run_job(&site.b.state(), "greedy", "nap3").await.unwrap_err();
    assert!(refused.contains("at once"), "{refused}");
    schedule::set_job(&site.b.config, "greedy", "nap4", "* * * * * *", "/api/job-nap").await.unwrap();
    let at = now() + 1;
    assert!(schedule::Scheduler::new(site.b.state()).tick(at).await.is_empty(), "the schedule ran a third");
    assert_eq!(schedule::jobs(&site.a.config, "greedy").await["nap4"].last_skipped_at, Some(at));
    // Another app is not held up.
    assert_eq!(run_now(&site.b, "other", "nap").await, (200, "started".into()));

    let mut most = 0;
    let done = until(Duration::from_secs(30), || {
        most = most.max((0..5).filter(|n| is_running(&site.a, "greedy", &format!("nap{n}"))).count());
        site.marks("greedy", "nap") >= 2 && !is_running(&site.a, "greedy", "nap0") && !is_running(&site.a, "greedy", "nap1")
    })
    .await;
    assert!(done, "{} marks", site.marks("greedy", "nap"));
    assert!(most <= 2, "{most} of the app's jobs ran at once");
    assert_eq!(site.marks("greedy", "nap"), 2);
}

/// An app's start rate is the site's: starts on two runners count against
/// one minute's allowance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_start_rate_holds_across_runners_on_postgres() {
    let site = Site::new(|| schedule::Jobs::new(3)).await;
    site.install("app");
    for n in 0..4 {
        schedule::set_job(&site.a.config, "app", &format!("mark{n}"), NEVER, "/api/job-mark").await.unwrap();
    }
    assert_eq!(run_now(&site.a, "app", "mark0").await.0, 200);
    assert_eq!(run_now(&site.b, "app", "mark1").await.0, 200);
    assert_eq!(run_now(&site.a, "app", "mark2").await.0, 200);
    for runner in [&site.b, &site.a] {
        let (status, body) = run_now(runner, "app", "mark3").await;
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("limit"), "{body}");
    }
    assert!(until(Duration::from_secs(20), || site.marks("app", "mark") == 3).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(site.marks("app", "mark"), 3, "a refused start ran");
}

/// A job that chains itself (`jobs.run` on its own name from inside the
/// run), asked for from both runners: B's ask while A runs it joins the
/// chain rather than starting beside it, and the twenty stages run back to
/// back, one at a time, each once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_chain_asked_for_on_two_runners_runs_each_stage_once_back_to_back_on_postgres() {
    let site = Site::new(schedule::Jobs::default).await;
    site.install("app");
    schedule::set_job(&site.a.config, "app", "chain", NEVER, "/api/chain").await.unwrap();

    let started = Instant::now();
    assert_eq!(run_now(&site.a, "app", "chain").await, (200, "started".into()));
    assert_eq!(run_now(&site.b, "app", "chain").await, (200, "queued".into()), "B started a chain beside A's");
    let done = until(Duration::from_secs(60), || site.marks("app", "stage") >= 20).await;
    assert!(done, "only {} stages", site.marks("app", "stage"));
    let took = started.elapsed();
    assert!(took < Duration::from_secs(30), "twenty stages took {took:?}");
    assert!(until(Duration::from_secs(10), || !is_running(&site.b, "app", "chain")).await);
    assert_eq!(site.marks("app", "stage"), 20, "a stage ran twice, or the chain ran past its end");
}
