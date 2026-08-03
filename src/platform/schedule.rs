//! Work an app does without anyone asking: refreshing a cache, pulling from
//! an API, tidying a table.
//!
//! A job is a cron expression and a path. When it fires, the host calls the
//! app's own handler exactly as a request would — same sandbox, same limits,
//! same database, no identity. So a job is just a route the app already has,
//! and nothing new has to be reasoned about to know what it can do.

use crate::{config::Config, content::slug::valid_slug, AppState};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

/// How often the scheduler looks for work. Jobs fire at most once per tick,
/// so this is also the coarsest resolution a schedule really has.
const TICK: Duration = Duration::from_secs(30);

/// A job that runs long enough to be stuck is not retried on the next tick;
/// it is left alone and reported.
const JOB_GUARDS: crate::runtime::wasm::Guards = crate::runtime::wasm::Guards {
    fuel: 2_000_000_000,
    memory_bytes: 128 * 1024 * 1024,
    wall_clock: Duration::from_secs(60),
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Standard cron with seconds leading, as the `cron` crate reads it.
    pub schedule: String,
    /// The path handed to the handler, e.g. `/api/refresh`.
    pub path: String,
    /// Unix seconds. Absent until it has run once.
    #[serde(default)]
    pub last_run: Option<u64>,
    /// What happened last time, for whoever is wondering why nothing changed.
    #[serde(default)]
    pub last_status: Option<String>,
}

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

    let mut jobs = read_jobs(config, app);
    jobs.insert(
        name.to_string(),
        Job {
            schedule: schedule.to_string(),
            path: path.to_string(),
            last_run: None,
            last_status: None,
        },
    );
    write_jobs(config, app, &jobs)?;
    Ok(format!("next run {}", next.to_rfc3339()))
}

pub fn remove_job(config: &Config, app: &str, name: &str) -> Result<(), String> {
    let mut jobs = read_jobs(config, app);
    if jobs.remove(name).is_none() {
        return Err(format!("{app} has no job called {name}"));
    }
    write_jobs(config, app, &jobs)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Whether a job is due, given when it last ran.
///
/// The question asked is "did a scheduled time pass since we last ran", not
/// "is it that second now" — a tick every thirty seconds would otherwise miss
/// almost everything.
fn is_due(job: &Job, at: u64) -> bool {
    let Ok(schedule) = cron::Schedule::from_str(&job.schedule) else {
        return false;
    };
    // From the last run, so a job that missed its turn while the server was
    // down still fires once. With no last run the window is a single tick,
    // so creating a yearly job does not set it off immediately.
    let since = job
        .last_run
        .unwrap_or_else(|| at.saturating_sub(TICK.as_secs()));
    let Some(after) = chrono::DateTime::from_timestamp(since as i64, 0) else {
        return false;
    };
    schedule
        .after(&after)
        .next()
        .is_some_and(|next| next.timestamp() as u64 <= at)
}

/// Runs one job now, whatever its schedule says, and records the outcome.
/// This is also what an on-demand run uses, so the two cannot behave
/// differently.
pub async fn run_job(state: &AppState, app: &str, name: &str) -> Result<String, String> {
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

    let runtime = state.runtime.clone();
    let config = state.config.clone();
    let owned_app = app.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        // No identity: a scheduled run is the app acting on its own behalf.
        runtime.handle(config, &owned_app, &wasm, None, request, JOB_GUARDS)
    })
    .await;

    let status = match outcome {
        Ok(Ok(response)) => format!("{}", response.status),
        Ok(Err(error)) => format!("failed: {error}"),
        Err(error) => format!("failed: {error}"),
    };

    record_run(&state.config, app, name, &status);
    Ok(status)
}

/// Records an outcome against a job. Used by the scheduler, and by tests
/// that need a job to look as though it has run.
pub fn record_run(config: &Config, app: &str, name: &str, status: &str) {
    let mut jobs = read_jobs(config, app);
    if let Some(job) = jobs.get_mut(name) {
        job.last_run = Some(now());
        job.last_status = Some(status.to_string());
        let _ = write_jobs(config, app, &jobs);
    }
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

/// Wakes every tick, runs whatever is due, and never runs two of the same job
/// at once — a slow job is skipped rather than stacked.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let running: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

        loop {
            tokio::time::sleep(TICK).await;
            let at = now();

            for app in apps_with_jobs(&state.config).await {
                for (name, job) in read_jobs(&state.config, &app) {
                    if !is_due(&job, at) {
                        continue;
                    }
                    let key = format!("{app}:{name}");
                    {
                        let mut running = running.lock().await;
                        if !running.insert(key.clone()) {
                            tracing::warn!(app, job = name, "still running; skipping this turn");
                            continue;
                        }
                    }

                    let state = state.clone();
                    let running = running.clone();
                    tokio::spawn(async move {
                        let (app_name, job_name) = (
                            key.split(':').next().unwrap_or_default().to_string(),
                            key.split(':').nth(1).unwrap_or_default().to_string(),
                        );
                        match run_job(&state, &app_name, &job_name).await {
                            Ok(status) => {
                                tracing::info!(app = app_name, job = job_name, status, "job ran")
                            }
                            Err(error) => {
                                tracing::warn!(app = app_name, job = job_name, error, "job failed")
                            }
                        }
                        running.lock().await.remove(&key);
                    });
                }
            }
        }
    });
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
            last_run: None,
            last_status: None,
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
            last_run: None,
            last_status: None,
        };
        // Without a last run the window is one tick, not all of history, so a
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
}
