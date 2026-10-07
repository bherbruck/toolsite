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
//! only sends to `/p/<app>/`. That keeps a page from *reading* a neighbour's
//! credential, and the host strips every `ts_` cookie before app code sees a
//! request. It does not stop a script in app A from *sending* a request to
//! `/p/<appB>/`: on one origin the browser attaches B's cookie to it, so A
//! can act as the visitor towards B. Subdomain mode closes that: each app
//! has its own host and a host-only cookie there, which the handoff sets
//! through a one-time code (see `handoff_to_app_host`). In path mode see
//! `platform::shield` for what is closed. Access to an app is granted by the
//! handoff, never assumed from being signed in.

use crate::{config::Config, content::slug::valid_slug, runtime::db};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension};
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
/// Whether a cookie name is one toolsite itself sets: the site session, an
/// app session, the admin flash. Every one starts `ts_`, and an app may
/// neither read them nor set them.
pub fn is_platform_cookie(name: &str) -> bool {
    // `__Host-ts_app` is ours too: the prefix only tells the browser how
    // to keep the cookie.
    let name = name.trim().to_ascii_lowercase();
    let name = name
        .strip_prefix("__host-")
        .or_else(|| name.strip_prefix("__secure-"))
        .unwrap_or(&name);
    name.starts_with("ts_")
}

/// A `Cookie` header with toolsite's own cookies taken out, or `None` when
/// nothing is left. A handler is the app author's code: it must never see
/// the visitor's site session (which would let it act as them anywhere on
/// the site) or another app's session, only cookies the app set itself.
pub fn without_platform_cookies(header: &str) -> Option<String> {
    let kept: Vec<&str> = header
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .filter(|pair| !is_platform_cookie(pair.split('=').next().unwrap_or("")))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

/// Whether a `Set-Cookie` value from a handler names a cookie toolsite owns.
/// Such a header is dropped: an app setting `ts_session` could sign a
/// visitor in as someone else (session fixation) or overwrite another app's
/// session.
pub fn sets_platform_cookie(set_cookie: &str) -> bool {
    is_platform_cookie(set_cookie.split(['=', ';']).next().unwrap_or(""))
}

pub fn app_cookie_name(app: &str) -> String {
    format!("ts_app_{app}")
}

/// A cookie is only attached to paths the `Path` prefixes, so app A's pages
/// never see B's cookie. A script in A can still send a request to
/// `/p/appB/...` and the browser attaches B's cookie to it: scoping by path
/// on one origin limits reading, not sending.
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
    let mut conn = db::open_unguarded(&site_db_path(config), config.max_db_bytes)?;
    crate::accounts::schema::migrate(&mut conn)?;
    db::lock_down(&conn)?;
    Ok(conn)
}

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: String,
    pub email: String,
    /// May see and change other people's access. Not a grant, because it is
    /// not about any one app.
    pub is_admin: bool,
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

/// The one place a password becomes a hash, so the parameters cannot drift
/// between sign-up, a setup link and a change.
fn hash_password(password: &str) -> Result<String, String> {
    let salt = new_salt()?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| e.to_string())?
        .to_string())
}

fn verify_password(stored: &str, password: &str) -> bool {
    PasswordHash::new(stored)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

pub fn sign_up(config: &Config, email: &str, password: &str) -> Result<User, String> {
    sign_up_as(config, email, password, false)
}

/// The admin flag is set here rather than granted later so the first account
/// can be made one at creation, before any admin exists to do it.
pub fn sign_up_as(
    config: &Config,
    email: &str,
    password: &str,
    is_admin: bool,
) -> Result<User, String> {
    let email = normalise(email);
    if !email.contains('@') || email.len() < 3 {
        return Err("Enter a valid email address.".into());
    }
    if password.chars().count() < 8 {
        return Err("Enter a password of at least 8 characters.".into());
    }

    let hash = hash_password(password)?;

    let conn = open(config)?;
    let id = crate::content::slug::random_token(16);
    conn.execute(
        "insert into users (id, email, password_hash, created_at, is_admin)
         values (?, ?, ?, ?, ?)",
        rusqlite::params![&id, &email, &hash, now() as i64, is_admin as i64],
    )
    .map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            "An account with this email exists.".to_string()
        } else {
            e.to_string()
        }
    })?;

    Ok(User {
        id,
        email,
        is_admin,
    })
}

/// Returns a session token on success. The same message is given whether the
/// email is unknown or the password is wrong, so this cannot be used to
/// enumerate accounts.
pub fn log_in(config: &Config, email: &str, password: &str) -> Result<(User, String), String> {
    let conn = open(config)?;
    let email = normalise(email);

    let found: Option<(String, Option<String>, bool)> = conn
        .query_row(
            "select id, password_hash, is_admin from users
              where email = ? and disabled_at is null",
            [&email],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)),
        )
        .ok();

    // A row with no password signed up through a provider, so there is
    // nothing here to verify against.
    let Some((id, Some(stored), is_admin)) = found else {
        // Spend comparable time on an unknown address so timing does not leak
        // which half was wrong.
        if let Ok(salt) = new_salt() {
            let _ = Argon2::default().hash_password(password.as_bytes(), &salt);
        }
        return Err("The email or password is not correct.".into());
    };

    if !verify_password(&stored, password) {
        return Err("The email or password is not correct.".into());
    }

    let token = crate::content::slug::random_token(48);
    let expires = now() + SESSION_LIFETIME.as_secs();
    conn.execute(
        "insert into sessions (token_hash, user_id, expires_at, scope) values (?, ?, ?, null)",
        rusqlite::params![hash_token(&token), &id, expires as i64],
    )
    .map_err(|e| e.to_string())?;

    Ok((User { id, email, is_admin }, token))
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
    let (id, email, is_admin, site_expires): (String, String, bool, i64) = conn
        .query_row(
            "select users.id, users.email, users.is_admin, sessions.expires_at
               from sessions join users on users.id = sessions.user_id
              where sessions.token_hash = ? and sessions.expires_at >= ?
                and sessions.scope is null and users.disabled_at is null",
            rusqlite::params![hash_token(site_token), now as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0, row.get(3)?)),
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

    Ok((User { id, email, is_admin }, token, expires.saturating_sub(now)))
}

