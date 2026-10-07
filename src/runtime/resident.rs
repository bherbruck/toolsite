//! Resident mode: one long-lived instance per app, for an app that asks for
//! it, which takes every one of its connection events.
//!
//! Normally each event runs in a fresh sandbox, so a handler keeps nothing
//! in memory. Server software built around an in-memory state machine (a
//! broker's sessions and subscriptions, a game's world) cannot run that way.
//! An app that declares `[resident]` gets one instance instead, created on
//! its first connection event and owned by one thread that runs events one
//! at a time, in the order they were queued, across all of the app's
//! connections. `on-tick`, when the handler exports it, runs on the same
//! thread between events.
//!
//! The instance is a sandbox like any other: the same imports, fuel and a
//! wall-clock deadline per call, and a memory cap for its whole life. Only
//! connection events reach it. Requests and jobs still run fresh; they share
//! the database and files with it, not memory.
//!
//! An instance only ever sees connections that connected to it. When it
//! fails (a trap, a call past its time or fuel, memory past its cap) it is
//! dropped and every connection it knew is closed, since what it held about
//! them is gone. The next connection starts a fresh one, after a pause that
//! doubles while it keeps failing. Stopping it, because the handler was
//! replaced or the app hidden or removed, does the same; the pause, if one
//! is running, carries over, so republishing is no way around it.
//!
//! What a site spends on them is bounded from here: how many instances run
//! at once (each is a thread) and the memory their caps add up to, counting
//! an instance that was stopped and is still finishing its call; and how
//! many events may wait for one instance.

use crate::{
    config::Config as SiteConfig,
    runtime::wasm::{ConnectionEvent, Guards, Resident, Runtime, User},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

/// The pause after the first failure. It doubles with each failure after.
const FIRST_PAUSE: Duration = Duration::from_secs(1);
/// The longest pause. An instance that lived this long before it failed
/// starts the count again.
const MAX_PAUSE: Duration = Duration::from_secs(60);
/// The shortest and longest `tick_ms` an app may declare.
pub const MIN_TICK_MS: u64 = 100;
pub const MAX_TICK_MS: u64 = 60_000;
/// The longest a last failure's reason is kept, in characters.
const MAX_REASON_CHARS: usize = 300;

const MIB: u64 = 1024 * 1024;

/// How one app's instance runs, from its manifest. A change to these starts
/// a new instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub memory_bytes: usize,
    pub tick: Option<Duration>,
}

/// What the admin page and MCP report about one app's instance.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    /// When the running instance started, in seconds since the Unix epoch.
    /// None when no instance runs.
    pub running_since: Option<u64>,
    /// Instances that failed and were dropped since the server started.
    pub restarts: u32,
    pub last_crash: Option<String>,
    pub last_crash_at: Option<u64>,
    /// When the next instance may start, while it pauses after a failure.
    pub next_start_at: Option<u64>,
    pub memory_bytes: u64,
    pub memory_limit_bytes: u64,
    pub tick_ms: Option<u64>,
    /// Connections the running instance has accepted and not yet closed.
    pub connections: usize,
}

type Answer = Result<Result<(), String>, String>;

/// Reads an app's handler as it is on disk, or None when it has none. The
/// platform knows where handlers live; this module only asks.
pub type Loader = fn(&SiteConfig, &str) -> Option<Vec<u8>>;

/// One connection event for the thread to run.
struct Job {
    user: Option<User>,
    conn: String,
    event: ConnectionEvent,
    guards: Guards,
    reply: oneshot::Sender<Answer>,
}

/// What the thread is sent: an event to run, or a nudge to look at
/// whether it was stopped.
enum Message {
    Job(Box<Job>),
    Wake,
}

/// One running thread and how it was started.
struct Worker {
    jobs: mpsc::SyncSender<Message>,
    stopped: Arc<AtomicBool>,
    settings: Settings,
}

impl Worker {
    /// The thread ends after the call it is in, if any: the nudge wakes one
    /// that waits for an event or a tick. Should the queue be full, the
    /// thread is busy and sees the flag when its call returns.
    fn stop(self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = self.jobs.try_send(Message::Wake);
    }
}

