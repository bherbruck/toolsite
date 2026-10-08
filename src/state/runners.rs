//! The runner registry: one row per toolsite process on a database, kept
//! fresh by a heartbeat, so a runner can tell whether it is alone.
//!
//! Until app databases leave the volume, two live runners mean two writers
//! on one set of SQLite files. A runner that sees another refuses those
//! databases (`Stores::sqlite_refusal`) and says so at error, which turns a
//! silent split brain into a loud one. The row also says where the runner is
//! reachable and what it serves, which an edge reads later to route work.

use deadpool_postgres::Pool;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// How often a runner says it is alive.
pub const HEARTBEAT: Duration = Duration::from_secs(5);
/// How long after its last heartbeat a runner still counts as live, for
/// the SQLite guard: generous, since a false "alone" is the costly mistake.
pub const LIVE_FOR: Duration = Duration::from_secs(30);
/// Rows of runners gone this long are swept when another registers. They
/// are process records, not data: nothing reads a dead runner's row.
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);

/// The pool a runner is in when `TOOLSITE_POOL` does not say.
pub const DEFAULT_POOL: &str = "default";
/// The port other runners reach this one on when `TOOLSITE_INTERNAL_PORT`
/// does not say.
pub const DEFAULT_INTERNAL_PORT: u16 = 8081;

pub struct Runner {
    /// Random per process: a restart is a new runner.
    pub id: String,
    /// What this runner serves: `control` and `worker` (`all`) until role
    /// sets arrive.
    pub roles: Vec<String>,
    /// The worker pool it belongs to, from `TOOLSITE_POOL`.
    pub pool: String,
    /// Where other runners reach this one, from `TOOLSITE_RUNNER_ADDRESS`.
    pub address: Option<String>,
    /// The port they reach it on, from `TOOLSITE_INTERNAL_PORT`.
    pub internal_port: u16,
    /// Other live runners at the last heartbeat.
    peers: Mutex<Vec<String>>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl Runner {
    pub fn new(roles: &[&str], pool: &str, address: Option<String>, internal_port: u16) -> Runner {
        Runner {
            id: crate::content::slug::random_token(12),
            roles: roles.iter().map(|role| role.to_string()).collect(),
            pool: pool.to_string(),
            address,
            internal_port,
            peers: Mutex::new(Vec::new()),
        }
    }

    /// Other runners live at the last heartbeat, by id.
    pub fn peers(&self) -> Vec<String> {
        self.peers.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Writes this runner's row, or refreshes it, and looks for others.
    /// Returns the other live runners. A row someone swept is written again.
    pub async fn beat(&self, pool: &Pool) -> Result<Vec<String>, String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        let at = now();
        client
            .execute(
                "insert into state.runners (id, roles, pool, address, internal_port, started_at, heartbeat_at)
                 values ($1, $2, $3, $4, $5, $6, $6)
                 on conflict (id) do update set heartbeat_at = excluded.heartbeat_at",
                &[&self.id, &self.roles, &self.pool, &self.address, &(self.internal_port as i32), &at],
            )
            .await
            .map_err(|e| format!("could not write this runner's heartbeat: {e}"))?;
        let rows = client
            .query(
                "select id from state.runners where id <> $1 and heartbeat_at > $2 order by id",
                &[&self.id, &(at - LIVE_FOR.as_secs() as i64)],
            )
            .await
            .map_err(|e| format!("could not read the runner registry: {e}"))?;
        let peers: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        *self.peers.lock().unwrap_or_else(|e| e.into_inner()) = peers.clone();
        Ok(peers)
    }

    /// The first heartbeat, after sweeping rows of runners long gone.
    pub async fn register(&self, pool: &Pool) -> Result<Vec<String>, String> {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        client
            .execute(
                "delete from state.runners where heartbeat_at < $1",
                &[&(now() - FORGET_AFTER.as_secs() as i64)],
            )
            .await
            .map_err(|e| format!("could not sweep the runner registry: {e}"))?;
        drop(client);
        let peers = self.beat(pool).await?;
        report(&self.id, &[], &peers);
        Ok(peers)
    }

    /// Takes this runner's row out, so a restart is not mistaken for a
    /// second runner while the old row is still fresh.
    pub async fn leave(&self, pool: &Pool) {
        let gone = async {
            let client = pool.get().await.map_err(|e| e.to_string())?;
            client
                .execute("delete from state.runners where id = $1", &[&self.id])
                .await
                .map_err(|e| e.to_string())
        };
        match tokio::time::timeout(Duration::from_secs(3), gone).await {
            Ok(Ok(_)) => tracing::info!(runner = %self.id, "left the runner registry"),
            Ok(Err(why)) => tracing::warn!(runner = %self.id, %why, "could not leave the runner registry"),
            Err(_) => tracing::warn!(runner = %self.id, "timed out leaving the runner registry"),
        }
    }

    /// Beats every `HEARTBEAT` for as long as the process runs, logging
    /// each time another runner appears or goes.
    pub fn spawn(self: Arc<Self>, pool: Pool) {
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(HEARTBEAT);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticks.tick().await;
            loop {
                ticks.tick().await;
                let before = self.peers();
                match self.beat(&pool).await {
                    Ok(peers) => report(&self.id, &before, &peers),
                    Err(why) => tracing::warn!(runner = %self.id, %why, "heartbeat failed"),
                }
            }
        });
    }

    /// On SIGTERM or Ctrl-C, leaves the registry and exits. A container
    /// stop then frees the row at once rather than after `LIVE_FOR`.
    pub fn leave_on_shutdown(self: Arc<Self>, pool: Pool) {
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) else {
                    return;
                };
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            self.leave(&pool).await;
            std::process::exit(0);
        });
    }
}

fn report(me: &str, before: &[String], now: &[String]) {
    if now.is_empty() {
        if !before.is_empty() {
            tracing::info!(runner = %me, "alone on the database again: app databases are served");
        }
        return;
    }
    if before != now {
        tracing::error!(
            runner = %me,
            others = %now.join(", "),
            "another toolsite runner is live on this database; app databases are SQLite files on one \
             volume, so this runner refuses them until it is alone"
        );
    }
}
