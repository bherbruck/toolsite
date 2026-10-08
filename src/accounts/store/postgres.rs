//! Accounts in Postgres, schema `accounts`, for a site on `DATABASE_URL`.
//!
//! Every value is a bound parameter and every statement a constant. Lists
//! sort with `collate "C"`, byte order, as SQLite does: the database's own
//! collation would put `ops/yard` after `opsx` on one server and before it
//! on another. The
//! single-use writes are single statements whose `where` re-checks the row
//! after any wait on its lock, which read committed does: two callers
//! spending one step, one recovery code or one invitation queue on the row,
//! and the second finds the condition already false.

use super::{AccountStore, PendingRow, ScopeRow, UserRow};
use crate::{accounts::users::User, state};
use deadpool_postgres::{Object, Pool};
use tokio_postgres::{error::SqlState, Row};

pub struct PostgresAccounts {
    pool: Pool,
}

impl PostgresAccounts {
    pub fn new(pool: Pool) -> PostgresAccounts {
        PostgresAccounts { pool }
    }

    /// Runs one piece of work on a pooled connection, from the blocking
    /// thread every account call is already on.
    fn run<T>(&self, work: impl AsyncFnOnce(&mut Object) -> Result<T, tokio_postgres::Error>) -> Result<T, String> {
        state::wait(async {
            let mut client = self
                .pool
                .get()
                .await
                .map_err(|e| format!("could not reach the account database: {}", state::pg::chain(&e)))?;
            work(&mut client).await.map_err(|e| state::pg::chain(&e))
        })
    }
}

fn user(row: &Row) -> User {
    User { id: row.get(0), email: row.get(1), is_admin: row.get(2) }
}

/// `prefix = path or prefix starts with path/`.
fn below(path: &str) -> String {
    format!("{path}/")
}

impl AccountStore for PostgresAccounts {
    fn insert_user(&self, new: &User, password_hash: Option<&str>, created_at: i64) -> Result<bool, String> {
        self.run(async |c| {
            match c
                .execute(
                    "insert into accounts.users (id, email, password_hash, created_at, is_admin) values ($1, $2, $3, $4, $5)",
                    &[&new.id, &new.email, &password_hash, &created_at, &new.is_admin],
                )
                .await
            {
                Ok(_) => Ok(true),
                Err(e) if e.code() == Some(&SqlState::UNIQUE_VIOLATION) => Ok(false),
                Err(e) => Err(e),
            }
        })
    }

