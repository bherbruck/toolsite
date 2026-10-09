//! Tokens on Postgres: a row per token in `platform.app_tokens`, keyed by
//! app, kind and id, with the digest unique within a kind.
//!
//! A mint is one insert and a revocation one delete, so neither can lose
//! the other's work. A check reads the app's digests of that kind and
//! compares each in constant time, then records use with one conditional
//! update that touches only that row and only when `last_used` is older than
//! the kind's resolution. An update that finds no row means the token was
//! revoked since it was read, and the check refuses it. A removal moves an
//! app's tokens into `platform.removed_records`, one record per kind in the
//! shape its sidecar has on files.

use super::{same, Kind, Token, Tokens};
use async_trait::async_trait;
use deadpool_postgres::Pool;

pub struct Postgres {
    pool: Pool,
}

/// A token as its sidecar stores it on files, field for field, for the
/// trash to keep what a removal took.
#[derive(serde::Serialize)]
struct Entry {
    id: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used: Option<u64>,
    created_at: u64,
    hash: String,
}

fn failed(what: &str) -> impl Fn(tokio_postgres::Error) -> String + '_ {
    move |e| format!("could not {what}: {}", crate::state::pg::chain(&e))
}

fn token_of(row: &tokio_postgres::Row) -> Token {
    Token {
        id: row.get("id"),
        label: row.get("label"),
        last_used: row.get::<_, Option<i64>>("last_used").map(|at| at as u64),
        created_at: row.get::<_, i64>("created_at") as u64,
    }
}

impl Postgres {
    pub fn new(pool: Pool) -> Postgres {
        Postgres { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, String> {
        self.pool
            .get()
            .await
            .map_err(|e| format!("could not reach Postgres for tokens: {}", crate::state::pg::chain(&e)))
    }

    async fn retire(&self, app: &str, at: u64) -> Result<Vec<(&'static str, String)>, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin a removal"))?;
        let mut out = Vec::new();
        for kind in Kind::ALL {
            let rows = transaction
                .query(
                    "delete from platform.app_tokens where app = $1 and kind = $2
                     returning id, label, last_used, created_at, hash",
                    &[&app, &kind.name()],
                )
                .await
                .map_err(failed("take an app's tokens"))?;
            if rows.is_empty() {
                continue;
            }
            let mut list: Vec<Entry> = rows
                .iter()
                .map(|row| {
                    let token = token_of(row);
                    Entry {
                        id: token.id,
                        label: token.label,
                        last_used: token.last_used,
                        created_at: token.created_at,
                        hash: row.get("hash"),
                    }
                })
                .collect();
            list.sort_by_key(|entry| entry.created_at);
            let text = crate::platform::records::pretty(&list)?;
            transaction
                .execute(
                    "insert into platform.removed_records (app, kind, record, removed_at) values ($1, $2, $3::text::json, $4)",
                    &[&app, &kind.extension(), &text, &(at as i64)],
                )
                .await
                .map_err(failed("keep an app's removed tokens"))?;
            out.push((kind.extension(), text));
        }
        transaction.commit().await.map_err(failed("commit a removal"))?;
        Ok(out)
    }
}

#[async_trait]
impl Tokens for Postgres {
    async fn insert(&self, app: &str, kind: Kind, token: &Token, hash: &str) -> Result<(), String> {
        self.client()
            .await?
            .execute(
                "insert into platform.app_tokens (app, kind, id, label, hash, created_at, last_used)
                 values ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &app,
                    &kind.name(),
                    &token.id,
                    &token.label,
                    &hash,
                    &(token.created_at as i64),
                    &token.last_used.map(|at| at as i64),
                ],
            )
            .await
            .map_err(failed("store a token"))?;
        Ok(())
    }

    async fn list(&self, app: &str, kind: Kind) -> Result<Vec<Token>, String> {
        let rows = self
            .client()
            .await?
            .query(
                "select id, label, last_used, created_at from platform.app_tokens
                  where app = $1 and kind = $2 order by created_at, id",
                &[&app, &kind.name()],
            )
            .await
            .map_err(failed("list tokens"))?;
        Ok(rows.iter().map(token_of).collect())
    }

    async fn list_all(&self, kind: Kind) -> Result<Vec<(String, Token)>, String> {
        let rows = self
            .client()
            .await?
            .query(
                "select app, id, label, last_used, created_at from platform.app_tokens where kind = $1",
                &[&kind.name()],
            )
            .await
            .map_err(failed("list tokens"))?;
        let mut out: Vec<(String, Token)> = rows.iter().map(|row| (row.get("app"), token_of(row))).collect();
        // By the app's bytes, as the files backend sorts, whatever collation
        // the database was made with.
        out.sort_by(|a, b| (&a.0, a.1.created_at).cmp(&(&b.0, b.1.created_at)));
        Ok(out)
    }

    async fn revoke(&self, app: &str, kind: Kind, id: &str) -> Result<bool, String> {
        let removed = self
            .client()
            .await?
            .execute(
                "delete from platform.app_tokens where app = $1 and kind = $2 and id = $3",
                &[&app, &kind.name(), &id],
            )
            .await
            .map_err(failed("revoke a token"))?;
        Ok(removed > 0)
    }

    async fn revoke_all(&self, app: &str, kind: Kind) -> Result<(), String> {
        self.client()
            .await?
            .execute("delete from platform.app_tokens where app = $1 and kind = $2", &[&app, &kind.name()])
            .await
            .map_err(failed("revoke tokens"))?;
        Ok(())
    }

    async fn check(&self, app: &str, kind: Kind, hash: &str, now: u64) -> Result<Option<Token>, String> {
        let client = self.client().await?;
        let rows = client
            .query(
                "select id, label, last_used, created_at, hash from platform.app_tokens where app = $1 and kind = $2",
                &[&app, &kind.name()],
            )
            .await
            .map_err(failed("read tokens"))?;
        let found = rows
            .iter()
            .fold(None, |found, row| if same(&row.get::<_, String>("hash"), hash) { Some(row) } else { found });
        let Some(row) = found else {
            return Ok(None);
        };
        let mut token = token_of(row);
        if !token.last_used.is_none_or(|at| now.saturating_sub(at) >= kind.resolution()) {
            return Ok(Some(token));
        }
        let now = now as i64;
        let stale_before = now - kind.resolution() as i64;
        let recorded = client
            .execute(
                "update platform.app_tokens set last_used = $4
                  where app = $1 and kind = $2 and id = $3
                    and (last_used is null or last_used <= $5)",
                &[&app, &kind.name(), &token.id, &now, &stale_before],
            )
            .await
            .map_err(failed("record a token's use"))?;
        if recorded == 0 {
            // Nothing written: another check recorded the use first, or the
            // token was revoked since it was read. Only the first is a token.
            let live = client
                .query_opt(
                    "select 1 from platform.app_tokens where app = $1 and kind = $2 and id = $3",
                    &[&app, &kind.name(), &token.id],
                )
                .await
                .map_err(failed("read a token"))?;
            if live.is_none() {
                return Ok(None);
            }
        }
        token.last_used = Some(now as u64);
        Ok(Some(token))
    }

    fn check_blocking(&self, app: &str, kind: Kind, hash: &str, now: u64) -> Result<Option<Token>, String> {
        crate::state::wait_in_place(self.check(app, kind, hash, now))
    }

    fn retire_blocking(&self, app: &str, at: u64) -> Result<Vec<(&'static str, String)>, String> {
        crate::state::wait_in_place(self.retire(app, at))
    }
}
