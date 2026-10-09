//! Work an app does without anyone asking: refreshing a cache, pulling from
//! an API, tidying a table.
//!
//! A job is a cron expression and a path. When it fires, the host calls the
//! app's own handler exactly as a request would — same sandbox, same
//! database, no identity, with the job's limits. So a job is just a route
//! the app already has, and nothing new has to be reasoned about to know
//! what it can do.
//!
//! One run per job at a time, however it was started: by its schedule, by a
//! person, or by the app through `jobs.run`, and on whichever runner. A
//! schedule that comes round while its job still runs is skipped, and the
//! skip recorded. Asked for while it runs, a job runs once more as soon as
//! it finishes, which is how an app chains stages back to back.
//!
//! The rules live here; the state is the site's. Job records are
//! `AppRecords` (`<app>.jobs` on files, `platform.jobs` on Postgres), a
//! run's slot and a rerun queued behind it are a lease, an app's starts are
//! a rate window, and a scheduled turn is claimed in `platform.job_turns`
//! before it runs. On files all of that is this process's, as it always
//! was; on Postgres every runner shares it.

use crate::{
    config::Config,
    platform::records,
    runtime::wasm::Runtime,
    state::leases::{Ended, Lease, Taken},
    AppState,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

/// The longest the scheduler sleeps without looking again, whatever the
/// schedules say. It wakes sooner for a job that is due sooner, and at once
/// when jobs change.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// The shortest it sleeps, so a job file it cannot write never turns the
/// loop into a spin.
const MIN_SLEEP: Duration = Duration::from_millis(100);

/// For a job that has never run: how far back a scheduled time may lie and
/// still fire. A window rather than all of history, so creating a yearly
/// job does not set it off at once.
const FALLBACK_WINDOW: u64 = 30;

/// Starts of jobs one app may make through `jobs.run` in a minute, queued
/// runs included. High enough for a worker that chains its stages back to
/// back; low enough that a loop cannot keep the server busy for nothing.
pub const DEFAULT_STARTS_PER_MINUTE: usize = 600;

/// Jobs of one app that may run at once, however they were started. Each
/// holds a blocking thread for up to the job's wall clock, so without this
/// an app declaring a hundred jobs and starting them together would take
/// the threads every other app's requests and jobs run on.
pub const DEFAULT_RUNNING_PER_APP: usize = 4;

/// Jobs one app may declare. The scheduler reads and plans every one on
/// each wake, and records each skipped turn in the app's job record, so a
/// thousand every-second jobs would have it rewriting records a thousand
/// times a second.
pub const MAX_JOBS_PER_APP: usize = 100;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Job {
    /// Standard cron with seconds leading, as the `cron` crate reads it.
    pub schedule: String,
    /// The path handed to the handler, e.g. `/api/refresh`.
    pub path: String,
    /// When the last run finished, in Unix seconds. Absent until it has run
    /// once.
    #[serde(default)]
    pub last_run: Option<u64>,
    /// What happened last time, for whoever is wondering why nothing changed.
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    /// The last scheduled time that came while the job was still running,
    /// and so was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_skipped_at: Option<u64>,
}

/// How long a run's slot lasts without renewal. A run renews it every third
/// of this while it goes, so a runner that dies mid-run lets the job go this
/// long after, and a run it owed is taken over by another runner.
pub const SLOT_TTL: Duration = Duration::from_secs(30);

/// What every job's slot is named under, in the site's leases.
const SLOT_PREFIX: &str = "job:";

/// The tries a start makes when the slot it saw changes hands underneath it:
/// held when asked for, gone when asked to go again.
const TRIES: usize = 8;

/// How this process takes part in the site's jobs: the limits, the runtime
/// a job started from inside a handler runs on, and the wake-up for the
/// scheduler. Which jobs are running, and what is queued behind them, are
/// the site's: leases in `config.stores`, held in this process's memory on
/// files and in Postgres otherwise, so the scheduler, a person and an app's
/// handler on any runner all see the same runs.
pub struct Jobs {
    /// What a job started from inside a handler runs on: the runtime and
    /// the async runtime of the server, set once they exist.
    attached: Mutex<Option<(Weak<Runtime>, tokio::runtime::Handle)>>,
    /// Woken when a job is set, removed or finishes, so the scheduler works
    /// out its next wake again.
    changed: tokio::sync::Notify,
    /// `jobs.run` starts per app per minute, from
    /// `TOOLSITE_JOB_STARTS_PER_MINUTE`.
    pub starts_per_minute: usize,
    /// Jobs of one app running at once, from
    /// `TOOLSITE_JOBS_RUNNING_PER_APP`, counted across every runner.
    pub running_per_app: usize,
    /// How long a slot lasts unrenewed: `SLOT_TTL`, shorter in tests that
    /// watch a runner die.
    pub slot_ttl: Duration,
}

impl Default for Jobs {
    fn default() -> Self {
        Self::new(DEFAULT_STARTS_PER_MINUTE)
    }
}

/// Why a job's slot could not be taken.
#[derive(Debug, Clone, PartialEq)]
enum Busy {
    /// The job itself is running.
    Running,
    /// The app already runs as many jobs as it may at once.
    App(usize),
    /// The slots could not be read: nothing was started.
    Failed(String),
}

fn too_many(app: &str, running: usize) -> String {
    format!("{app} already runs {running} jobs at once, which is this site's limit; try again when one finishes")
}

/// Whether a run began, or was queued behind one in progress.
#[derive(Debug, Clone, PartialEq)]
pub enum Ran {
    /// It ran, and this is the handler's status.
    Finished(String),
    /// It was running already, and runs once more when that run finishes.
    Queued,
}

/// The lease a run of `app`'s job `name` holds. Grouped by app, so an app
/// whose name starts another's is never counted with it.
pub fn slot(app: &str, name: &str) -> String {
    format!("{SLOT_PREFIX}{app}/{name}")
}

impl Jobs {
    pub fn new(starts_per_minute: usize) -> Self {
        Self {
            attached: Mutex::default(),
            changed: tokio::sync::Notify::new(),
            starts_per_minute,
            running_per_app: DEFAULT_RUNNING_PER_APP,
            slot_ttl: SLOT_TTL,
        }
    }

    /// The same, letting `running` jobs of one app run at once.
    pub fn with_running_per_app(self, running: usize) -> Self {
        Self { running_per_app: running.max(1), ..self }
    }

    /// The same, with slots that last `ttl` unrenewed.
    pub fn with_slot_ttl(self, ttl: Duration) -> Self {
        Self { slot_ttl: ttl.max(Duration::from_millis(30)), ..self }
    }

    /// Gives jobs started from inside a handler somewhere to run. Called
    /// wherever a runtime meets an async context: the router, the
    /// scheduler, a run. Outside one it does nothing.
    pub fn attach(&self, runtime: &Arc<Runtime>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            *self.attached.lock().unwrap() = Some((Arc::downgrade(runtime), handle));
        }
    }

    /// Tells the scheduler that what it planned around has changed.
    pub fn changed(&self) {
        self.changed.notify_one();
    }
}