/// When the next instance may start after failures. Kept by the app, not by
/// one thread, so it outlives a stop.
#[derive(Default)]
struct Backoff {
    pause: Duration,
    retry_at: Option<Instant>,
}

#[derive(Default)]
struct Entry {
    worker: Option<Worker>,
    /// Kept across stops, so restarts and the last failure stay visible.
    status: Arc<Mutex<Status>>,
    backoff: Arc<Mutex<Backoff>>,
}

/// The threads alive across the site and the memory their caps reserve,
/// counting a stopped one until it has returned.
#[derive(Default)]
struct Running {
    threads: usize,
    reserved_bytes: u64,
}

/// One thread's place in `Running`, given back when the thread ends,
/// however it ends.
struct Slot {
    running: Arc<Mutex<Running>>,
    bytes: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        running.threads -= 1;
        running.reserved_bytes -= self.bytes;
    }
}

/// Every resident app's instance. One per process, like the connection
/// registry the instances act on.
pub struct Residents {
    /// The memory an app gets when it does not say, in MB.
    pub default_memory_mb: u64,
    /// The most an app may ask for, in MB.
    pub max_memory_mb: u64,
    /// The most instances that may run at once, from `TOOLSITE_RESIDENT_MAX`.
    pub max_instances: usize,
    /// The memory all running instances' caps may add up to, in MB, from
    /// `TOOLSITE_RESIDENT_TOTAL_MB`.
    pub total_memory_mb: u64,
    /// The events that may wait for one instance, from
    /// `TOOLSITE_RESIDENT_QUEUE`. One more is refused.
    pub queue_depth: usize,
    apps: Mutex<HashMap<String, Entry>>,
    running: Arc<Mutex<Running>>,
    /// Held while a deploy checks a handler against `[resident]` and writes
    /// either one, so a manifest and a handler uploaded at once cannot each
    /// pass against the other's old version.
    declaring: tokio::sync::Mutex<()>,
}

