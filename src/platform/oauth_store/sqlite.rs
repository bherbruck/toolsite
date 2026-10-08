//! The OAuth store in file mode: `.site/oauth.db`, which no slug can name
//! and which nothing else opens. Kept at full sync, unlike an app's
//! database: a token issued is a token on disk, power cut or not.
//!
//! The synchronous methods are the code this module always ran; the trait
//! runs them on a blocking thread.

use super::{
    hash, now, Client, Grant, Issued, OAuthStore, Redeemed, ACCESS_LIFETIME, CODE_LIFETIME, IDLE_CLIENT_LIFETIME,
    REFRESH_LIFETIME, TOKEN_LEN,
};
use crate::{config::Config, content::slug::random_token, runtime::db};
use rusqlite::{Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use std::{path::PathBuf, sync::LazyLock};

/// Append only, as with the accounts ladder.
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../../../migrations/oauth/001_initial.sql")),
        M::up(include_str!("../../../migrations/oauth/002_resource.sql")),
    ])
});

#[derive(Clone)]
pub struct SqliteOAuth {
    path: PathBuf,
    max_db_bytes: u64,
}

impl SqliteOAuth {
    pub fn of(config: &Config) -> SqliteOAuth {
        SqliteOAuth {
            path: config.data_dir.join(".site").join("oauth.db"),
            max_db_bytes: config.max_db_bytes,
        }
    }

    fn open(&self) -> Result<Connection, String> {
        let mut conn = db::open_unguarded(&self.path, self.max_db_bytes)?;
        MIGRATIONS.to_latest(&mut conn).map_err(|e| e.to_string())?;
        db::lock_down(&conn)?;
        Ok(conn)
    }

    pub fn register_client(&self, name: Option<&str>, redirect_uris: &[String]) -> Result<Client, String> {
        let conn = self.open()?;
        sweep(&conn);
        let id = random_token(24);
        let uris = serde_json::to_string(redirect_uris).map_err(|e| e.to_string())?;
        conn.execute(
            "insert into clients (id, name, redirect_uris, created_at) values (?, ?, ?, ?)",
            rusqlite::params![id, name, uris, now() as i64],
        )
        .map_err(|e| e.to_string())?;
        Ok(Client {
            id,
            name: name.map(str::to_string),
            redirect_uris: redirect_uris.to_vec(),
        })
    }