/// Whether `app`'s job `name` is running now, on any runner. For a
/// synchronous caller; `running` answers for a whole app.
pub fn is_running(config: &Config, app: &str, name: &str) -> bool {
    config.stores.leases.state_blocking(&slot(app, name)).ok().flatten().is_some()
}

/// The names of `app`'s jobs running now, on any runner.
pub async fn running(config: &Config, app: &str) -> BTreeSet<String> {
    let prefix = slot(app, "");
    match config.stores.leases.live(app).await {
        Ok(names) => names.iter().filter_map(|lease| lease.strip_prefix(&prefix)).map(str::to_string).collect(),
        Err(why) => {
            tracing::warn!(app, %why, "running jobs could not be read");
            BTreeSet::new()
        }
    }
}

/// Takes the job's one slot, or says why not: it is running, or its app
/// runs as many jobs as it may, on whichever runners.
async fn claim(config: &Config, app: &str, name: &str) -> Result<Lease, Busy> {
    let group = Some((app, config.jobs.running_per_app));
    match config.stores.leases.acquire(&slot(app, name), group, config.jobs.slot_ttl).await {
        Ok(Taken::Lease(lease)) => Ok(lease),
        Ok(Taken::Held) => Err(Busy::Running),
        Ok(Taken::Full(running)) => Err(Busy::App(running)),
        Err(why) => Err(Busy::Failed(why)),
    }
}

/// After a run: true, with the slot still held, when another run was
/// queued behind it; otherwise the slot is given up. A slot that was lost
/// (it ran out and another runner took it over) is the other runner's to
/// settle, so this one stops.
async fn again_or_release(config: &Config, app: &str, name: &str, lease: &Lease) -> bool {
    match config.stores.leases.again_or_release(lease, config.jobs.slot_ttl).await {
        Ok(Ended::Again) => true,
        Ok(Ended::Released) => false,
        Ok(Ended::Lost) => {
            tracing::warn!(app, job = name, "the job's slot ran out during the run and was taken over");
            false
        }
        Err(why) => {
            // Left to run out: no other runner starts it until then.
            tracing::warn!(app, job = name, %why, "the job's slot could not be settled");
            false
        }
    }
}

/// Counts one start against the app's rate, for the whole site, or
/// refuses it.
async fn count_start(config: &Config, app: &str) -> Result<(), String> {
    let key = format!("job-starts:{app}");
    match config.stores.rates.spend(&key, config.jobs.starts_per_minute, Duration::from_secs(60)).await? {
        Ok(()) => Ok(()),
        Err(count) => Err(format!(
            "{app} has started {count} jobs in the last minute, which is this site's limit; try again shortly"
        )),
    }
}

/// Keeps a run's slot while the run goes: renewed every third of its life,
/// until this is dropped.
struct Keeping(tokio::task::JoinHandle<()>);

