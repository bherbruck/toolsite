//! Leases: a name one holder has for a while, and must renew to keep. A job
//! run holds its job's lease, so a job runs once at a time however many
//! runners could start it, and a runner that dies mid-run lets the name go
//! when the lease runs out rather than holding it for ever.
//!
//! Every new holder gets the next `epoch`, a fencing token: a holder that
//! paused past its lease and finds the name taken over cannot renew it,
//! release it or act on a request made of the new holder, since each of
//! those names the epoch it was given.
//!
//! A lease may belong to a group with a ceiling (an app, for its jobs), and
//! is then refused while that many of the group's are live. And a live lease
//! can be asked to go again: the holder, finishing, either finds the request
//! and keeps the lease for one more turn, or finds none and lets it go, in
//! one step, so a request is never lost between the two. A request left on
//! a lease whose holder died is an orphan, for another runner to take over.
//!
//! File mode keeps leases in this process's memory, as job slots always
//! were. Postgres mode keeps them in `state.leases`, timed by the database's
//! clock so runners whose clocks disagree still agree on who holds what.

use async_trait::async_trait;
use deadpool_postgres::Pool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

/// One holding of a name. The epoch is what proves it is still this one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub name: String,
    pub epoch: i64,
}

/// What asking for a lease came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken {
    Lease(Lease),
    /// Someone holds the name now.
    Held,
    /// The group already holds as many live leases as it may: this many.
    Full(usize),
}

/// How a holder's turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Someone asked for one more turn: the lease is kept, renewed, and the
    /// request spent.
    Again,
    /// Nobody asked: the lease is given up.
    Released,
    /// The lease is no longer this holder's: it ran out and was taken over.
    Lost,
}

/// Where leases are kept. Times are milliseconds; `ttl` is how long from
/// now a lease lasts without renewal.
#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// Takes `name` for `holder`, unless it is held live, or `group` names a
    /// group that holds its ceiling of live leases. A lease that ran out is
    /// taken over, with the next epoch and no request carried across.
    async fn acquire(&self, name: &str, group: Option<(&str, usize)>, holder: &str, ttl: i64) -> Result<Taken, String>;
    /// Extends a lease from now. False when it is no longer this holding.
    async fn renew(&self, lease: &Lease, ttl: i64) -> Result<bool, String>;
    /// Gives a lease up, dropping any request on it. False when it is no
    /// longer this holding: an old holder's release is refused.
    async fn release(&self, lease: &Lease) -> Result<bool, String>;
    /// A live lease's request flag: `Some(asked)` while it is held, `None`
    /// when nobody holds the name.
    async fn state(&self, name: &str) -> Result<Option<bool>, String>;
    /// `state`, for synchronous callers.
    fn state_blocking(&self, name: &str) -> Result<Option<bool>, String>;
    /// Asks the live holder of `name` for one more turn. Answers whether it
    /// had been asked already, or `None` when nobody holds the name, so a
    /// request is only ever made of a holder who will see it.
    async fn ask_again(&self, name: &str) -> Result<Option<bool>, String>;
    /// The holder's end of a turn: keeps the lease for one more if it was
    /// asked, else gives it up, in one step.
    async fn again_or_release(&self, lease: &Lease, ttl: i64) -> Result<Ended, String>;
    /// Names of a group's live leases.
    async fn live(&self, group: &str) -> Result<Vec<String>, String>;
    /// Leases under `prefix` that ran out while asked to go again, as (name,
    /// group): their holders are gone and the request is still owed.
    async fn orphans(&self, prefix: &str) -> Result<Vec<(String, Option<String>)>, String>;
}

/// The handle every layer takes leases through: the store, and who this
/// process is to it.
#[derive(Clone)]
pub struct Leases {
    store: Arc<dyn LeaseStore>,
    holder: Arc<str>,
}

impl Default for Leases {
    fn default() -> Self {
        Leases::memory()
    }
}

fn millis(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX).max(1)
}

impl Leases {
    /// This process's memory.
    pub fn memory() -> Leases {
        Leases::with_store(Arc::new(Memory::default()), &crate::content::slug::random_token(12))
    }

    /// Shared through Postgres, held in this process's name.
    pub fn postgres(pool: Pool, holder: &str) -> Leases {
        Leases::with_store(Arc::new(Postgres { pool }), holder)
    }

    pub fn with_store(store: Arc<dyn LeaseStore>, holder: &str) -> Leases {
        Leases { store, holder: holder.into() }
    }

