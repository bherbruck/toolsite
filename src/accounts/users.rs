//! People who *use* published apps.
//!
//! Kept deliberately separate from `auth.rs`, which decides who may *publish*.
//! Conflating the two is how a visitor ends up holding a deploy token.
//!
//! Identity is global and permissions are per app: one account across the
//! site, with a grant naming which apps it may reach. That way a private app
//! is "these people", not "a shared password", and a person has one login.

use crate::{config::Config, content::slug::valid_slug, runtime::db};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Sessions last a fortnight; long enough not to nag, short enough that a
/// stolen cookie expires.
const SESSION_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24 * 14);
pub const SESSION_COOKIE: &str = "ts_session";

/// Lives under a dot-directory, which no slug can name: `valid_slug` refuses
/// a leading `.`, so no published app can ever collide with it or reach it.
fn site_db_path(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("auth.db")
}

fn open(config: &Config) -> Result<Connection, String> {
    let conn = db::open_at(&site_db_path(config))?;
    conn.execute_batch(
        "create table if not exists users (
            id            text primary key,
            email         text not null unique,
            password_hash text,
            created_at    integer not null
         );
         -- One account, several ways to sign in. Empty until a provider is
         -- wired up, but adding it later would mean migrating live accounts.
         create table if not exists identities (
            provider    text not null,
            provider_id text not null,
            user_id     text not null,
            primary key (provider, provider_id)
         );
         create table if not exists sessions (
            token_hash text primary key,
            user_id    text not null,
            expires_at integer not null
         );
         create table if not exists grants (
            user_id text not null,
            app     text not null,
            role    text not null,
            primary key (user_id, app)
         );",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: String,
    pub email: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Sessions are stored hashed, so a leaked database does not hand over live
/// sessions the way a leaked table of raw tokens would.
fn hash_token(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn new_salt() -> Result<SaltString, String> {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    SaltString::encode_b64(&bytes).map_err(|e| e.to_string())
}

fn normalise(email: &str) -> String {
    email.trim().to_lowercase()
}

pub fn sign_up(config: &Config, email: &str, password: &str) -> Result<User, String> {
    let email = normalise(email);
    if !email.contains('@') || email.len() < 3 {
        return Err("that does not look like an email address".into());
    }
    if password.chars().count() < 8 {
        return Err("password must be at least 8 characters".into());
    }

    let salt = new_salt()?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| e.to_string())?
        .to_string();

    let conn = open(config)?;
    let id = crate::content::slug::random_token(16);
    conn.execute(
        "insert into users (id, email, password_hash, created_at) values (?, ?, ?, ?)",
        rusqlite::params![&id, &email, &hash, now() as i64],
    )
    .map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            "that email is already registered".to_string()
        } else {
            e.to_string()
        }
    })?;

    Ok(User { id, email })
}