impl Drop for Keeping {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn keep(config: &Arc<Config>, app: &str, name: &str, lease: &Lease) -> Keeping {
    let (config, app, name, lease) = (config.clone(), app.to_string(), name.to_string(), lease.clone());
    Keeping(tokio::spawn(async move {
        let ttl = config.jobs.slot_ttl;
        loop {
            tokio::time::sleep(ttl / 3).await;
            match config.stores.leases.renew(&lease, ttl).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(app, job = name, "the job's slot was taken over while it ran");
                    return;
                }
                Err(why) => tracing::warn!(app, job = name, %why, "the job's slot could not be renewed"),
            }
        }
    }))
}

/// An app is one segment, the top-level directory a handler runs from. A
/// job named for a path inside one would be counted, capped and rate
/// limited under a name of its own, out of its app's limits.
fn valid_app(app: &str) -> bool {
    crate::platform::tokens::valid_app(app)
}

/// Jobs from their stored text. One that does not parse is passed over and
/// said, rather than taking the app's other jobs with it.
fn parse_jobs(app: &str, texts: BTreeMap<String, String>) -> BTreeMap<String, Job> {
    texts
        .into_iter()
        .filter_map(|(name, text)| match serde_json::from_str(&text) {
            Ok(job) => Some((name, job)),
            Err(why) => {
                tracing::warn!(app, job = name, %why, "a job could not be read");
                None
            }
        })
        .collect()
}

fn unreadable(app: &str, why: String) -> BTreeMap<String, String> {
    tracing::warn!(app, %why, "jobs could not be read");
    BTreeMap::new()
}

/// An app's jobs, by name.
pub async fn jobs(config: &Config, app: &str) -> BTreeMap<String, Job> {
    if !valid_app(app) {
        return BTreeMap::new();
    }
    let texts = records::of(config).jobs(app).await.unwrap_or_else(|why| unreadable(app, why));
    parse_jobs(app, texts)
}

/// `jobs`, for a synchronous caller.
pub fn read_jobs(config: &Config, app: &str) -> BTreeMap<String, Job> {
    if !valid_app(app) {
        return BTreeMap::new();
    }
    let texts = records::of(config).jobs_blocking(app).unwrap_or_else(|why| unreadable(app, why));
    parse_jobs(app, texts)
}

fn job_text(job: &Job) -> Result<String, String> {
    serde_json::to_string(job).map_err(|e| e.to_string())
}

