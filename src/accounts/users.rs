//! People who *use* published apps.
//!
//! Kept deliberately separate from `auth.rs`, which decides who may *publish*.
//! Conflating the two is how a visitor ends up holding a deploy token.
//!
//! Identity is global and permissions are per app: one account across the
//! site, with a grant naming which apps it may reach. That way a private app
//! is "these people", not "a shared password", and a person has one login.
//!
//! Sessions come in two tiers, because every app shares one origin. A *site*
//! session proves who the person is and nothing else. An *app* session is a
//! separate token, scoped to one app and delivered in a cookie the browser
//! only sends to `/p/<app>/`. A page can therefore never borrow the visitor's
//! standing with a neighbouring app: the credential is not in its jar to
//! send. Access to an app is granted by the handoff, never assumed from being
//! signed in.

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
/// An app session is re-minted by a redirect the visitor never sees, so a
/// tight ceiling costs nothing. It is also capped at the site session's own
/// expiry, so signing in once can never keep an app open for longer.
const APP_SESSION_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24);
pub const SESSION_COOKIE: &str = "ts_session";

/// One cookie per app. The name keeps a browser's jar readable; the `Path` is
/// what actually does the work — see `app_cookie_path`.
pub fn app_cookie_name(app: &str) -> String {
    format!("ts_app_{app}")
}

/// The whole isolation story in one line: a cookie is only sent to paths the
/// `Path` prefixes, so app A's script gets nothing back for `/p/appB/...`.
fn app_cookie_path(app: &str) -> String {
    format!("/p/{app}/")
}

/// An app session names one app, and its cookie name and `Path` are built
/// from that name. `valid_slug` also admits `a/b`, which would put a slash in
/// a cookie name and a second segment in the path, so a scope is stricter: a
/// single segment, which is exactly what the first segment of `/p/<app>/` is.
fn valid_app_scope(app: &str) -> bool {
    valid_slug(app) && !app.contains('/')
}

/// Lives under a dot-directory, which no slug can name: `valid_slug` refuses
/// a leading `.`, so no published app can ever collide with it or reach it.
fn site_db_path(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("auth.db")
}

fn open(config: &Config) -> Result<Connection, String> {
    // Migrations read `pragma user_version`, which the authorizer refuses, so
    // the schema is brought up to date before the door is closed.
    let mut conn = db::open_unguarded(&site_db_path(config))?;
    crate::accounts::schema::migrate(&mut conn)?;
    db::lock_down(&conn)?;
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
        "insert into sessions (token_hash, user_id, expires_at, scope) values (?, ?, ?, null)",
        rusqlite::params![hash_token(&token), &id, expires as i64],
    )
    .map_err(|e| e.to_string())?;

    Ok((User { id, email }, token))
}

