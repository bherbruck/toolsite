//! The runner registry: one row per toolsite process on a database, kept
//! fresh by a heartbeat, so a runner can tell whether it is alone.
//!
//! Until app databases leave the volume, two live runners mean two writers
//! on one set of SQLite files. A runner that sees another refuses those
//! databases (`Stores::sqlite_refusal`) and says so at error, which turns a
//! silent split brain into a loud one. Later steps route work to a runner by
//! its `address`.

use deadpool_postgres::Pool;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// How often a runner says it is alive.
pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// How long after its last heartbeat a runner still counts as live.
pub const LIVE_FOR: Duration = Duration::from_secs(30);
/// Rows of runners gone this long are swept when another registers. They
/// are process records, not data: nothing reads a dead runner's row.
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);

pub struct Runner {
    /// Random per process: a restart is a new runner.
    pub id: String,
    /// What this runner serves. `all` until roles split.
    pub role: String,
    /// Where other runners reach this one, from `TOOLSITE_RUNNER_ADDRESS`.
    pub address: Option<String>,
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
    pub fn new(role: &str, address: Option<String>) -> Runner {
        Runner {
            id: crate::content::slug::random_token(12),
            role: role.to_string(),
            address,
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
                "insert into state.runners (id, role, address, started_at, heartbeat_at)
                 values ($1, $2, $3, $4, $4)
                 on conflict (id) do update set heartbeat_at = excluded.heartbeat_at",
                &[&self.id, &self.role, &self.address, &at],
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