/// The edit `update` hands the store: the stored job, changed.
fn edit_job(change: impl FnOnce(&mut Job) + Send + 'static) -> crate::platform::records::DocEdit<'static> {
    Box::new(move |current| {
        let mut job: Job = serde_json::from_str(current.unwrap_or_default()).map_err(|e| format!("a job could not be read: {e}"))?;
        change(&mut job);
        job_text(&job)
    })
}

/// Changes one job's record with it held, on any runner. Nothing happens if
/// the job is gone.
async fn update(config: &Config, app: &str, name: &str, change: impl FnOnce(&mut Job) + Send + 'static) {
    if let Err(why) = records::of(config).update_job(app, name, edit_job(change)).await {
        tracing::warn!(app, job = name, %why, "the job's record could not be written");
    }
}

/// Marks a run started: before the start is answered or the turn passed
/// on, so the next plan, here or on another runner, counts from it.
async fn begin(config: &Config, app: &str, name: &str, at: u64) {
    update(config, app, name, move |job| job.last_started_at = Some(at)).await;
}

/// Adds or replaces a job. The schedule is parsed here so a bad expression is
/// refused while someone is watching, rather than silently never firing.
pub async fn set_job(
    config: &Config,
    app: &str,
    name: &str,
    schedule: &str,
    path: &str,
) -> Result<String, String> {
    if !valid_app(app) {
        return Err(format!("invalid app name '{app}'"));
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("a job's name must be letters, numbers, '-' or '_'".into());
    }
    if !path.starts_with('/') {
        return Err("path must start with '/', e.g. /api/refresh".into());
    }
    let parsed = cron::Schedule::from_str(schedule).map_err(|e| {
        format!("{e}. Six fields, seconds first — '0 */5 * * * *' is every five minutes")
    })?;
    let next = parsed
        .upcoming(Utc)
        .next()
        .ok_or("that schedule never fires")?;

    let job = Job { schedule: schedule.to_string(), path: path.to_string(), ..Job::default() };
    if let Err(count) = records::of(config).set_job(app, name, &job_text(&job)?, MAX_JOBS_PER_APP).await? {
        return Err(format!("{app} already has {count} jobs, the most one app may declare"));
    }
    config.jobs.changed();
    Ok(format!("next run {}", next.to_rfc3339()))
}

pub async fn remove_job(config: &Config, app: &str, name: &str) -> Result<(), String> {
    if !valid_app(app) || !records::of(config).remove_job(app, name).await? {
        return Err(format!("{app} has no job called {name}"));
    }
    config.jobs.changed();
    Ok(())
}

fn now() -> u64 {
    now_ms() / 1000
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The moment a job's schedule is counted from: the latest of when it last
/// started, finished or was skipped, or for a job that never ran, a short
/// window back from `at`.
fn since(job: &Job, at: u64) -> u64 {
    [job.last_run, job.last_started_at, job.last_skipped_at]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_else(|| at.saturating_sub(FALLBACK_WINDOW))
}

/// The first scheduled time after the job last ran, in Unix seconds. In the
/// past or now means due.
fn next_due(job: &Job, at: u64) -> Option<u64> {
    let schedule = cron::Schedule::from_str(&job.schedule).ok()?;
    let after = chrono::DateTime::from_timestamp(since(job, at) as i64, 0)?;
    schedule.after(&after).next().map(|next| next.timestamp() as u64)
}

/// Whether a job is due at `at`: did a scheduled time pass since it last
/// ran, not "is it that second now". A job that missed its turns while the
/// server was down fires once, not once per turn.
fn is_due(job: &Job, at: u64) -> bool {
    next_due(job, at).is_some_and(|next| next <= at)
}

/// The turn a due job fires for at `at`: the latest scheduled time not after
/// `at`. Every scheduler that finds the job due at `at` names the same turn,
/// whatever it last read, so two of them on one database claim it once.
fn turn(job: &Job, at: u64) -> Option<u64> {
    if !is_due(job, at) {
        return None;
    }
    let first = next_due(job, at)?;
    let schedule = cron::Schedule::from_str(&job.schedule).ok()?;
    let after = chrono::DateTime::from_timestamp(at as i64 + 1, 0)?;
    let latest = schedule.after(&after).next_back().map(|turn| turn.timestamp() as u64);
    Some(latest.filter(|latest| *latest >= first && *latest <= at).unwrap_or(first))
}

/// Runs a job once, as the scheduler would, and records the outcome. The
/// caller holds its slot, has recorded the start, and settles the slot
/// after; the slot is kept alive here while the handler runs.
async fn run_once(state: &AppState, app: &str, name: &str, lease: &Lease) -> Result<String, String> {
    let job = jobs(&state.config, app).await.remove(name).ok_or_else(|| format!("{app} has no job called {name}"))?;

    let wasm = tokio::fs::read(state.config.data_dir.join(app).join("handler.wasm"))
        .await
        .map_err(|_| format!("{app} has no handler to run"))?;

    let request = crate::runtime::wasm::Request {
        method: "GET".to_string(),
        path: job.path.clone(),
        query: String::new(),
        // Says plainly that nobody is waiting on the other end, so a handler
        // can behave differently if it wants to.
        headers: vec![("x-toolsite-scheduled".to_string(), name.to_string())],
        body: Vec::new(),
    };

    let guards = crate::runtime::limits::of(&state.config, app).await.job;
    let _keeping = keep(&state.config, app, name, lease);
    let started_ms = now_ms();
    let runtime = state.runtime.clone();
    let config = state.config.clone();
    let owned_app = app.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        // No identity: a scheduled run is the app acting on its own behalf.
        runtime.handle(config, &owned_app, &wasm, None, request, guards)
    })
    .await;

    let status = match outcome {
        Ok(Ok(response)) => format!("{}", response.status),
        Ok(Err(error)) => format!("failed: {error}"),
        Err(error) => format!("failed: {error}"),
    };
    let finished_ms = now_ms();
    let recorded = status.clone();
    update(&state.config, app, name, move |job| {
        job.last_run = Some(finished_ms / 1000);
        job.last_finished_at = Some(finished_ms / 1000);
        job.last_duration_ms = Some(finished_ms.saturating_sub(started_ms));
        job.last_status = Some(recorded);
    })
    .await;
    Ok(status)
}

/// Runs whatever was queued behind a run that just ended, one after the
/// other, until nothing is; then gives the slot up.
async fn run_queued(state: &AppState, app: &str, name: &str, lease: &Lease) {
    while again_or_release(&state.config, app, name, lease).await {
        begin(&state.config, app, name, now()).await;
        match run_once(state, app, name, lease).await {
            Ok(status) => tracing::info!(app, job = name, status, "queued job ran"),
            Err(error) => tracing::warn!(app, job = name, error, "queued job failed"),
        }
    }
    state.config.jobs.changed();
}

/// Runs a job whose slot is held and whose start is recorded, then what is
/// queued behind it, in the background.
fn spawn_runs(
    on: &tokio::runtime::Handle,
    state: AppState,
    app: String,
    name: String,
    lease: Lease,
    how: &'static str,
) -> tokio::task::JoinHandle<()> {
    on.spawn(async move {
        match run_once(&state, &app, &name, &lease).await {
            Ok(status) => tracing::info!(app, job = name, status, how, "job ran"),
            Err(error) => tracing::warn!(app, job = name, error, how, "job failed"),
        }
        run_queued(&state, &app, &name, &lease).await;
    })
}

