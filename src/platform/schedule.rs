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
//! person, or by the app through `jobs.run`. A schedule that comes round
//! while its job still runs is skipped, and the skip recorded. Asked for
//! while it runs, a job runs once more as soon as it finishes, which is how
//! an app chains stages back to back.

use crate::{config::Config, content::slug::valid_slug, runtime::wasm::Runtime, AppState};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
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

/// Every job in progress in this process, and what is waiting on them. One
/// per process, shared by every copy of the config like `connections`, so
/// the scheduler, a person and an app's handler all see the same runs.
pub struct Jobs {
    /// `app/job` for each job in progress, and whether another run is
    /// queued behind it.
    running: Mutex<HashMap<String, bool>>,
    /// When each app's recent `jobs.run` starts happened, the last minute's.
    starts: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// What a job started from inside a handler runs on: the runtime and
    /// the async runtime of the server, set once they exist.
    attached: Mutex<Option<(Weak<Runtime>, tokio::runtime::Handle)>>,
    /// Woken when a job is set, removed or finishes, so the scheduler works
    /// out its next wake again.
    changed: tokio::sync::Notify,
    /// `jobs.run` starts per app per minute, from
    /// `TOOLSITE_JOB_STARTS_PER_MINUTE`.
    pub starts_per_minute: usize,
}

impl Default for Jobs {
    fn default() -> Self {
        Self::new(DEFAULT_STARTS_PER_MINUTE)
    }
}

/// Whether a run began, or was queued behind one in progress.
#[derive(Debug, Clone, PartialEq)]
pub enum Ran {
    /// It ran, and this is the handler's status.
    Finished(String),
    /// It was running already, and runs once more when that run finishes.
    Queued,
}

fn key(app: &str, name: &str) -> String {
    format!("{app}/{name}")
}

impl Jobs {
    pub fn new(starts_per_minute: usize) -> Self {
        Self {
            running: Mutex::default(),
            starts: Mutex::default(),
            attached: Mutex::default(),
            changed: tokio::sync::Notify::new(),
            starts_per_minute,
        }
    }

    /// Gives jobs started from inside a handler somewhere to run. Called
    /// wherever a runtime meets an async context: the router, the
    /// scheduler, a run. Outside one it does nothing.
    pub fn attach(&self, runtime: &Arc<Runtime>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            *self.attached.lock().unwrap() = Some((Arc::downgrade(runtime), handle));
        }
    }

    pub fn is_running(&self, app: &str, name: &str) -> bool {
        self.running.lock().unwrap().contains_key(&key(app, name))
    }

    /// Tells the scheduler that what it planned around has changed.
    pub fn changed(&self) {
        self.changed.notify_one();
    }

    /// Takes the job's one slot, or says it is taken.
    fn claim(&self, app: &str, name: &str) -> bool {
        let mut running = self.running.lock().unwrap();
        let key = key(app, name);
        if running.contains_key(&key) {
            return false;
        }
        running.insert(key, false);
        true
    }

    /// After a run: true, with the slot still held, when another run was
    /// queued behind it; otherwise the slot is given up.
    fn again_or_release(&self, app: &str, name: &str) -> bool {
        let mut running = self.running.lock().unwrap();
        let key = key(app, name);
        match running.get_mut(&key) {
            Some(again) if *again => {
                *again = false;
                true
            }
            _ => {
                running.remove(&key);
                false
            }
        }
    }

    /// Counts one start against the app's rate, or refuses it.
    fn count_start(&self, app: &str) -> Result<(), String> {
        let mut starts = self.starts.lock().unwrap();
        let recent = starts.entry(app.to_string()).or_default();
        let minute_ago = Instant::now().checked_sub(Duration::from_secs(60));
        while recent.front().is_some_and(|at| minute_ago.is_some_and(|ago| *at < ago)) {
            recent.pop_front();
        }
        if recent.len() >= self.starts_per_minute {
            return Err(format!(
                "{app} has started {} jobs in the last minute, which is this site's limit; try again shortly",
                recent.len()
            ));
        }
        recent.push_back(Instant::now());
        Ok(())
    }
}

/// One lock for every job file in the process: runs finishing together
/// would otherwise each write over the other's record.
static FILES: Mutex<()> = Mutex::new(());

fn path_for(config: &Config, app: &str) -> Option<PathBuf> {
    valid_slug(app).then(|| config.data_dir.join(format!("{app}.jobs")))
}

