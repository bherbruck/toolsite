//! What the OAuth server remembers: registered clients, codes in flight, and
//! the tokens it has handed out. Kept in `.site/oauth.db`, which no slug can
//! name and which nothing else opens.
//!
//! Everything a client presents is stored hashed, so a copy of this file is
//! not a set of live credentials. Rows name a user by the id accounts gave
//! them and never join across to that database; the caller resolves the id
//! through the accounts API, which is also where "still active" is decided.

use crate::{config::Config, content::slug::random_token, runtime::db};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A code is exchanged within seconds of being issued; a minute is generous.
pub(crate) const CODE_LIFETIME: Duration = Duration::from_secs(60);
/// Access tokens are what an MCP client sends with every request, so a leak
/// is a day of use, not a standing way in.
pub(crate) const ACCESS_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24);
/// A refresh token is the connection itself: the person signs in once and
/// their client keeps working for a month of inactivity before asking again.
pub(crate) const REFRESH_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24 * 30);
/// A client that registered and never finished signing in is noise; it is
/// swept once nothing could still be using it.
const IDLE_CLIENT_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24 * 7);

const TOKEN_LEN: usize = 48;

/// Append only, as with the accounts ladder.
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../../migrations/oauth/001_initial.sql")),
        M::up(include_str!("../../migrations/oauth/002_resource.sql")),
    ])
});

fn path(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("oauth.db")
}