/// Runs one job now, whatever its schedule says, for a person, and answers
/// how it went. Already running, on this runner or another, it queues one
/// more run behind the current one instead. The same path a schedule takes,
/// so the two cannot behave differently.
pub async fn run_job(state: &AppState, app: &str, name: &str) -> Result<Ran, String> {
    state.config.jobs.attach(&state.runtime);
    if !jobs(&state.config, app).await.contains_key(name) {
        return Err(format!("{app} has no job called {name}"));
    }
    let mut held = None;
    for _ in 0..TRIES {
        match claim(&state.config, app, name).await {
            Ok(lease) => {
                held = Some(lease);
                break;
            }
            // Asked of the holder only while it holds: a run that ended in
            // between leaves this to take the slot itself.
            Err(Busy::Running) => match state.config.stores.leases.ask_again(&slot(app, name)).await? {
                Some(_) => return Ok(Ran::Queued),
                None => continue,
            },
            Err(Busy::App(running)) => return Err(too_many(app, running)),
            Err(Busy::Failed(why)) => return Err(why),
        }
    }
    let lease = held.ok_or_else(|| format!("{name} changed hands too often to start; try again"))?;
    begin(&state.config, app, name, now()).await;
    let outcome = run_once(state, app, name, &lease).await;
    // The slot is settled before answering, so a caller who asks again
    // once this returns gets a run of its own. Whatever was queued
    // meanwhile runs on without the caller.
    if again_or_release(&state.config, app, name, &lease).await {
        begin(&state.config, app, name, now()).await;
        spawn_runs(&tokio::runtime::Handle::current(), state.clone(), app.to_string(), name.to_string(), lease, "queued");
    } else {
        state.config.jobs.changed();
    }
    outcome.map(Ran::Finished)
}

/// `jobs.run` from inside an app's handler: starts one of the app's own
/// jobs in the background, or queues one more run of it if it is running
/// on any runner. `app` is the running app, never anything the guest said.
/// Called on the blocking thread a handler runs on.
pub fn start_from_app(config: &Arc<Config>, app: &str, name: &str) -> Result<String, String> {
    if !read_jobs(config, app).contains_key(name) {
        return Err(format!("{app} declares no job called {name:?}"));
    }
    let Some((runtime, handle)) = config.jobs.attached.lock().unwrap().clone() else {
        return Err("jobs cannot be started on this server yet".to_string());
    };
    let Some(runtime) = runtime.upgrade() else {
        return Err("jobs cannot be started on this server yet".to_string());
    };
    let state = AppState { config: config.clone(), runtime };
    handle.block_on(start(state, handle.clone(), app, name))
}

async fn start(state: AppState, on: tokio::runtime::Handle, app: &str, name: &str) -> Result<String, String> {
    let (config, leases, slot) = (&state.config, &state.config.stores.leases, slot(app, name));
    // A start is counted once, however many times a slot changing hands
    // sends this round again.
    let mut counted = false;
    for _ in 0..TRIES {
        match leases.state(&slot).await? {
            // Queued already: one more ask is the same run.
            Some(true) => return Ok("queued".to_string()),
            Some(false) => {
                if !counted {
                    count_start(config, app).await?;
                    counted = true;
                }
                // Made only of a live holder, who sees it as it finishes.
                if leases.ask_again(&slot).await?.is_some() {
                    return Ok("queued".to_string());
                }
                continue;
            }
            None => {}
        }
        let lease = match claim(config, app, name).await {
            Ok(lease) => lease,
            Err(Busy::Running) => continue,
            Err(Busy::App(running)) => return Err(too_many(app, running)),
            Err(Busy::Failed(why)) => return Err(why),
        };
        if !counted && let Err(why) = count_start(config, app).await {
            // Given back. A run someone queued in that moment counted its
            // own start, so it runs.
            if again_or_release(config, app, name, &lease).await {
                begin(config, app, name, now()).await;
                spawn_runs(&on, state.clone(), app.to_string(), name.to_string(), lease, "started by the app");
            }
            return Err(why);
        }
        // Recorded before "started" is answered: the run's own task may not
        // get going for a while, and a schedule planned meanwhile must count
        // from this start, not from whenever that is.
        begin(config, app, name, now()).await;
        spawn_runs(&on, state.clone(), app.to_string(), name.to_string(), lease, "started by the app");
        return Ok("started".to_string());
    }
    Err(format!("{name} changed hands too often to start; try again"))
}

/// Records an outcome against a job, as if it had just finished. For tests
/// that need a job to look as though it has run.
pub fn record_run(config: &Config, app: &str, name: &str, status: &str) {
    let (at, status) = (now(), status.to_string());
    let edit = edit_job(move |job| {
        job.last_run = Some(at);
        job.last_finished_at = Some(at);
        job.last_status = Some(status);
    });
    if let Err(why) = records::of(config).update_job_blocking(app, name, edit) {
        tracing::warn!(app, job = name, %why, "the job's record could not be written");
    }
}