    pub async fn acquire(&self, name: &str, group: Option<(&str, usize)>, ttl: Duration) -> Result<Taken, String> {
        self.store.acquire(name, group, &self.holder, millis(ttl)).await
    }

    pub async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<bool, String> {
        self.store.renew(lease, millis(ttl)).await
    }

    pub async fn release(&self, lease: &Lease) -> Result<bool, String> {
        self.store.release(lease).await
    }

    pub async fn state(&self, name: &str) -> Result<Option<bool>, String> {
        self.store.state(name).await
    }

    pub fn state_blocking(&self, name: &str) -> Result<Option<bool>, String> {
        self.store.state_blocking(name)
    }

    pub async fn ask_again(&self, name: &str) -> Result<Option<bool>, String> {
        self.store.ask_again(name).await
    }

    pub async fn again_or_release(&self, lease: &Lease, ttl: Duration) -> Result<Ended, String> {
        self.store.again_or_release(lease, millis(ttl)).await
    }

    pub async fn live(&self, group: &str) -> Result<Vec<String>, String> {
        self.store.live(group).await
    }

    pub async fn orphans(&self, prefix: &str) -> Result<Vec<(String, Option<String>)>, String> {
        self.store.orphans(prefix).await
    }
}

// --- memory -----------------------------------------------------------------

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

struct Row {
    group: Option<String>,
    epoch: i64,
    expires_at: i64,
    again: bool,
}

/// File mode's store: one map behind one lock, so every method is a whole
/// step. A released lease keeps its row, so the next holder's epoch still
/// grows; rows are one per name ever leased, which for jobs is bounded by
/// the jobs declared.
#[derive(Default)]
pub struct Memory {
    rows: Mutex<HashMap<String, Row>>,
}

impl Memory {
    fn rows(&self) -> std::sync::MutexGuard<'_, HashMap<String, Row>> {
        self.rows.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn state_now(&self, name: &str) -> Option<bool> {
        let now = now_ms();
        self.rows().get(name).filter(|row| row.expires_at > now).map(|row| row.again)
    }
}

#[async_trait]
impl LeaseStore for Memory {
    async fn acquire(&self, name: &str, group: Option<(&str, usize)>, _holder: &str, ttl: i64) -> Result<Taken, String> {
        let now = now_ms();
        let mut rows = self.rows();
        if rows.get(name).is_some_and(|row| row.expires_at > now) {
            return Ok(Taken::Held);
        }
        if let Some((group, most)) = group {
            let live = rows
                .iter()
                .filter(|(other, row)| *other != name && row.group.as_deref() == Some(group) && row.expires_at > now)
                .count();
            if live >= most {
                return Ok(Taken::Full(live));
            }
        }
        let epoch = rows.get(name).map_or(1, |row| row.epoch + 1);
        rows.insert(
            name.to_string(),
            Row { group: group.map(|(group, _)| group.to_string()), epoch, expires_at: now.saturating_add(ttl), again: false },
        );
        Ok(Taken::Lease(Lease { name: name.to_string(), epoch }))
    }

    async fn renew(&self, lease: &Lease, ttl: i64) -> Result<bool, String> {
        let mut rows = self.rows();
        let Some(row) = rows.get_mut(&lease.name).filter(|row| row.epoch == lease.epoch) else {
            return Ok(false);
        };
        row.expires_at = now_ms().saturating_add(ttl);
        Ok(true)
    }

    async fn release(&self, lease: &Lease) -> Result<bool, String> {
        let mut rows = self.rows();
        let Some(row) = rows.get_mut(&lease.name).filter(|row| row.epoch == lease.epoch) else {
            return Ok(false);
        };
        row.expires_at = 0;
        row.again = false;
        Ok(true)
    }

    async fn state(&self, name: &str) -> Result<Option<bool>, String> {
        Ok(self.state_now(name))
    }

    fn state_blocking(&self, name: &str) -> Result<Option<bool>, String> {
        Ok(self.state_now(name))
    }

    async fn ask_again(&self, name: &str) -> Result<Option<bool>, String> {
        let now = now_ms();
        let mut rows = self.rows();
        let Some(row) = rows.get_mut(name).filter(|row| row.expires_at > now) else {
            return Ok(None);
        };
        Ok(Some(std::mem::replace(&mut row.again, true)))
    }

