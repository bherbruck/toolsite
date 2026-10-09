//! Rate windows: how many times something may happen under one key in a
//! sliding window, counted for the whole site. An app's job starts are one
//! such key, so a loop that starts jobs is held to its rate however many
//! runners it reaches.
//!
//! File mode keeps each key's recent spends in this process's memory, as it
//! always did. Postgres mode keeps counts in `state.rate_windows`, one row per
//! sixtieth of the window, under an advisory lock per key so a check and its
//! spend are one step. A slice counts until the whole of it has left the
//! window, so the database errs towards refusing, never towards one more.

use async_trait::async_trait;
use deadpool_postgres::Pool;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Slices a window is counted in on Postgres.
const SLICES: i64 = 60;

#[async_trait]
pub trait RateStore: Send + Sync {
    /// Spends one under `key` if fewer than `per` were spent in the last
    /// `window` milliseconds; otherwise refuses, with how many were.
    async fn spend(&self, key: &str, per: usize, window: i64) -> Result<Result<(), usize>, String>;
}

#[derive(Clone)]
pub struct RateWindows {
    store: Arc<dyn RateStore>,
}

impl Default for RateWindows {
    fn default() -> Self {
        RateWindows::memory()
    }
}

impl RateWindows {
    pub fn memory() -> RateWindows {
        RateWindows { store: Arc::new(Memory::default()) }
    }

    pub fn postgres(pool: Pool) -> RateWindows {
        RateWindows { store: Arc::new(Postgres { pool }) }
    }

    /// Spends one under `key`, or answers how many were spent in the window.
    pub async fn spend(&self, key: &str, per: usize, window: Duration) -> Result<Result<(), usize>, String> {
        let window = i64::try_from(window.as_millis()).unwrap_or(i64::MAX).max(1);
        self.store.spend(key, per, window).await
    }
}

// --- memory -----------------------------------------------------------------

/// Every spend in the window, per key: exact, and what file mode always kept.
#[derive(Default)]
pub struct Memory {
    spent: Mutex<HashMap<String, VecDeque<Instant>>>,
}

#[async_trait]
impl RateStore for Memory {
    async fn spend(&self, key: &str, per: usize, window: i64) -> Result<Result<(), usize>, String> {
        let mut spent = self.spent.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let recent = spent.entry(key.to_string()).or_default();
        let since = Instant::now().checked_sub(Duration::from_millis(window as u64));
        while recent.front().is_some_and(|at| since.is_some_and(|since| *at < since)) {
            recent.pop_front();
        }
        if recent.len() >= per {
            return Ok(Err(recent.len()));
        }
        recent.push_back(Instant::now());
        Ok(Ok(()))
    }
}

// --- Postgres ---------------------------------------------------------------

pub struct Postgres {
    pool: Pool,
}

fn failed(what: &str) -> impl Fn(tokio_postgres::Error) -> String + '_ {
    move |e| format!("could not {what}: {}", super::pg::chain(&e))
}

#[async_trait]
impl RateStore for Postgres {
    async fn spend(&self, key: &str, per: usize, window: i64) -> Result<Result<(), usize>, String> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| format!("could not reach Postgres for rates: {}", super::pg::chain(&e)))?;
        let transaction = client.transaction().await.map_err(failed("begin counting a rate"))?;
        transaction
            .execute("select pg_advisory_xact_lock($1::int4, hashtext($2))", &[&crate::state::pg::LOCK_RATES, &key])
            .await
            .map_err(failed("hold a rate"))?;
        let now: i64 = transaction
            .query_one("select (extract(epoch from clock_timestamp()) * 1000)::bigint", &[])
            .await
            .map_err(failed("read the database's clock"))?
            .get(0);
        let slice = (window / SLICES).max(1);
        let since = now - window;
        transaction
            .execute("delete from state.rate_windows where key = $1 and window_start + $2 <= $3", &[&key, &slice, &since])
            .await
            .map_err(failed("forget an old rate"))?;
        let spent: i64 = transaction
            .query_one("select coalesce(sum(count), 0)::bigint from state.rate_windows where key = $1", &[&key])
            .await
            .map_err(failed("count a rate"))?
            .get(0);
        if spent as usize >= per {
            transaction.commit().await.map_err(failed("commit a rate"))?;
            return Ok(Err(spent as usize));
        }
        transaction
            .execute(
                "insert into state.rate_windows (key, window_start, count) values ($1, $2, 1)
                 on conflict (key, window_start) do update set count = state.rate_windows.count + 1",
                &[&key, &(now - now.rem_euclid(slice))],
            )
            .await
            .map_err(failed("spend a rate"))?;
        transaction.commit().await.map_err(failed("commit a rate"))?;
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::store::conformance::{drop_postgres_database, postgres_database};

    /// `a` and `b` are two runners' handles on one store.
    async fn conformance(a: RateWindows, b: RateWindows) {
        let minute = Duration::from_secs(60);
        assert_eq!(a.spend("job-starts:busy", 3, minute).await.unwrap(), Ok(()));
        assert_eq!(b.spend("job-starts:busy", 3, minute).await.unwrap(), Ok(()));
        assert_eq!(a.spend("job-starts:busy", 3, minute).await.unwrap(), Ok(()));
        assert_eq!(b.spend("job-starts:busy", 3, minute).await.unwrap(), Err(3), "a fourth start in the minute");
        assert_eq!(a.spend("job-starts:busy", 3, minute).await.unwrap(), Err(3));
        // Keys are apart, even one that only starts the same.
        assert_eq!(a.spend("job-starts:busy/sub", 3, minute).await.unwrap(), Ok(()));
        assert_eq!(a.spend("job-starts:quiet", 3, minute).await.unwrap(), Ok(()));

        // Spends leave the window as it slides.
        let short = Duration::from_millis(600);
        for _ in 0..2 {
            assert_eq!(a.spend("short", 2, short).await.unwrap(), Ok(()));
        }
        assert_eq!(b.spend("short", 2, short).await.unwrap(), Err(2));
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(b.spend("short", 2, short).await.unwrap(), Ok(()));
    }

    /// Many spends at once on two runners: exactly the rate goes through.
    async fn spends_at_once_hold_the_rate(a: RateWindows, b: RateWindows) {
        let tasks: Vec<_> = (0..40)
            .map(|n| {
                let rates = if n % 2 == 0 { a.clone() } else { b.clone() };
                tokio::spawn(async move { rates.spend("burst", 10, Duration::from_secs(60)).await.unwrap() })
            })
            .collect();
        let mut through = 0;
        for task in tasks {
            if task.await.unwrap().is_ok() {
                through += 1;
            }
        }
        assert_eq!(through, 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_memory_rates_conform() {
        let rates = RateWindows::memory();
        conformance(rates.clone(), rates.clone()).await;
        spends_at_once_hold_the_rate(rates.clone(), rates).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
    async fn the_postgres_rates_conform() {
        let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
        conformance(RateWindows::postgres(pool.clone()), RateWindows::postgres(pool.clone())).await;
        spends_at_once_hold_the_rate(RateWindows::postgres(pool.clone()), RateWindows::postgres(pool.clone())).await;
        std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
    }
}