impl Default for Residents {
    fn default() -> Self {
        Self::new(128, 512)
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn unix_at(at: Instant) -> u64 {
    unix_now() + at.saturating_duration_since(Instant::now()).as_secs()
}

impl Residents {
    /// The memory default and ceiling as given, with the other limits at
    /// their defaults: 20 instances, 2048 MB between them, 256 waiting
    /// events each.
    pub fn new(default_memory_mb: u64, max_memory_mb: u64) -> Self {
        Self {
            default_memory_mb,
            max_memory_mb,
            max_instances: 20,
            total_memory_mb: 2048,
            queue_depth: 256,
            apps: Mutex::new(HashMap::new()),
            running: Arc::default(),
            declaring: tokio::sync::Mutex::new(()),
        }
    }

    /// Serialises the check of a handler against `[resident]` with the
    /// write of either. Held across both by a manifest deploy and a handler
    /// deploy.
    pub async fn declaring(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.declaring.lock().await
    }

    /// Threads running instances now, counting stopped ones not yet ended.
    pub fn threads(&self) -> usize {
        self.running.lock().unwrap().threads
    }

    /// Takes a place for one more thread with a cap of `bytes`, or says why
    /// there is none.
    fn claim(&self, app: &str, bytes: u64) -> Result<Slot, String> {
        let mut running = self.running.lock().unwrap();
        if running.threads >= self.max_instances {
            return Err(format!(
                "{app} cannot start its resident instance: this site runs at most {} at once \
                 (TOOLSITE_RESIDENT_MAX), and that many run now",
                self.max_instances
            ));
        }
        let budget = self.total_memory_mb.saturating_mul(MIB);
        if running.reserved_bytes.saturating_add(bytes) > budget {
            return Err(format!(
                "{app} cannot start its resident instance: its {} MB would take the memory resident \
                 instances hold past this site's {} MB (TOOLSITE_RESIDENT_TOTAL_MB)",
                bytes / MIB,
                self.total_memory_mb
            ));
        }
        running.threads += 1;
        running.reserved_bytes += bytes;
        Ok(Slot {
            running: self.running.clone(),
            bytes,
        })
    }

    /// The settings for what an app declared: its memory, or the default,
    /// never past the ceiling nor under 1 MB; and its tick, if any, within
    /// the range a manifest may ask for.
    pub fn settings(&self, memory_mb: Option<u64>, tick_ms: Option<u64>) -> Settings {
        let memory_mb = memory_mb.unwrap_or(self.default_memory_mb).min(self.max_memory_mb).max(1);
        Settings {
            memory_bytes: (memory_mb * MIB) as usize,
            tick: tick_ms.map(|ms| Duration::from_millis(ms.clamp(MIN_TICK_MS, MAX_TICK_MS))),
        }
    }

    /// Runs one connection event on the app's instance, starting one, with
    /// the handler `load` reads, if none runs. The answer is the handler's;
    /// an error means the event did not run to its end, and the connection
    /// should close.
    #[allow(clippy::too_many_arguments)]
    pub async fn deliver(
        &self,
        runtime: &Arc<Runtime>,
        site: &Arc<SiteConfig>,
        load: Loader,
        app: &str,
        settings: Settings,
        user: Option<User>,
        conn: &str,
        event: ConnectionEvent,
        guards: Guards,
    ) -> Answer {
        let connect = matches!(event, ConnectionEvent::Connect(_));
        let (jobs, stopped) = {
            let mut apps = self.apps.lock().unwrap();
            let entry = apps.entry(app.to_string()).or_default();
            if entry.worker.as_ref().is_some_and(|worker| worker.settings != settings) {
                tracing::info!(app, "resident instance stopped: its settings changed");
                if let Some(worker) = entry.worker.take() {
                    worker.stop();
                }
            }
            if entry.worker.is_none() && !connect {
                // Only a connect starts an instance. Anything else is for a
                // connection of one that is gone, such as the close each of
                // its connections gets when it is stopped: starting a thread
                // to refuse it would leave the thread behind.
                return if matches!(event, ConnectionEvent::Close) {
                    Ok(Ok(()))
                } else {
                    Err("the app's resident instance restarted after this connection opened; connect again".to_string())
                };
            }
            if entry.worker.is_none() {
                let slot = self.claim(app, settings.memory_bytes as u64).inspect_err(|why| {
                    tracing::warn!(app, reason = %why, "resident instance refused");
                })?;
                let (status, backoff) = (entry.status.clone(), entry.backoff.clone());
                entry.worker = Some(spawn(runtime.clone(), site.clone(), load, app, settings, self.queue_depth, status, backoff, slot)?);
            }
            let worker = entry.worker.as_ref().expect("spawned above");
            (worker.jobs.clone(), worker.stopped.clone())
        };
        let (reply, answer) = oneshot::channel();
        let job = Job {
            user,
            conn: conn.to_string(),
            event,
            guards,
            reply,
        };
        match jobs.try_send(Message::Job(Box::new(job))) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                // Refused rather than queued without end: what waits is
                // memory, and an instance this far behind will not catch up.
                tracing::warn!(app, conn, waiting = self.queue_depth, "resident event refused: the queue is full");
                return Err(format!(
                    "the app's resident instance is behind: {} events wait for it already",
                    self.queue_depth
                ));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                // The thread is gone without being stopped; the next event
                // starts another.
                self.forget_dead(app, &stopped);
                return Err("the app's resident instance stopped; connect again".to_string());
            }
        }
        answer
            .await
            .unwrap_or_else(|_| Err("the app's resident instance stopped; connect again".to_string()))
    }

    /// Forgets a worker whose thread is gone, if it is still the current
    /// one. `stopped` tells one worker from the next.
    fn forget_dead(&self, app: &str, stopped: &Arc<AtomicBool>) {
        if let Some(entry) = self.apps.lock().unwrap().get_mut(app)
            && entry.worker.as_ref().is_some_and(|worker| Arc::ptr_eq(&worker.stopped, stopped))
        {
            entry.worker = None;
        }
    }