pub fn read_jobs(config: &Config, app: &str) -> BTreeMap<String, Job> {
    let Some(path) = path_for(config, app) else {
        return BTreeMap::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_jobs(config: &Config, app: &str, jobs: &BTreeMap<String, Job>) -> Result<(), String> {
    let path = path_for(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(jobs).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Changes one job's record under the file lock. Nothing happens if the job
/// is gone.
fn update(config: &Config, app: &str, name: &str, change: impl FnOnce(&mut Job)) {
    let _held = FILES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut jobs = read_jobs(config, app);
    if let Some(job) = jobs.get_mut(name) {
        change(job);
        let _ = write_jobs(config, app, &jobs);
    }
}

/// Adds or replaces a job. The schedule is parsed here so a bad expression is
/// refused while someone is watching, rather than silently never firing.
pub fn set_job(
    config: &Config,
    app: &str,
    name: &str,
    schedule: &str,
    path: &str,
) -> Result<String, String> {
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

    {
        let _held = FILES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut jobs = read_jobs(config, app);
        jobs.insert(
            name.to_string(),
            Job {
                schedule: schedule.to_string(),
                path: path.to_string(),
                last_run: None,
                last_status: None,
                last_started_at: None,
                last_finished_at: None,
                last_duration_ms: None,
                last_skipped_at: None,
            },
        );
        write_jobs(config, app, &jobs)?;
    }
    config.jobs.changed();
    Ok(format!("next run {}", next.to_rfc3339()))
}

pub fn remove_job(config: &Config, app: &str, name: &str) -> Result<(), String> {
    {
        let _held = FILES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut jobs = read_jobs(config, app);
        if jobs.remove(name).is_none() {
            return Err(format!("{app} has no job called {name}"));
        }
        write_jobs(config, app, &jobs)?;
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

/// Runs a job once, as the scheduler would, and records the outcome. The
/// caller holds its slot.
async fn run_once(state: &AppState, app: &str, name: &str) -> Result<String, String> {
    let jobs = read_jobs(&state.config, app);
    let job = jobs.get(name).cloned().ok_or_else(|| format!("{app} has no job called {name}"))?;

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
    let started_ms = now_ms();
    update(&state.config, app, name, |job| job.last_started_at = Some(started_ms / 1000));
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
    update(&state.config, app, name, |job| {
        job.last_run = Some(finished_ms / 1000);
        job.last_finished_at = Some(finished_ms / 1000);
        job.last_duration_ms = Some(finished_ms.saturating_sub(started_ms));
        job.last_status = Some(status.clone());
    });
    Ok(status)
}

/// Runs whatever was queued behind a run that just ended, one after the
/// other, until nothing is; then gives the slot up.
async fn run_queued(state: &AppState, app: &str, name: &str) {
    while state.config.jobs.again_or_release(app, name) {
        match run_once(state, app, name).await {
            Ok(status) => tracing::info!(app, job = name, status, "queued job ran"),
            Err(error) => tracing::warn!(app, job = name, error, "queued job failed"),
        }
    }
    state.config.jobs.changed();
}

/// Runs one job now, whatever its schedule says, for a person, and answers
/// how it went. Already running, it queues one more run behind the current
/// one instead. The same path a schedule takes, so the two cannot behave
/// differently.
pub async fn run_job(state: &AppState, app: &str, name: &str) -> Result<Ran, String> {
    state.config.jobs.attach(&state.runtime);
    if !read_jobs(&state.config, app).contains_key(name) {
        return Err(format!("{app} has no job called {name}"));
    }
    if !state.config.jobs.claim(app, name) {
        queue_again(&state.config, app, name);
        return Ok(Ran::Queued);
    }
    let outcome = run_once(state, app, name).await;
    // The slot is settled before answering, so a caller who asks again
    // once this returns gets a run of its own. Whatever was queued
    // meanwhile runs on without the caller.
    if state.config.jobs.again_or_release(app, name) {
        let (state, app, name) = (state.clone(), app.to_string(), name.to_string());
        tokio::spawn(async move {
            if let Err(error) = run_once(&state, &app, &name).await {
                tracing::warn!(app, job = name, error, "queued job failed");
            }
            run_queued(&state, &app, &name).await;
        });
    } else {
        state.config.jobs.changed();
    }
    outcome.map(Ran::Finished)
}

/// Marks a running job to run once more. False when it was not running.
fn queue_again(config: &Config, app: &str, name: &str) -> bool {
    match config.jobs.running.lock().unwrap().get_mut(&key(app, name)) {
        Some(again) => {
            *again = true;
            true
        }
        None => false,
    }
}

/// `jobs.run` from inside an app's handler: starts one of the app's own
/// jobs in the background, or queues one more run of it if it is running.
/// `app` is the running app, never anything the guest said.
pub fn start_from_app(config: &Arc<Config>, app: &str, name: &str) -> Result<String, String> {
    if !read_jobs(config, app).contains_key(name) {
        return Err(format!("{app} declares no job called {name:?}"));
    }
    let jobs = &config.jobs;
    // Decided under the one lock, so a run that ends meanwhile either sees
    // the queued flag or has already let the slot go and this starts anew.
    let mut running = jobs.running.lock().unwrap();
    if let Some(again) = running.get_mut(&key(app, name)) {
        if !*again {
            jobs.count_start(app)?;
            *again = true;
        }
        return Ok("queued".to_string());
    }
    let Some((runtime, handle)) = jobs.attached.lock().unwrap().clone() else {
        return Err("jobs cannot be started on this server yet".to_string());
    };
    let Some(runtime) = runtime.upgrade() else {
        return Err("jobs cannot be started on this server yet".to_string());
    };
    jobs.count_start(app)?;
    running.insert(key(app, name), false);
    drop(running);
    let state = AppState { config: config.clone(), runtime };
    let (app, name) = (app.to_string(), name.to_string());
    handle.spawn(async move {
        match run_once(&state, &app, &name).await {
            Ok(status) => tracing::info!(app, job = name, status, "job ran, started by the app"),
            Err(error) => tracing::warn!(app, job = name, error, "job failed, started by the app"),
        }
        run_queued(&state, &app, &name).await;
    });
    Ok("started".to_string())
}

/// Records an outcome against a job, as if it had just finished. For tests
/// that need a job to look as though it has run.
pub fn record_run(config: &Config, app: &str, name: &str, status: &str) {
    let at = now();
    update(config, app, name, |job| {
        job.last_run = Some(at);
        job.last_finished_at = Some(at);
        job.last_status = Some(status.to_string());
    });
}

/// Every app that has jobs, found the same way the index finds pages.
async fn apps_with_jobs(config: &Config) -> Vec<String> {
    let Ok(mut entries) = tokio::fs::read_dir(&config.data_dir).await else {
        return Vec::new();
    };
    let mut apps = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(app) = name.strip_suffix(".jobs") {
            apps.push(app.to_string());
        }
    }
    apps
}

/// What runs an app's jobs on their schedules. A process starts one, from
/// `main`, for the data directory it serves: two would each fire every job.
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
        let mut soonest: Option<u64> = None;
        for app in apps_with_jobs(&self.state.config).await {
            for job in read_jobs(&self.state.config, &app).values() {
                if let Some(next) = next_due(job, at) {
                    soonest = Some(soonest.map_or(next, |s| s.min(next)));
                }
            }
        }
        match soonest {
            Some(next) => Duration::from_millis((next * 1000).saturating_sub(at_ms)).clamp(MIN_SLEEP, MAX_SLEEP),
            None => MAX_SLEEP,
        }
    }

    /// Starts every job due at `at`, in Unix seconds. One still running is
    /// skipped and the skip recorded, which also counts the turn as taken.
    /// Returns the runs it started, which finish on their own; awaiting
    /// them is for whoever wants to know when.
    pub async fn tick(&self, at: u64) -> Vec<tokio::task::JoinHandle<()>> {
        let mut started = Vec::new();
        for app in apps_with_jobs(&self.state.config).await {
            for (name, job) in read_jobs(&self.state.config, &app) {
                if !is_due(&job, at) {
                    continue;
                }
                if !self.state.config.jobs.claim(&app, &name) {
                    tracing::warn!(app, job = name, "still running; skipping this turn");
                    update(&self.state.config, &app, &name, |job| job.last_skipped_at = Some(at));
                    continue;
                }
                // Marked started now, not when the task gets round to it,
                // so the next wake is planned from this turn.
                update(&self.state.config, &app, &name, |job| job.last_started_at = Some(at));
                let (state, app) = (self.state.clone(), app.clone());
                started.push(tokio::spawn(async move {
                    match run_once(&state, &app, &name).await {
                        Ok(status) => tracing::info!(app, job = name, status, "job ran"),
                        Err(error) => tracing::warn!(app, job = name, error, "job failed"),
                    }
                    run_queued(&state, &app, &name).await;
                }));
            }
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

    #[test]
    fn a_schedule_is_checked_when_it_is_set_not_when_it_should_fire() {
        let (_dir, config) = config();
        assert!(set_job(&config, "app", "refresh", "0 */5 * * * *", "/api/refresh").is_ok());

        let error = set_job(&config, "app", "refresh", "not a schedule", "/api/x").unwrap_err();
        assert!(error.contains("Six fields"), "the error should show the shape: {error}");
        // A path a handler could never receive is refused too.
        assert!(set_job(&config, "app", "refresh", "0 * * * * *", "api/x").is_err());
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

    #[test]
    fn jobs_belong_to_one_app() {
        let (_dir, config) = config();
        set_job(&config, "mine", "refresh", "0 * * * * *", "/api/x").unwrap();
        assert!(read_jobs(&config, "theirs").is_empty());
        assert!(remove_job(&config, "theirs", "refresh").is_err());
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

    #[test]
    fn a_job_runs_once_at_a_time_and_one_more_run_queues_behind_it() {
        let jobs = Jobs::new(10);
        assert!(jobs.claim("app", "work"));
        assert!(!jobs.claim("app", "work"), "a second run got the slot");
        assert!(jobs.claim("app", "other"), "another job of the app was held up");
        // Nothing queued: finishing lets the slot go.
        assert!(!jobs.again_or_release("app", "work"));
        assert!(!jobs.is_running("app", "work"));
    }

    #[test]
    fn starts_past_the_rate_are_refused() {
        let jobs = Jobs::new(3);
        for _ in 0..3 {
            jobs.count_start("busy").unwrap();
        }
        assert!(jobs.count_start("busy").unwrap_err().contains("limit"));
        // The rate is per app.
        jobs.count_start("quiet").unwrap();
    }
}