    async fn again_or_release(&self, lease: &Lease, ttl: i64) -> Result<Ended, String> {
        let mut rows = self.rows();
        let Some(row) = rows.get_mut(&lease.name).filter(|row| row.epoch == lease.epoch) else {
            return Ok(Ended::Lost);
        };
        if std::mem::replace(&mut row.again, false) {
            row.expires_at = now_ms().saturating_add(ttl);
            Ok(Ended::Again)
        } else {
            row.expires_at = 0;
            Ok(Ended::Released)
        }
    }

    async fn live(&self, group: &str) -> Result<Vec<String>, String> {
        let now = now_ms();
        let mut names: Vec<String> = self
            .rows()
            .iter()
            .filter(|(_, row)| row.group.as_deref() == Some(group) && row.expires_at > now)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        Ok(names)
    }

    async fn orphans(&self, prefix: &str) -> Result<Vec<(String, Option<String>)>, String> {
        let now = now_ms();
        let mut found: Vec<(String, Option<String>)> = self
            .rows()
            .iter()
            .filter(|(name, row)| name.starts_with(prefix) && row.again && row.expires_at <= now)
            .map(|(name, row)| (name.clone(), row.group.clone()))
            .collect();
        found.sort();
        Ok(found)
    }
}

// --- Postgres ---------------------------------------------------------------

/// Postgres mode's store: `state.leases`. Taking a lease holds an advisory
/// lock on its group (or on the name, without one) while the live leases
/// are counted, so two runners cannot both take a group's last place; every
/// other change is one statement on the row. "Now" is always the database's.
pub struct Postgres {
    pool: Pool,
}

impl Postgres {
    async fn client(&self) -> Result<deadpool_postgres::Client, String> {
        self.pool
            .get()
            .await
            .map_err(|e| format!("could not reach Postgres for leases: {}", super::pg::chain(&e)))
    }
}

fn failed(what: &str) -> impl Fn(tokio_postgres::Error) -> String + '_ {
    move |e| format!("could not {what}: {}", super::pg::chain(&e))
}