    /// Drops the app's instance, if one runs, and closes the connections it
    /// knew. For a handler replaced, an app hidden or removed, or resident
    /// mode withdrawn. The next event starts a fresh one at once.
    pub fn stop(&self, app: &str) {
        let worker = self.apps.lock().unwrap().get_mut(app).and_then(|entry| entry.worker.take());
        if let Some(worker) = worker {
            tracing::info!(app, "resident instance stopped");
            worker.stop();
        }
    }

    /// What the app's instance is doing, or None for an app that never ran
    /// resident since the server started.
    pub fn status(&self, app: &str) -> Option<Status> {
        let apps = self.apps.lock().unwrap();
        apps.get(app).map(|entry| entry.status.lock().unwrap().clone())
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn(
    runtime: Arc<Runtime>,
    site: Arc<SiteConfig>,
    load: Loader,
    app: &str,
    settings: Settings,
    queue_depth: usize,
    status: Arc<Mutex<Status>>,
    backoff: Arc<Mutex<Backoff>>,
    slot: Slot,
) -> Result<Worker, String> {
    let (jobs, queue) = mpsc::sync_channel(queue_depth.max(1));
    let stopped = Arc::new(AtomicBool::new(false));
    {
        let mut status = status.lock().unwrap();
        status.memory_limit_bytes = settings.memory_bytes as u64;
        status.tick_ms = settings.tick.map(|tick| tick.as_millis() as u64);
    }
    let thread = Thread {
        runtime,
        site,
        load,
        app: app.to_string(),
        settings,
        status,
        stopped: stopped.clone(),
        live: None,
        backoff,
        next_tick: Instant::now(),
    };
    // Its own thread rather than a task: every call blocks, on wasm or on a
    // host import such as SQLite, and it may hold the thread for as long as
    // the wall clock allows.
    std::thread::Builder::new()
        .name(format!("resident-{app}"))
        .spawn(move || {
            let _slot = slot;
            thread.run(queue)
        })
        .map_err(|e| format!("could not start the app's resident instance: {e}"))?;
    Ok(Worker { jobs, stopped, settings })
}

struct Live {
    instance: Resident,
    /// The connections that connected to this instance and are still open.
    conns: HashSet<String>,
    since: Instant,
}

struct Thread {
    runtime: Arc<Runtime>,
    site: Arc<SiteConfig>,
    load: Loader,
    app: String,
    settings: Settings,
    status: Arc<Mutex<Status>>,
    stopped: Arc<AtomicBool>,
    live: Option<Live>,
    /// How long to wait after the last failure before starting again.
    backoff: Arc<Mutex<Backoff>>,
    next_tick: Instant,
}

impl Thread {
    fn run(mut self, queue: mpsc::Receiver<Message>) {
        loop {
            let ticking = self.settings.tick.is_some() && self.live.as_ref().is_some_and(|live| live.instance.ticks());
            let job = if ticking {
                match queue.recv_timeout(self.next_tick.saturating_duration_since(Instant::now())) {
                    Ok(job) => Some(job),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match queue.recv() {
                    Ok(job) => Some(job),
                    Err(_) => break,
                }
            };
            if self.stopped.load(Ordering::SeqCst) {
                if let Some(Message::Job(job)) = job {
                    let _ = job.reply.send(Err("the app's resident instance stopped; connect again".to_string()));
                }
                break;
            }
            match job {
                None => self.tick(),
                Some(Message::Wake) => {}
                Some(Message::Job(job)) => {
                    let Job {
                        user,
                        conn,
                        event,
                        guards,
                        reply,
                    } = *job;
                    let answer = self.event(user, conn, event, guards);
                    let _ = reply.send(answer);
                }
            }
        }
        {
            let mut status = self.status.lock().unwrap();
            status.running_since = None;
            status.memory_bytes = 0;
            status.connections = 0;
        }
        if let Some(live) = self.live.take() {
            self.close_all(&live);
        }
    }

    fn event(&mut self, user: Option<User>, conn: String, event: ConnectionEvent, guards: Guards) -> Answer {
        let connect = matches!(event, ConnectionEvent::Connect(_));
        let close = matches!(event, ConnectionEvent::Close);
        let known = self.live.as_ref().is_some_and(|live| live.conns.contains(&conn));
        if !connect && !known {
            // Opened on an instance that is gone: this one never saw it.
            return if close {
                Ok(Ok(()))
            } else {
                Err("the app's resident instance restarted after this connection opened; connect again".to_string())
            };
        }
        if self.live.is_none() {
            self.start(guards)?;
        }
        let live = self.live.as_mut().expect("started above");
        match live.instance.connection_event(user, &conn, event, guards) {
            Ok(answer) => {
                if close {
                    live.conns.remove(&conn);
                } else if connect && answer.is_ok() {
                    live.conns.insert(conn);
                }
                let mut status = self.status.lock().unwrap();
                status.memory_bytes = live.instance.memory_bytes() as u64;
                status.connections = live.conns.len();
                Ok(answer)
            }
            Err(error) => Err(self.failed(&error, guards)),
        }
    }

    /// Starts an instance, unless the last one failed too recently.
    fn start(&mut self, guards: Guards) -> Result<(), String> {
        let retry_at = self.backoff().retry_at;
        if let Some(at) = retry_at
            && Instant::now() < at
        {
            let wait = at.saturating_duration_since(Instant::now()).as_millis().div_ceil(1000);
            return Err(format!("the app's resident instance failed and starts again in {wait} s"));
        }
        let wasm = (self.load)(&self.site, &self.app).ok_or_else(|| "the app has no handler".to_string())?;
        let instance = self
            .runtime
            .resident(self.site.clone(), &self.app, &wasm, self.settings.memory_bytes, guards)
            .map_err(|error| self.failed(&error, guards))?;
        self.live = Some(Live {
            instance,
            conns: HashSet::new(),
            since: Instant::now(),
        });
        if let Some(tick) = self.settings.tick {
            self.next_tick = Instant::now() + tick;
        }
        let mut status = self.status.lock().unwrap();
        status.running_since = Some(unix_now());
        status.next_start_at = None;
        tracing::info!(app = %self.app, "resident instance started");
        Ok(())
    }

    fn tick(&mut self) {
        let Some(tick) = self.settings.tick else {
            return;
        };
        let now = Instant::now();
        self.next_tick += tick;
        if self.next_tick < now {
            // Behind, after a slow event: skip what was missed.
            self.next_tick = now + tick;
        }
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let guards = Guards::default();
        match live.instance.tick(now_ms, guards) {
            Ok(()) => self.status.lock().unwrap().memory_bytes = live.instance.memory_bytes() as u64,
            Err(error) => {
                self.failed(&error, guards);
            }
        }
    }

    fn backoff(&self) -> MutexGuard<'_, Backoff> {
        self.backoff.lock().unwrap()
    }

    /// Drops the instance after a failure, closes what it knew and sets
    /// the pause before the next. Returns the reason.
    fn failed(&mut self, error: &anyhow::Error, guards: Guards) -> String {
        let reason = self.reason(error, guards);
        let lived = self.live.as_ref().map_or(Duration::ZERO, |live| live.since.elapsed());
        let (pause, retry_at) = {
            let mut backoff = self.backoff();
            backoff.pause = if backoff.pause.is_zero() || lived >= MAX_PAUSE {
                FIRST_PAUSE
            } else {
                (backoff.pause * 2).min(MAX_PAUSE)
            };
            let retry_at = Instant::now() + backoff.pause;
            backoff.retry_at = Some(retry_at);
            (backoff.pause, retry_at)
        };
        tracing::warn!(
            app = %self.app,
            reason = %reason,
            pause_ms = pause.as_millis() as u64,
            detail = %format!("{error:#}"),
            "resident instance dropped: it failed"
        );
        {
            let mut status = self.status.lock().unwrap();
            status.running_since = None;
            status.restarts += 1;
            status.last_crash = Some(reason.clone());
            status.last_crash_at = Some(unix_now());
            status.next_start_at = Some(unix_at(retry_at));
            status.memory_bytes = 0;
            status.connections = 0;
        }
        // Closed last, so whoever sees a connection close sees why.
        if let Some(live) = self.live.take() {
            self.close_all(&live);
        }
        reason
    }

    /// Why an instance failed, in words for a person reading the admin page:
    /// one line, of a bounded length. The full error, backtrace and all,
    /// goes to the log.
    fn reason(&self, error: &anyhow::Error, guards: Guards) -> String {
        one_line(&self.describe(error, guards), MAX_REASON_CHARS)
    }

    fn describe(&self, error: &anyhow::Error, guards: Guards) -> String {
        let detail = format!("{error:#}");
        match error.downcast_ref::<wasmtime::Trap>() {
            Some(wasmtime::Trap::Interrupt) => {
                format!("a call ran past its time limit of {} ms", guards.wall_clock.as_millis())
            }
            Some(wasmtime::Trap::OutOfFuel) => "a call used all of its fuel".to_string(),
            _ if detail.contains("memory limit:") => format!(
                "the instance reached its memory cap of {} MB",
                self.settings.memory_bytes as u64 / MIB
            ),
            Some(trap) => format!("the handler trapped: {trap}"),
            None => {
                // The first line names the failure; the backtrace is in the log.
                let first = detail.lines().next().unwrap_or_default();
                format!("the instance failed: {first}")
            }
        }
    }

    /// Closes the connections an instance knew. Each still gets its close
    /// event, which no instance runs, since none knows it.
    fn close_all(&self, live: &Live) {
        for conn in &live.conns {
            let _ = self.site.connections.close(&self.app, conn, None);
        }
    }
}

/// `text` as one line of at most `max` characters, control characters
/// turned to spaces, so nothing in it can break the page it is shown on
/// or pass for another line of a log.
fn one_line(text: &str, max: usize) -> String {
    let mut line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max + 1)
        .collect();
    if line.chars().count() > max {
        line = line.chars().take(max.saturating_sub(1)).collect();
        line.push('…');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_reason_is_one_bounded_line() {
        let long = format!("first\nsecond\r\u{1b}[31m{}", "x".repeat(1000));
        let line = one_line(&long, MAX_REASON_CHARS);
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert_eq!(line.chars().count(), MAX_REASON_CHARS);
        assert_eq!(one_line("short", MAX_REASON_CHARS), "short");
    }

    #[test]
    fn declared_numbers_outside_the_range_are_brought_inside_it() {
        let residents = Residents::new(128, 512);
        // A meta written by hand, or a ceiling lowered after a deploy.
        let settings = residents.settings(Some(u64::MAX), Some(1));
        assert_eq!(settings.memory_bytes as u64, 512 * MIB);
        assert_eq!(settings.tick, Some(Duration::from_millis(MIN_TICK_MS)));
        let settings = residents.settings(Some(0), Some(u64::MAX));
        assert_eq!(settings.memory_bytes as u64, MIB);
        assert_eq!(settings.tick, Some(Duration::from_millis(MAX_TICK_MS)));
    }

    #[test]
    fn instances_past_the_count_or_the_memory_budget_are_refused_and_their_places_come_back() {
        let mut residents = Residents::new(128, 512);
        residents.max_instances = 2;
        residents.total_memory_mb = 300;
        let first = residents.claim("a", 128 * MIB).unwrap();
        let second = residents.claim("b", 128 * MIB).unwrap();
        let error = residents.claim("c", MIB).err().unwrap();
        assert!(error.contains("TOOLSITE_RESIDENT_MAX"), "{error}");
        drop(second);
        let error = residents.claim("c", 256 * MIB).err().unwrap();
        assert!(error.contains("TOOLSITE_RESIDENT_TOTAL_MB"), "{error}");
        let third = residents.claim("c", 128 * MIB).unwrap();
        assert_eq!(residents.threads(), 2);
        drop((first, third));
        assert_eq!(residents.threads(), 0);
        assert_eq!(residents.running.lock().unwrap().reserved_bytes, 0);
    }
}
