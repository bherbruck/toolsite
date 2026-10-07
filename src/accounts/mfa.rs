//! Two-step sign-in with an authenticator app: TOTP (RFC 6238), SHA-1, six
//! digits, 30-second steps.
//!
//! A password proves something the person knows; the code proves they hold
//! the phone the secret went to. The two meet in `after_primary`, which
//! every way of signing in goes through once the first proof is in: it
//! answers with a session, or with a *pending* sign-in that the code page
//! (or, when the site's policy requires two-step sign-in and the account has
//! none, the setup page) must finish. A pending sign-in is not a session:
//! it lives in its own table and cookie, and nothing that asks for a
//! session accepts it.
//!
//! What this module holds to:
//!
//! - The shared secret is sealed with the site key (`seal`), never logged,
//!   and shown only while setup waits for its first code.
//! - A code is accepted for the current step and one either side, and each
//!   step once per account (`last_step`), so a code seen over a shoulder or
//!   in a log is already spent.
//! - Recovery codes are stored as keyed digests and work once each.
//! - Five wrong codes end a pending sign-in; ten wrong codes in fifteen
//!   minutes stop an account taking codes at all until the window passes.
//!   Refusals are logged at warn, and the code never is.

use crate::{
    accounts::users::{self, User},
    config::Config,
    seal,
};
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use hmac::{KeyInit, Mac};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Seconds per code, as every authenticator app assumes.
pub const STEP: u64 = 30;
/// How long a pending sign-in waits for its code.
pub const PENDING_LIFETIME: u64 = 5 * 60;
const MAX_FAILURES_PER_PENDING: i64 = 5;
const MAX_FAILURES_PER_ACCOUNT: i64 = 10;
const ACCOUNT_WINDOW: u64 = 15 * 60;
const RECOVERY_CODE_COUNT: usize = 10;
/// 32 symbols, so a random byte maps to one without bias; no `l`, `o`, `0`
/// or `1`, which are easy to misread on paper.
const RECOVERY_ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

// --- settings -----------------------------------------------------------

/// Who must have two-step sign-in, from `TOOLSITE_REQUIRE_MFA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Off,
    /// Site admins. The default: they can change everyone's access.
    Admins,
    Everyone,
}

impl Policy {
    /// `None` is the default, `admins`.
    pub fn parse(value: Option<&str>) -> Result<Policy, String> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("admins") => Ok(Policy::Admins),
            Some("everyone") => Ok(Policy::Everyone),
            Some("off") => Ok(Policy::Off),
            Some(other) => Err(format!("TOOLSITE_REQUIRE_MFA must be admins, everyone or off, not {other:?}")),
        }
    }

    pub fn requires(self, user: &User) -> bool {
        match self {
            Policy::Off => false,
            Policy::Admins => user.is_admin,
            Policy::Everyone => true,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Admins => "admins",
            Policy::Everyone => "everyone",
        }
    }
}

/// The time codes are checked against. The system clock, unless a test
/// fixed it, which is what lets a test compute the code a phone would show.
#[derive(Clone, Default)]
pub struct Clock(Option<Arc<AtomicU64>>);

impl Clock {
    pub fn fixed(unix: u64) -> Clock {
        Clock(Some(Arc::new(AtomicU64::new(unix))))
    }

    /// Moves a fixed clock; does nothing to the system clock.
    pub fn set(&self, unix: u64) {
        if let Some(fixed) = &self.0 {
            fixed.store(unix, Ordering::SeqCst);
        }
    }