fn open(config: &Config) -> Result<Connection, String> {
    let mut conn = db::open_unguarded(&path(config), config.max_db_bytes)?;
    MIGRATIONS.to_latest(&mut conn).map_err(|e| e.to_string())?;
    db::lock_down(&conn)?;
    Ok(conn)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hash(secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    pub id: String,
    /// What the client calls itself. Shown, never trusted.
    pub name: Option<String>,
    pub redirect_uris: Vec<String>,
}

/// Records a client and hands back its id. Registration proves nothing and
/// grants nothing: a token still needs a person to sign in and consent, and
/// the redirect URIs are pinned here so that consent can only be sent where
/// the client said up front.
pub fn register_client(
    config: &Config,
    name: Option<&str>,
    redirect_uris: &[String],
) -> Result<Client, String> {
    let conn = open(config)?;
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

pub fn client(config: &Config, id: &str) -> Option<Client> {
    let conn = open(config).ok()?;
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

/// The consent a person just gave, waiting to be exchanged.
pub struct Grant<'a> {
    pub client_id: &'a str,
    pub user_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    /// The resource the client asked for, normalised, if it named one.
    pub resource: Option<&'a str>,
}

/// Mints a one-time code for the grant.
pub fn issue_code(config: &Config, grant: &Grant<'_>) -> Result<String, String> {
    let conn = open(config)?;
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

/// What a redeemed code was issued for. The caller still has to check that
/// the client, redirect URI and PKCE verifier presented match.
pub struct Redeemed {
    pub client_id: String,
    pub user_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub resource: Option<String>,
}

/// Consumes a code: whatever the outcome of the checks that follow, the same
/// code cannot be tried twice. Nothing for a code that is unknown or expired.
pub fn redeem_code(config: &Config, code: &str) -> Option<Redeemed> {
    let mut conn = open(config).ok()?;
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

pub struct Issued {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// A fresh access/refresh pair for this person and client.
pub fn issue_tokens(config: &Config, client_id: &str, user_id: &str, resource: Option<&str>) -> Result<Issued, String> {
    let conn = open(config)?;
    issue_tokens_on(&conn, client_id, user_id, resource)
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

/// Trades a refresh token for a new pair, retiring the old one in the same
/// transaction: a refresh token works exactly once, so a copy that surfaces
/// later is refused rather than quietly minting a second connection. Nothing
/// if the token is unknown, expired, or was issued to a different client.
pub fn rotate_refresh(config: &Config, client_id: &str, refresh_token: &str) -> Option<Issued> {
    let mut conn = open(config).ok()?;
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

/// Who an access token was issued to, if it is live. Says which account and
/// which client; whether that account may still act is the accounts API's
/// answer, not this table's.
pub fn access_token_holder(config: &Config, token: &str) -> Option<(String, String)> {
    access_token_grant(config, token).map(|(user, client, _)| (user, client))
}

/// The holder, the client and the resource the token was issued for.
pub fn access_token_grant(config: &Config, token: &str) -> Option<(String, String, Option<String>)> {
    let conn = open(config).ok()?;
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

/// Ends every connection an account's clients hold: its access and refresh
/// tokens and any code not yet exchanged. What turning on two-step sign-in
/// does, so a client connected with a password alone has to sign in again
/// and give a code. Returns how many rows went.
pub fn revoke_for_user(config: &Config, user_id: &str) -> Result<usize, String> {
    let conn = open(config)?;
    let tokens = conn.execute("delete from tokens where user_id = ?", [user_id]).map_err(|e| e.to_string())?;
    let codes = conn.execute("delete from codes where user_id = ?", [user_id]).map_err(|e| e.to_string())?;
    Ok(tokens + codes)
}

/// Expired rows go opportunistically rather than on a timer, as sessions do.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        (dir, config)
    }

    /// Backdates a row so the tests do not have to wait a day.
    fn age(config: &Config, table: &str, by: Duration) {
        let conn = open(config).unwrap();
        conn.execute(
            &format!("update {table} set expires_at = expires_at - ?"),
            [by.as_secs() as i64],
        )
        .unwrap();
    }

    #[test]
    fn every_migration_is_valid_sql() {
        MIGRATIONS.validate().unwrap();
    }

    #[test]
    fn a_code_is_redeemed_exactly_once() {
        let (_dir, config) = config();
        let client = register_client(&config, Some("t"), &["https://c.test/cb".into()]).unwrap();
        let code = issue_code(
            &config,
            &Grant {
                client_id: &client.id,
                user_id: "u1",
                redirect_uri: "https://c.test/cb",
                code_challenge: "ch",
                resource: None,
            },
        )
        .unwrap();
        let first = redeem_code(&config, &code).expect("first redemption");
        assert_eq!(first.user_id, "u1");
        assert_eq!(first.code_challenge, "ch");
        assert!(redeem_code(&config, &code).is_none(), "a code replayed");
    }

    #[test]
    fn an_expired_code_is_refused_and_spent() {
        let (_dir, config) = config();
        let client = register_client(&config, None, &["https://c.test/cb".into()]).unwrap();
        let code = issue_code(
            &config,
            &Grant {
                client_id: &client.id,
                user_id: "u1",
                redirect_uri: "https://c.test/cb",
                code_challenge: "ch",
                resource: None,
            },
        )
        .unwrap();
        age(&config, "codes", CODE_LIFETIME + Duration::from_secs(1));
        assert!(redeem_code(&config, &code).is_none());
    }

    #[test]
    fn tokens_are_stored_hashed() {
        let (_dir, config) = config();
        let client = register_client(&config, None, &["https://c.test/cb".into()]).unwrap();
        let issued = issue_tokens(&config, &client.id, "u1", None).unwrap();
        let conn = open(&config).unwrap();
        let mut statement = conn.prepare("select token_hash from tokens").unwrap();
        let stored: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(stored.len(), 2);
        assert!(!stored.contains(&issued.access_token));
        assert!(!stored.contains(&issued.refresh_token));
        assert_eq!(
            access_token_holder(&config, &issued.access_token),
            Some(("u1".to_string(), client.id.clone()))
        );
    }

    #[test]
    fn an_expired_access_token_stops_working() {
        let (_dir, config) = config();
        let client = register_client(&config, None, &["https://c.test/cb".into()]).unwrap();
        let issued = issue_tokens(&config, &client.id, "u1", None).unwrap();
        age(&config, "tokens", ACCESS_LIFETIME + Duration::from_secs(1));
        assert!(access_token_holder(&config, &issued.access_token).is_none());
    }

    #[test]
    fn a_refresh_token_works_once_and_only_for_its_client() {
        let (_dir, config) = config();
        let client = register_client(&config, None, &["https://c.test/cb".into()]).unwrap();
        let other = register_client(&config, None, &["https://o.test/cb".into()]).unwrap();
        let issued = issue_tokens(&config, &client.id, "u1", None).unwrap();

        assert!(
            rotate_refresh(&config, &other.id, &issued.refresh_token).is_none(),
            "another client used the refresh token"
        );
        let next = rotate_refresh(&config, &client.id, &issued.refresh_token).expect("rotation");
        assert_ne!(next.refresh_token, issued.refresh_token);
        assert!(
            rotate_refresh(&config, &client.id, &issued.refresh_token).is_none(),
            "a retired refresh token was accepted"
        );
        assert!(access_token_holder(&config, &next.access_token).is_some());
    }

    #[test]
    fn an_idle_registration_is_swept_but_a_connected_one_stays() {
        let (_dir, config) = config();
        let idle = register_client(&config, None, &["https://c.test/cb".into()]).unwrap();
        let live = register_client(&config, None, &["https://c.test/cb".into()]).unwrap();
        issue_tokens(&config, &live.id, "u1", None).unwrap();
        {
            let conn = open(&config).unwrap();
            conn.execute(
                "update clients set created_at = created_at - ?",
                [(IDLE_CLIENT_LIFETIME.as_secs() + 1) as i64],
            )
            .unwrap();
        }
        // Any registration runs the sweep.
        register_client(&config, None, &["https://c.test/cb".into()]).unwrap();
        assert!(client(&config, &idle.id).is_none());
        assert!(client(&config, &live.id).is_some());
    }
}
