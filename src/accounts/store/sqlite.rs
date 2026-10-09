//! Accounts in `.site/auth.db`: the default, and what a site without
//! `DATABASE_URL` has always used. The ladder in `accounts::schema` shapes
//! the file; each method opens it, so a call holds no lock past its return.

use super::{AccountStore, PendingRow, ScopeRow, UserRow};
use crate::{accounts::users::User, config::Config, runtime::db};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;

pub struct SqliteAccounts {
    path: PathBuf,
    max_bytes: u64,
}

/// Lives under a dot-directory, which no slug can name: `valid_slug` refuses
/// a leading `.`, so no published app can ever collide with it or reach it.
fn site_db_path(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("auth.db")
}

/// The account database, migrated and behind the authorizer, for tests
/// that look at the file itself.
#[cfg(test)]
pub(crate) fn open(config: &Config) -> Result<Connection, String> {
    SqliteAccounts::new(config).open()
}

fn user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User { id: row.get(0)?, email: row.get(1)?, is_admin: row.get::<_, i64>(2)? != 0 })
}

fn err(e: rusqlite::Error) -> String {
    e.to_string()
}

impl SqliteAccounts {
    pub fn new(config: &Config) -> SqliteAccounts {
        SqliteAccounts { path: site_db_path(config), max_bytes: config.max_db_bytes }
    }

    fn open(&self) -> Result<Connection, String> {
        // Migrations read `pragma user_version`, which the authorizer refuses, so
        // the schema is brought up to date before the door is closed.
        let mut conn = db::open_unguarded(&self.path, self.max_bytes)?;
        crate::accounts::schema::migrate(&mut conn)?;
        db::lock_down(&conn)?;
        Ok(conn)
    }