/// Mints a token good for one app, from a proven site session. Returns the
/// token and how long it lives, which is the cookie's `Max-Age`.
///
/// This is the only way an app session comes into being, and it takes a site
/// session to do it — an app session cannot mint another, so holding one for
/// app A is not a step towards holding one for app B.
pub fn create_app_session(
    config: &Config,
    site_token: &str,
    app: &str,
) -> Result<(User, String, u64), String> {
    if !valid_app_scope(app) {
        return Err("invalid app name".into());
    }
    let conn = open(config)?;
    let now = now();
    let (id, email, site_expires): (String, String, i64) = conn
        .query_row(
            "select users.id, users.email, sessions.expires_at
               from sessions join users on users.id = sessions.user_id
              where sessions.token_hash = ? and sessions.expires_at >= ?
                and sessions.scope is null",
            rusqlite::params![hash_token(site_token), now as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| "not signed in".to_string())?;

    // Never outlives the site session it descends from — and a site session
    // with nothing left to lend hands back nothing, since `Max-Age=0` is a
    // deletion and the visitor would bounce between gate and handoff until it
    // finally expired.
    let expires = (now + APP_SESSION_LIFETIME.as_secs()).min(site_expires.max(0) as u64);
    if expires <= now {
        return Err("not signed in".into());
    }
    let token = crate::content::slug::random_token(48);
    conn.execute(
        "insert into sessions (token_hash, user_id, expires_at, scope) values (?, ?, ?, ?)",
        rusqlite::params![hash_token(&token), &id, expires as i64, app],
    )
    .map_err(|e| e.to_string())?;

    Ok((User { id, email }, token, expires.saturating_sub(now)))
}

pub fn log_out(config: &Config, token: &str) -> Result<(), String> {
    let conn = open(config)?;
    let hash = hash_token(token);
    // Every app session this person holds descends from a site session, and
    // the browser will not send a `/p/<app>/`-scoped cookie to `/auth/logout`
    // for us to clear, so the server is the only place they can die. Skipping
    // this would leave a scoped cookie working after sign-out.
    conn.execute(
        "delete from sessions
          where scope is not null
            and user_id = (select user_id from sessions where token_hash = ?)",
        [&hash],
    )
    .map_err(|e| e.to_string())?;
    conn.execute("delete from sessions where token_hash = ?", [&hash])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Who a *site* session token belongs to. A token scoped to an app is not
/// accepted here: it proves the bearer reached one app, not that it may act
/// site-wide.
pub fn site_session_user(config: &Config, token: &str) -> Option<User> {
    session_user(config, token, None)
}

/// Who an *app* session token belongs to, for that app alone. A token minted
/// for another app fails the scope test, which is the isolation this rests on
/// once a cookie has escaped its path by some other route.
pub fn app_session_user(config: &Config, token: &str, app: &str) -> Option<User> {
    if !valid_app_scope(app) {
        return None;
    }
    session_user(config, token, Some(app))
}

/// Nothing if the token is unknown, expired, or of the wrong tier.
fn session_user(config: &Config, token: &str, scope: Option<&str>) -> Option<User> {
    let conn = open(config).ok()?;
    // Expired rows are swept opportunistically rather than by a timer.
    let _ = conn.execute("delete from sessions where expires_at < ?", [now() as i64]);

    conn.query_row(
        "select users.id, users.email
           from sessions join users on users.id = sessions.user_id
          where sessions.token_hash = ? and sessions.expires_at >= ?
            and sessions.scope is ?",
        rusqlite::params![hash_token(token), now() as i64, scope],
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

fn cookie_value(header: Option<&str>, name: &str) -> Option<String> {
    header?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(cookie, _)| *cookie == name)
        .map(|(_, value)| value.to_string())
}

/// Reads the site session cookie out of a Cookie header.
pub fn token_from_cookies(header: Option<&str>) -> Option<String> {
    cookie_value(header, SESSION_COOKIE)
}

/// Reads one app's session cookie. A request under `/p/appB/` never carries
/// app A's cookie, so this returns nothing for a script asking on another
/// app's behalf.
pub fn app_token_from_cookies(header: Option<&str>, app: &str) -> Option<String> {
    cookie_value(header, &app_cookie_name(app))
}

pub fn set_cookie_header(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age={}",
        SESSION_LIFETIME.as_secs()
    )
}

/// Scoped to the app's own subtree. Everything else matches the site cookie:
/// out of reach of script, and never sent over plain HTTP.
pub fn set_app_cookie_header(app: &str, token: &str, max_age: u64) -> String {
    format!(
        "{name}={token}; Path={path}; HttpOnly; SameSite=Lax; Secure; Max-Age={max_age}",
        name = app_cookie_name(app),
        path = app_cookie_path(app),
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
        assert_eq!(site_session_user(&config, &token).unwrap(), user);
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
        assert!(site_session_user(&config, &token).is_none());
    }

    #[test]
    fn an_expired_session_stops_working() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, token) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();

        let conn = open(&config).unwrap();
        conn.execute("update sessions set expires_at = 1", []).unwrap();
        assert!(site_session_user(&config, &token).is_none());
    }

    #[test]
    fn an_invented_token_is_worthless() {
        let (_t, config) = config();
        assert!(site_session_user(&config, "not-a-real-token").is_none());
        assert!(app_session_user(&config, "not-a-real-token", "app").is_none());
    }

    /// The tiers are separate credentials, not two names for one.
    #[test]
    fn a_site_session_is_not_an_app_session_and_the_reverse() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, site) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, app, _) = create_app_session(&config, &site, "notes").unwrap();

        assert!(app_session_user(&config, &site, "notes").is_none());
        assert!(site_session_user(&config, &app).is_none());
        assert!(app_session_user(&config, &app, "notes").is_some());
    }

    #[test]
    fn an_app_session_only_speaks_for_its_own_app() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, site) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, notes, _) = create_app_session(&config, &site, "notes").unwrap();

        assert!(app_session_user(&config, &notes, "notes").is_some());
        assert!(app_session_user(&config, &notes, "invoices").is_none());
    }

    /// Otherwise reaching one app would be a step towards reaching the next.
    #[test]
    fn an_app_session_cannot_mint_another_app_session() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, site) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, notes, _) = create_app_session(&config, &site, "notes").unwrap();

        assert!(create_app_session(&config, &notes, "invoices").is_err());
    }

    #[test]
    fn an_app_session_needs_a_live_site_session_and_a_real_app_name() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, site) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();

        assert!(create_app_session(&config, "forged", "notes").is_err());
        for bad in ["../etc", "a/b", "", "with space"] {
            assert!(
                create_app_session(&config, &site, bad).is_err(),
                "minted a session scoped to {bad:?}"
            );
        }
    }

    #[test]
    fn an_app_session_never_outlives_the_site_session() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, site) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();

        // A site session about to expire cannot hand out a longer-lived one.
        let conn = open(&config).unwrap();
        let soon = now() + 60;
        conn.execute("update sessions set expires_at = ?", [soon as i64])
            .unwrap();

        let (_, _, max_age) = create_app_session(&config, &site, "notes").unwrap();
        assert!(max_age <= 60, "app session outlived its site session");

        let expires: i64 = conn
            .query_row(
                "select expires_at from sessions where scope = 'notes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(expires <= soon as i64);
    }

    #[test]
    fn signing_out_takes_every_app_session_with_it() {
        let (_t, config) = config();
        sign_up(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, site) = log_in(&config, "someone@example.com", "correct horse battery").unwrap();
        let (_, notes, _) = create_app_session(&config, &site, "notes").unwrap();
        let (_, invoices, _) = create_app_session(&config, &site, "invoices").unwrap();

        log_out(&config, &site).unwrap();
        assert!(site_session_user(&config, &site).is_none());
        assert!(app_session_user(&config, &notes, "notes").is_none());
        assert!(app_session_user(&config, &invoices, "invoices").is_none());
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
        let crowded = Some("theme=dark; ts_session=abc123; ts_app_notes=def456; other=1");
        assert_eq!(token_from_cookies(crowded).unwrap(), "abc123");
        assert_eq!(app_token_from_cookies(crowded, "notes").unwrap(), "def456");
        // A neighbour's cookie is not this app's, even in the same header.
        assert!(app_token_from_cookies(crowded, "invoices").is_none());
        assert!(token_from_cookies(Some("theme=dark")).is_none());
        assert!(token_from_cookies(None).is_none());
    }

    #[test]
    fn the_cookie_is_not_reachable_from_script_or_plain_http() {
        for header in [set_cookie_header("abc"), set_app_cookie_header("notes", "abc", 60)] {
            assert!(header.contains("HttpOnly"), "{header}");
            assert!(header.contains("Secure"), "{header}");
            assert!(header.contains("SameSite=Lax"), "{header}");
        }
    }

    #[test]
    fn an_apps_cookie_is_confined_to_that_apps_path() {
        let header = set_app_cookie_header("notes", "abc", 60);
        assert!(header.contains("Path=/p/notes/"), "{header}");
        assert!(header.starts_with("ts_app_notes=abc;"), "{header}");
        // The site cookie is the one thing that stays origin-wide.
        assert!(set_cookie_header("abc").contains("Path=/;"));
    }

    #[test]
    fn a_scope_must_be_one_path_segment_so_it_fits_a_cookie() {
        assert!(valid_app_scope("notes"));
        assert!(valid_app_scope("my-app_2"));
        for bad in ["", "a/b", "..", "with space", "semi;colon", ".hidden"] {
            assert!(!valid_app_scope(bad), "accepted {bad:?} as a scope");
        }
    }

    #[test]
    fn a_background_request_is_not_a_visitor_navigating() {
        fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, value.parse().unwrap());
            }
            map
        }
        // A real navigation, and a client that sends no fetch metadata.
        assert!(is_visitor_navigation(&headers(&[
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "document"),
        ])));
        assert!(is_visitor_navigation(&headers(&[])));
        // What a script gets to send.
        assert!(!is_visitor_navigation(&headers(&[
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-dest", "empty"),
        ])));
        assert!(!is_visitor_navigation(&headers(&[
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "iframe"),
        ])));
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
pub(crate) fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(path) if path.starts_with('/') && !path.starts_with("//") => path.to_string(),
        _ => "/".to_string(),
    }
}