/// An app session for an account with no site session behind it: what a
/// preview render signs in with. Short-lived by the caller's choice and
/// scoped to one app, so the cookie it becomes opens that app and nothing
/// else. Nothing on the request side can mint one; only the screenshot path
/// does, for an account an admin named.
pub fn create_app_session_for(
    config: &Config,
    user_id: &str,
    app: &str,
    lifetime: Duration,
) -> Result<(String, u64), String> {
    if !valid_app_scope(app) {
        return Err("invalid app name".into());
    }
    let conn = open(config)?;
    let active: bool = conn
        .query_row(
            "select count(*) from users where id = ? and disabled_at is null",
            [user_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .map_err(|e| e.to_string())?;
    if !active {
        return Err("no such active account".into());
    }
    let now = now();
    let max_age = lifetime.as_secs().min(APP_SESSION_LIFETIME.as_secs()).max(1);
    let token = crate::content::slug::random_token(48);
    conn.execute(
        "insert into sessions (token_hash, user_id, expires_at, scope) values (?, ?, ?, ?)",
        rusqlite::params![hash_token(&token), user_id, (now + max_age) as i64, app],
    )
    .map_err(|e| e.to_string())?;
    Ok((token, max_age))
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

/// The account behind an id, if it is still active. What anything holding a
/// user id across a boundary asks before acting on it, so a disabled account
/// is refused wherever its id has been remembered.
pub fn user_by_id(config: &Config, id: &str) -> Option<User> {
    let conn = open(config).ok()?;
    conn.query_row(
        "select id, email, is_admin from users where id = ? and disabled_at is null",
        [id],
        |row| {
            Ok(User {
                id: row.get(0)?,
                email: row.get(1)?,
                is_admin: row.get::<_, i64>(2)? != 0,
            })
        },
    )
    .ok()
}

// --- signing in through a provider ---------------------------------------
//
// The provider proves an email; these decide what account that is. Kept
// here because they touch the users and identities tables; the protocol
// itself lives in `providers.rs`.

/// The account a provider identity was linked to before, if any. Nothing for
/// a disabled account: a provider login is still a login.
/// The active account with this email, if there is one.
pub fn user_by_email(config: &Config, email: &str) -> Option<User> {
    let conn = open(config).ok()?;
    conn.query_row(
        "select id, email, is_admin from users where email = ? and disabled_at is null",
        [normalise(email)],
        |row| {
            Ok(User {
                id: row.get(0)?,
                email: row.get(1)?,
                is_admin: row.get::<_, i64>(2)? != 0,
            })
        },
    )
    .ok()
}

pub fn user_by_identity(config: &Config, provider: &str, provider_id: &str) -> Option<User> {
    let conn = open(config).ok()?;
    conn.query_row(
        "select users.id, users.email, users.is_admin
           from identities join users on users.id = identities.user_id
          where identities.provider = ? and identities.provider_id = ?
            and users.disabled_at is null",
        rusqlite::params![provider, provider_id],
        |row| {
            Ok(User {
                id: row.get(0)?,
                email: row.get(1)?,
                is_admin: row.get::<_, i64>(2)? != 0,
            })
        },
    )
    .ok()
}

/// What stands at an email: an active account, a disabled one, or nothing.
/// Three answers rather than two because a disabled account must be refused
/// outright, not treated as room to create a new one.
pub enum AtEmail {
    Active(User),
    Disabled,
    Nobody,
}

pub fn account_at_email(config: &Config, email: &str) -> Result<AtEmail, String> {
    let conn = open(config)?;
    let email = normalise(email);
    let found: Option<(String, bool, Option<i64>)> = conn
        .query_row(
            "select id, is_admin, disabled_at from users where email = ?",
            [&email],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0, row.get(2)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(match found {
        None => AtEmail::Nobody,
        Some((_, _, Some(_))) => AtEmail::Disabled,
        Some((id, is_admin, None)) => AtEmail::Active(User { id, email, is_admin }),
    })
}

/// Remembers that this provider identity is this account, so the next sign-in
/// does not depend on the email staying the same.
pub fn link_identity(config: &Config, provider: &str, provider_id: &str, user_id: &str) -> Result<(), String> {
    let conn = open(config)?;
    conn.execute(
        "insert into identities (provider, provider_id, user_id) values (?, ?, ?)
         on conflict(provider, provider_id) do update set user_id = excluded.user_id",
        rusqlite::params![provider, provider_id, user_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// An account with no password, the way an invitation creates one: the
/// provider is how this person signs in. Never an admin.
pub fn create_provider_account(config: &Config, email: &str) -> Result<User, String> {
    let email = normalise(email);
    if !email.contains('@') || email.len() < 3 {
        return Err("Enter a valid email address.".into());
    }
    let conn = open(config)?;
    let id = crate::content::slug::random_token(16);
    conn.execute(
        "insert into users (id, email, password_hash, created_at, is_admin)
         values (?, ?, null, ?, 0)",
        rusqlite::params![&id, &email, now() as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(User {
        id,
        email,
        is_admin: false,
    })
}

/// Whether the account has a password at all. One made through a provider
/// does not, and has nothing to change.
pub fn has_password(config: &Config, user_id: &str) -> bool {
    let Ok(conn) = open(config) else {
        return false;
    };
    conn.query_row(
        "select password_hash is not null from users where id = ?",
        [user_id],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

/// The providers an account signs in through, by name, for the account page.
pub fn identities_for(config: &Config, user_id: &str) -> Vec<String> {
    let Ok(conn) = open(config) else {
        return Vec::new();
    };
    let Ok(mut statement) = conn.prepare(
        "select provider from identities where user_id = ? order by provider",
    ) else {
        return Vec::new();
    };
    statement
        .query_map([user_id], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// A person changes their own password: the current one has to be right,
/// the new one has to be long enough, and every session but the one they
/// are using ends, so a password changed because of a leak closes whatever
/// the leak opened. `current_session` is the raw site token from the cookie.
pub fn change_password(
    config: &Config,
    user_id: &str,
    current: &str,
    new: &str,
    current_session: &str,
) -> Result<(), String> {
    if new.chars().count() < 8 {
        return Err("Enter a new password of at least 8 characters.".into());
    }
    let conn = open(config)?;
    let stored: Option<String> = conn
        .query_row(
            "select password_hash from users where id = ? and disabled_at is null",
            [user_id],
            |row| row.get(0),
        )
        .map_err(|_| "no such account".to_string())?;
    let Some(stored) = stored else {
        return Err("This account signs in through a provider. It has no password.".into());
    };
    if !verify_password(&stored, current) {
        return Err("The current password is not correct.".into());
    }
    let hash = hash_password(new)?;
    conn.execute(
        "update users set password_hash = ? where id = ?",
        rusqlite::params![&hash, user_id],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "delete from sessions where user_id = ? and token_hash != ?",
        rusqlite::params![user_id, hash_token(current_session)],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// A site session for an account whose identity was proven some other way
/// than a password. Refuses a disabled account, like every other door.
pub fn start_session(config: &Config, user_id: &str) -> Result<String, String> {
    let conn = open(config)?;
    let active: bool = conn
        .query_row(
            "select disabled_at is null from users where id = ?",
            [user_id],
            |row| row.get(0),
        )
        .map_err(|_| "no such account".to_string())?;
    if !active {
        return Err("This account is disabled.".into());
    }
    let token = crate::content::slug::random_token(48);
    let expires = now() + SESSION_LIFETIME.as_secs();
    conn.execute(
        "insert into sessions (token_hash, user_id, expires_at, scope) values (?, ?, ?, null)",
        rusqlite::params![hash_token(&token), user_id, expires as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(token)
}

/// The apps whose tools this account wants listed on its connector.
pub fn pins_for(config: &Config, user_id: &str) -> Vec<String> {
    let Ok(conn) = open(config) else {
        return Vec::new();
    };
    let Ok(mut statement) = conn.prepare("select app from pins where user_id = ? order by app") else {
        return Vec::new();
    };
    statement
        .query_map([user_id], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// Pins or unpins an app's tools for this account. Whether the account may
/// open the app is the caller's question; a pin on an app it cannot open
/// lists nothing.
pub fn set_pin(config: &Config, user_id: &str, app: &str, pinned: bool) -> Result<(), String> {
    if !valid_app_scope(app) {
        return Err("invalid app name".into());
    }
    let conn = open(config)?;
    if pinned {
        conn.execute(
            "insert into pins (user_id, app, created_at) values (?, ?, ?) on conflict(user_id, app) do nothing",
            rusqlite::params![user_id, app, now() as i64],
        )
        .map_err(|e| e.to_string())?;
    } else {
        conn.execute("delete from pins where user_id = ? and app = ?", rusqlite::params![user_id, app])
            .map_err(|e| e.to_string())?;
    }
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
        "select users.id, users.email, users.is_admin
           from sessions join users on users.id = sessions.user_id
          where sessions.token_hash = ? and sessions.expires_at >= ?
            and sessions.scope is ? and users.disabled_at is null",
        rusqlite::params![hash_token(token), now() as i64, scope],
        |row| {
            Ok(User {
                id: row.get(0)?,
                email: row.get(1)?,
                is_admin: row.get::<_, i64>(2)? != 0,
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

/// What this account was granted on this app, if anything.
pub fn role_for(config: &Config, user_id: &str, app: &str) -> Option<String> {
    let conn = open(config).ok()?;
    conn.query_row(
        "select role from grants where user_id = ? and app = ?",
        rusqlite::params![user_id, app],
        |row| row.get(0),
    )
    .ok()
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

// --- platform scopes ----------------------------------------------------
//
// A grant's role is the app's business. A scope is the platform's: it says
// what an account may do to the platform from one point of the project tree
// down, in three words the platform itself acts on.

/// Ordered: a stronger scope can do everything a weaker one can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Viewer,
    Editor,
    Admin,
}

impl Scope {
    pub fn parse(word: &str) -> Option<Scope> {
        match word {
            "viewer" => Some(Scope::Viewer),
            "editor" => Some(Scope::Editor),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Viewer => "viewer",
            Scope::Editor => "editor",
            Scope::Admin => "admin",
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One scope row: who holds what, where.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeGrant {
    pub email: String,
    pub user_id: String,
    pub prefix: String,
    pub scope: Scope,
}

/// A prefix is the root (empty), a folder path, or an app's path. Segments
/// follow the slug rules, so a prefix can never name `.site` or climb.
pub fn valid_prefix(prefix: &str) -> bool {
    prefix.is_empty() || valid_slug(prefix)
}

/// Whether a scope at `prefix` covers `path`: the root covers everything,
/// otherwise the path is the prefix or sits under it.
pub fn prefix_covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}

pub fn grant_scope(
    config: &Config,
    email: &str,
    prefix: &str,
    scope: Scope,
    granted_by: Option<&str>,
) -> Result<(), String> {
    if !valid_prefix(prefix) {
        return Err("prefix must be empty, or path segments of letters, numbers, '-' or '_'".into());
    }
    let conn = open(config)?;
    let email = normalise(email);
    let user_id: String = conn
        .query_row("select id from users where email = ?", [&email], |row| row.get(0))
        .map_err(|_| format!("no account for {email}"))?;
    conn.execute(
        "insert into scopes (user_id, prefix, scope, granted_by, created_at) values (?, ?, ?, ?, ?)
         on conflict(user_id, prefix) do update set scope = excluded.scope, granted_by = excluded.granted_by",
        rusqlite::params![user_id, prefix, scope.as_str(), granted_by, now() as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn revoke_scope(config: &Config, email: &str, prefix: &str) -> Result<(), String> {
    let conn = open(config)?;
    let email = normalise(email);
    let changed = conn
        .execute(
            "delete from scopes where prefix = ? and user_id in (select id from users where email = ?)",
            rusqlite::params![prefix, email],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("{email} holds no scope at '{prefix}'"));
    }
    Ok(())
}

/// Every scope row, with the account's email, ordered by prefix then email.
pub fn list_scopes(config: &Config) -> Result<Vec<ScopeGrant>, String> {
    let conn = open(config)?;
    let mut statement = conn
        .prepare(
            "select users.email, users.id, scopes.prefix, scopes.scope
               from scopes join users on users.id = scopes.user_id
              order by scopes.prefix, users.email",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    Ok(rows
        .filter_map(Result::ok)
        .filter_map(|(email, user_id, prefix, scope)| {
            Scope::parse(&scope).map(|scope| ScopeGrant {
                email,
                user_id,
                prefix,
                scope,
            })
        })
        .collect())
}

/// The scopes one account holds, by prefix.
pub fn scopes_for(config: &Config, user_id: &str) -> Vec<(String, Scope)> {
    let Ok(conn) = open(config) else {
        return Vec::new();
    };
    let Ok(mut statement) = conn.prepare("select prefix, scope from scopes where user_id = ? order by prefix") else {
        return Vec::new();
    };
    statement
        .query_map([user_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map(|rows| {
            rows.filter_map(Result::ok)
                .filter_map(|(prefix, scope)| Scope::parse(&scope).map(|scope| (prefix, scope)))
                .collect()
        })
        .unwrap_or_default()
}

/// The outermost locked project over `path`, if any. A locked project
/// discards every row set inside it, so the outermost lock decides.
fn lock_over<'a>(path: &str, locks: &'a [String]) -> Option<&'a str> {
    locks
        .iter()
        .filter(|lock| !lock.is_empty() && prefix_covers(lock, path))
        .min_by_key(|lock| lock.len())
        .map(String::as_str)
}

/// Whether a row at `prefix` counts for `path`: it covers the path, and no
/// locked project sits above the row while covering the path.
fn row_counts(prefix: &str, path: &str, lock: Option<&str>) -> bool {
    prefix_covers(prefix, path) && lock.is_none_or(|lock| prefix_covers(prefix, lock))
}

/// Where an account's access at a path comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum AccessSource {
    /// A site admin holds admin everywhere.
    SiteAdmin,
    /// A permission row at this prefix (empty for the whole site).
    Row(String),
    /// Access to one app given on that app, which reads as viewer.
    AppAccess,
}

/// The one question every check asks: what may this account do at `path`?
/// The strongest scope held on the path or any prefix above it; a site admin
/// is admin everywhere. Scopes only add, so nothing below can narrow what a
/// row above grants. `locks` are the locked projects
/// (`store::locked_prefixes_blocking`): under one, rows set inside it are
/// ignored.
pub fn effective_scope(config: &Config, user: &User, path: &str, locks: &[String]) -> Option<Scope> {
    explain_scope(config, user, path, None, locks).map(|(scope, _)| scope)
}

/// What the account may do to one app, which sits in `folder` (empty for
/// the root): the strongest scope over the app's path, or viewer if the
/// account holds access given on the app itself. Under a locked project
/// that app access is ignored, like any row set inside the lock.
pub fn app_scope(config: &Config, user: &User, folder: &str, app: &str, locks: &[String]) -> Option<Scope> {
    let path = if folder.is_empty() { app.to_string() } else { format!("{folder}/{app}") };
    explain_scope(config, user, &path, Some(app), locks).map(|(scope, _)| scope)
}

/// The scope and the place it comes from: the same rule as
/// [`effective_scope`] and [`app_scope`], with the reason kept, for the
/// "check a person" box. Pass `app` when `path` is an app's path.
pub fn explain_scope(
    config: &Config,
    user: &User,
    path: &str,
    app: Option<&str>,
    locks: &[String],
) -> Option<(Scope, AccessSource)> {
    if user.is_admin {
        return Some((Scope::Admin, AccessSource::SiteAdmin));
    }
    let lock = lock_over(path, locks);
    let best = scopes_for(config, &user.id)
        .into_iter()
        .filter(|(prefix, _)| row_counts(prefix, path, lock))
        // The strongest wins; between equals, the nearest row names it.
        .max_by(|(pa, a), (pb, b)| a.cmp(b).then(pa.len().cmp(&pb.len())))
        .map(|(prefix, scope)| (scope, AccessSource::Row(prefix)));
    let app_access = match app {
        Some(app) if lock.is_none() && has_grant(config, user, app) => Some((Scope::Viewer, AccessSource::AppAccess)),
        _ => None,
    };
    match (best, app_access) {
        (Some(row), _) => Some(row),
        (None, other) => other,
    }
}

/// The folder a new app goes in when the caller names none: the one folder
/// the account is editor or admin of. The root if it holds that there;
/// nothing if it holds several folders and must say which.
pub fn default_folder_for(config: &Config, user: &User) -> Option<String> {
    if user.is_admin {
        return Some(String::new());
    }
    let mut folders: Vec<String> = scopes_for(config, &user.id)
        .into_iter()
        .filter(|(_, scope)| *scope >= Scope::Editor)
        .map(|(prefix, _)| prefix)
        .collect();
    if folders.iter().any(String::is_empty) {
        return Some(String::new());
    }
    folders.sort();
    folders.dedup();
    // Folders nested in one another are one choice: the outermost.
    let outermost: Vec<String> = folders
        .iter()
        .filter(|f| !folders.iter().any(|other| other != *f && prefix_covers(other, f)))
        .cloned()
        .collect();
    match outermost.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Whether the account holds `at_least` anywhere at all. What decides if a
/// client it connects may publish.
pub fn holds_anywhere(config: &Config, user: &User, at_least: Scope) -> bool {
    user.is_admin || scopes_for(config, &user.id).into_iter().any(|(_, scope)| scope >= at_least)
}

/// Whether the account holds any scope strictly inside `prefix`, which is
/// what lets them walk down the tree to it.
pub fn holds_below(config: &Config, user: &User, prefix: &str) -> bool {
    scopes_for(config, &user.id)
        .into_iter()
        .any(|(held, _)| held != prefix && prefix_covers(prefix, &held))
}

/// Moves an app's own scope rows when the app moves in the tree, so a scope
/// given on the app follows it.
/// Moves every scope row at `from` or below it to the same place under
/// `to`, in one transaction. Running it again finds nothing to move, so a
/// rename that stopped halfway can simply be run again.
pub fn move_scope_tree(config: &Config, from: &str, to: &str) -> Result<usize, String> {
    let mut conn = open(config)?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let changed = tx
        .execute(
            "update or replace scopes set prefix = ?1 || substr(prefix, ?2)
              where prefix = ?3 or substr(prefix, 1, ?4) = ?5",
            rusqlite::params![
                to,
                (from.len() + 1) as i64,
                from,
                (from.len() + 1) as i64,
                format!("{from}/"),
            ],
        )
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(changed)
}

/// Takes every permission an app held away with it: the access rows at its
/// path and below, and its old per-app grants. Returns them, so whoever
/// removes the app can keep a copy with the rest of what was removed. A
/// later app or project at the same path must start with nobody on it.
pub fn forget_app(config: &Config, app: &str, path: &str) -> Result<serde_json::Value, String> {
    let mut conn = open(config)?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = {
        let mut statement = tx
            .prepare(
                "select users.email, scopes.prefix, scopes.scope from scopes join users on users.id = scopes.user_id
                  where scopes.prefix = ?1 or substr(scopes.prefix, 1, ?2) = ?3",
            )
            .map_err(|e| e.to_string())?;
        let found = statement
            .query_map(rusqlite::params![path, (path.len() + 1) as i64, format!("{path}/")], |row| {
                Ok(serde_json::json!({ "email": row.get::<_, String>(0)?, "path": row.get::<_, String>(1)?, "scope": row.get::<_, String>(2)? }))
            })
            .map_err(|e| e.to_string())?;
        found.filter_map(Result::ok).collect()
    };
    let grants: Vec<serde_json::Value> = {
        let mut statement = tx
            .prepare("select users.email, grants.role from grants join users on users.id = grants.user_id where grants.app = ?1")
            .map_err(|e| e.to_string())?;
        let found = statement
            .query_map([app], |row| Ok(serde_json::json!({ "email": row.get::<_, String>(0)?, "role": row.get::<_, String>(1)? })))
            .map_err(|e| e.to_string())?;
        found.filter_map(Result::ok).collect()
    };
    tx.execute(
        "delete from scopes where prefix = ?1 or substr(prefix, 1, ?2) = ?3",
        rusqlite::params![path, (path.len() + 1) as i64, format!("{path}/")],
    )
    .map_err(|e| e.to_string())?;
    tx.execute("delete from grants where app = ?1", [app]).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "app": app, "path": path, "access": rows, "grants": grants }))
}

/// Removes every scope row at `path` or below it.
pub fn remove_scope_tree(config: &Config, path: &str) -> Result<usize, String> {
    let conn = open(config)?;
    conn.execute(
        "delete from scopes where prefix = ?1 or substr(prefix, 1, ?2) = ?3",
        rusqlite::params![path, (path.len() + 1) as i64, format!("{path}/")],
    )
    .map_err(|e| e.to_string())
}

pub fn move_scopes(config: &Config, from: &str, to: &str) -> Result<(), String> {
    let conn = open(config)?;
    conn.execute(
        "update or replace scopes set prefix = ? where prefix = ?",
        rusqlite::params![to, from],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// The site session cookie's name. In subdomain mode over TLS it carries
/// the `__Host-` prefix, which a browser accepts only from the host itself
/// with no `Domain`: an app on a sibling host cannot plant a session of its
/// choosing on the main host by setting a cookie for the parent domain.
fn site_cookie_name(config: &Config) -> &'static str {
    if config.apps.is_some() && !secure_flag(config).is_empty() {
        "__Host-ts_session"
    } else {
        SESSION_COOKIE
    }
}

/// The app session cookie's name on an app host, where the host already
/// says which app it is. `__Host-` for the same reason as the site's.
fn app_host_cookie_name(config: &Config) -> &'static str {
    match &config.apps {
        Some(apps) if apps.secure() => "__Host-ts_app",
        _ => "ts_app",
    }
}

/// The cookie that ties a handoff to the browser that began it. See
/// `begin_handoff`.
fn handoff_cookie_name(config: &Config) -> &'static str {
    match &config.apps {
        Some(apps) if apps.secure() => "__Host-ts_handoff",
        _ => "ts_handoff",
    }
}

/// `Secure;` for an app host's cookies, when the browser will keep one.
fn app_host_secure_flag(config: &Config) -> &'static str {
    match &config.apps {
        Some(apps) if apps.secure() => " Secure;",
        _ => "",
    }
}

fn cookie_value(header: Option<&str>, name: &str) -> Option<String> {
    header?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(cookie, _)| *cookie == name)
        .map(|(_, value)| value.to_string())
}

/// Reads the site session cookie out of a Cookie header. An app host never
/// receives it, being another host, and nothing there asks for it.
pub fn token_from_cookies(config: &Config, header: Option<&str>) -> Option<String> {
    cookie_value(header, site_cookie_name(config))
}

/// Reads one app's session cookie. In path mode a request under `/p/appB/`
/// never carries app A's cookie, so this returns nothing for a script
/// asking on another app's behalf. In subdomain mode the cookie belongs to
/// the app's host, and a token minted for another app fails the scope test
/// wherever it is presented.
pub fn app_token_from_cookies(config: &Config, header: Option<&str>, app: &str) -> Option<String> {
    match &config.apps {
        Some(_) => cookie_value(header, app_host_cookie_name(config)),
        None => cookie_value(header, &app_cookie_name(app)),
    }
}

/// `Secure` whenever the site is reached over TLS, which is every real
/// deployment. A plain-http address — a LAN box, a laptop — would have the
/// browser drop a Secure cookie on the floor and sign-in would silently do
/// nothing, so there the flag is left off. The address is the deployment's
/// own word for how it is reached; nothing from the request decides this.
fn secure_flag(config: &Config) -> &'static str {
    match config.base_url.as_deref() {
        Some(base) if base.starts_with("http://") => "",
        _ => " Secure;",
    }
}

pub fn set_cookie_header(config: &Config, token: &str) -> String {
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax;{} Max-Age={}",
        site_cookie_name(config),
        secure_flag(config),
        SESSION_LIFETIME.as_secs()
    )
}

/// Scoped to the app's own subtree. Everything else matches the site cookie:
/// out of reach of script, and never sent over plain HTTP. In subdomain mode
/// it is a host-only cookie of the app's host: no `Domain`, so no other
/// host is ever sent it.
pub fn set_app_cookie_header(config: &Config, app: &str, token: &str, max_age: u64) -> String {
    if config.apps.is_some() {
        return format!(
            "{name}={token}; Path=/; HttpOnly; SameSite=Lax;{secure} Max-Age={max_age}",
            name = app_host_cookie_name(config),
            secure = app_host_secure_flag(config),
        );
    }
    format!(
        "{name}={token}; Path={path}; HttpOnly; SameSite=Lax;{secure} Max-Age={max_age}",
        name = app_cookie_name(app),
        path = app_cookie_path(app),
        secure = secure_flag(config),
    )
}

pub fn clear_cookie_header(config: &Config) -> String {
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax;{} Max-Age=0",
        site_cookie_name(config),
        secure_flag(config)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        (
            tempfile::tempdir().unwrap(),
            Config::local(dir.keep(), "test-token"),
        )
    }


    // --- scopes -------------------------------------------------------------

    fn person(config: &Config, email: &str) -> User {
        sign_up(config, email, "correct horse battery").unwrap()
    }

    #[test]
    fn the_strongest_scope_above_a_path_applies_and_nothing_narrows_it() {
        let (_t, config) = config();
        let ann = person(&config, "ann@example.com");
        grant_scope(&config, "ann@example.com", "ops", Scope::Admin, None).unwrap();
        grant_scope(&config, "ann@example.com", "ops/yard", Scope::Viewer, None).unwrap();
        assert_eq!(effective_scope(&config, &ann, "ops/yard/checklist", &[]), Some(Scope::Admin), "a weaker row below narrowed an admin above");
        assert_eq!(effective_scope(&config, &ann, "ops", &[]), Some(Scope::Admin));
        assert_eq!(effective_scope(&config, &ann, "finance", &[]), None);
        assert_eq!(effective_scope(&config, &ann, "opsx", &[]), None, "a prefix matched by text, not by segment");
    }

    #[test]
    fn one_person_holds_many_scopes_and_each_applies_in_its_place() {
        let (_t, config) = config();
        let bo = person(&config, "bo@example.com");
        grant_scope(&config, "bo@example.com", "ops/warehouse", Scope::Editor, None).unwrap();
        grant_scope(&config, "bo@example.com", "finance/reports", Scope::Viewer, None).unwrap();
        grant_scope(&config, "bo@example.com", "labs", Scope::Admin, None).unwrap();
        assert_eq!(effective_scope(&config, &bo, "ops/warehouse/tool", &[]), Some(Scope::Editor));
        assert_eq!(effective_scope(&config, &bo, "finance/reports/q3", &[]), Some(Scope::Viewer));
        assert_eq!(effective_scope(&config, &bo, "labs/x/y", &[]), Some(Scope::Admin));
        assert_eq!(effective_scope(&config, &bo, "finance/ledger", &[]), None);
        assert!(holds_anywhere(&config, &bo, Scope::Editor));
        assert!(holds_below(&config, &bo, "finance"));
        assert!(!holds_below(&config, &bo, "ops/warehouse"), "a row at the folder itself is not below it");
        assert_eq!(default_folder_for(&config, &bo), None, "editor in two folders has to say which");
        assert_eq!(scopes_for(&config, &bo.id).len(), 3);
    }

    #[test]
    fn a_grant_on_an_app_is_viewer_on_that_app_wherever_it_sits() {
        let (_t, config) = config();
        let cy = person(&config, "cy@example.com");
        grant(&config, "cy@example.com", "ledger", "editor").unwrap();
        assert_eq!(app_scope(&config, &cy, "finance", "ledger", &[]), Some(Scope::Viewer));
        assert_eq!(app_scope(&config, &cy, "", "ledger", &[]), Some(Scope::Viewer));
        assert_eq!(app_scope(&config, &cy, "finance", "other", &[]), None);
        revoke(&config, "cy@example.com", "ledger").unwrap();
        assert_eq!(app_scope(&config, &cy, "finance", "ledger", &[]), None, "a revoked grant still opened the app");
        // A real scope on the folder is what it is, grant or not.
        grant_scope(&config, "cy@example.com", "finance", Scope::Editor, None).unwrap();
        assert_eq!(app_scope(&config, &cy, "finance", "ledger", &[]), Some(Scope::Editor));
    }

    #[test]
    fn a_site_admin_is_admin_everywhere_and_a_scope_needs_a_valid_prefix() {
        let (_t, config) = config();
        let root = sign_up_as(&config, "root@example.com", "correct horse battery", true).unwrap();
        assert_eq!(effective_scope(&config, &root, "anything/at/all", &[]), Some(Scope::Admin));
        assert_eq!(default_folder_for(&config, &root), Some(String::new()));
        assert!(grant_scope(&config, "root@example.com", "../x", Scope::Viewer, None).is_err());
        assert!(grant_scope(&config, "root@example.com", ".site", Scope::Viewer, None).is_err());
        assert!(grant_scope(&config, "nobody@example.com", "ops", Scope::Viewer, None).is_err());
        assert!(revoke_scope(&config, "root@example.com", "ops").is_err(), "revoking nothing said yes");
    }

    #[test]
    fn an_apps_own_scopes_follow_it_when_it_moves() {
        let (_t, config) = config();
        let di = person(&config, "di@example.com");
        grant_scope(&config, "di@example.com", "tool", Scope::Editor, None).unwrap();
        assert_eq!(effective_scope(&config, &di, "tool", &[]), Some(Scope::Editor));
        move_scopes(&config, "tool", "ops/tool").unwrap();
        assert_eq!(effective_scope(&config, &di, "tool", &[]), None);
        assert_eq!(effective_scope(&config, &di, "ops/tool", &[]), Some(Scope::Editor));
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
        let (_dir, config) = config();
        assert_eq!(token_from_cookies(&config, crowded).unwrap(), "abc123");
        assert_eq!(app_token_from_cookies(&config, crowded, "notes").unwrap(), "def456");
        // A neighbour's cookie is not this app's, even in the same header.
        assert!(app_token_from_cookies(&config, crowded, "invoices").is_none());
        assert!(token_from_cookies(&config, Some("theme=dark")).is_none());
        assert!(token_from_cookies(&config, None).is_none());
    }

    #[test]
    fn a_prefixed_cookie_of_toolsites_is_still_toolsites() {
        // An app host's session is `__Host-ts_app`: kept from the handler
        // that would read it and from a handler that would set it.
        for name in ["__Host-ts_app", "__host-ts_session", "__Secure-ts_handoff", "ts_app_notes", " ts_session"] {
            assert!(is_platform_cookie(name), "{name}");
        }
        for name in ["theme", "__Host-theme", "app_ts_", "__Host-"] {
            assert!(!is_platform_cookie(name), "{name}");
        }
        assert_eq!(without_platform_cookies("__Host-ts_app=s; theme=dark; __Host-ts_handoff=n").as_deref(), Some("theme=dark"));
        assert!(sets_platform_cookie("__Host-ts_app=forged; Path=/; Secure"));
    }

    #[test]
    fn a_form_token_cannot_be_worked_out_from_the_account_id() {
        // A site that signs clients in has no static token; the key must
        // still be one only the server holds.
        let dir = tempfile::tempdir().unwrap();
        let config = Config { valid_tokens: Vec::new(), ..Config::local(dir.path().to_path_buf(), "t") };
        let token = derive_form_token(&config, "user-1");
        let guessable = URL_SAFE_NO_PAD.encode(Sha256::digest(b"form::user-1"));
        assert_ne!(token, guessable);
        // Stable for the same site, different for another.
        assert_eq!(token, derive_form_token(&config, "user-1"));
        let other = tempfile::tempdir().unwrap();
        let elsewhere = Config { valid_tokens: Vec::new(), ..Config::local(other.path().to_path_buf(), "t") };
        assert_ne!(token, derive_form_token(&elsewhere, "user-1"));
    }

    fn deployed_at(base: Option<&str>) -> Config {
        Config {
            base_url: base.map(str::to_string),
            ..Config::local(std::env::temp_dir(), "t")
        }
    }

    #[test]
    fn the_cookie_is_not_reachable_from_script_or_plain_http() {
        // A real address is https, and so is an unset one by default.
        for config in [deployed_at(Some("https://site.test")), deployed_at(None)] {
            for header in [
                set_cookie_header(&config, "abc"),
                set_app_cookie_header(&config, "notes", "abc", 60),
                clear_cookie_header(&config),
            ] {
                assert!(header.contains("HttpOnly"), "{header}");
                assert!(header.contains("Secure"), "{header}");
                assert!(header.contains("SameSite=Lax"), "{header}");
            }
        }
    }

    #[test]
    fn a_plain_http_deployment_gets_a_cookie_the_browser_will_keep() {
        // Otherwise sign-in on a LAN box silently does nothing: the browser
        // drops a Secure cookie set over http.
        let config = deployed_at(Some("http://10.0.0.5:8080"));
        for header in [
            set_cookie_header(&config, "abc"),
            set_app_cookie_header(&config, "notes", "abc", 60),
        ] {
            assert!(!header.contains("Secure"), "{header}");
            assert!(header.contains("HttpOnly"), "{header}");
        }
    }

    #[test]
    fn an_apps_cookie_is_confined_to_that_apps_path() {
        let config = deployed_at(None);
        let header = set_app_cookie_header(&config, "notes", "abc", 60);
        assert!(header.contains("Path=/p/notes/"), "{header}");
        assert!(header.starts_with("ts_app_notes=abc;"), "{header}");
        // The site cookie is the one thing that stays origin-wide.
        assert!(set_cookie_header(&config, "abc").contains("Path=/;"));
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

pub async fn login_form(
    State(config): State<Arc<Config>>,
    Query(params): Query<NextPage>,
) -> Response {
    let next = safe_next(params.next.as_deref());
    let encoded_next = urlencoding::encode(&next);
    let markup = crate::ui::form_page(
        "Sign in",
        maud::html! {
            form."column" method="post" action="/auth/login" {
                h1 { "Sign in" }
                input type="hidden" name="next" value=(next);
                input name="email" type="email" placeholder="Email"
                      autocomplete="username" required autofocus;
                input name="password" type="password" placeholder="Password"
                      autocomplete="current-password" required;
                button type="submit" { "Sign in" }
            }
            // There is no mailer, so there is no reset email; an admin's
            // setup link is the way back in, and the page says so.
            p."muted" style="margin: .75rem 0 0; font-size: .8rem" {
                "If you forgot your password, ask an admin for a setup link."
            }
            // One button per provider the deployment configured. Each goes
            // out through /auth/login/<slug> and comes back to `next`.
            @if !config.providers.is_empty() {
                div."column" style="margin-top: 1rem" {
                    p."muted" style="margin: 0 0 .5rem" { "or" }
                    @for provider in &config.providers {
                        a."btn quiet" href={ "/auth/login/" (provider.slug) "?next=" (encoded_next) } {
                            "Sign in with " (provider.name)
                        }
                    }
                }
            }
        },
    );
    Html(markup.into_string()).into_response()
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
    let worker = config.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        log_in(&worker, &credentials.email, &credentials.password)
    })
    .await;

    match outcome {
        Ok(Ok((_, token))) => (
            [(header::SET_COOKIE, set_cookie_header(&config, &token))],
            Redirect::to(&next),
        )
            .into_response(),
        Ok(Err(message)) => (StatusCode::UNAUTHORIZED, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed.").into_response(),
    }
}

pub async fn logout(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    if let Some(token) = token_from_cookies(
        &config,
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    ) {
        let worker = config.clone();
        let _ = tokio::task::spawn_blocking(move || log_out(&worker, &token)).await;
    }
    (
        [(header::SET_COOKIE, clear_cookie_header(&config))],
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
    /// Subdomain mode only: the nonce the app host put in its handoff
    /// cookie, which the code is bound to.
    state: Option<String>,
}

/// A sign-in on its way from the main host to an app host. The app session
/// is minted on the main host, where the site session is; the app host gets
/// it by presenting the code, once, within a minute, from the browser that
/// holds the matching handoff cookie.
pub struct HandoffTicket {
    pub app: String,
    token: String,
    max_age: u64,
    /// Within the app's path, starting `/p/<app>`.
    next: String,
    state: String,
    expires_at: std::time::Instant,
}

/// How long a code waits for the app host to collect it.
const HANDOFF_TTL: Duration = Duration::from_secs(60);
/// How long the app host waits for the visitor to come back signed in.
const HANDOFF_STATE_LIFETIME: Duration = Duration::from_secs(600);

/// A nonce as `begin_handoff` makes them.
fn valid_state(state: &str) -> bool {
    (20..=64).contains(&state.len()) && state.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Where in an app a visitor is sent back to: a path under `/p/<app>`, or
/// the app's root. Only ever used on the app's own host, so it can name no
/// other host or app.
fn next_within_app(app: &str, next: Option<&str>) -> String {
    let root = format!("/p/{app}");
    match next {
        Some(path)
            if (path == root || path.starts_with(&format!("{root}/")))
                && !path.contains("//")
                && !path.contains('\\') =>
        {
            path.to_string()
        }
        _ => format!("{root}/"),
    }
}

/// Subdomain mode: sends a visitor on an app host to the main host to be
/// signed in. A nonce goes both in the redirect and in a host-only cookie
/// here, and the code the main host hands back is good only alongside that
/// cookie. Without it, someone could collect a code for their own account
/// and walk a victim's browser to the landing with it, signing the victim
/// into the app as them. An existing nonce is reused, so two tabs signing
/// in at once do not undo each other.
pub fn begin_handoff(config: &Config, app: &str, next: &str, headers: &HeaderMap) -> Response {
    let state = cookie_value(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
        handoff_cookie_name(config),
    )
    .filter(|state| valid_state(state))
    .unwrap_or_else(|| crate::content::slug::random_token(32));
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    let target = format!(
        "{base}/auth/handoff?app={}&next={}&state={state}",
        urlencoding::encode(app),
        urlencoding::encode(next)
    );
    (
        [
            (
                header::SET_COOKIE,
                format!(
                    "{}={state}; Path=/; HttpOnly; SameSite=Lax;{} Max-Age={}",
                    handoff_cookie_name(config),
                    app_host_secure_flag(config),
                    HANDOFF_STATE_LIFETIME.as_secs()
                ),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Redirect::to(&target),
    )
        .into_response()
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

    if config.apps.is_some() {
        return handoff_to_app_host(config, params, headers).await;
    }

    let sign_in = || {
        Redirect::to(&format!(
            "/auth/login?next={}",
            urlencoding::encode(&next)
        ))
        .into_response()
    };
    let Some(site_token) = token_from_cookies(
        &config,
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    ) else {
        return sign_in();
    };

    let app = params.app.clone();
    let worker = config.clone();
    let outcome =
        tokio::task::spawn_blocking(move || create_app_session(&worker, &site_token, &app)).await;

    match outcome {
        Ok(Ok((_, token, max_age))) => (
            [(
                header::SET_COOKIE,
                set_app_cookie_header(&config, &params.app, &token, max_age),
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

/// The handoff in subdomain mode, on the main host: mints the app session
/// from the site session and sends a one-time code for it to the app's own
/// host. The host is built from the configuration and the app's stored
/// label, never from anything in the request, so a code cannot be sent to
/// another app or another host.
async fn handoff_to_app_host(config: Arc<Config>, params: HandoffParams, headers: HeaderMap) -> Response {
    let app = params.app.clone();
    let Some(state) = params.state.clone().filter(|state| valid_state(state)) else {
        tracing::warn!(app = %app, "handoff refused: no state from the app host");
        return (StatusCode::BAD_REQUEST, "Open the app again to sign in.").into_response();
    };
    if !valid_app_scope(&app)
        || !crate::content::store::app_exists(&config, &app).await
        || crate::content::store::is_hidden(&config, &app).await
    {
        tracing::warn!(app = %app, "handoff refused: no such app");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let next = next_within_app(&app, params.next.as_deref());
    let Some(site_token) = token_from_cookies(&config, headers.get(header::COOKIE).and_then(|v| v.to_str().ok())) else {
        // Back here once signed in, with the same state, so the app host's
        // cookie still matches.
        let again = format!(
            "/auth/handoff?app={}&next={}&state={state}",
            urlencoding::encode(&app),
            urlencoding::encode(&next)
        );
        return Redirect::to(&format!("/auth/login?next={}", urlencoding::encode(&again))).into_response();
    };
    let lookup = (config.clone(), site_token.clone());
    let Some(user) = tokio::task::spawn_blocking(move || site_session_user(&lookup.0, &lookup.1)).await.ok().flatten() else {
        let again = format!(
            "/auth/handoff?app={}&next={}&state={state}",
            urlencoding::encode(&app),
            urlencoding::encode(&next)
        );
        return Redirect::to(&format!("/auth/login?next={}", urlencoding::encode(&again))).into_response();
    };
    // Not worth a session for an app that would refuse this person: the app
    // would learn of a visitor it turned away.
    let within = next.strip_prefix(&format!("/p/{app}")).unwrap_or("/").to_string();
    let gate = crate::content::store::effective_gate(&config, &app, &within).await.gate;
    if !crate::content::serve::admits(&config, &gate, &app, Some(&user)).await {
        tracing::warn!(app = %app, email = %user.email, "handoff refused: the gate does not admit this account");
        return (StatusCode::FORBIDDEN, "You do not have access to this app.").into_response();
    }
    let worker = (config.clone(), app.clone());
    let minted = tokio::task::spawn_blocking(move || create_app_session(&worker.0, &site_token, &worker.1)).await;
    let (token, max_age) = match minted {
        Ok(Ok((_, token, max_age))) => (token, max_age),
        Ok(Err(_)) => return Redirect::to(&format!("/auth/login?next={}", urlencoding::encode(&next))).into_response(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "handoff failed").into_response(),
    };
    let code = crate::content::slug::random_token(40);
    {
        let now = std::time::Instant::now();
        let mut handoffs = config.handoffs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        handoffs.retain(|_, ticket| ticket.expires_at > now);
        handoffs.insert(
            code.clone(),
            HandoffTicket { app: app.clone(), token, max_age, next, state, expires_at: now + HANDOFF_TTL },
        );
    }
    let lookup = (config.clone(), app.clone());
    let Ok(origin) = tokio::task::spawn_blocking(move || crate::content::origins::app_base(&lookup.0, &lookup.1)).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "handoff failed").into_response();
    };
    ([(header::CACHE_CONTROL, "no-store")], Redirect::to(&format!("{origin}/auth/landing?code={code}"))).into_response()
}

#[derive(serde::Deserialize)]
pub struct LandingParams {
    code: String,
}

/// `GET /auth/landing?code=` on an app host: the end of a handoff. Trades
/// the code for the app's host-only cookie, if the code was minted for this
/// app and this browser holds the handoff cookie it was bound to.
pub async fn landing(
    State(config): State<Arc<Config>>,
    host: Option<axum::Extension<crate::content::origins::AppHost>>,
    Query(params): Query<LandingParams>,
    headers: HeaderMap,
) -> Response {
    let Some(axum::Extension(crate::content::origins::AppHost(host_app))) = host else {
        tracing::warn!("landing refused: not on an app host");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let ticket = {
        let now = std::time::Instant::now();
        let mut handoffs = config.handoffs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        handoffs.retain(|_, ticket| ticket.expires_at > now);
        handoffs.remove(&params.code)
    };
    let Some(ticket) = ticket else {
        tracing::warn!(app = %host_app, "landing refused: code unknown, expired or already used");
        return (
            StatusCode::BAD_REQUEST,
            Html(format!(
                "This sign-in link was already used or has expired. <a href=\"/p/{}/\">Open the app again</a>.",
                crate::content::slug::escape_html(&host_app)
            )),
        )
            .into_response();
    };
    if ticket.app != host_app {
        tracing::warn!(host = %host_app, code_for = %ticket.app, "landing refused: a code for another app");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let held = cookie_value(headers.get(header::COOKIE).and_then(|v| v.to_str().ok()), handoff_cookie_name(&config));
    if held.as_deref() != Some(ticket.state.as_str()) {
        tracing::warn!(app = %host_app, has_cookie = held.is_some(), "landing refused: the code was not begun in this browser");
        return (StatusCode::FORBIDDEN, "This sign-in was begun in another browser. Open the app again.").into_response();
    }
    let mut response = Redirect::to(&ticket.next).into_response();
    let headers = response.headers_mut();
    for cookie in [
        set_app_cookie_header(&config, &ticket.app, &ticket.token, ticket.max_age),
        format!(
            "{}=; Path=/; HttpOnly; SameSite=Lax;{} Max-Age=0",
            handoff_cookie_name(&config),
            app_host_secure_flag(&config)
        ),
    ] {
        if let Ok(value) = header::HeaderValue::from_str(&cookie) {
            headers.append(header::SET_COOKIE, value);
        }
    }
    headers.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    headers.insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
    response
}

/// Resolves the caller from their *site* session cookie, if any. Says who the
/// person is; says nothing about what they may reach.
pub async fn current_site_user(config: &Arc<Config>, headers: &HeaderMap) -> Option<User> {
    let token = token_from_cookies(
        config,
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
        config,
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

// --- admin queries ------------------------------------------------------

pub struct Account {
    pub email: String,
    pub created: String,
    pub is_admin: bool,
    pub is_active: bool,
}

/// Turns an account off, or back on. Existing sessions are dropped rather
/// than left to expire, so access ends now; the session lookup also refuses a
/// disabled account, which covers anything issued in between.
pub fn set_active(config: &Config, email: &str, active: bool) -> Result<(), String> {
    let conn = open(config)?;
    let email = normalise(email);
    let changed = conn
        .execute(
            "update users set disabled_at = ? where email = ?",
            rusqlite::params![if active { None } else { Some(now() as i64) }, &email],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("no account for {email}"));
    }
    if !active {
        conn.execute(
            "delete from sessions where user_id in (select id from users where email = ?)",
            [&email],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn list_accounts(config: &Config) -> Result<Vec<Account>, String> {
    let conn = open(config)?;
    let mut statement = conn
        .prepare("select email, created_at, is_admin, disabled_at from users order by email")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let created: i64 = row.get(1)?;
            Ok(Account {
                email: row.get(0)?,
                // Whole days is all this needs to convey; anything finer
                // would mean a date-formatting dependency.
                created: format!("{} days ago", (now().saturating_sub(created as u64)) / 86_400),
                is_admin: row.get::<_, i64>(2)? != 0,
                is_active: row.get::<_, Option<i64>>(3)?.is_none(),
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Every grant on the site: app, account, role.
pub fn list_grants(config: &Config) -> Result<Vec<(String, String, String)>, String> {
    let conn = open(config)?;
    let mut statement = conn
        .prepare(
            "select grants.app, users.email, grants.role
               from grants join users on users.id = grants.user_id
              order by grants.app, users.email",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// A value only this server can produce for this account, used to tie an
/// admin form to the session that rendered it. Derived from the account and
/// the server's own secret, so it is not the session token and cannot be
/// replayed as one.
pub fn derive_form_token(config: &Config, user_id: &str) -> String {
    let secret = URL_SAFE_NO_PAD.encode(form_secret(config));
    URL_SAFE_NO_PAD.encode(Sha256::digest(format!("form:{secret}:{user_id}").as_bytes()))
}

/// The key form tokens are derived with: 32 random bytes kept in
/// `.site/form.key`, made on first use. It used to be the static MCP token,
/// which a site that signs clients in does not have; then the key was empty
/// and anyone who knew an account's id could work out its form token. An
/// app's own code learns ids through `identity`, and every app shares this
/// origin, so the key has to be one only the server holds.
fn form_secret(config: &Config) -> [u8; 32] {
    let path = config.data_dir.join(".site").join("form.key");
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice())
    {
        return key;
    }
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Written through a temp file so two first requests cannot both win
    // with different keys and leave pages carrying a token that fails.
    let temp = path.with_extension(format!("tmp{}", crate::content::slug::random_token(6)));
    if std::fs::write(&temp, key).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        // Keep whichever key reached the disk first.
        if std::fs::hard_link(&temp, &path).is_err() {
            let _ = std::fs::remove_file(&temp);
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(existing) = <[u8; 32]>::try_from(bytes.as_slice())
            {
                return existing;
            }
        }
        let _ = std::fs::remove_file(&temp);
    }
    key
}

// --- invitations --------------------------------------------------------

/// Long enough to reach someone by whatever channel, short enough that a
/// forgotten link is not a standing way in.
const INVITE_LIFETIME: Duration = Duration::from_secs(60 * 60 * 48);

/// Creates an account nobody can sign in to yet, and a one-time link for
/// setting its password. The password never passes through whoever is doing
/// the inviting — not their shell history, not their clipboard.
pub fn invite(config: &Config, email: &str, is_admin: bool) -> Result<(User, String), String> {
    let email = normalise(email);
    if !email.contains('@') || email.len() < 3 {
        return Err("Enter a valid email address.".into());
    }

    let conn = open(config)?;
    let id = crate::content::slug::random_token(16);
    conn.execute(
        "insert into users (id, email, password_hash, created_at, is_admin)
         values (?, ?, null, ?, ?)",
        rusqlite::params![&id, &email, now() as i64, is_admin as i64],
    )
    .map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            "An account with this email exists.".to_string()
        } else {
            e.to_string()
        }
    })?;

    let token = new_invite(&conn, &id)?;
    Ok((
        User {
            id,
            email,
            is_admin,
        },
        token,
    ))
}

/// Issues a fresh invitation for an existing account, replacing any
/// outstanding one so an old link stops working.
pub fn reinvite(config: &Config, email: &str) -> Result<String, String> {
    let conn = open(config)?;
    let email = normalise(email);
    let id: String = conn
        .query_row("select id from users where email = ?", [&email], |row| {
            row.get(0)
        })
        .map_err(|_| format!("no account for {email}"))?;
    new_invite(&conn, &id)
}

fn new_invite(conn: &Connection, user_id: &str) -> Result<String, String> {
    conn.execute("delete from invites where user_id = ?", [user_id])
        .map_err(|e| e.to_string())?;
    let token = crate::content::slug::random_token(48);
    conn.execute(
        "insert into invites (token_hash, user_id, expires_at) values (?, ?, ?)",
        rusqlite::params![
            hash_token(&token),
            user_id,
            (now() + INVITE_LIFETIME.as_secs()) as i64
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(token)
}

/// Who an invitation is for, without spending it.
pub fn invited_account(config: &Config, token: &str) -> Option<User> {
    let conn = open(config).ok()?;
    conn.query_row(
        "select users.id, users.email, users.is_admin
           from invites join users on users.id = invites.user_id
          where invites.token_hash = ? and invites.expires_at >= ?
            and users.disabled_at is null",
        rusqlite::params![hash_token(token), now() as i64],
        |row| {
            Ok(User {
                id: row.get(0)?,
                email: row.get(1)?,
                is_admin: row.get::<_, i64>(2)? != 0,
            })
        },
    )
    .ok()
}

/// Spends an invitation: sets the password and signs the person in. The
/// invitation is consumed whether or not anything else follows, so a link
/// works exactly once.
pub fn accept_invite(
    config: &Config,
    token: &str,
    password: &str,
) -> Result<(User, String), String> {
    let user = invited_account(config, token).ok_or("This link is not valid.")?;
    if password.chars().count() < 8 {
        return Err("Enter a password of at least 8 characters.".into());
    }

    let hash = hash_password(password)?;

    let conn = open(config)?;
    conn.execute(
        "update users set password_hash = ? where id = ?",
        rusqlite::params![&hash, &user.id],
    )
    .map_err(|e| e.to_string())?;
    conn.execute("delete from invites where token_hash = ?", [hash_token(token)])
        .map_err(|e| e.to_string())?;

    let session = crate::content::slug::random_token(48);
    conn.execute(
        "insert into sessions (token_hash, user_id, expires_at, scope) values (?, ?, ?, null)",
        rusqlite::params![
            hash_token(&session),
            &user.id,
            (now() + SESSION_LIFETIME.as_secs()) as i64
        ],
    )
    .map_err(|e| e.to_string())?;

    Ok((user, session))
}

/// Where to send someone to finish setting up. Absolute when the deployment
/// knows its own address, since this is meant to be pasted into a message.
pub fn invite_url(config: &Config, token: &str) -> String {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    format!("{base}/auth/setup?token={token}")
}

#[derive(serde::Deserialize)]
pub struct InviteToken {
    token: String,
}

pub async fn setup_form(
    State(config): State<Arc<Config>>,
    Query(params): Query<InviteToken>,
) -> Response {
    let token = params.token.clone();
    let config2 = config.clone();
    let account =
        tokio::task::spawn_blocking(move || invited_account(&config2, &token))
            .await
            .ok()
            .flatten();

    let Some(account) = account else {
        return (
            StatusCode::GONE,
            "This link has expired or was used before.",
        )
            .into_response();
    };

    let markup = crate::ui::form_page(
        "Set a password",
        maud::html! {
            form."column" method="post" action="/auth/setup" {
                h1 { "Set a password" }
                input type="hidden" name="token" value=(params.token);
                // A password manager needs the account name in the same form
                // to save the pair; without it, it stores a password with no
                // username and asks the person to type the email by hand.
                input type="email" name="email" value=(account.email)
                      autocomplete="username" readonly;
                input name="password" type="password" placeholder="Password, at least 8 characters"
                      autocomplete="new-password" required autofocus;
                button type="submit" { "Set password and sign in" }
            }
        },
    );
    Html(markup.into_string()).into_response()
}

#[derive(serde::Deserialize)]
pub struct NewPassword {
    token: String,
    password: String,
}

pub async fn setup_submit(
    State(config): State<Arc<Config>>,
    Form(form): Form<NewPassword>,
) -> Response {
    let config2 = config.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        accept_invite(&config2, &form.token, &form.password)
    })
    .await;

    match outcome {
        // Signed in on the spot: having just proved they hold the link and
        // chosen the password, asking them to type it again is theatre.
        Ok(Ok((user, session))) => (
            [(header::SET_COOKIE, set_cookie_header(&config, &session))],
            Redirect::to(if user.is_admin { "/admin" } else { "/" }),
        )
            .into_response(),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "The password was not set.").into_response(),
    }
}