/// Returns a session token on success. The same message is given whether the
/// email is unknown or the password is wrong, so this cannot be used to
/// enumerate accounts.
pub fn log_in(config: &Config, email: &str, password: &str) -> Result<(User, String), String> {
    let conn = open(config)?;
    let email = normalise(email);

    let found: Option<(String, Option<String>)> = conn
        .query_row(
            "select id, password_hash from users where email = ?",
            [&email],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    // A row with no password signed up through a provider, so there is
    // nothing here to verify against.
    let Some((id, Some(stored))) = found else {
        // Spend comparable time on an unknown address so timing does not leak
        // which half was wrong.
        if let Ok(salt) = new_salt() {
            let _ = Argon2::default().hash_password(password.as_bytes(), &salt);
        }
        return Err("email or password is incorrect".into());
    };

    let parsed = PasswordHash::new(&stored).map_err(|e| e.to_string())?;
    if Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_err()
    {
        return Err("email or password is incorrect".into());
    }

    let token = crate::content::slug::random_token(48);
    let expires = now() + SESSION_LIFETIME.as_secs();
    conn.execute(
        "insert into sessions (token_hash, user_id, expires_at) values (?, ?, ?)",
        rusqlite::params![hash_token(&token), &id, expires as i64],
    )
    .map_err(|e| e.to_string())?;

    Ok((User { id, email }, token))
}

pub fn log_out(config: &Config, token: &str) -> Result<(), String> {
    let conn = open(config)?;
    conn.execute(
        "delete from sessions where token_hash = ?",
        [hash_token(token)],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Who a session token belongs to, or nothing if it is unknown or expired.
pub fn session_user(config: &Config, token: &str) -> Option<User> {
    let conn = open(config).ok()?;
    // Expired rows are swept opportunistically rather than by a timer.
    let _ = conn.execute("delete from sessions where expires_at < ?", [now() as i64]);

    conn.query_row(
        "select users.id, users.email
           from sessions join users on users.id = sessions.user_id
          where sessions.token_hash = ? and sessions.expires_at >= ?",
        rusqlite::params![hash_token(token), now() as i64],
        |row| {
            Ok(User {
                id: row.get(0)?,
                email: row.get(1)?,
            })
        },
    )
    .ok()
}

pub fn grant(config: &Config, email: &str, app: &str, role: &str) -> Result<(), String> {
    if !valid_slug(app) {
        return Err("invalid app name".into());
    }
    let conn = open(config)?;
    let email = normalise(email);
    let user_id: String = conn
        .query_row("select id from users where email = ?", [&email], |row| {
            row.get(0)
        })
        .map_err(|_| format!("no account for {email}"))?;

    conn.execute(
        "insert into grants (user_id, app, role) values (?, ?, ?)
         on conflict(user_id, app) do update set role = excluded.role",
        rusqlite::params![user_id, app, role],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn revoke(config: &Config, email: &str, app: &str) -> Result<(), String> {
    let conn = open(config)?;
    let email = normalise(email);
    conn.execute(
        "delete from grants where app = ? and user_id in (select id from users where email = ?)",
        rusqlite::params![app, email],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn has_grant(config: &Config, user: &User, app: &str) -> bool {
    let Ok(conn) = open(config) else {
        return false;
    };
    conn.query_row(
        "select 1 from grants where user_id = ? and app = ?",
        rusqlite::params![&user.id, app],
        |_| Ok(()),
    )
    .is_ok()
}

/// Reads the session cookie out of a Cookie header.
pub fn token_from_cookies(header: Option<&str>) -> Option<String> {
    header?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_string())
}

pub fn set_cookie_header(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age={}",
        SESSION_LIFETIME.as_secs()
    )
}

pub fn clear_cookie_header() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        (
            tempfile::tempdir().unwrap(),
            Config::local(dir.keep(), "test-token", true),
        )
    }

    #[test]
    fn a_password_is_never_stored_in_the_clear() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();

        let conn = open(&config).unwrap();
        let stored: String = conn
            .query_row("select password_hash from users", [], |row| row.get(0))
            .unwrap();
        assert!(!stored.contains("correct horse"));
        assert!(stored.starts_with("$argon2"), "got {stored}");
    }

    #[test]
    fn signing_in_needs_the_right_password() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();

        assert!(log_in(&config, "someone@example.com", "wrong").is_err());
        let (user, token) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();
        assert_eq!(user.email, "someone@example.com");
        assert_eq!(session_user(&config, &token).unwrap(), user);
    }

    #[test]
    fn a_wrong_password_and_an_unknown_account_are_indistinguishable() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();

        let wrong = log_in(&config, "someone@example.com", "nope").unwrap_err();
        let missing = log_in(&config, "nobody@example.com", "nope").unwrap_err();
        assert_eq!(wrong, missing, "the error tells an attacker which exists");
    }

    #[test]
    fn email_case_and_spacing_do_not_create_a_second_account() {
        let (_t, config) = config();
        sign_up(&config, "Someone@Example.com", "correct horse battery").unwrap();
        assert!(sign_up(&config, "  someone@example.com ", "another password").is_err());
        assert!(log_in(&config, "SOMEONE@EXAMPLE.COM", "correct horse battery").is_ok());
    }

    #[test]
    fn weak_input_is_refused() {
        let (_t, config) = config();
        assert!(sign_up(&config, "not-an-email", "correct horse battery").is_err());
        assert!(sign_up(&config, "someone@example.com", "short").is_err());
    }

    #[test]
    fn sessions_are_stored_hashed_so_the_table_is_not_a_set_of_keys() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, token) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();

        let conn = open(&config).unwrap();
        let stored: String = conn
            .query_row("select token_hash from sessions", [], |row| row.get(0))
            .unwrap();
        assert_ne!(stored, token);
        assert_eq!(stored, hash_token(&token));
    }

    #[test]
    fn logging_out_ends_the_session() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, token) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();

        log_out(&config, &token).unwrap();
        assert!(session_user(&config, &token).is_none());
    }

    #[test]
    fn an_expired_session_stops_working() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, token) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();

        let conn = open(&config).unwrap();
        conn.execute("update sessions set expires_at = 1", []).unwrap();
        assert!(session_user(&config, &token).is_none());
    }

    #[test]
    fn an_invented_token_is_worthless() {
        let (_t, config) = config();
        assert!(session_user(&config, "not-a-real-token").is_none());
    }

    #[test]
    fn grants_are_per_app() {
        let (_t, config) = config();
        let user = sign_up(&config, "someone@example.com", "correct horse battery").unwrap();

        assert!(!has_grant(&config, &user, "private"));
        grant(&config, "someone@example.com", "private", "viewer").unwrap();
        assert!(has_grant(&config, &user, "private"));
        // A grant on one app says nothing about another.
        assert!(!has_grant(&config, &user, "other"));

        revoke(&config, "someone@example.com", "private").unwrap();
        assert!(!has_grant(&config, &user, "private"));
    }

    #[test]
    fn the_session_cookie_is_read_from_a_crowded_header() {
        assert_eq!(
            token_from_cookies(Some("theme=dark; ts_session=abc123; other=1")).unwrap(),
            "abc123"
        );
        assert!(token_from_cookies(Some("theme=dark")).is_none());
        assert!(token_from_cookies(None).is_none());
    }

    #[test]
    fn the_cookie_is_not_reachable_from_script_or_plain_http() {
        let header = set_cookie_header("abc");
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Secure"));
        assert!(header.contains("SameSite=Lax"));
    }
}