    pub fn now(&self) -> u64 {
        match &self.0 {
            Some(fixed) => fixed.load(Ordering::SeqCst),
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub policy: Policy,
    /// `TOOLSITE_MFA_FOR_PROVIDERS=1`: a sign-in through Google, Entra,
    /// GitHub or OIDC also owes toolsite's code. Off by default, since the
    /// provider enforces its own second step.
    pub for_providers: bool,
    pub clock: Clock,
}

impl Settings {
    /// From the two environment values; `None` for one that is unset.
    pub fn from_env(policy: Option<&str>, for_providers: Option<&str>) -> Result<Settings, String> {
        Ok(Settings {
            policy: Policy::parse(policy)?,
            for_providers: for_providers.is_some_and(|v| !matches!(v.trim(), "" | "0" | "false" | "off")),
            clock: Clock::default(),
        })
    }

    /// Nothing required. What `Config::local` starts with, for tests and
    /// embedding; a deployment reads the environment, where the default is
    /// `admins`.
    pub fn off() -> Settings {
        Settings { policy: Policy::Off, for_providers: false, clock: Clock::default() }
    }
}

// --- the code itself ----------------------------------------------------

/// RFC 4226's HOTP, six digits.
fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary = u32::from_be_bytes([digest[offset] & 0x7f, digest[offset + 1], digest[offset + 2], digest[offset + 3]]);
    binary % 1_000_000
}

fn decode_secret(secret: &str) -> Option<Vec<u8>> {
    let cleaned: String = secret.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
    data_encoding::BASE32_NOPAD.decode(cleaned.trim_end_matches('=').as_bytes()).ok()
}

/// The code an authenticator app shows for this base32 secret at this time.
/// Public so a test, or a script driving a browser, can be the phone.
pub fn code_at(secret: &str, unix: u64) -> Option<String> {
    Some(format!("{:06}", hotp(&decode_secret(secret)?, unix / STEP)))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The step a code matches: the current one or one either side, and later
/// than `last_step`, so a step is spent once it is used.
fn matching_step(secret: &[u8], code: &str, now: u64, last_step: u64) -> Option<u64> {
    let current = now / STEP;
    [current.saturating_sub(1), current, current + 1]
        .into_iter()
        .filter(|step| *step > last_step)
        .find(|step| constant_time_eq(format!("{:06}", hotp(secret, *step)).as_bytes(), code.as_bytes()))
}

fn new_secret() -> String {
    let mut bytes = [0u8; 20];
    rand::rng().fill_bytes(&mut bytes);
    data_encoding::BASE32_NOPAD.encode(&bytes)
}

/// The secret in groups of four, for typing into an app by hand.
pub fn grouped(secret: &str) -> String {
    secret.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// The `otpauth://` URI an authenticator app reads from the QR code.
pub fn otpauth_uri(config: &Config, email: &str, secret: &str) -> String {
    let issuer = config
        .base_url
        .as_deref()
        .and_then(|base| url::Url::parse(base).ok())
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "toolsite".to_string());
    format!(
        "otpauth://totp/{}:{}?secret={secret}&issuer={}&algorithm=SHA1&digits=6&period={STEP}",
        urlencoding::encode(&issuer),
        urlencoding::encode(email),
        urlencoding::encode(&issuer),
    )
}

/// The QR code for a URI, as SVG drawn on the server: no script needed to
/// show it. Dark on white whatever the page theme, which is what cameras
/// read best.
pub fn qr_svg(uri: &str) -> String {
    match qrcode::QrCode::new(uri.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(200, 200)
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

/// `abcd-efgh` back to `abcdefgh`: case, spaces and dashes do not matter.
fn normalise_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

fn recovery_hash(config: &Config, user_id: &str, code: &str) -> Result<String, String> {
    seal::keyed_hash(config, &format!("recovery:{user_id}"), code)
}

/// Replaces the account's recovery codes with ten new ones and returns them
/// for showing once, as `abcd-efgh`.
fn new_recovery_codes(config: &Config, conn: &Connection, user_id: &str) -> Result<Vec<String>, String> {
    conn.execute("delete from recovery_codes where user_id = ?", [user_id]).map_err(|e| e.to_string())?;
    let mut shown = Vec::with_capacity(RECOVERY_CODE_COUNT);
    while shown.len() < RECOVERY_CODE_COUNT {
        let mut bytes = [0u8; 8];
        rand::rng().fill_bytes(&mut bytes);
        let code: String = bytes.iter().map(|b| RECOVERY_ALPHABET[(b & 31) as usize] as char).collect();
        let inserted = conn
            .execute(
                "insert or ignore into recovery_codes (code_hash, user_id, used_at) values (?, ?, null)",
                rusqlite::params![recovery_hash(config, user_id, &code)?, user_id],
            )
            .map_err(|e| e.to_string())?;
        if inserted == 1 {
            shown.push(format!("{}-{}", &code[..4], &code[4..]));
        }
    }
    Ok(shown)
}

/// What a code turned out to be.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Used {
    App,
    Recovery,
}

/// Checks a code for an account and spends it if it is good. `confirmed`
/// picks the enabled secret, or the one that setup is waiting on.
fn accept(
    config: &Config,
    conn: &Connection,
    user_id: &str,
    code: &str,
    confirmed: bool,
    allow_recovery: bool,
) -> Result<Option<Used>, String> {
    let code = normalise_code(code);
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
        let row: Option<(String, i64)> = conn
            .query_row(
                "select secret, last_step from mfa where user_id = ? and (enabled_at is not null) = ?",
                rusqlite::params![user_id, confirmed],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((sealed, last_step)) = row else {
            return Ok(None);
        };
        let secret = seal::open(config, &sealed)
            .and_then(|s| decode_secret(&s))
            .ok_or("The two-step secret could not be read. Ask an admin to reset two-step sign-in.")?;
        let Some(step) = matching_step(&secret, &code, config.mfa.clock.now(), last_step.max(0) as u64) else {
            return Ok(None);
        };
        // Only one request can move the step past this point, so two
        // requests racing with the same code cannot both get in.
        let moved = conn
            .execute(
                "update mfa set last_step = ?1 where user_id = ?2 and last_step < ?1",
                rusqlite::params![step as i64, user_id],
            )
            .map_err(|e| e.to_string())?;
        return Ok((moved == 1).then_some(Used::App));
    }
    if allow_recovery && code.len() == 8 {
        let spent = conn
            .execute(
                "update recovery_codes set used_at = ? where code_hash = ? and user_id = ? and used_at is null",
                rusqlite::params![users_now() as i64, recovery_hash(config, user_id, &code)?, user_id],
            )
            .map_err(|e| e.to_string())?;
        return Ok((spent == 1).then_some(Used::Recovery));
    }
    Ok(None)
}

fn users_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn account_locked(config: &Config, conn: &Connection, user_id: &str) -> bool {
    let since = config.mfa.clock.now().saturating_sub(ACCOUNT_WINDOW) as i64;
    conn.query_row(
        "select count(*) from mfa_failures where user_id = ? and at > ?",
        rusqlite::params![user_id, since],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n >= MAX_FAILURES_PER_ACCOUNT)
    .unwrap_or(true)
}

fn record_failure(config: &Config, conn: &Connection, user_id: &str) {
    let now = config.mfa.clock.now();
    let _ = conn.execute(
        "delete from mfa_failures where at <= ?",
        [now.saturating_sub(ACCOUNT_WINDOW) as i64],
    );
    let _ = conn.execute(
        "insert into mfa_failures (user_id, at) values (?, ?)",
        rusqlite::params![user_id, now as i64],
    );
}

/// Checks a code from a signed-in person (turning it off, new recovery
/// codes), under the same per-account limit as sign-in.
fn accept_from_account(config: &Config, conn: &Connection, user: &User, code: &str, allow_recovery: bool) -> Result<Used, String> {
    if account_locked(config, conn, &user.id) {
        tracing::warn!(email = %user.email, "two-step code refused: too many wrong codes for this account");
        return Err("Too many wrong codes. Wait 15 minutes, then try again.".into());
    }
    match accept(config, conn, &user.id, code, true, allow_recovery)? {
        Some(used) => Ok(used),
        None => {
            record_failure(config, conn, &user.id);
            tracing::warn!(email = %user.email, "two-step code refused on the account page: not correct");
            Err("The code is not correct.".into())
        }
    }
}

// --- the account's own settings ----------------------------------------

/// What the account page shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub enabled: bool,
    /// Setup has begun and waits for its first code.
    pub setting_up: bool,
    pub recovery_left: usize,
}

pub fn status(config: &Config, user_id: &str) -> Status {
    let Ok(conn) = users::open(config) else {
        return Status::default();
    };
    let enabled: Option<bool> = conn
        .query_row("select enabled_at is not null from mfa where user_id = ?", [user_id], |row| row.get(0))
        .optional()
        .ok()
        .flatten();
    let recovery_left = conn
        .query_row(
            "select count(*) from recovery_codes where user_id = ? and used_at is null",
            [user_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize;
    Status {
        enabled: enabled == Some(true),
        setting_up: enabled == Some(false),
        recovery_left: if enabled == Some(true) { recovery_left } else { 0 },
    }
}

pub fn is_enabled(config: &Config, user_id: &str) -> bool {
    status(config, user_id).enabled
}

/// The secret setup is waiting on, if setup has begun. Nothing once it is
/// confirmed: after that the secret is never shown again.
pub fn setup_secret(config: &Config, user_id: &str) -> Option<String> {
    let conn = users::open(config).ok()?;
    let sealed: String = conn
        .query_row("select secret from mfa where user_id = ? and enabled_at is null", [user_id], |row| row.get(0))
        .ok()?;
    seal::open(config, &sealed)
}

/// Starts setup, or returns the secret of a setup already begun, so a
/// reload shows the same QR code the phone may already have scanned.
pub fn begin_setup(config: &Config, user_id: &str) -> Result<String, String> {
    if is_enabled(config, user_id) {
        return Err("Two-step sign-in is already on.".into());
    }
    if let Some(secret) = setup_secret(config, user_id) {
        return Ok(secret);
    }
    let secret = new_secret();
    let conn = users::open(config)?;
    conn.execute(
        "insert into mfa (user_id, secret, enabled_at, last_step) values (?, ?, null, 0)
         on conflict(user_id) do update set secret = excluded.secret, last_step = 0 where enabled_at is null",
        rusqlite::params![user_id, seal::seal(config, &secret)?],
    )
    .map_err(|e| e.to_string())?;
    Ok(secret)
}

pub fn cancel_setup(config: &Config, user_id: &str) -> Result<(), String> {
    let conn = users::open(config)?;
    conn.execute("delete from mfa where user_id = ? and enabled_at is null", [user_id]).map_err(|e| e.to_string())?;
    Ok(())
}

/// Turns it on once setup's secret is in the phone, and ends every session
/// of the account except `keep` (the raw site token of the session that
/// confirmed, if any), app sessions included: whatever was signed in
/// without a code now has to sign in with one. Returns the recovery codes,
/// for showing once.
fn enable(config: &Config, conn: &Connection, user_id: &str, keep: Option<&str>) -> Result<Vec<String>, String> {
    let changed = conn
        .execute(
            "update mfa set enabled_at = ? where user_id = ? and enabled_at is null",
            rusqlite::params![users_now() as i64, user_id],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Start setup again.".into());
    }
    let codes = new_recovery_codes(config, conn, user_id)?;
    let keep = keep.map(users::hash_token).unwrap_or_default();
    conn.execute(
        "delete from sessions where user_id = ? and token_hash != ?",
        rusqlite::params![user_id, keep],
    )
    .map_err(|e| e.to_string())?;
    conn.execute("delete from mfa_pending where user_id = ?", [user_id]).map_err(|e| e.to_string())?;
    Ok(codes)
}

/// Confirms setup from the account page with the first code. `keep` is the
/// site session that sent it, which stays signed in.
pub fn confirm_setup(config: &Config, user: &User, code: &str, keep: &str) -> Result<Vec<String>, String> {
    let conn = users::open(config)?;
    if accept(config, &conn, &user.id, code, false, false)?.is_none() {
        tracing::warn!(email = %user.email, "two-step setup refused: the code is not correct");
        return Err("The code is not correct. Check the time on your phone, then enter the code the app shows now.".into());
    }
    let codes = enable(config, &conn, &user.id, Some(keep))?;
    tracing::info!(email = %user.email, "two-step sign-in turned on");
    Ok(codes)
}

/// New recovery codes, replacing the old ones. Takes a code from the app,
/// not a recovery code: whoever holds one recovery code should not be able
/// to mint ten.
pub fn regenerate_recovery_codes(config: &Config, user: &User, code: &str) -> Result<Vec<String>, String> {
    let conn = users::open(config)?;
    if !is_enabled(config, &user.id) {
        return Err("Two-step sign-in is off.".into());
    }
    if normalise_code(code).len() != 6 {
        return Err("Enter the 6-digit code from your authenticator app.".into());
    }
    accept_from_account(config, &conn, user, code, false)?;
    let codes = new_recovery_codes(config, &conn, &user.id)?;
    tracing::info!(email = %user.email, "recovery codes replaced");
    Ok(codes)
}

/// Turns it off with a code from the app or a recovery code. Refused when
/// the site's policy requires it for this person.
pub fn turn_off(config: &Config, user: &User, code: &str) -> Result<(), String> {
    if config.mfa.policy.requires(user) {
        return Err("This site requires two-step sign-in for your account. You cannot turn it off.".into());
    }
    let conn = users::open(config)?;
    if !is_enabled(config, &user.id) {
        return Err("Two-step sign-in is off.".into());
    }
    accept_from_account(config, &conn, user, code, true)?;
    remove(&conn, &user.id)?;
    tracing::info!(email = %user.email, "two-step sign-in turned off");
    Ok(())
}

fn remove(conn: &Connection, user_id: &str) -> Result<(), String> {
    for sql in [
        "delete from mfa where user_id = ?",
        "delete from recovery_codes where user_id = ?",
        "delete from mfa_pending where user_id = ?",
    ] {
        conn.execute(sql, [user_id]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// An admin's reset, for someone who lost their phone and their recovery
/// codes: removes the secret and the codes and ends every session of the
/// account, so whoever might hold the phone is signed out too. Says whether
/// two-step sign-in was on.
pub fn reset(config: &Config, email: &str) -> Result<bool, String> {
    let conn = users::open(config)?;
    let user_id: String = conn
        .query_row("select id from users where email = ?", [email.trim().to_lowercase()], |row| row.get(0))
        .map_err(|_| format!("no account for {}", email.trim()))?;
    let was_on = is_enabled(config, &user_id);
    remove(&conn, &user_id)?;
    conn.execute("delete from sessions where user_id = ?", [&user_id]).map_err(|e| e.to_string())?;
    Ok(was_on)
}

// --- signing in -----------------------------------------------------------

/// How the first proof was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primary {
    /// A password, or a setup link (which sets one).
    Password,
    /// Google, Entra, GitHub or an OIDC issuer.
    Provider,
}

/// What follows the first proof.
#[derive(Debug)]
pub enum Step {
    /// No code owed: a site session token.
    Session(String),
    /// A pending sign-in token that the code page finishes.
    Code(String),
    /// A pending sign-in token that the setup page finishes.
    Setup(String),
}

/// The one door every sign-in goes through after its first proof.
pub fn after_primary(config: &Config, user: &User, primary: Primary, next: &str) -> Result<Step, String> {
    let applies = primary == Primary::Password || config.mfa.for_providers;
    if applies && is_enabled(config, &user.id) {
        return Ok(Step::Code(new_pending(config, &user.id, "code", next)?));
    }
    if applies && config.mfa.policy.requires(user) {
        return Ok(Step::Setup(new_pending(config, &user.id, "setup", next)?));
    }
    users::start_session(config, &user.id).map(Step::Session)
}

fn new_pending(config: &Config, user_id: &str, stage: &str, next: &str) -> Result<String, String> {
    let conn = users::open(config)?;
    let now = config.mfa.clock.now();
    let _ = conn.execute("delete from mfa_pending where expires_at < ?", [now as i64]);
    let token = crate::content::slug::random_token(48);
    conn.execute(
        "insert into mfa_pending (token_hash, user_id, stage, next, expires_at, failures) values (?, ?, ?, ?, ?, 0)",
        rusqlite::params![users::hash_token(&token), user_id, stage, next, (now + PENDING_LIFETIME) as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(token)
}

/// A pending sign-in, while it lasts.
pub struct Pending {
    pub user: User,
    pub stage: String,
    pub next: String,
    failures: i64,
}

fn load_pending(config: &Config, conn: &Connection, token: &str) -> Option<Pending> {
    conn.query_row(
        "select users.id, users.email, users.is_admin, mfa_pending.stage, mfa_pending.next, mfa_pending.failures
           from mfa_pending join users on users.id = mfa_pending.user_id
          where mfa_pending.token_hash = ? and mfa_pending.expires_at >= ? and users.disabled_at is null",
        rusqlite::params![users::hash_token(token), config.mfa.clock.now() as i64],
        |row| {
            Ok(Pending {
                user: User { id: row.get(0)?, email: row.get(1)?, is_admin: row.get::<_, i64>(2)? != 0 },
                stage: row.get(3)?,
                next: row.get(4)?,
                failures: row.get(5)?,
            })
        },
    )
    .ok()
}

pub fn pending(config: &Config, token: &str) -> Option<Pending> {
    load_pending(config, &users::open(config).ok()?, token)
}

/// Why a code did not finish a sign-in.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Not correct; this many tries left.
    Wrong(i64),
    /// Too many wrong codes: the pending sign-in is gone.
    Ended,
    /// Too many wrong codes for the account across sign-ins.
    Locked,
    /// No pending sign-in: never was, expired, or already finished.
    Expired,
    Failed(String),
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::Wrong(1) => "The code is not correct. You can try 1 more time.".into(),
            Refusal::Wrong(left) => format!("The code is not correct. You can try {left} more times."),
            Refusal::Ended => "Too many wrong codes. Sign in again.".into(),
            Refusal::Locked => "Too many wrong codes for this account. Wait 15 minutes, then sign in again.".into(),
            Refusal::Expired => "This sign-in has expired. Sign in again.".into(),
            Refusal::Failed(_) => "An error occurred on this server. Sign in again.".into(),
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Refusal::Locked => StatusCode::TOO_MANY_REQUESTS,
            Refusal::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNAUTHORIZED,
        }
    }
}

/// A finished sign-in: the account, its new site session, where to go, and
/// recovery codes when the sign-in also turned two-step sign-in on.
pub struct Finished {
    pub user: User,
    pub session: String,
    pub next: String,
    pub recovery_codes: Vec<String>,
}

/// Counts a wrong code against a pending sign-in, ending it at the limit.
fn wrong_code(conn: &Connection, token: &str, pending: &Pending, what: &str) -> Refusal {
    let failures = pending.failures + 1;
    tracing::warn!(email = %pending.user.email, failures, "{what}: the code is not correct");
    if failures >= MAX_FAILURES_PER_PENDING {
        let _ = conn.execute("delete from mfa_pending where token_hash = ?", [users::hash_token(token)]);
        tracing::warn!(email = %pending.user.email, "{what}: too many wrong codes, the pending sign-in ended");
        return Refusal::Ended;
    }
    let _ = conn.execute(
        "update mfa_pending set failures = ? where token_hash = ?",
        rusqlite::params![failures, users::hash_token(token)],
    );
    Refusal::Wrong(MAX_FAILURES_PER_PENDING - failures)
}

fn finish(conn: &Connection, config: &Config, token: &str, pending: Pending, recovery_codes: Vec<String>) -> Result<Finished, Refusal> {
    conn.execute("delete from mfa_pending where token_hash = ?", [users::hash_token(token)])
        .map_err(|e| Refusal::Failed(e.to_string()))?;
    let session = users::start_session(config, &pending.user.id).map_err(Refusal::Failed)?;
    Ok(Finished { user: pending.user, session, next: pending.next, recovery_codes })
}

/// The code page's answer: a code from the app or a recovery code.
pub fn finish_with_code(config: &Config, token: &str, code: &str) -> Result<Finished, Refusal> {
    let conn = users::open(config).map_err(Refusal::Failed)?;
    let pending = load_pending(config, &conn, token).filter(|p| p.stage == "code").ok_or(Refusal::Expired)?;
    if account_locked(config, &conn, &pending.user.id) {
        tracing::warn!(email = %pending.user.email, "two-step sign-in refused: too many wrong codes for this account");
        return Err(Refusal::Locked);
    }
    match accept(config, &conn, &pending.user.id, code, true, true).map_err(Refusal::Failed)? {
        Some(used) => {
            tracing::info!(
                email = %pending.user.email,
                with = if used == Used::Recovery { "a recovery code" } else { "the authenticator app" },
                "two-step sign-in finished"
            );
            finish(&conn, config, token, pending, Vec::new())
        }
        None => {
            record_failure(config, &conn, &pending.user.id);
            Err(wrong_code(&conn, token, &pending, "two-step sign-in refused"))
        }
    }
}

/// The secret for the setup a pending sign-in owes, begun on first view.
pub fn pending_setup(config: &Config, token: &str) -> Option<(Pending, String)> {
    let pending = pending(config, token).filter(|p| p.stage == "setup")?;
    let secret = begin_setup(config, &pending.user.id).ok()?;
    Some((pending, secret))
}

/// Finishes a sign-in that the policy held for setup: the first code turns
/// two-step sign-in on and the session follows.
pub fn finish_setup(config: &Config, token: &str, code: &str) -> Result<Finished, Refusal> {
    let conn = users::open(config).map_err(Refusal::Failed)?;
    let pending = load_pending(config, &conn, token).filter(|p| p.stage == "setup").ok_or(Refusal::Expired)?;
    match accept(config, &conn, &pending.user.id, code, false, false).map_err(Refusal::Failed)? {
        Some(_) => {
            let codes = enable(config, &conn, &pending.user.id, None).map_err(Refusal::Failed)?;
            tracing::info!(email = %pending.user.email, "two-step sign-in turned on at sign-in, as the site requires");
            finish(&conn, config, token, pending, codes)
        }
        None => Err(wrong_code(&conn, token, &pending, "two-step setup refused")),
    }
}

// --- the cookie -----------------------------------------------------------

/// The pending sign-in's cookie. `ts_` like every cookie of toolsite's, so
/// no app sees it, and `__Host-` wherever the site session is.
fn cookie_name(config: &Config) -> &'static str {
    if users::site_cookie_name(config).starts_with("__Host-") { "__Host-ts_mfa" } else { "ts_mfa" }
}

pub fn pending_cookie_header(config: &Config, token: &str) -> String {
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax;{} Max-Age={PENDING_LIFETIME}",
        cookie_name(config),
        users::secure_flag(config)
    )
}

pub fn clear_pending_cookie_header(config: &Config) -> String {
    format!("{}=; Path=/; HttpOnly; SameSite=Lax;{} Max-Age=0", cookie_name(config), users::secure_flag(config))
}

pub fn pending_token(config: &Config, headers: &HeaderMap) -> Option<String> {
    users::cookie_value(headers.get(header::COOKIE).and_then(|v| v.to_str().ok()), cookie_name(config))
}

/// Adds Set-Cookie headers to a response, each its own header.
pub fn with_cookies(mut response: Response, cookies: &[String]) -> Response {
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// --- HTTP -------------------------------------------------------------------

/// What every sign-in route answers once the first proof is in: a session
/// and the way on, or the code page, or the setup page.
pub async fn sign_in(config: &Arc<Config>, user: User, primary: Primary, next: &str) -> Response {
    let (worker, owned_next) = (config.clone(), next.to_string());
    let step = tokio::task::spawn_blocking(move || after_primary(&worker, &user, primary, &owned_next)).await;
    match step {
        Ok(Ok(Step::Session(token))) => with_cookies(
            Redirect::to(next).into_response(),
            &[users::set_cookie_header(config, &token), clear_pending_cookie_header(config)],
        ),
        Ok(Ok(Step::Code(token))) => {
            with_cookies(Redirect::to("/auth/mfa").into_response(), &[pending_cookie_header(config, &token)])
        }
        Ok(Ok(Step::Setup(token))) => {
            with_cookies(Redirect::to("/auth/mfa/setup").into_response(), &[pending_cookie_header(config, &token)])
        }
        Ok(Err(message)) => {
            tracing::warn!(%message, "sign-in refused after the first proof");
            (StatusCode::FORBIDDEN, message).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed.").into_response(),
    }
}

/// A page saying a sign-in cannot go on, with the way back to the start.
pub fn ended_page(config: &Config, refusal: &Refusal) -> Response {
    if let Refusal::Failed(why) = refusal {
        tracing::warn!(%why, "two-step sign-in failed");
    }
    let markup = crate::ui::form_page(
        "Two-step sign-in",
        maud::html! {
            div."column" {
                h1 { "Two-step sign-in" }
                div."flash error" role="alert" { span { (refusal.message()) } }
                a."btn" href="/auth/login" { "Sign in again" }
            }
        },
    );
    with_cookies((refusal.status(), Html(markup.into_string())).into_response(), &[clear_pending_cookie_header(config)])
}

fn code_page(pending: &Pending, error: Option<&str>) -> maud::Markup {
    crate::ui::form_page(
        "Two-step sign-in",
        maud::html! {
            form."column" method="post" action="/auth/mfa" {
                h1 { "Two-step sign-in" }
                p."muted" { "Signed in as " strong { (pending.user.email) } "." }
                @if let Some(error) = error {
                    div."flash error" role="alert" { span { (error) } }
                }
                label for="code" { "Enter the 6-digit code from your authenticator app." }
                input id="code" name="code" type="text" inputmode="numeric" autocomplete="one-time-code"
                      placeholder="123456" maxlength="20" required autofocus;
                button type="submit" { "Continue" }
                p."muted" style="margin: .25rem 0 0; font-size: .8rem" {
                    "No phone? Enter one of your recovery codes in the same field. Each recovery code works one time."
                }
            }
            p."muted" style="margin: .75rem 0 0; font-size: .8rem" {
                a href="/auth/login" { "Cancel and sign in again" }
            }
        },
    )
}

/// `GET /auth/mfa`: asks for the code.
pub async fn code_form(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let Some(token) = pending_token(&config, &headers) else {
        return Redirect::to("/auth/login").into_response();
    };
    let worker = config.clone();
    let found = tokio::task::spawn_blocking(move || pending(&worker, &token)).await.ok().flatten();
    match found {
        Some(p) if p.stage == "code" => {
            with_cookies(Html(code_page(&p, None).into_string()).into_response(), &[])
        }
        Some(_) => Redirect::to("/auth/mfa/setup").into_response(),
        None => ended_page(&config, &Refusal::Expired),
    }
}

#[derive(serde::Deserialize)]
pub struct CodeForm {
    code: String,
}

impl CodeForm {
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// `POST /auth/mfa`: the code, then the session.
pub async fn code_submit(State(config): State<Arc<Config>>, headers: HeaderMap, Form(form): Form<CodeForm>) -> Response {
    let Some(token) = pending_token(&config, &headers) else {
        tracing::warn!("two-step sign-in refused: no pending sign-in cookie");
        return ended_page(&config, &Refusal::Expired);
    };
    let worker = config.clone();
    let lookup = token.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let result = finish_with_code(&worker, &lookup, &form.code);
        let still = match &result {
            Err(Refusal::Wrong(_)) => pending(&worker, &lookup),
            _ => None,
        };
        (result, still)
    })
    .await;
    match outcome {
        Ok((Ok(done), _)) => with_cookies(
            Redirect::to(&done.next).into_response(),
            &[users::set_cookie_header(&config, &done.session), clear_pending_cookie_header(&config)],
        ),
        Ok((Err(refusal @ Refusal::Wrong(_)), Some(p))) => with_cookies(
            (StatusCode::UNAUTHORIZED, Html(code_page(&p, Some(&refusal.message())).into_string())).into_response(),
            &[],
        ),
        Ok((Err(refusal), _)) => ended_page(&config, &refusal),
        Err(_) => ended_page(&config, &Refusal::Failed("the task did not finish".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238, appendix B, SHA-1: the 8-digit values' last six digits.
    #[test]
    fn the_code_matches_the_rfc_6238_test_vectors() {
        let secret = data_encoding::BASE32_NOPAD.encode(b"12345678901234567890");
        for (time, code) in [(59, "287082"), (1111111109, "081804"), (1111111111, "050471"), (1234567890, "005924"), (2000000000, "279037")] {
            assert_eq!(code_at(&secret, time).as_deref(), Some(code), "at {time}");
        }
    }

    #[test]
    fn a_code_counts_one_step_either_side_and_not_two() {
        let raw = b"12345678901234567890";
        let secret = data_encoding::BASE32_NOPAD.encode(raw);
        let now = 1_000_000_020;
        for offset in [-30i64, 0, 30] {
            let code = code_at(&secret, (now as i64 + offset) as u64).unwrap();
            assert!(matching_step(raw, &code, now, 0).is_some(), "offset {offset} refused");
        }
        for offset in [-60i64, 60] {
            let code = code_at(&secret, (now as i64 + offset) as u64).unwrap();
            assert!(matching_step(raw, &code, now, 0).is_none(), "offset {offset} accepted");
        }
        // A step already spent is refused even inside the window.
        let code = code_at(&secret, now).unwrap();
        assert!(matching_step(raw, &code, now, now / STEP).is_none());
    }

    #[test]
    fn the_policy_reads_admins_by_default() {
        assert_eq!(Policy::parse(None).unwrap(), Policy::Admins);
        assert_eq!(Policy::parse(Some("everyone")).unwrap(), Policy::Everyone);
        assert_eq!(Policy::parse(Some("OFF")).unwrap(), Policy::Off);
        assert!(Policy::parse(Some("sometimes")).is_err());
        assert!(!Settings::from_env(None, None).unwrap().for_providers);
        assert!(Settings::from_env(None, Some("1")).unwrap().for_providers);
        assert!(!Settings::from_env(None, Some("0")).unwrap().for_providers);
    }

    #[test]
    fn a_recovery_code_is_read_however_it_is_typed() {
        assert_eq!(normalise_code(" ABCD-efgh "), "abcdefgh");
        assert_eq!(normalise_code("123 456"), "123456");
    }

    #[test]
    fn the_qr_code_is_an_svg() {
        let svg = qr_svg("otpauth://totp/site:a%40b.c?secret=JBSWY3DPEHPK3PXP");
        assert!(svg.contains("<svg"), "{svg}");
    }
}