#[async_trait]
impl LeaseStore for Postgres {
    async fn acquire(&self, name: &str, group: Option<(&str, usize)>, holder: &str, ttl: i64) -> Result<Taken, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin taking a lease"))?;
        let turn = group.map_or(name, |(group, _)| group);
        transaction
            .execute("select pg_advisory_xact_lock($1::int4, hashtext($2))", &[&crate::state::pg::LOCK_LEASES, &turn])
            .await
            .map_err(failed("hold a lease's group"))?;
        let held = transaction
            .query_opt(
                "select 1 from state.leases
                  where name = $1 and expires_at > (extract(epoch from clock_timestamp()) * 1000)::bigint
                  for update",
                &[&name],
            )
            .await
            .map_err(failed("read a lease"))?;
        if held.is_some() {
            return Ok(Taken::Held);
        }
        if let Some((group, most)) = group {
            let live: i64 = transaction
                .query_one(
                    "select count(*) from state.leases
                      where grp = $1 and name <> $2 and expires_at > (extract(epoch from clock_timestamp()) * 1000)::bigint",
                    &[&group, &name],
                )
                .await
                .map_err(failed("count a group's leases"))?
                .get(0);
            if live as usize >= most {
                return Ok(Taken::Full(live as usize));
            }
        }
        let group = group.map(|(group, _)| group);
        let epoch: i64 = transaction
            .query_one(
                "insert into state.leases (name, grp, holder, epoch, expires_at, again)
                 values ($1, $2, $3, 1, (extract(epoch from clock_timestamp()) * 1000)::bigint + $4, false)
                 on conflict (name) do update
                    set grp = excluded.grp, holder = excluded.holder, epoch = state.leases.epoch + 1,
                        expires_at = excluded.expires_at, again = false
                 returning epoch",
                &[&name, &group, &holder, &ttl],
            )
            .await
            .map_err(failed("take a lease"))?
            .get(0);
        transaction.commit().await.map_err(failed("commit a lease"))?;
        Ok(Taken::Lease(Lease { name: name.to_string(), epoch }))
    }

    async fn renew(&self, lease: &Lease, ttl: i64) -> Result<bool, String> {
        let renewed = self
            .client()
            .await?
            .execute(
                "update state.leases set expires_at = (extract(epoch from clock_timestamp()) * 1000)::bigint + $3
                  where name = $1 and epoch = $2",
                &[&lease.name, &lease.epoch, &ttl],
            )
            .await
            .map_err(failed("renew a lease"))?;
        Ok(renewed > 0)
    }

    async fn release(&self, lease: &Lease) -> Result<bool, String> {
        let released = self
            .client()
            .await?
            .execute(
                "update state.leases set expires_at = 0, again = false where name = $1 and epoch = $2",
                &[&lease.name, &lease.epoch],
            )
            .await
            .map_err(failed("release a lease"))?;
        Ok(released > 0)
    }

    async fn state(&self, name: &str) -> Result<Option<bool>, String> {
        let row = self
            .client()
            .await?
            .query_opt(
                "select again from state.leases
                  where name = $1 and expires_at > (extract(epoch from clock_timestamp()) * 1000)::bigint",
                &[&name],
            )
            .await
            .map_err(failed("read a lease"))?;
        Ok(row.map(|row| row.get(0)))
    }

    fn state_blocking(&self, name: &str) -> Result<Option<bool>, String> {
        super::wait_in_place(self.state(name))
    }

    async fn ask_again(&self, name: &str) -> Result<Option<bool>, String> {
        // The old flag is read under the row's lock in the same statement,
        // so of two asks at once exactly one learns it was the first.
        let row = self
            .client()
            .await?
            .query_opt(
                "with old as (
                     select again from state.leases
                      where name = $1 and expires_at > (extract(epoch from clock_timestamp()) * 1000)::bigint
                      for update
                 )
                 update state.leases l set again = true from old where l.name = $1
                 returning old.again",
                &[&name],
            )
            .await
            .map_err(failed("ask a lease's holder again"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn again_or_release(&self, lease: &Lease, ttl: i64) -> Result<Ended, String> {
        let row = self
            .client()
            .await?
            .query_opt(
                "with old as (select again from state.leases where name = $1 and epoch = $2 for update)
                 update state.leases l
                    set again = false,
                        expires_at = case when old.again
                                          then (extract(epoch from clock_timestamp()) * 1000)::bigint + $3
                                          else 0 end
                   from old
                  where l.name = $1
                 returning old.again",
                &[&lease.name, &lease.epoch, &ttl],
            )
            .await
            .map_err(failed("end a lease's turn"))?;
        Ok(match row.map(|row| row.get::<_, bool>(0)) {
            Some(true) => Ended::Again,
            Some(false) => Ended::Released,
            None => Ended::Lost,
        })
    }

    async fn live(&self, group: &str) -> Result<Vec<String>, String> {
        let rows = self
            .client()
            .await?
            .query(
                "select name from state.leases
                  where grp = $1 and expires_at > (extract(epoch from clock_timestamp()) * 1000)::bigint",
                &[&group],
            )
            .await
            .map_err(failed("list a group's leases"))?;
        let mut names: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        names.sort();
        Ok(names)
    }

    async fn orphans(&self, prefix: &str) -> Result<Vec<(String, Option<String>)>, String> {
        let rows = self
            .client()
            .await?
            .query(
                "select name, grp from state.leases
                  where again and starts_with(name, $1)
                    and expires_at <= (extract(epoch from clock_timestamp()) * 1000)::bigint",
                &[&prefix],
            )
            .await
            .map_err(failed("find orphaned leases"))?;
        let mut found: Vec<(String, Option<String>)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        found.sort();
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::store::conformance::{drop_postgres_database, postgres_database};

    const LONG: Duration = Duration::from_secs(60);

    fn lease(taken: Taken) -> Lease {
        match taken {
            Taken::Lease(lease) => lease,
            other => panic!("no lease: {other:?}"),
        }
    }

    /// What both stores must do. `a` and `b` are two runners on one store.
    async fn conformance(a: Leases, b: Leases) {
        // One holder at a time, across runners.
        let first = lease(a.acquire("job:app/work", None, LONG).await.unwrap());
        assert_eq!(b.acquire("job:app/work", None, LONG).await.unwrap(), Taken::Held);
        assert_eq!(b.state("job:app/work").await.unwrap(), Some(false));
        assert_eq!(b.state_blocking("job:app/other").unwrap(), None);

        // A request for one more turn: the first asker learns it was first,
        // and the holder keeps the lease once, then lets it go.
        assert_eq!(b.ask_again("job:app/work").await.unwrap(), Some(false));
        assert_eq!(a.ask_again("job:app/work").await.unwrap(), Some(true));
        assert_eq!(a.again_or_release(&first, LONG).await.unwrap(), Ended::Again);
        assert_eq!(b.state("job:app/work").await.unwrap(), Some(false), "the request was not spent");
        assert_eq!(a.again_or_release(&first, LONG).await.unwrap(), Ended::Released);
        assert_eq!(b.state("job:app/work").await.unwrap(), None);
        assert_eq!(b.ask_again("job:app/work").await.unwrap(), None, "a request was made of nobody");

        // The next holder has the next epoch, and the old one can do
        // nothing with its lease any more.
        let second = lease(b.acquire("job:app/work", None, LONG).await.unwrap());
        assert!(second.epoch > first.epoch, "{second:?} after {first:?}");
        assert!(!a.renew(&first, LONG).await.unwrap(), "an old holder renewed");
        assert!(!a.release(&first).await.unwrap(), "an old holder released the new holder's lease");
        assert_eq!(a.again_or_release(&first, LONG).await.unwrap(), Ended::Lost);
        assert_eq!(a.state("job:app/work").await.unwrap(), Some(false), "the new holder lost its lease");
        assert!(b.renew(&second, LONG).await.unwrap());
        assert!(b.release(&second).await.unwrap());

        // A group's ceiling holds across runners, and a name that only
        // starts the same is another group.
        let one = lease(a.acquire("job:busy/one", Some(("busy", 2)), LONG).await.unwrap());
        lease(b.acquire("job:busy/two", Some(("busy", 2)), LONG).await.unwrap());
        assert_eq!(a.acquire("job:busy/three", Some(("busy", 2)), LONG).await.unwrap(), Taken::Full(2));
        assert_eq!(b.acquire("job:busy/three", Some(("busy", 2)), LONG).await.unwrap(), Taken::Full(2));
        lease(b.acquire("job:busy/sub/one", Some(("busy/sub", 2)), LONG).await.unwrap());
        assert_eq!(a.live("busy").await.unwrap(), ["job:busy/one", "job:busy/two"]);
        assert!(a.release(&one).await.unwrap());
        lease(b.acquire("job:busy/three", Some(("busy", 2)), LONG).await.unwrap());

        // A holder that stops renewing loses the name when its time is up,
        // and a request left on it is an orphan until someone takes it over.
        let dying = lease(a.acquire("job:app/nap", Some(("app", 4)), Duration::from_millis(300)).await.unwrap());
        assert_eq!(b.ask_again("job:app/nap").await.unwrap(), Some(false));
        assert!(b.orphans("job:").await.unwrap().is_empty(), "a live lease counted as an orphan");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(b.state("job:app/nap").await.unwrap(), None);
        assert_eq!(b.orphans("job:").await.unwrap(), vec![("job:app/nap".to_string(), Some("app".to_string()))]);
        assert!(b.orphans("other:").await.unwrap().is_empty());
        let taken = lease(b.acquire("job:app/nap", Some(("app", 4)), LONG).await.unwrap());
        assert!(taken.epoch > dying.epoch);
        assert!(b.orphans("job:").await.unwrap().is_empty(), "a request survived the takeover");
        assert_eq!(a.again_or_release(&dying, LONG).await.unwrap(), Ended::Lost, "the dead holder came back and went again");
        assert_eq!(b.again_or_release(&taken, LONG).await.unwrap(), Ended::Released);
    }

    /// Many runners after one name at once: exactly one holds it.
    async fn one_of_many_wins(leases: Vec<Leases>) {
        let tasks: Vec<_> = leases
            .into_iter()
            .map(|leases| tokio::spawn(async move { leases.acquire("job:race/x", Some(("race", 1)), LONG).await.unwrap() }))
            .collect();
        let mut won = 0;
        for task in tasks {
            if matches!(task.await.unwrap(), Taken::Lease(_)) {
                won += 1;
            }
        }
        assert_eq!(won, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_memory_leases_conform() {
        let store: Arc<dyn LeaseStore> = Arc::new(Memory::default());
        conformance(Leases::with_store(store.clone(), "a"), Leases::with_store(store.clone(), "b")).await;
        one_of_many_wins((0..16).map(|n| Leases::with_store(store.clone(), &n.to_string())).collect()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
    async fn the_postgres_leases_conform() {
        let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
        conformance(Leases::postgres(pool.clone(), "a"), Leases::postgres(pool.clone(), "b")).await;
        one_of_many_wins((0..16).map(|n| Leases::postgres(pool.clone(), &n.to_string())).collect()).await;
        std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
    }
}