/// Whether the browser says this request is the visitor navigating, rather
/// than a page fetching something in the background.
///
/// This is what stops the handoff from being a way around the scoping it
/// exists to serve: without it, a script in app A could `fetch('/auth/handoff
/// ?app=appB')`, the browser would attach the site cookie, and app B's cookie
/// would land in the jar for app A to then use. Fetch metadata headers are
/// forbidden header names, so script cannot forge them, and every browser
/// modern enough to be a threat here sends them. A client that sends none at
/// all — curl, a script on the server side — is taken at face value.
pub(crate) fn is_visitor_navigation(headers: &HeaderMap) -> bool {
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_ascii_lowercase())
    };
    if value("sec-fetch-mode").is_some_and(|mode| mode != "navigate") {
        return false;
    }
    // An iframe is `iframe`, not `document`, so app A cannot mint a cookie by
    // framing app B either.
    if value("sec-fetch-dest").is_some_and(|dest| dest != "document") {
        return false;
    }
    true
}

/// One header's value for logging. Only ever called with fetch metadata,
/// which carries nothing secret.
fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> &'h str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
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
    match current_site_user(&config, &headers).await {
        Some(user) => Json(serde_json::json!({ "id": user.id, "email": user.email })).into_response(),
        None => (StatusCode::UNAUTHORIZED, "not signed in").into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct HandoffParams {
    app: String,
    next: Option<String>,
}

/// Trades a site session for a session scoped to one app.
///
/// This is the only door between the two tiers. Being signed in does not
/// admit anyone anywhere by itself; walking through here does, for one app,
/// and hands back a cookie the browser will only ever send to that app.
pub async fn handoff(
    State(config): State<Arc<Config>>,
    Query(params): Query<HandoffParams>,
    headers: HeaderMap,
) -> Response {
    let next = safe_next(params.next.as_deref());
    if params.next.as_deref().is_some_and(|asked| asked != next) {
        tracing::warn!(
            next = %params.next.as_deref().unwrap_or_default(),
            "handoff refused an off-site next; going to the site root instead"
        );
    }
    if !valid_app_scope(&params.app) {
        // Header names only: a Cookie header's value is a live session.
        let header_names: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
        tracing::warn!(
            app = %params.app,
            headers = ?header_names,
            "handoff refused: not an app name"
        );
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if !is_visitor_navigation(&headers) {
        tracing::warn!(
            app = %params.app,
            mode = %header_str(&headers, "sec-fetch-mode"),
            dest = %header_str(&headers, "sec-fetch-dest"),
            "handoff refused: not a navigation"
        );
        return (
            StatusCode::FORBIDDEN,
            "an app session is issued to a visitor, not to a script",
        )
            .into_response();
    }

    let sign_in = || {
        Redirect::to(&format!(
            "/auth/login?next={}",
            urlencoding::encode(&next)
        ))
        .into_response()
    };
    let Some(site_token) = token_from_cookies(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    ) else {
        return sign_in();
    };

    let app = params.app.clone();
    let outcome =
        tokio::task::spawn_blocking(move || create_app_session(&config, &site_token, &app)).await;

    match outcome {
        Ok(Ok((_, token, max_age))) => (
            [(
                header::SET_COOKIE,
                set_app_cookie_header(&params.app, &token, max_age),
            )],
            Redirect::to(&next),
        )
            .into_response(),
        // An expired or forged site session is not an error to report, it is
        // a reason to sign in again.
        Ok(Err(_)) => sign_in(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "handoff failed").into_response(),
    }
}

/// Resolves the caller from their *site* session cookie, if any. Says who the
/// person is; says nothing about what they may reach.
pub async fn current_site_user(config: &Arc<Config>, headers: &HeaderMap) -> Option<User> {
    let token = token_from_cookies(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    )?;
    let config = config.clone();
    tokio::task::spawn_blocking(move || site_session_user(&config, &token))
        .await
        .ok()
        .flatten()
}

/// Resolves the caller from the cookie scoped to `app`, and only that cookie.
/// This is what an app's gate and its `identity.current-user` import run on.
pub async fn current_app_user(config: &Arc<Config>, app: &str, headers: &HeaderMap) -> Option<User> {
    let token = app_token_from_cookies(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
        app,
    )?;
    let (config, app) = (config.clone(), app.to_string());
    tokio::task::spawn_blocking(move || app_session_user(&config, &token, &app))
        .await
        .ok()
        .flatten()
}