    fn user_by_id(&self, id: &str) -> Result<Option<User>, String> {
        self.run(async |c| {
            let row = c
                .query_opt("select id, email, is_admin from accounts.users where id = $1 and disabled_at is null", &[&id])
                .await?;
            Ok(row.as_ref().map(user))
        })
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, String> {
        self.run(async |c| {
            let row = c
                .query_opt("select id, email, is_admin from accounts.users where email = $1 and disabled_at is null", &[&email])
                .await?;
            Ok(row.as_ref().map(user))
        })
    }

    fn account_by_email(&self, email: &str) -> Result<Option<(User, Option<i64>)>, String> {
        self.run(async |c| {
            let row = c
                .query_opt("select id, email, is_admin, disabled_at from accounts.users where email = $1", &[&email])
                .await?;
            Ok(row.map(|row| (user(&row), row.get(3))))
        })
    }

    fn credentials(&self, email: &str) -> Result<Option<(User, Option<String>)>, String> {
        self.run(async |c| {
            let row = c
                .query_opt(
                    "select id, email, is_admin, password_hash from accounts.users where email = $1 and disabled_at is null",
                    &[&email],
                )
                .await?;
            Ok(row.map(|row| (user(&row), row.get(3))))
        })
    }

    fn password_hash(&self, user_id: &str) -> Result<Option<Option<String>>, String> {
        self.run(async |c| {
            let row = c
                .query_opt("select password_hash from accounts.users where id = $1 and disabled_at is null", &[&user_id])
                .await?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn has_password(&self, user_id: &str) -> Result<bool, String> {
        self.run(async |c| {
            let row = c
                .query_opt("select password_hash is not null from accounts.users where id = $1", &[&user_id])
                .await?;
            Ok(row.is_some_and(|row| row.get(0)))
        })
    }

    fn is_active(&self, user_id: &str) -> Result<Option<bool>, String> {
        self.run(async |c| {
            let row = c.query_opt("select disabled_at is null from accounts.users where id = $1", &[&user_id]).await?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute("update accounts.users set password_hash = $1 where id = $2", &[&hash, &user_id]).await?;
            Ok(())
        })
    }

    fn set_disabled(&self, email: &str, at: Option<i64>) -> Result<Option<String>, String> {
        self.run(async |c| {
            let row = c
                .query_opt("update accounts.users set disabled_at = $1 where email = $2 returning id", &[&at, &email])
                .await?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn list_users(&self) -> Result<Vec<UserRow>, String> {
        self.run(async |c| {
            let rows = c
                .query(
                    "select email, created_at, is_admin, disabled_at,
                            exists(select 1 from accounts.mfa where mfa.user_id = users.id and mfa.enabled_at is not null)
                       from accounts.users order by email collate \"C\"",
                    &[],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|row| UserRow {
                    email: row.get(0),
                    created_at: row.get(1),
                    is_admin: row.get(2),
                    disabled_at: row.get(3),
                    mfa: row.get(4),
                })
                .collect())
        })
    }

    fn link_identity(&self, provider: &str, provider_id: &str, user_id: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute(
                "insert into accounts.identities (provider, provider_id, user_id) values ($1, $2, $3)
                 on conflict (provider, provider_id) do update set user_id = excluded.user_id",
                &[&provider, &provider_id, &user_id],
            )
            .await?;
            Ok(())
        })
    }

    fn user_by_identity(&self, provider: &str, provider_id: &str) -> Result<Option<User>, String> {
        self.run(async |c| {
            let row = c
                .query_opt(
                    "select users.id, users.email, users.is_admin
                       from accounts.identities join accounts.users on users.id = identities.user_id
                      where identities.provider = $1 and identities.provider_id = $2
                        and users.disabled_at is null",
                    &[&provider, &provider_id],
                )
                .await?;
            Ok(row.as_ref().map(user))
        })
    }

    fn identities_for(&self, user_id: &str) -> Result<Vec<String>, String> {
        self.run(async |c| {
            let rows = c
                .query("select provider from accounts.identities where user_id = $1 order by provider collate \"C\"", &[&user_id])
                .await?;
            Ok(rows.iter().map(|row| row.get(0)).collect())
        })
    }

    fn insert_session(&self, hash: &str, user_id: &str, expires_at: i64, scope: Option<&str>) -> Result<(), String> {
        self.run(async |c| {
            c.execute(
                "insert into accounts.sessions (token_hash, user_id, expires_at, scope) values ($1, $2, $3, $4)",
                &[&hash, &user_id, &expires_at, &scope],
            )
            .await?;
            Ok(())
        })
    }

    fn session_user(&self, hash: &str, scope: Option<&str>, now: i64) -> Result<Option<User>, String> {
        self.run(async |c| {
            // Expired rows are swept opportunistically rather than by a timer.
            let _ = c.execute("delete from accounts.sessions where expires_at < $1", &[&now]).await;
            let row = c
                .query_opt(
                    "select users.id, users.email, users.is_admin
                       from accounts.sessions join accounts.users on users.id = sessions.user_id
                      where sessions.token_hash = $1 and sessions.expires_at >= $2
                        and sessions.scope is not distinct from $3 and users.disabled_at is null",
                    &[&hash, &now, &scope],
                )
                .await?;
            Ok(row.as_ref().map(user))
        })
    }

    fn site_session(&self, hash: &str, now: i64) -> Result<Option<(User, i64)>, String> {
        self.run(async |c| {
            let row = c
                .query_opt(
                    "select users.id, users.email, users.is_admin, sessions.expires_at
                       from accounts.sessions join accounts.users on users.id = sessions.user_id
                      where sessions.token_hash = $1 and sessions.expires_at >= $2
                        and sessions.scope is null and users.disabled_at is null",
                    &[&hash, &now],
                )
                .await?;
            Ok(row.map(|row| (user(&row), row.get(3))))
        })
    }

    fn end_session(&self, hash: &str) -> Result<(), String> {
        self.run(async |c| {
            let tx = c.transaction().await?;
            tx.execute(
                "delete from accounts.sessions
                  where scope is not null
                    and user_id = (select user_id from accounts.sessions where token_hash = $1)",
                &[&hash],
            )
            .await?;
            tx.execute("delete from accounts.sessions where token_hash = $1", &[&hash]).await?;
            tx.commit().await
        })
    }

    fn delete_sessions_for(&self, user_id: &str, except: Option<&str>) -> Result<(), String> {
        let except = except.unwrap_or_default();
        self.run(async |c| {
            c.execute("delete from accounts.sessions where user_id = $1 and token_hash <> $2", &[&user_id, &except])
                .await?;
            Ok(())
        })
    }

    fn replace_invite(&self, user_id: &str, hash: &str, expires_at: i64) -> Result<(), String> {
        self.run(async |c| {
            let tx = c.transaction().await?;
            tx.execute("delete from accounts.invites where user_id = $1", &[&user_id]).await?;
            tx.execute(
                "insert into accounts.invites (token_hash, user_id, expires_at) values ($1, $2, $3)",
                &[&hash, &user_id, &expires_at],
            )
            .await?;
            tx.commit().await
        })
    }

    fn invited(&self, hash: &str, now: i64) -> Result<Option<User>, String> {
        self.run(async |c| {
            let row = c
                .query_opt(
                    "select users.id, users.email, users.is_admin
                       from accounts.invites join accounts.users on users.id = invites.user_id
                      where invites.token_hash = $1 and invites.expires_at >= $2
                        and users.disabled_at is null",
                    &[&hash, &now],
                )
                .await?;
            Ok(row.as_ref().map(user))
        })
    }

    fn take_invite(&self, hash: &str, now: i64) -> Result<Option<User>, String> {
        self.run(async |c| {
            let row = c
                .query_opt(
                    "with taken as (
                         delete from accounts.invites
                          where token_hash = $1 and expires_at >= $2
                            and user_id in (select id from accounts.users where disabled_at is null)
                         returning user_id
                     )
                     select users.id, users.email, users.is_admin
                       from taken join accounts.users on users.id = taken.user_id",
                    &[&hash, &now],
                )
                .await?;
            Ok(row.as_ref().map(user))
        })
    }

    fn set_grant(&self, user_id: &str, app: &str, role: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute(
                "insert into accounts.grants (user_id, app, role) values ($1, $2, $3)
                 on conflict (user_id, app) do update set role = excluded.role",
                &[&user_id, &app, &role],
            )
            .await?;
            Ok(())
        })
    }

    fn delete_grant(&self, user_id: &str, app: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute("delete from accounts.grants where user_id = $1 and app = $2", &[&user_id, &app]).await?;
            Ok(())
        })
    }

    fn grant_of(&self, user_id: &str, app: &str) -> Result<Option<String>, String> {
        self.run(async |c| {
            let row = c
                .query_opt("select role from accounts.grants where user_id = $1 and app = $2", &[&user_id, &app])
                .await?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn list_grants(&self) -> Result<Vec<(String, String, String)>, String> {
        self.run(async |c| {
            let rows = c
                .query(
                    "select grants.app, users.email, grants.role
                       from accounts.grants join accounts.users on users.id = grants.user_id
                      order by grants.app collate \"C\", users.email collate \"C\"",
                    &[],
                )
                .await?;
            Ok(rows.iter().map(|row| (row.get(0), row.get(1), row.get(2))).collect())
        })
    }

    fn set_scope(&self, user_id: &str, prefix: &str, scope: &str, granted_by: Option<&str>, now: i64) -> Result<(), String> {
        self.run(async |c| {
            c.execute(
                "insert into accounts.scopes (user_id, prefix, scope, granted_by, created_at) values ($1, $2, $3, $4, $5)
                 on conflict (user_id, prefix) do update set scope = excluded.scope, granted_by = excluded.granted_by",
                &[&user_id, &prefix, &scope, &granted_by, &now],
            )
            .await?;
            Ok(())
        })
    }

    fn delete_scope(&self, user_id: &str, prefix: &str) -> Result<usize, String> {
        self.run(async |c| {
            c.execute("delete from accounts.scopes where prefix = $1 and user_id = $2", &[&prefix, &user_id])
                .await
                .map(|n| n as usize)
        })
    }

    fn scopes_for(&self, user_id: &str) -> Result<Vec<(String, String)>, String> {
        self.run(async |c| {
            let rows = c
                .query("select prefix, scope from accounts.scopes where user_id = $1 order by prefix collate \"C\"", &[&user_id])
                .await?;
            Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
        })
    }

    fn list_scopes(&self) -> Result<Vec<ScopeRow>, String> {
        self.run(async |c| {
            let rows = c
                .query(
                    "select users.email, users.id, scopes.prefix, scopes.scope
                       from accounts.scopes join accounts.users on users.id = scopes.user_id
                      order by scopes.prefix collate \"C\", users.email collate \"C\"",
                    &[],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|row| ScopeRow { email: row.get(0), user_id: row.get(1), prefix: row.get(2), scope: row.get(3) })
                .collect())
        })
    }

    fn rename_scope(&self, from: &str, to: &str) -> Result<(), String> {
        self.run(async |c| {
            let tx = c.transaction().await?;
            let moving = tx
                .query(
                    "delete from accounts.scopes where prefix = $1
                     returning user_id, scope, granted_by, created_at",
                    &[&from],
                )
                .await?;
            for row in &moving {
                place_scope(&tx, row, to).await?;
            }
            tx.commit().await
        })
    }

    fn move_scope_tree(&self, from: &str, to: &str) -> Result<usize, String> {
        self.run(async |c| {
            let tx = c.transaction().await?;
            // Taken out first and put back after, so a moved row meets only
            // rows that stay where they are, and replaces any it lands on.
            let moving = tx
                .query(
                    "delete from accounts.scopes where prefix = $1 or starts_with(prefix, $2)
                     returning user_id, scope, granted_by, created_at, prefix",
                    &[&from, &below(from)],
                )
                .await?;
            for row in &moving {
                let prefix: String = row.get(4);
                place_scope(&tx, row, &format!("{to}{}", &prefix[from.len()..])).await?;
            }
            tx.commit().await?;
            Ok(moving.len())
        })
    }

    fn remove_scope_tree(&self, path: &str) -> Result<usize, String> {
        self.run(async |c| {
            c.execute("delete from accounts.scopes where prefix = $1 or starts_with(prefix, $2)", &[&path, &below(path)])
                .await
                .map(|n| n as usize)
        })
    }

    fn forget_app(&self, app: &str, path: &str) -> Result<serde_json::Value, String> {
        self.run(async |c| {
            let tx = c.transaction().await?;
            let access = tx
                .query(
                    "delete from accounts.scopes
                      where prefix = $1 or starts_with(prefix, $2)
                     returning (select email from accounts.users where users.id = scopes.user_id), prefix, scope",
                    &[&path, &below(path)],
                )
                .await?;
            let grants = tx
                .query(
                    "delete from accounts.grants where app = $1
                     returning (select email from accounts.users where users.id = grants.user_id), role",
                    &[&app],
                )
                .await?;
            tx.commit().await?;
            let access: Vec<serde_json::Value> = access
                .iter()
                .map(|row| serde_json::json!({ "email": row.get::<_, String>(0), "path": row.get::<_, String>(1), "scope": row.get::<_, String>(2) }))
                .collect();
            let grants: Vec<serde_json::Value> = grants
                .iter()
                .map(|row| serde_json::json!({ "email": row.get::<_, String>(0), "role": row.get::<_, String>(1) }))
                .collect();
            Ok(serde_json::json!({ "app": app, "path": path, "access": access, "grants": grants }))
        })
    }

    fn pins_for(&self, user_id: &str) -> Result<Vec<String>, String> {
        self.run(async |c| {
            let rows = c.query("select app from accounts.pins where user_id = $1 order by app collate \"C\"", &[&user_id]).await?;
            Ok(rows.iter().map(|row| row.get(0)).collect())
        })
    }

    fn set_pin(&self, user_id: &str, app: &str, pinned: bool, now: i64) -> Result<(), String> {
        self.run(async |c| {
            if pinned {
                c.execute(
                    "insert into accounts.pins (user_id, app, created_at) values ($1, $2, $3)
                     on conflict (user_id, app) do nothing",
                    &[&user_id, &app, &now],
                )
                .await?;
            } else {
                c.execute("delete from accounts.pins where user_id = $1 and app = $2", &[&user_id, &app]).await?;
            }
            Ok(())
        })
    }

    fn mfa_enabled(&self, user_id: &str) -> Result<Option<bool>, String> {
        self.run(async |c| {
            let row = c.query_opt("select enabled_at is not null from accounts.mfa where user_id = $1", &[&user_id]).await?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn recovery_left(&self, user_id: &str) -> Result<usize, String> {
        self.run(async |c| {
            let row = c
                .query_one(
                    "select count(*) from accounts.recovery_codes where user_id = $1 and used_at is null",
                    &[&user_id],
                )
                .await?;
            Ok(row.get::<_, i64>(0) as usize)
        })
    }

    fn mfa_secret(&self, user_id: &str, setup_by: Option<&str>) -> Result<Option<(String, i64)>, String> {
        self.run(async |c| {
            let row = match setup_by {
                None => {
                    c.query_opt(
                        "select secret, last_step from accounts.mfa where user_id = $1 and enabled_at is not null",
                        &[&user_id],
                    )
                    .await?
                }
                Some(by) => {
                    c.query_opt(
                        "select secret, last_step from accounts.mfa
                          where user_id = $1 and enabled_at is null and begun_by = $2",
                        &[&user_id, &by],
                    )
                    .await?
                }
            };
            Ok(row.map(|row| (row.get(0), row.get(1))))
        })
    }

    fn begin_mfa(&self, user_id: &str, sealed: &str, begun_by: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute(
                "insert into accounts.mfa as mfa (user_id, secret, enabled_at, last_step, begun_by) values ($1, $2, null, 0, $3)
                 on conflict (user_id) do update set secret = excluded.secret, last_step = 0, begun_by = excluded.begun_by
                 where mfa.enabled_at is null",
                &[&user_id, &sealed, &begun_by],
            )
            .await?;
            Ok(())
        })
    }

    fn cancel_mfa_setup(&self, user_id: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute("delete from accounts.mfa where user_id = $1 and enabled_at is null", &[&user_id]).await?;
            Ok(())
        })
    }

    fn enable_mfa(&self, user_id: &str, now: i64) -> Result<bool, String> {
        self.run(async |c| {
            c.execute(
                "update accounts.mfa set enabled_at = $1, begun_by = null where user_id = $2 and enabled_at is null",
                &[&now, &user_id],
            )
            .await
            .map(|changed| changed == 1)
        })
    }

    fn advance_step(&self, user_id: &str, step: i64) -> Result<bool, String> {
        self.run(async |c| {
            c.execute(
                "update accounts.mfa set last_step = $1 where user_id = $2 and last_step < $1",
                &[&step, &user_id],
            )
            .await
            .map(|moved| moved == 1)
        })
    }

    fn replace_recovery_codes(&self, user_id: &str, hashes: &[String]) -> Result<(), String> {
        self.run(async |c| {
            let tx = c.transaction().await?;
            tx.execute("delete from accounts.recovery_codes where user_id = $1", &[&user_id]).await?;
            for hash in hashes {
                tx.execute(
                    "insert into accounts.recovery_codes (code_hash, user_id, used_at) values ($1, $2, null)",
                    &[hash, &user_id],
                )
                .await?;
            }
            tx.commit().await
        })
    }

    fn spend_recovery_code(&self, user_id: &str, hash: &str, now: i64) -> Result<bool, String> {
        self.run(async |c| {
            c.execute(
                "update accounts.recovery_codes set used_at = $1
                  where code_hash = $2 and user_id = $3 and used_at is null",
                &[&now, &hash, &user_id],
            )
            .await
            .map(|spent| spent == 1)
        })
    }

    fn remove_mfa(&self, user_id: &str) -> Result<(), String> {
        self.run(async |c| {
            let tx = c.transaction().await?;
            for sql in [
                "delete from accounts.mfa where user_id = $1",
                "delete from accounts.recovery_codes where user_id = $1",
                "delete from accounts.mfa_pending where user_id = $1",
            ] {
                tx.execute(sql, &[&user_id]).await?;
            }
            tx.commit().await
        })
    }

    fn failures_since(&self, user_id: &str, since: i64) -> Result<i64, String> {
        self.run(async |c| {
            let row = c
                .query_one("select count(*) from accounts.mfa_failures where user_id = $1 and at > $2", &[&user_id, &since])
                .await?;
            Ok(row.get(0))
        })
    }

    fn record_failure(&self, user_id: &str, at: i64, forget_before: i64) -> Result<(), String> {
        self.run(async |c| {
            let _ = c.execute("delete from accounts.mfa_failures where at <= $1", &[&forget_before]).await;
            c.execute("insert into accounts.mfa_failures (user_id, at) values ($1, $2)", &[&user_id, &at]).await?;
            Ok(())
        })
    }

    fn insert_pending(&self, hash: &str, user_id: &str, stage: &str, next: &str, expires_at: i64, now: i64) -> Result<(), String> {
        self.run(async |c| {
            let _ = c.execute("delete from accounts.mfa_pending where expires_at < $1", &[&now]).await;
            c.execute(
                "insert into accounts.mfa_pending (token_hash, user_id, stage, next, expires_at, failures)
                 values ($1, $2, $3, $4, $5, 0)",
                &[&hash, &user_id, &stage, &next, &expires_at],
            )
            .await?;
            Ok(())
        })
    }

    fn pending(&self, hash: &str, now: i64) -> Result<Option<PendingRow>, String> {
        self.run(async |c| {
            let row = c
                .query_opt(
                    "select users.id, users.email, users.is_admin, mfa_pending.stage, mfa_pending.next, mfa_pending.failures
                       from accounts.mfa_pending join accounts.users on users.id = mfa_pending.user_id
                      where mfa_pending.token_hash = $1 and mfa_pending.expires_at >= $2 and users.disabled_at is null",
                    &[&hash, &now],
                )
                .await?;
            Ok(row.map(|row| PendingRow { user: user(&row), stage: row.get(3), next: row.get(4), failures: row.get(5) }))
        })
    }

    fn fail_pending(&self, hash: &str) -> Result<Option<i64>, String> {
        self.run(async |c| {
            let row = c
                .query_opt(
                    "update accounts.mfa_pending set failures = failures + 1 where token_hash = $1 returning failures",
                    &[&hash],
                )
                .await?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn delete_pending(&self, hash: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute("delete from accounts.mfa_pending where token_hash = $1", &[&hash]).await?;
            Ok(())
        })
    }

    fn delete_pending_for(&self, user_id: &str) -> Result<(), String> {
        self.run(async |c| {
            c.execute("delete from accounts.mfa_pending where user_id = $1", &[&user_id]).await?;
            Ok(())
        })
    }
}

/// Puts a scope row taken out by a move back at `prefix`, replacing a row
/// already there, as SQLite's `update or replace` does.
async fn place_scope(tx: &deadpool_postgres::Transaction<'_>, row: &Row, prefix: &str) -> Result<(), tokio_postgres::Error> {
    let (user_id, scope, granted_by, created_at): (String, String, Option<String>, i64) =
        (row.get(0), row.get(1), row.get(2), row.get(3));
    tx.execute(
        "insert into accounts.scopes (user_id, prefix, scope, granted_by, created_at) values ($1, $2, $3, $4, $5)
         on conflict (user_id, prefix) do update
            set scope = excluded.scope, granted_by = excluded.granted_by, created_at = excluded.created_at",
        &[&user_id, &prefix, &scope, &granted_by, &created_at],
    )
    .await?;
    Ok(())
}