/// Every job on the site, read in one scan.
async fn all_jobs(config: &Config) -> Vec<(String, String, Job)> {
    let all = records::of(config).all_jobs().await.unwrap_or_else(|why| {
        tracing::warn!(%why, "jobs could not be listed");
        Vec::new()
    });
    all.into_iter()
        .filter_map(|(app, name, text)| match serde_json::from_str(&text) {
            Ok(job) => Some((app, name, job)),
            Err(why) => {
                tracing::warn!(app, job = name, %why, "a job could not be read");
                None
            }
        })
        .collect()
}

/// What runs an app's jobs on their schedules. A process starts one, from
/// `main`. On files that is the only one for the data directory; on Postgres
/// every runner starts one, and each turn of each job is claimed in the
/// database before it runs, so it fires once however many are looking.
#[derive(Clone)]
pub struct Scheduler {
    state: AppState,
}

impl Scheduler {
    pub fn new(state: AppState) -> Self {
        state.config.jobs.attach(&state.runtime);
        Self { state }
    }

    /// Sleeps until the next job is due, by its cron expression to the
    /// second, starts what is due, and goes round again. A change to the
    /// jobs wakes it to plan again.
    pub fn spawn(self) {
        tokio::spawn(async move {
            loop {
                let wait = self.until_next(now_ms()).await;
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {
                        self.tick(now()).await;
                    }
                    _ = self.state.config.jobs.changed.notified() => {}
                }
            }
        });
    }

    /// How long from `at_ms` until the soonest job is due, within
    /// `MIN_SLEEP` and `MAX_SLEEP`.
    pub async fn until_next(&self, at_ms: u64) -> Duration {
        let at = at_ms / 1000;
        let soonest = all_jobs(&self.state.config).await.iter().filter_map(|(_, _, job)| next_due(job, at)).min();
        match soonest {
            Some(next) => Duration::from_millis((next * 1000).saturating_sub(at_ms)).clamp(MIN_SLEEP, MAX_SLEEP),
            None => MAX_SLEEP,
        }
    }

    /// Starts every job due at `at`, in Unix seconds, whose turn no other
    /// scheduler has claimed. One still running, here or on another runner,
    /// is skipped and the skip recorded, which also counts the turn as
    /// taken. A run some runner owed when it stopped is taken over too.
    /// Returns the runs it started, which finish on their own; awaiting
    /// them is for whoever wants to know when.
    pub async fn tick(&self, at: u64) -> Vec<tokio::task::JoinHandle<()>> {
        let config = &self.state.config;
        let records = records::of(config);
        let mut started = Vec::new();
        for (app, name, job) in all_jobs(config).await {
            let Some(due) = turn(&job, at) else {
                continue;
            };
            match records.fire(&app, &name, due).await {
                Ok(true) => {}
                // Another scheduler has this turn.
                Ok(false) => continue,
                Err(why) => {
                    tracing::warn!(app, job = name, %why, "the turn could not be claimed; left for the next wake");
                    continue;
                }
            }
            let lease = match claim(config, &app, &name).await {
                Ok(lease) => lease,
                Err(busy) => {
                    match busy {
                        Busy::Running => tracing::warn!(app, job = name, "still running; skipping this turn"),
                        Busy::App(running) => {
                            tracing::warn!(app, job = name, running, "the app runs as many jobs as it may; skipping this turn")
                        }
                        Busy::Failed(why) => tracing::warn!(app, job = name, %why, "the job's slot could not be read; skipping this turn"),
                    }
                    update(config, &app, &name, move |job| job.last_skipped_at = Some(at)).await;
                    continue;
                }
            };
            // Marked started now, not when the task gets round to it, so
            // the next wake is planned from this turn.
            begin(config, &app, &name, at).await;
            started.push(spawn_runs(&tokio::runtime::Handle::current(), self.state.clone(), app, name, lease, "scheduled"));
        }
        started.extend(self.take_over().await);
        started
    }

    /// Runs owed by a runner that stopped: a job asked to go again whose
    /// slot ran out unsettled. Taking the slot spends the request, so one
    /// scheduler runs it.
    async fn take_over(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let config = &self.state.config;
        let orphans = config.stores.leases.orphans(SLOT_PREFIX).await.unwrap_or_else(|why| {
            tracing::warn!(%why, "runs owed by stopped runners could not be read");
            Vec::new()
        });
        let mut started = Vec::new();
        for (lease, group) in orphans {
            let Some(app) = group else {
                continue;
            };
            let Some(name) = lease.strip_prefix(&slot(&app, "")).map(str::to_string) else {
                continue;
            };
            // Held or full: another runner took it, or the app has no room
            // yet. Still owed, so the next wake looks again.
            let Ok(lease) = claim(config, &app, &name).await else {
                continue;
            };
            tracing::info!(app, job = name, "running a queued run whose runner stopped");
            begin(config, &app, &name, now()).await;
            started.push(spawn_runs(&tokio::runtime::Handle::current(), self.state.clone(), app, name, lease, "taken over"));
        }
        started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "test-token");
        (dir, config)
    }

    #[tokio::test]
    async fn a_schedule_is_checked_when_it_is_set_not_when_it_should_fire() {
        let (_dir, config) = config();
        assert!(set_job(&config, "app", "refresh", "0 */5 * * * *", "/api/refresh").await.is_ok());

        let error = set_job(&config, "app", "refresh", "not a schedule", "/api/x").await.unwrap_err();
        assert!(error.contains("Six fields"), "the error should show the shape: {error}");
        // A path a handler could never receive is refused too.
        assert!(set_job(&config, "app", "refresh", "0 * * * * *", "api/x").await.is_err());
    }

    #[test]
    fn a_job_is_due_when_a_scheduled_time_has_passed_since_it_last_ran() {
        // Fixed instants rather than the wall clock: on a minute schedule,
        // "a second ago" is or is not due depending on where in the minute
        // the test happens to run.
        const BOUNDARY: u64 = 1_000_000_020; // divisible by 60
        let at = BOUNDARY + 10;
        let every_minute = Job {
            schedule: "0 * * * * *".to_string(),
            path: "/api/x".to_string(),
            ..Job::default()
        };

        // Last ran before the boundary, so that minute came round: due.
        let missed = Job {
            last_run: Some(BOUNDARY - 20),
            ..every_minute.clone()
        };
        assert!(is_due(&missed, at));

        // Ran after it, and the next one has not arrived: not due.
        let recent = Job {
            last_run: Some(BOUNDARY + 5),
            ..every_minute.clone()
        };
        assert!(!is_due(&recent, at));

        // Down for an hour: it fires once, not once per missed minute.
        let long_gone = Job {
            last_run: Some(BOUNDARY - 3600),
            ..every_minute
        };
        assert!(is_due(&long_gone, at));
    }

    #[test]
    fn a_yearly_job_is_not_due_just_because_it_never_ran() {
        let new_year = Job {
            schedule: "0 0 0 1 1 *".to_string(),
            path: "/api/x".to_string(),
            ..Job::default()
        };
        // Without a last run the window is half a minute, not all of history, so a
        // rare job does not fire the moment it is created.
        assert!(!is_due(&new_year, now()));
    }

    #[tokio::test]
    async fn jobs_belong_to_one_app() {
        let (_dir, config) = config();
        set_job(&config, "mine", "refresh", "0 * * * * *", "/api/x").await.unwrap();
        assert!(read_jobs(&config, "theirs").is_empty());
        assert!(remove_job(&config, "theirs", "refresh").await.is_err());
        // A name that is not an app's reaches no file.
        assert!(set_job(&config, "../mine", "refresh", "0 * * * * *", "/api/x").await.is_err());
        assert!(read_jobs(&config, "../mine").is_empty());
    }

    #[test]
    fn a_ten_second_schedule_fires_every_ten_seconds_not_every_tick() {
        // The scheduler sleeps until the next due time, so the gaps between
        // runs are the schedule's own. Walk it: each wake is the time
        // `next_due` names, and each run is recorded as it would be.
        const START: u64 = 1_000_000_000; // a multiple of ten
        let mut job = Job {
            schedule: "*/10 * * * * *".to_string(),
            path: "/api/x".to_string(),
            last_run: Some(START),
            ..Job::default()
        };
        let mut fired = Vec::new();
        let mut at = START;
        while fired.len() < 6 {
            at = next_due(&job, at).unwrap();
            assert!(is_due(&job, at));
            assert!(!is_due(&job, at - 1), "due a second early at {at}");
            fired.push(at);
            job.last_started_at = Some(at);
            job.last_run = Some(at);
        }
        let gaps: Vec<u64> = fired.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(gaps, [10, 10, 10, 10, 10], "fired at {fired:?}");

        // Every second means every second.
        let every_second = Job { schedule: "* * * * * *".to_string(), ..job };
        assert_eq!(next_due(&every_second, at), Some(at + 1));
    }

    #[test]
    fn a_skipped_turn_counts_as_taken() {
        const START: u64 = 1_000_000_000;
        let job = Job {
            schedule: "*/10 * * * * *".to_string(),
            path: "/api/x".to_string(),
            last_run: Some(START),
            last_skipped_at: Some(START + 10),
            ..Job::default()
        };
        // Not due again at the turn it skipped, so the loop sleeps to the next.
        assert!(!is_due(&job, START + 10));
        assert_eq!(next_due(&job, START + 10), Some(START + 20));
    }

    #[tokio::test]
    async fn a_job_runs_once_at_a_time_and_one_more_run_queues_behind_it() {
        let (_dir, config) = config();
        let work = claim(&config, "app", "work").await.unwrap();
        assert_eq!(claim(&config, "app", "work").await, Err(Busy::Running), "a second run got the slot");
        // Asking while it runs queues exactly one more.
        assert_eq!(config.stores.leases.ask_again(&slot("app", "work")).await.unwrap(), Some(false));
        assert_eq!(config.stores.leases.ask_again(&slot("app", "work")).await.unwrap(), Some(true));
        assert!(again_or_release(&config, "app", "work", &work).await, "the queued run was lost");
        assert!(is_running(&config, "app", "work"));
        assert!(!again_or_release(&config, "app", "work", &work).await);
        let again = claim(&config, "app", "work").await.expect("the slot was not given back");
        claim(&config, "app", "other").await.expect("another job of the app was held up");
        assert_eq!(running(&config, "app").await, BTreeSet::from(["other".to_string(), "work".to_string()]));
        // Nothing queued: finishing lets the slot go.
        assert!(!again_or_release(&config, "app", "work", &again).await);
        assert!(!is_running(&config, "app", "work"));
    }

    #[tokio::test]
    async fn an_app_runs_at_most_its_share_of_jobs_at_once_and_another_app_is_not_held_up() {
        let (_dir, config) = config();
        let config = Config { jobs: Arc::new(Jobs::new(100).with_running_per_app(2)), ..config };
        let one = claim(&config, "busy", "one").await.unwrap();
        claim(&config, "busy", "two").await.unwrap();
        assert_eq!(claim(&config, "busy", "three").await, Err(Busy::App(2)));
        // Another app, even one whose slug starts with this one's, runs.
        claim(&config, "busy/sub", "one").await.unwrap();
        claim(&config, "quiet", "one").await.unwrap();
        // One finishing frees a place.
        assert!(!again_or_release(&config, "busy", "one", &one).await);
        claim(&config, "busy", "three").await.unwrap();
    }

    #[tokio::test]
    async fn an_app_declares_at_most_a_hundred_jobs() {
        let (_dir, config) = config();
        for n in 0..MAX_JOBS_PER_APP {
            set_job(&config, "app", &format!("job{n}"), "0 0 3 * * *", "/api/x").await.unwrap();
        }
        let error = set_job(&config, "app", "one-more", "0 0 3 * * *", "/api/x").await.unwrap_err();
        assert!(error.contains("most"), "{error}");
        // Changing one it has is still fine.
        set_job(&config, "app", "job0", "0 0 4 * * *", "/api/y").await.unwrap();
        assert_eq!(read_jobs(&config, "app").len(), MAX_JOBS_PER_APP);
    }

    #[tokio::test]
    async fn a_schedule_that_never_fires_is_refused_at_once_and_never_spins_the_scheduler() {
        let (_dir, config) = config();
        let started = std::time::Instant::now();
        for never in ["0 0 0 30 2 *", "0 0 0 31 4 *", "0 0 0 1 1 * 2001"] {
            let error = set_job(&config, "app", "never", never, "/api/x").await.unwrap_err();
            assert!(error.contains("never fires"), "{never}: {error}");
            // A job file written by hand, or by an older server, with one.
            let job = Job { schedule: never.to_string(), path: "/api/x".into(), ..Job::default() };
            assert_eq!(next_due(&job, now()), None, "{never}");
        }
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn starts_past_the_rate_are_refused() {
        let (_dir, config) = config();
        let config = Config { jobs: Arc::new(Jobs::new(3)), ..config };
        for _ in 0..3 {
            count_start(&config, "busy").await.unwrap();
        }
        assert!(count_start(&config, "busy").await.unwrap_err().contains("limit"));
        // The rate is per app.
        count_start(&config, "quiet").await.unwrap();
    }

    /// Two schedulers that read the job at different moments, one before
    /// and one after the other recorded its start, still name one turn: the
    /// latest scheduled time, not the first after whatever each last read.
    #[test]
    fn a_due_job_fires_for_its_latest_turn_whatever_was_last_read() {
        const START: u64 = 1_000_000_000; // a multiple of ten
        let job = Job { schedule: "*/2 * * * * *".to_string(), path: "/api/x".to_string(), ..Job::default() };
        // Never run: the fallback window reaches back thirty seconds, but
        // the turn is the latest one.
        assert_eq!(turn(&job, START + 1), Some(START));
        assert_eq!(turn(&job, START + 2), Some(START + 2));
        // Down for an hour: one turn, the latest.
        let gone = Job { last_run: Some(START - 3600), ..job.clone() };
        assert_eq!(turn(&gone, START + 3), Some(START + 2));
        // Started at the turn: nothing more due until the next.
        let started = Job { last_started_at: Some(START + 2), ..job.clone() };
        assert_eq!(turn(&started, START + 3), None);
        assert_eq!(turn(&started, START + 4), Some(START + 4));
        let every_second = Job { schedule: "* * * * * *".to_string(), ..job };
        assert_eq!(turn(&every_second, START + 7), Some(START + 7));
    }
}