    pub fn client(&self, id: &str) -> Option<Client> {
        let conn = self.open().ok()?;
        conn.query_row(
            "select id, name, redirect_uris from clients where id = ?",
            [id],
            |row| {
                let uris: String = row.get(2)?;
                Ok(Client {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    redirect_uris: serde_json::from_str(&uris).unwrap_or_default(),
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn issue_code(&self, grant: &Grant<'_>) -> Result<String, String> {
        let conn = self.open()?;
        let code = random_token(TOKEN_LEN);
        conn.execute(
            "insert into codes (code_hash, client_id, user_id, redirect_uri, code_challenge, expires_at, resource)
             values (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                hash(&code),
                grant.client_id,
                grant.user_id,
                grant.redirect_uri,
                grant.code_challenge,
                (now() + CODE_LIFETIME.as_secs()) as i64,
                grant.resource,
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(code)
    }

    pub fn redeem_code(&self, code: &str) -> Option<Redeemed> {
        let mut conn = self.open().ok()?;
        let tx = conn.transaction().ok()?;
        let found = tx
            .query_row(
                "select client_id, user_id, redirect_uri, code_challenge, expires_at, resource
                   from codes where code_hash = ?",
                [hash(code)],
                |row| {
                    Ok((
                        Redeemed {
                            client_id: row.get(0)?,
                            user_id: row.get(1)?,
                            redirect_uri: row.get(2)?,
                            code_challenge: row.get(3)?,
                            resource: row.get(5)?,
                        },
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten();
        tx.execute("delete from codes where code_hash = ?", [hash(code)]).ok()?;
        tx.commit().ok()?;
        let (redeemed, expires_at) = found?;
        (expires_at >= now() as i64).then_some(redeemed)
    }

    pub fn issue_tokens(&self, client_id: &str, user_id: &str, resource: Option<&str>) -> Result<Issued, String> {
        let conn = self.open()?;
        issue_tokens_on(&conn, client_id, user_id, resource)
    }

    pub fn rotate_refresh(&self, client_id: &str, refresh_token: &str) -> Option<Issued> {
        let mut conn = self.open().ok()?;
        let tx = conn.transaction().ok()?;
        let found = tx
            .query_row(
                "select user_id, expires_at, resource from tokens
                  where token_hash = ? and kind = 'refresh' and client_id = ?",
                rusqlite::params![hash(refresh_token), client_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Option<String>>(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (user_id, expires_at, resource) = found?;
        tx.execute("delete from tokens where token_hash = ?", [hash(refresh_token)])
            .ok()?;
        if expires_at < now() as i64 {
            tx.commit().ok()?;
            return None;
        }
        // A refreshed pair is for the same resource as the one it replaces.
        let issued = issue_tokens_on(&tx, client_id, &user_id, resource.as_deref()).ok()?;
        tx.commit().ok()?;
        Some(issued)
    }

    pub fn access_token_grant(&self, token: &str) -> Option<(String, String, Option<String>)> {
        let conn = self.open().ok()?;
        sweep(&conn);
        conn.query_row(
            "select user_id, client_id, resource from tokens
              where token_hash = ? and kind = 'access' and expires_at >= ?",
            rusqlite::params![hash(token), now() as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn revoke_for_user(&self, user_id: &str) -> Result<usize, String> {
        let conn = self.open()?;
        let tokens = conn.execute("delete from tokens where user_id = ?", [user_id]).map_err(|e| e.to_string())?;
        let codes = conn.execute("delete from codes where user_id = ?", [user_id]).map_err(|e| e.to_string())?;
        Ok(tokens + codes)
    }

    pub fn sweep(&self) {
        if let Ok(conn) = self.open() {
            sweep(&conn);
        }
    }
}

fn issue_tokens_on(conn: &Connection, client_id: &str, user_id: &str, resource: Option<&str>) -> Result<Issued, String> {
    let access_token = random_token(TOKEN_LEN);
    let refresh_token = random_token(TOKEN_LEN);
    let issued_at = now();
    for (token, kind, lifetime) in [
        (&access_token, "access", ACCESS_LIFETIME),
        (&refresh_token, "refresh", REFRESH_LIFETIME),
    ] {
        conn.execute(
            "insert into tokens (token_hash, kind, client_id, user_id, expires_at, created_at, resource)
             values (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                hash(token),
                kind,
                client_id,
                user_id,
                (issued_at + lifetime.as_secs()) as i64,
                issued_at as i64,
                resource,
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(Issued {
        access_token,
        refresh_token,
        expires_in: ACCESS_LIFETIME.as_secs(),
    })
}

fn sweep(conn: &Connection) {
    let cutoff = now() as i64;
    let _ = conn.execute("delete from tokens where expires_at < ?", [cutoff]);
    let _ = conn.execute("delete from codes where expires_at < ?", [cutoff]);
    let _ = conn.execute(
        "delete from clients
          where created_at < ?
            and id not in (select client_id from tokens)
            and id not in (select client_id from codes)",
        [cutoff - IDLE_CLIENT_LIFETIME.as_secs() as i64],
    );
}

/// Runs a synchronous method on a blocking thread, as every caller did
/// before the trait. A panicked task is a failure, never a hang.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(work).await.ok()
}

const TASK_FAILED: &str = "the OAuth store task failed";

#[async_trait::async_trait]
impl OAuthStore for SqliteOAuth {
    async fn register_client(&self, name: Option<&str>, redirect_uris: &[String]) -> Result<Client, String> {
        let (this, name, uris) = (self.clone(), name.map(str::to_string), redirect_uris.to_vec());
        blocking(move || SqliteOAuth::register_client(&this, name.as_deref(), &uris))
            .await
            .unwrap_or_else(|| Err(TASK_FAILED.into()))
    }

    async fn client(&self, id: &str) -> Option<Client> {
        let (this, id) = (self.clone(), id.to_string());
        blocking(move || SqliteOAuth::client(&this, &id)).await.flatten()
    }

    async fn issue_code(&self, grant: &Grant<'_>) -> Result<String, String> {
        let this = self.clone();
        let (client_id, user_id, redirect_uri, challenge, resource) = (
            grant.client_id.to_string(),
            grant.user_id.to_string(),
            grant.redirect_uri.to_string(),
            grant.code_challenge.to_string(),
            grant.resource.map(str::to_string),
        );
        blocking(move || {
            SqliteOAuth::issue_code(
                &this,
                &Grant {
                    client_id: &client_id,
                    user_id: &user_id,
                    redirect_uri: &redirect_uri,
                    code_challenge: &challenge,
                    resource: resource.as_deref(),
                },
            )
        })
        .await
        .unwrap_or_else(|| Err(TASK_FAILED.into()))
    }

    async fn redeem_code(&self, code: &str) -> Option<Redeemed> {
        let (this, code) = (self.clone(), code.to_string());
        blocking(move || SqliteOAuth::redeem_code(&this, &code)).await.flatten()
    }

    async fn issue_tokens(&self, client_id: &str, user_id: &str, resource: Option<&str>) -> Result<Issued, String> {
        let this = self.clone();
        let (client_id, user_id, resource) = (client_id.to_string(), user_id.to_string(), resource.map(str::to_string));
        blocking(move || SqliteOAuth::issue_tokens(&this, &client_id, &user_id, resource.as_deref()))
            .await
            .unwrap_or_else(|| Err(TASK_FAILED.into()))
    }

    async fn rotate_refresh(&self, client_id: &str, refresh_token: &str) -> Option<Issued> {
        let (this, client_id, token) = (self.clone(), client_id.to_string(), refresh_token.to_string());
        blocking(move || SqliteOAuth::rotate_refresh(&this, &client_id, &token)).await.flatten()
    }

    async fn access_token_grant(&self, token: &str) -> Option<(String, String, Option<String>)> {
        let (this, token) = (self.clone(), token.to_string());
        blocking(move || SqliteOAuth::access_token_grant(&this, &token)).await.flatten()
    }

    async fn revoke_for_user(&self, user_id: &str) -> Result<usize, String> {
        let (this, user_id) = (self.clone(), user_id.to_string());
        blocking(move || SqliteOAuth::revoke_for_user(&this, &user_id))
            .await
            .unwrap_or_else(|| Err(TASK_FAILED.into()))
    }

    async fn sweep(&self) {
        let this = self.clone();
        blocking(move || SqliteOAuth::sweep(&this)).await;
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl super::Backdoor for SqliteOAuth {
    async fn age(&self, rows: super::Aged, by: i64) {
        let sql = match rows {
            super::Aged::Codes => "update codes set expires_at = expires_at - ?",
            super::Aged::Tokens => "update tokens set expires_at = expires_at - ?",
            super::Aged::Clients => "update clients set created_at = created_at - ?",
        };
        let this = self.clone();
        blocking(move || this.open().unwrap().execute(sql, [by]).unwrap())
            .await
            .unwrap();
    }

    async fn stored_tokens(&self) -> Vec<(String, String, String)> {
        let this = self.clone();
        blocking(move || {
            let conn = this.open().unwrap();
            let mut statement = conn.prepare("select token_hash, kind, user_id from tokens").unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .await
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_database_keeps_full_sync_unlike_an_apps() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteOAuth::of(&Config::local(dir.path().to_path_buf(), "t"));
        let conn = store.open().unwrap();
        conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>).unwrap();
        let sync: i64 = conn.query_row("pragma synchronous", [], |row| row.get(0)).unwrap();
        // 2 is FULL: a token issued is a token on disk, power cut or not.
        assert_eq!(sync, 2);
    }

    #[test]
    fn every_migration_is_valid_sql() {
        MIGRATIONS.validate().unwrap();
    }
}