// --- HTTP surface -------------------------------------------------------
//
// Deliberately small: sign-in, sign-out, and "who am I". Accounts are created
// by the owner over MCP rather than by open registration, so there is no
// public signup route to abuse.

use axum::{
    extract::{Form, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use std::sync::Arc;

#[derive(serde::Deserialize)]
pub struct NextPage {
    next: Option<String>,
}

/// Only same-site paths, so `?next=` cannot bounce a signed-in visitor to
/// somebody else's domain.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(path) if path.starts_with('/') && !path.starts_with("//") => path.to_string(),
        _ => "/".to_string(),
    }
}

pub async fn login_form(Query(params): Query<NextPage>) -> Response {
    let next = crate::content::slug::escape_html(&safe_next(params.next.as_deref()));
    Html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in</title>
<style>
  body {{ font: 16px/1.6 system-ui, sans-serif; color-scheme: light dark;
         display: grid; place-items: center; min-height: 100vh; margin: 0; }}
  form {{ display: flex; flex-direction: column; gap: .75rem; width: min(20rem, 90vw); }}
  input {{ padding: .6rem .7rem; border-radius: .4rem; border: 1px solid #8884;
          background: transparent; color: inherit; font-size: 1rem; }}
  button {{ padding: .6rem; border-radius: .4rem; border: 0; font-size: 1rem;
           background: #4f46e5; color: #fff; cursor: pointer; }}
  h1 {{ font-size: 1.2rem; margin: 0 0 .5rem; }}
</style></head>
<body>
<form method="post" action="/auth/login">
  <h1>Sign in</h1>
  <input type="hidden" name="next" value="{next}">
  <input name="email" type="email" placeholder="Email" autocomplete="username" required autofocus>
  <input name="password" type="password" placeholder="Password" autocomplete="current-password" required>
  <button type="submit">Sign in</button>
</form>
</body></html>"#
    ))
    .into_response()
}

#[derive(serde::Deserialize)]
pub struct Credentials {
    email: String,
    password: String,
    next: Option<String>,
}

pub async fn login_submit(
    State(config): State<Arc<Config>>,
    Form(credentials): Form<Credentials>,
) -> Response {
    let next = safe_next(credentials.next.as_deref());
    let outcome = tokio::task::spawn_blocking(move || {
        log_in(&config, &credentials.email, &credentials.password)
    })
    .await;

    match outcome {
        Ok(Ok((_, token))) => (
            [(header::SET_COOKIE, set_cookie_header(&token))],
            Redirect::to(&next),
        )
            .into_response(),
        Ok(Err(message)) => (StatusCode::UNAUTHORIZED, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "sign-in failed").into_response(),
    }
}

pub async fn logout(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    if let Some(token) = token_from_cookies(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    ) {
        let _ = tokio::task::spawn_blocking(move || log_out(&config, &token)).await;
    }
    (
        [(header::SET_COOKIE, clear_cookie_header())],
        Redirect::to("/"),
    )
        .into_response()
}

pub async fn me(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    match current_user(&config, &headers).await {
        Some(user) => Json(serde_json::json!({ "id": user.id, "email": user.email })).into_response(),
        None => (StatusCode::UNAUTHORIZED, "not signed in").into_response(),
    }
}

/// Resolves the caller from their session cookie, if any.
pub async fn current_user(config: &Arc<Config>, headers: &HeaderMap) -> Option<User> {
    let token = token_from_cookies(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    )?;
    let config = config.clone();
    tokio::task::spawn_blocking(move || session_user(&config, &token))
        .await
        .ok()
        .flatten()
}