    fn strings(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<String>, String> {
        let conn = self.open()?;
        let mut statement = conn.prepare(sql).map_err(err)?;
        let rows = statement.query_map(params, |row| row.get::<_, String>(0)).map_err(err)?;
        Ok(rows.filter_map(Result::ok).collect())
    }
}

/// `prefix = path or prefix starts with path/`, as SQL over `?1` (the path)
/// and `?2` (the path and a slash). The length is SQLite's own, in
/// characters as `substr` counts them: a length in bytes from Rust would
/// cut a path with a letter of more than one byte short, and miss its tree.
const AT_OR_BELOW: &str = "prefix = ?1 or substr(prefix, 1, length(?2)) = ?2";

impl AccountStore for SqliteAccounts {
    fn insert_user(&self, user: &User, password_hash: Option<&str>, created_at: i64) -> Result<bool, String> {
        let conn = self.open()?;
        match conn.execute(
            "insert into users (id, email, password_hash, created_at, is_admin) values (?, ?, ?, ?, ?)",
            rusqlite::params![&user.id, &user.email, password_hash, created_at, user.is_admin as i64],
        ) {
            Ok(_) => Ok(true),
            Err(e) if e.to_string().contains("UNIQUE") => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }

    fn user_by_id(&self, id: &str) -> Result<Option<User>, String> {
        self.open()?
            .query_row("select id, email, is_admin from users where id = ? and disabled_at is null", [id], user)
            .optional()
            .map_err(err)
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, String> {
        self.open()?
            .query_row("select id, email, is_admin from users where email = ? and disabled_at is null", [email], user)
            .optional()
            .map_err(err)
    }

    fn account_by_email(&self, email: &str) -> Result<Option<(User, Option<i64>)>, String> {
        self.open()?
            .query_row("select id, email, is_admin, disabled_at from users where email = ?", [email], |row| {
                Ok((user(row)?, row.get(3)?))
            })
            .optional()
            .map_err(err)
    }

    fn credentials(&self, email: &str) -> Result<Option<(User, Option<String>)>, String> {
        self.open()?
            .query_row(
                "select id, email, is_admin, password_hash from users where email = ? and disabled_at is null",
                [email],
                |row| Ok((user(row)?, row.get(3)?)),
            )
            .optional()
            .map_err(err)
    }

    fn password_hash(&self, user_id: &str) -> Result<Option<Option<String>>, String> {
        self.open()?
            .query_row("select password_hash from users where id = ? and disabled_at is null", [user_id], |row| row.get(0))
            .optional()
            .map_err(err)
    }

    fn has_password(&self, user_id: &str) -> Result<bool, String> {
        self.open()?
            .query_row("select password_hash is not null from users where id = ?", [user_id], |row| row.get::<_, bool>(0))
            .optional()
            .map(|found| found.unwrap_or(false))
            .map_err(err)
    }

    fn is_active(&self, user_id: &str) -> Result<Option<bool>, String> {
        self.open()?
            .query_row("select disabled_at is null from users where id = ?", [user_id], |row| row.get(0))
            .optional()
            .map_err(err)
    }

    fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<(), String> {
        self.open()?
            .execute("update users set password_hash = ? where id = ?", rusqlite::params![hash, user_id])
            .map(drop)
            .map_err(err)
    }

    fn set_disabled(&self, email: &str, at: Option<i64>) -> Result<Option<String>, String> {
        let conn = self.open()?;
        let changed = conn
            .execute("update users set disabled_at = ? where email = ?", rusqlite::params![at, email])
            .map_err(err)?;
        if changed == 0 {
            return Ok(None);
        }
        conn.query_row("select id from users where email = ?", [email], |row| row.get(0)).optional().map_err(err)
    }

    fn list_users(&self) -> Result<Vec<UserRow>, String> {
        let conn = self.open()?;
        let mut statement = conn
            .prepare(
                "select email, created_at, is_admin, disabled_at,
                        exists(select 1 from mfa where mfa.user_id = users.id and mfa.enabled_at is not null)
                   from users order by email",
            )
            .map_err(err)?;
        let rows = statement
            .query_map([], |row| {
                Ok(UserRow {
                    email: row.get(0)?,
                    created_at: row.get(1)?,
                    is_admin: row.get::<_, i64>(2)? != 0,
                    disabled_at: row.get(3)?,
                    mfa: row.get::<_, i64>(4)? != 0,
                })
            })
            .map_err(err)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    fn link_identity(&self, provider: &str, provider_id: &str, user_id: &str) -> Result<(), String> {
        self.open()?
            .execute(
                "insert into identities (provider, provider_id, user_id) values (?, ?, ?)
                 on conflict(provider, provider_id) do update set user_id = excluded.user_id",
                rusqlite::params![provider, provider_id, user_id],
            )
            .map(drop)
            .map_err(err)
    }

    fn user_by_identity(&self, provider: &str, provider_id: &str) -> Result<Option<User>, String> {
        self.open()?
            .query_row(
                "select users.id, users.email, users.is_admin
                   from identities join users on users.id = identities.user_id
                  where identities.provider = ? and identities.provider_id = ?
                    and users.disabled_at is null",
                rusqlite::params![provider, provider_id],
                user,
            )
            .optional()
            .map_err(err)
    }

    fn identities_for(&self, user_id: &str) -> Result<Vec<String>, String> {
        self.strings("select provider from identities where user_id = ? order by provider", [user_id])
    }

    fn insert_session(&self, hash: &str, user_id: &str, expires_at: i64, scope: Option<&str>) -> Result<(), String> {
        self.open()?
            .execute(
                "insert into sessions (token_hash, user_id, expires_at, scope) values (?, ?, ?, ?)",
                rusqlite::params![hash, user_id, expires_at, scope],
            )
            .map(drop)
            .map_err(err)
    }

    fn session_user(&self, hash: &str, scope: Option<&str>, now: i64) -> Result<Option<User>, String> {
        let conn = self.open()?;
        // Expired rows are swept opportunistically rather than by a timer.
        let _ = conn.execute("delete from sessions where expires_at < ?", [now]);
        conn.query_row(
            "select users.id, users.email, users.is_admin
               from sessions join users on users.id = sessions.user_id
              where sessions.token_hash = ? and sessions.expires_at >= ?
                and sessions.scope is ? and users.disabled_at is null",
            rusqlite::params![hash, now, scope],
            user,
        )
        .optional()
        .map_err(err)
    }

    fn site_session(&self, hash: &str, now: i64) -> Result<Option<(User, i64)>, String> {
        self.open()?
            .query_row(
                "select users.id, users.email, users.is_admin, sessions.expires_at
                   from sessions join users on users.id = sessions.user_id
                  where sessions.token_hash = ? and sessions.expires_at >= ?
                    and sessions.scope is null and users.disabled_at is null",
                rusqlite::params![hash, now],
                |row| Ok((user(row)?, row.get(3)?)),
            )
            .optional()
            .map_err(err)
    }

    fn end_session(&self, hash: &str) -> Result<(), String> {
        let conn = self.open()?;
        conn.execute(
            "delete from sessions
              where scope is not null
                and user_id = (select user_id from sessions where token_hash = ?)",
            [hash],
        )
        .map_err(err)?;
        conn.execute("delete from sessions where token_hash = ?", [hash]).map(drop).map_err(err)
    }

    fn delete_sessions_for(&self, user_id: &str, except: Option<&str>) -> Result<(), String> {
        self.open()?
            .execute(
                "delete from sessions where user_id = ? and token_hash != ?",
                rusqlite::params![user_id, except.unwrap_or_default()],
            )
            .map(drop)
            .map_err(err)
    }

    fn replace_invite(&self, user_id: &str, hash: &str, expires_at: i64) -> Result<(), String> {
        let mut conn = self.open()?;
        let tx = conn.transaction().map_err(err)?;
        tx.execute("delete from invites where user_id = ?", [user_id]).map_err(err)?;
        tx.execute(
            "insert into invites (token_hash, user_id, expires_at) values (?, ?, ?)",
            rusqlite::params![hash, user_id, expires_at],
        )
        .map_err(err)?;
        tx.commit().map_err(err)
    }

    fn invited(&self, hash: &str, now: i64) -> Result<Option<User>, String> {
        self.open()?
            .query_row(
                "select users.id, users.email, users.is_admin
                   from invites join users on users.id = invites.user_id
                  where invites.token_hash = ? and invites.expires_at >= ?
                    and users.disabled_at is null",
                rusqlite::params![hash, now],
                user,
            )
            .optional()
            .map_err(err)
    }

    fn take_invite(&self, hash: &str, now: i64) -> Result<Option<User>, String> {
        let conn = self.open()?;
        let taken: Option<String> = conn
            .query_row(
                "delete from invites
                  where token_hash = ?1 and expires_at >= ?2
                    and user_id in (select id from users where disabled_at is null)
                 returning user_id",
                rusqlite::params![hash, now],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)?;
        match taken {
            Some(id) => self.user_by_id(&id),
            None => Ok(None),
        }
    }

    fn set_grant(&self, user_id: &str, app: &str, role: &str) -> Result<(), String> {
        self.open()?
            .execute(
                "insert into grants (user_id, app, role) values (?, ?, ?)
                 on conflict(user_id, app) do update set role = excluded.role",
                rusqlite::params![user_id, app, role],
            )
            .map(drop)
            .map_err(err)
    }

    fn delete_grant(&self, user_id: &str, app: &str) -> Result<(), String> {
        self.open()?
            .execute("delete from grants where user_id = ? and app = ?", rusqlite::params![user_id, app])
            .map(drop)
            .map_err(err)
    }

    fn grant_of(&self, user_id: &str, app: &str) -> Result<Option<String>, String> {
        self.open()?
            .query_row("select role from grants where user_id = ? and app = ?", rusqlite::params![user_id, app], |row| {
                row.get(0)
            })
            .optional()
            .map_err(err)
    }

    fn list_grants(&self) -> Result<Vec<(String, String, String)>, String> {
        let conn = self.open()?;
        let mut statement = conn
            .prepare(
                "select grants.app, users.email, grants.role
                   from grants join users on users.id = grants.user_id
                  order by grants.app, users.email",
            )
            .map_err(err)?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(err)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    fn set_scope(&self, user_id: &str, prefix: &str, scope: &str, granted_by: Option<&str>, now: i64) -> Result<(), String> {
        self.open()?
            .execute(
                "insert into scopes (user_id, prefix, scope, granted_by, created_at) values (?, ?, ?, ?, ?)
                 on conflict(user_id, prefix) do update set scope = excluded.scope, granted_by = excluded.granted_by",
                rusqlite::params![user_id, prefix, scope, granted_by, now],
            )
            .map(drop)
            .map_err(err)
    }

    fn delete_scope(&self, user_id: &str, prefix: &str) -> Result<usize, String> {
        self.open()?
            .execute("delete from scopes where prefix = ? and user_id = ?", rusqlite::params![prefix, user_id])
            .map_err(err)
    }

    fn scopes_for(&self, user_id: &str) -> Result<Vec<(String, String)>, String> {
        let conn = self.open()?;
        let mut statement = conn.prepare("select prefix, scope from scopes where user_id = ? order by prefix").map_err(err)?;
        let rows = statement.query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?))).map_err(err)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    fn list_scopes(&self) -> Result<Vec<ScopeRow>, String> {
        let conn = self.open()?;
        let mut statement = conn
            .prepare(
                "select users.email, users.id, scopes.prefix, scopes.scope
                   from scopes join users on users.id = scopes.user_id
                  order by scopes.prefix, users.email",
            )
            .map_err(err)?;
        let rows = statement
            .query_map([], |row| Ok(ScopeRow { email: row.get(0)?, user_id: row.get(1)?, prefix: row.get(2)?, scope: row.get(3)? }))
            .map_err(err)?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    fn rename_scope(&self, from: &str, to: &str) -> Result<(), String> {
        self.open()?
            .execute("update or replace scopes set prefix = ? where prefix = ?", rusqlite::params![to, from])
            .map(drop)
            .map_err(err)
    }

    fn move_scope_tree(&self, from: &str, to: &str) -> Result<usize, String> {
        let mut conn = self.open()?;
        let tx = conn.transaction().map_err(err)?;
        let changed = tx
            .execute(
                "update or replace scopes set prefix = ?3 || substr(prefix, length(?1) + 1)
                  where prefix = ?1 or substr(prefix, 1, length(?2)) = ?2",
                rusqlite::params![from, format!("{from}/"), to],
            )
            .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(changed)
    }

    fn remove_scope_tree(&self, path: &str) -> Result<usize, String> {
        self.open()?
            .execute(
                &format!("delete from scopes where {AT_OR_BELOW}"),
                rusqlite::params![path, format!("{path}/")],
            )
            .map_err(err)
    }

    fn forget_app(&self, app: &str, path: &str) -> Result<serde_json::Value, String> {
        let mut conn = self.open()?;
        let tx = conn.transaction().map_err(err)?;
        let below = rusqlite::params![path, format!("{path}/")];
        let rows: Vec<serde_json::Value> = {
            let mut statement = tx
                .prepare(
                    "select users.email, scopes.prefix, scopes.scope from scopes join users on users.id = scopes.user_id
                      where scopes.prefix = ?1 or substr(scopes.prefix, 1, length(?2)) = ?2",
                )
                .map_err(err)?;
            let found = statement
                .query_map(below, |row| {
                    Ok(serde_json::json!({ "email": row.get::<_, String>(0)?, "path": row.get::<_, String>(1)?, "scope": row.get::<_, String>(2)? }))
                })
                .map_err(err)?;
            found.filter_map(Result::ok).collect()
        };
        let grants: Vec<serde_json::Value> = {
            let mut statement = tx
                .prepare("select users.email, grants.role from grants join users on users.id = grants.user_id where grants.app = ?1")
                .map_err(err)?;
            let found = statement
                .query_map([app], |row| Ok(serde_json::json!({ "email": row.get::<_, String>(0)?, "role": row.get::<_, String>(1)? })))
                .map_err(err)?;
            found.filter_map(Result::ok).collect()
        };
        tx.execute(&format!("delete from scopes where {AT_OR_BELOW}"), below).map_err(err)?;
        tx.execute("delete from grants where app = ?1", [app]).map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(serde_json::json!({ "app": app, "path": path, "access": rows, "grants": grants }))
    }

    fn pins_for(&self, user_id: &str) -> Result<Vec<String>, String> {
        self.strings("select app from pins where user_id = ? order by app", [user_id])
    }

    fn set_pin(&self, user_id: &str, app: &str, pinned: bool, now: i64) -> Result<(), String> {
        let conn = self.open()?;
        if pinned {
            conn.execute(
                "insert into pins (user_id, app, created_at) values (?, ?, ?) on conflict(user_id, app) do nothing",
                rusqlite::params![user_id, app, now],
            )
        } else {
            conn.execute("delete from pins where user_id = ? and app = ?", rusqlite::params![user_id, app])
        }
        .map(drop)
        .map_err(err)
    }

    fn mfa_enabled(&self, user_id: &str) -> Result<Option<bool>, String> {
        self.open()?
            .query_row("select enabled_at is not null from mfa where user_id = ?", [user_id], |row| row.get(0))
            .optional()
            .map_err(err)
    }

    fn recovery_left(&self, user_id: &str) -> Result<usize, String> {
        self.open()?
            .query_row(
                "select count(*) from recovery_codes where user_id = ? and used_at is null",
                [user_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(err)
    }

    fn mfa_secret(&self, user_id: &str, setup_by: Option<&str>) -> Result<Option<(String, i64)>, String> {
        let conn = self.open()?;
        match setup_by {
            None => conn.query_row(
                "select secret, last_step from mfa where user_id = ? and enabled_at is not null",
                [user_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ),
            Some(by) => conn.query_row(
                "select secret, last_step from mfa where user_id = ? and enabled_at is null and begun_by = ?",
                rusqlite::params![user_id, by],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ),
        }
        .optional()
        .map_err(err)
    }

    fn begin_mfa(&self, user_id: &str, sealed: &str, begun_by: &str) -> Result<(), String> {
        self.open()?
            .execute(
                "insert into mfa (user_id, secret, enabled_at, last_step, begun_by) values (?1, ?2, null, 0, ?3)
                 on conflict(user_id) do update set secret = excluded.secret, last_step = 0, begun_by = excluded.begun_by
                 where enabled_at is null",
                rusqlite::params![user_id, sealed, begun_by],
            )
            .map(drop)
            .map_err(err)
    }

    fn cancel_mfa_setup(&self, user_id: &str) -> Result<(), String> {
        self.open()?
            .execute("delete from mfa where user_id = ? and enabled_at is null", [user_id])
            .map(drop)
            .map_err(err)
    }

    fn enable_mfa(&self, user_id: &str, now: i64) -> Result<bool, String> {
        self.open()?
            .execute(
                "update mfa set enabled_at = ?, begun_by = null where user_id = ? and enabled_at is null",
                rusqlite::params![now, user_id],
            )
            .map(|changed| changed == 1)
            .map_err(err)
    }

    fn advance_step(&self, user_id: &str, step: i64) -> Result<bool, String> {
        self.open()?
            .execute("update mfa set last_step = ?1 where user_id = ?2 and last_step < ?1", rusqlite::params![step, user_id])
            .map(|moved| moved == 1)
            .map_err(err)
    }

    fn replace_recovery_codes(&self, user_id: &str, hashes: &[String]) -> Result<(), String> {
        let mut conn = self.open()?;
        let tx = conn.transaction().map_err(err)?;
        tx.execute("delete from recovery_codes where user_id = ?", [user_id]).map_err(err)?;
        for hash in hashes {
            tx.execute(
                "insert into recovery_codes (code_hash, user_id, used_at) values (?, ?, null)",
                rusqlite::params![hash, user_id],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)
    }

    fn spend_recovery_code(&self, user_id: &str, hash: &str, now: i64) -> Result<bool, String> {
        self.open()?
            .execute(
                "update recovery_codes set used_at = ? where code_hash = ? and user_id = ? and used_at is null",
                rusqlite::params![now, hash, user_id],
            )
            .map(|spent| spent == 1)
            .map_err(err)
    }

    fn remove_mfa(&self, user_id: &str) -> Result<(), String> {
        let conn = self.open()?;
        for sql in [
            "delete from mfa where user_id = ?",
            "delete from recovery_codes where user_id = ?",
            "delete from mfa_pending where user_id = ?",
        ] {
            conn.execute(sql, [user_id]).map_err(err)?;
        }
        Ok(())
    }

    fn failures_since(&self, user_id: &str, since: i64) -> Result<i64, String> {
        self.open()?
            .query_row(
                "select count(*) from mfa_failures where user_id = ? and at > ?",
                rusqlite::params![user_id, since],
                |row| row.get(0),
            )
            .map_err(err)
    }

    fn try_attempt(&self, user_id: &str, at: i64, since: i64, limit: i64) -> Result<bool, String> {
        let mut conn = self.open()?;
        // Immediate: the write lock is taken before the count is read, so
        // two callers cannot both read a count under the limit.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(err)?;
        tx.execute("delete from mfa_failures where at <= ?", [since]).map_err(err)?;
        let counted: i64 = tx
            .query_row("select count(*) from mfa_failures where user_id = ? and at > ?", rusqlite::params![user_id, since], |row| row.get(0))
            .map_err(err)?;
        if counted >= limit {
            return Ok(false);
        }
        tx.execute("insert into mfa_failures (user_id, at) values (?, ?)", rusqlite::params![user_id, at]).map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(true)
    }

    fn release_attempt(&self, user_id: &str, at: i64) -> Result<(), String> {
        self.open()?
            .execute(
                "delete from mfa_failures where rowid = (select rowid from mfa_failures where user_id = ? and at = ? limit 1)",
                rusqlite::params![user_id, at],
            )
            .map(drop)
            .map_err(err)
    }

    fn insert_pending(&self, hash: &str, user_id: &str, stage: &str, next: &str, expires_at: i64, now: i64) -> Result<(), String> {
        let conn = self.open()?;
        let _ = conn.execute("delete from mfa_pending where expires_at < ?", [now]);
        conn.execute(
            "insert into mfa_pending (token_hash, user_id, stage, next, expires_at, failures) values (?, ?, ?, ?, ?, 0)",
            rusqlite::params![hash, user_id, stage, next, expires_at],
        )
        .map(drop)
        .map_err(err)
    }

    fn pending(&self, hash: &str, now: i64) -> Result<Option<PendingRow>, String> {
        self.open()?
            .query_row(
                "select users.id, users.email, users.is_admin, mfa_pending.stage, mfa_pending.next, mfa_pending.failures
                   from mfa_pending join users on users.id = mfa_pending.user_id
                  where mfa_pending.token_hash = ? and mfa_pending.expires_at >= ? and users.disabled_at is null",
                rusqlite::params![hash, now],
                |row| Ok(PendingRow { user: user(row)?, stage: row.get(3)?, next: row.get(4)?, failures: row.get(5)? }),
            )
            .optional()
            .map_err(err)
    }

    fn try_pending(&self, hash: &str, limit: i64) -> Result<Option<i64>, String> {
        self.open()?
            .query_row(
                "update mfa_pending set failures = failures + 1 where token_hash = ? and failures < ? returning failures",
                rusqlite::params![hash, limit],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)
    }

    fn delete_pending(&self, hash: &str) -> Result<(), String> {
        self.open()?.execute("delete from mfa_pending where token_hash = ?", [hash]).map(drop).map_err(err)
    }

    fn delete_pending_for(&self, user_id: &str) -> Result<(), String> {
        self.open()?.execute("delete from mfa_pending where user_id = ?", [user_id]).map(drop).map_err(err)
    }
}
