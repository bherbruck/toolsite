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
//!   and shown only while setup waits for its first code, and then only to
//!   the session or pending sign-in that began that setup.
//! - A code is accepted for the current step and one either side, and each
//!   step once per account (`last_step`), so a code seen over a shoulder or
//!   in a log is already spent.
//! - Recovery codes are stored as keyed digests and work once each.
//! - Five wrong codes end a pending sign-in; ten wrong codes in fifteen
//!   minutes stop an account taking codes at all until the window passes.
//!   Refusals are logged at warn, and the code never is.

use crate::{
    accounts::{
        store::{self, AccountStore},
        users::{self, User},
    },
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
fn new_recovery_codes(config: &Config, accounts: &dyn AccountStore, user_id: &str) -> Result<Vec<String>, String> {
    let mut codes: Vec<String> = Vec::with_capacity(RECOVERY_CODE_COUNT);
    while codes.len() < RECOVERY_CODE_COUNT {
        let mut bytes = [0u8; 8];
        rand::rng().fill_bytes(&mut bytes);
        let code: String = bytes.iter().map(|b| RECOVERY_ALPHABET[(b & 31) as usize] as char).collect();
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    let hashes = codes.iter().map(|code| recovery_hash(config, user_id, code)).collect::<Result<Vec<_>, _>>()?;
    accounts.replace_recovery_codes(user_id, &hashes)?;
    Ok(codes.iter().map(|code| format!("{}-{}", &code[..4], &code[4..])).collect())
}

/// What a code turned out to be.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Used {
    App,
    Recovery,
}

/// Which secret a code is checked against.
#[derive(Clone, Copy)]
enum Secret<'a> {
    /// The one that is on.
    Enabled,
    /// The one setup waits on, if the sign-in whose raw token this is began
    /// it. Anyone else's setup is not there to confirm.
    Setup(&'a str),
}

/// Checks a code for an account and spends it if it is good.
fn accept(
    config: &Config,
    accounts: &dyn AccountStore,
    user_id: &str,
    code: &str,
    secret: Secret<'_>,
    allow_recovery: bool,
) -> Result<Option<Used>, String> {
    let code = normalise_code(code);
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
        let setup_by = match secret {
            Secret::Enabled => None,
            Secret::Setup(by) => Some(users::hash_token(by)),
        };
        let Some((sealed, last_step)) = accounts.mfa_secret(user_id, setup_by.as_deref())? else {
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
        return Ok(accounts.advance_step(user_id, step as i64)?.then_some(Used::App));
    }
    if allow_recovery && code.len() == 8 {
        let spent = accounts.spend_recovery_code(user_id, &recovery_hash(config, user_id, &code)?, users_now() as i64)?;
        return Ok(spent.then_some(Used::Recovery));
    }
    Ok(None)
}

fn users_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Counts a code against the account before it is checked, so codes sent
/// at once cannot all be checked under a limit none of them has counted
/// yet. `Some` is the count to take back if the code is right; `None` when
/// the account has had its limit, or the count could not be made.
fn take_attempt(config: &Config, accounts: &dyn AccountStore, user_id: &str) -> Option<i64> {
    let now = config.mfa.clock.now();
    let since = now.saturating_sub(ACCOUNT_WINDOW) as i64;
    accounts
        .try_attempt(user_id, now as i64, since, MAX_FAILURES_PER_ACCOUNT)
        .ok()?
        .then_some(now as i64)
}

/// Checks a code from a signed-in person (turning it off, new recovery
/// codes), under the same per-account limit as sign-in.
fn accept_from_account(config: &Config, accounts: &dyn AccountStore, user: &User, code: &str, allow_recovery: bool) -> Result<Used, String> {
    let Some(at) = take_attempt(config, accounts, &user.id) else {
        tracing::warn!(email = %user.email, "two-step code refused: too many wrong codes for this account");
        return Err("Too many wrong codes. Wait 15 minutes, then try again.".into());
    };
    match accept(config, accounts, &user.id, code, Secret::Enabled, allow_recovery) {
        Ok(Some(used)) => {
            let _ = accounts.release_attempt(&user.id, at);
            Ok(used)
        }
        Ok(None) => {
            tracing::warn!(email = %user.email, "two-step code refused on the account page: not correct");
            Err("The code is not correct.".into())
        }
        Err(why) => {
            let _ = accounts.release_attempt(&user.id, at);
            Err(why)
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
    let accounts = store::of(config);
    let enabled = accounts.mfa_enabled(user_id).ok().flatten();
    let recovery_left = accounts.recovery_left(user_id).unwrap_or(0);
    Status {
        enabled: enabled == Some(true),
        setting_up: enabled == Some(false),
        recovery_left: if enabled == Some(true) { recovery_left } else { 0 },
    }
}

pub fn is_enabled(config: &Config, user_id: &str) -> bool {
    status(config, user_id).enabled
}

/// The secret setup is waiting on, if setup has begun and `by` (the raw
/// token of a site session or a pending sign-in) began it. Nothing once it
/// is confirmed: after that the secret is never shown again.
pub fn setup_secret(config: &Config, user_id: &str, by: &str) -> Option<String> {
    let (sealed, _) = store::of(config).mfa_secret(user_id, Some(&users::hash_token(by))).ok()??;
    seal::open(config, &sealed)
}

/// Starts setup for `by`, or returns the secret of the setup `by` already
/// began, so a reload shows the same QR code the phone may already have
/// scanned. A setup another sign-in began is replaced with a new secret:
/// whoever saw that one must not be holding the secret this person scans.
pub fn begin_setup(config: &Config, user_id: &str, by: &str) -> Result<String, String> {
    if is_enabled(config, user_id) {
        return Err("Two-step sign-in is already on.".into());
    }
    if let Some(secret) = setup_secret(config, user_id, by) {
        return Ok(secret);
    }
    let secret = new_secret();
    store::of(config).begin_mfa(user_id, &seal::seal(config, &secret)?, &users::hash_token(by))?;
    Ok(secret)
}

/// Whether the site's policy requires two-step sign-in of this account and
/// it is not on: a session from before the policy, or one that came through
/// a door the policy does not cover. Such a session may set it up, and may
/// not use an admin's powers until it has.
pub fn owes_setup(config: &Config, user: &User) -> bool {
    config.mfa.policy.requires(user)
        && (config.mfa.for_providers || users::has_password(config, &user.id))
        && !is_enabled(config, &user.id)
}

pub fn cancel_setup(config: &Config, user_id: &str) -> Result<(), String> {
    store::of(config).cancel_mfa_setup(user_id)
}

/// Turns it on once setup's secret is in the phone, and ends every session
/// of the account except `keep` (the raw site token of the session that
/// confirmed, if any), app sessions included: whatever was signed in
/// without a code now has to sign in with one. Returns the recovery codes,
/// for showing once.
fn enable(config: &Config, accounts: &dyn AccountStore, user_id: &str, keep: Option<&str>) -> Result<Vec<String>, String> {
    if !accounts.enable_mfa(user_id, users_now() as i64)? {
        return Err("Start setup again.".into());
    }
    let codes = new_recovery_codes(config, accounts, user_id)?;
    accounts.delete_sessions_for(user_id, keep.map(users::hash_token).as_deref())?;
    accounts.delete_pending_for(user_id)?;
    Ok(codes)
}

/// Confirms setup from the account page with the first code. `keep` is the
/// site session that sent it, which stays signed in, and which must be the
/// one that began the setup.
pub fn confirm_setup(config: &Config, user: &User, code: &str, keep: &str) -> Result<Vec<String>, String> {
    let accounts = store::of(config);
    if accept(config, &*accounts, &user.id, code, Secret::Setup(keep), false)?.is_none() {
        tracing::warn!(email = %user.email, "two-step setup refused: the code is not correct");
        return Err("The code is not correct. Check the time on your phone, then enter the code the app shows now.".into());
    }
    let codes = enable(config, &*accounts, &user.id, Some(keep))?;
    tracing::info!(email = %user.email, "two-step sign-in turned on");
    Ok(codes)
}

/// New recovery codes, replacing the old ones. Takes a code from the app,
/// not a recovery code: whoever holds one recovery code should not be able
/// to mint ten.
pub fn regenerate_recovery_codes(config: &Config, user: &User, code: &str) -> Result<Vec<String>, String> {
    let accounts = store::of(config);
    if !is_enabled(config, &user.id) {
        return Err("Two-step sign-in is off.".into());
    }
    if normalise_code(code).len() != 6 {
        return Err("Enter the 6-digit code from your authenticator app.".into());
    }
    accept_from_account(config, &*accounts, user, code, false)?;
    let codes = new_recovery_codes(config, &*accounts, &user.id)?;
    tracing::info!(email = %user.email, "recovery codes replaced");
    Ok(codes)
}

/// Turns it off with a code from the app or a recovery code. Refused when
/// the site's policy requires it for this person.
pub fn turn_off(config: &Config, user: &User, code: &str) -> Result<(), String> {
    if config.mfa.policy.requires(user) {
        return Err("This site requires two-step sign-in for your account. You cannot turn it off.".into());
    }
    let accounts = store::of(config);
    if !is_enabled(config, &user.id) {
        return Err("Two-step sign-in is off.".into());
    }
    accept_from_account(config, &*accounts, user, code, true)?;
    accounts.remove_mfa(&user.id)?;
    tracing::info!(email = %user.email, "two-step sign-in turned off");
    Ok(())
}

/// An admin's reset, for someone who lost their phone and their recovery
/// codes: removes the secret and the codes and ends every session of the
/// account, so whoever might hold the phone is signed out too. Returns the
/// account's id, for the caller to revoke its OAuth tokens as well (they are
/// the platform's), and whether two-step sign-in was on.
pub fn reset(config: &Config, email: &str) -> Result<(String, bool), String> {
    let accounts = store::of(config);
    let (user, _) = accounts
        .account_by_email(&email.trim().to_lowercase())
        .ok()
        .flatten()
        .ok_or_else(|| format!("no account for {}", email.trim()))?;
    let was_on = is_enabled(config, &user.id);
    accounts.remove_mfa(&user.id)?;
    accounts.delete_sessions_for(&user.id, None)?;
    Ok((user.id, was_on))
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
    let now = config.mfa.clock.now();
    let token = crate::content::slug::random_token(48);
    store::of(config).insert_pending(&users::hash_token(&token), user_id, stage, next, (now + PENDING_LIFETIME) as i64, now as i64)?;
    Ok(token)
}

/// A pending sign-in, while it lasts.
pub struct Pending {
    pub user: User,
    pub stage: String,
    pub next: String,
}

fn load_pending(config: &Config, accounts: &dyn AccountStore, token: &str) -> Option<Pending> {
    let row = accounts.pending(&users::hash_token(token), config.mfa.clock.now() as i64).ok()??;
    Some(Pending { user: row.user, stage: row.stage, next: row.next })
}

pub fn pending(config: &Config, token: &str) -> Option<Pending> {
    load_pending(config, &*store::of(config), token)
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

/// Takes one of a pending sign-in's tries before its code is checked, so
/// codes sent at once cannot all be checked: answers which try this is, or
/// ends the sign-in when it has had them all.
fn take_try(accounts: &dyn AccountStore, token: &str, pending: &Pending, what: &str) -> Result<i64, Refusal> {
    let hash = users::hash_token(token);
    match accounts.try_pending(&hash, MAX_FAILURES_PER_PENDING).map_err(Refusal::Failed)? {
        Some(tries) => Ok(tries),
        None => {
            let _ = accounts.delete_pending(&hash);
            tracing::warn!(email = %pending.user.email, "{what}: too many codes, the pending sign-in ended");
            Err(Refusal::Ended)
        }
    }
}

/// A wrong code on the `tries`th try, ending the sign-in on the last.
fn wrong_code(accounts: &dyn AccountStore, token: &str, pending: &Pending, tries: i64, what: &str) -> Refusal {
    tracing::warn!(email = %pending.user.email, failures = tries, "{what}: the code is not correct");
    if tries >= MAX_FAILURES_PER_PENDING {
        let _ = accounts.delete_pending(&users::hash_token(token));
        tracing::warn!(email = %pending.user.email, "{what}: too many wrong codes, the pending sign-in ended");
        return Refusal::Ended;
    }
    Refusal::Wrong(MAX_FAILURES_PER_PENDING - tries)
}

fn finish(accounts: &dyn AccountStore, config: &Config, token: &str, pending: Pending, recovery_codes: Vec<String>) -> Result<Finished, Refusal> {
    accounts.delete_pending(&users::hash_token(token)).map_err(Refusal::Failed)?;
    let session = users::start_session(config, &pending.user.id).map_err(Refusal::Failed)?;
    Ok(Finished { user: pending.user, session, next: pending.next, recovery_codes })
}

/// The code page's answer: a code from the app or a recovery code.
pub fn finish_with_code(config: &Config, token: &str, code: &str) -> Result<Finished, Refusal> {
    let accounts = store::of(config);
    let accounts = &*accounts;
    let pending = load_pending(config, accounts, token).filter(|p| p.stage == "code").ok_or(Refusal::Expired)?;
    let tries = take_try(accounts, token, &pending, "two-step sign-in refused")?;
    let Some(at) = take_attempt(config, accounts, &pending.user.id) else {
        tracing::warn!(email = %pending.user.email, "two-step sign-in refused: too many wrong codes for this account");
        return Err(Refusal::Locked);
    };
    match accept(config, accounts, &pending.user.id, code, Secret::Enabled, true) {
        Ok(Some(used)) => {
            let _ = accounts.release_attempt(&pending.user.id, at);
            tracing::info!(
                email = %pending.user.email,
                with = if used == Used::Recovery { "a recovery code" } else { "the authenticator app" },
                "two-step sign-in finished"
            );
            finish(accounts, config, token, pending, Vec::new())
        }
        Ok(None) => Err(wrong_code(accounts, token, &pending, tries, "two-step sign-in refused")),
        Err(why) => {
            let _ = accounts.release_attempt(&pending.user.id, at);
            Err(Refusal::Failed(why))
        }
    }
}

/// The secret for the setup a pending sign-in owes, begun on first view.
pub fn pending_setup(config: &Config, token: &str) -> Option<(Pending, String)> {
    let pending = pending(config, token).filter(|p| p.stage == "setup")?;
    let secret = begin_setup(config, &pending.user.id, token).ok()?;
    Some((pending, secret))
}

/// Finishes a sign-in that the policy held for setup: the first code turns
/// two-step sign-in on and the session follows.
pub fn finish_setup(config: &Config, token: &str, code: &str) -> Result<Finished, Refusal> {
    let accounts = store::of(config);
    let accounts = &*accounts;
    let pending = load_pending(config, accounts, token).filter(|p| p.stage == "setup").ok_or(Refusal::Expired)?;
    // Another sign-in began a setup of its own since this one was shown its
    // secret: this one is over, rather than taking the setup back.
    let still_ours = accounts
        .mfa_secret(&pending.user.id, Some(&users::hash_token(token)))
        .map_err(Refusal::Failed)?
        .is_some();
    if !still_ours {
        tracing::warn!(email = %pending.user.email, "two-step setup refused: another sign-in began setup since");
        let _ = accounts.delete_pending(&users::hash_token(token));
        return Err(Refusal::Expired);
    }
    let tries = take_try(accounts, token, &pending, "two-step setup refused")?;
    match accept(config, accounts, &pending.user.id, code, Secret::Setup(token), false).map_err(Refusal::Failed)? {
        Some(_) => {
            let codes = enable(config, accounts, &pending.user.id, None).map_err(Refusal::Failed)?;
            tracing::info!(email = %pending.user.email, "two-step sign-in turned on at sign-in, as the site requires");
            finish(accounts, config, token, pending, codes)
        }
        None => Err(wrong_code(accounts, token, &pending, tries, "two-step setup refused")),
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
