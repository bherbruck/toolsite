//! Where accounts are kept: the SQL behind `users` and `mfa`, and nothing
//! else.
//!
//! The rules stay with their modules. `users::log_in` still checks the
//! password and decides what a session is; `insert_session` only stores the
//! row. A store never hashes, seals, compares a secret or decides who may do
//! what: it is handed digests and sealed text and gives them back.
//!
//! One implementation so far: `sqlite`, the file `.site/auth.db`. Postgres
//! joins it next. The methods are synchronous because every caller is:
//! account functions run on a blocking thread already (`spawn_blocking`, a
//! wasm host call, the `toolsite user` commands), and an async trait would
//! send each SQLite call through a second blocking hop to arrive where it
//! started.
//!
//! Single use and replay rest on conditional writes, never on a read and a
//! later write: `advance_step`, `spend_recovery_code`, `take_invite` and
//! `enable_mfa` each say in their answer whether this call was the one that
//! moved the row, and that holds the same on both backends under any
//! number of concurrent callers.

pub mod sqlite;

use crate::{accounts::users::User, config::Config};

/// One account as the admin list shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct UserRow {
    pub email: String,
    pub created_at: i64,
    pub is_admin: bool,
    pub disabled_at: Option<i64>,
    /// Two-step sign-in is on.
    pub mfa: bool,
}

/// One scope row, with the account's email.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeRow {
    pub email: String,
    pub user_id: String,
    pub prefix: String,
    pub scope: String,
}

/// A pending two-step sign-in as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRow {
    pub user: User,
    pub stage: String,
    pub next: String,
    pub failures: i64,
}

/// Every statement the account rules need. Emails arrive normalised and
/// tokens arrive as digests; the store compares them as given.
pub trait AccountStore: Send + Sync {
    // --- people ---

    /// Adds an account. `Ok(false)` when the email is taken.
    fn insert_user(&self, user: &User, password_hash: Option<&str>, created_at: i64) -> Result<bool, String>;
    /// The active account with this id.
    fn user_by_id(&self, id: &str) -> Result<Option<User>, String>;
    /// The active account with this email.
    fn user_by_email(&self, email: &str) -> Result<Option<User>, String>;
    /// The account with this email, active or not, and when it was disabled.
    fn account_by_email(&self, email: &str) -> Result<Option<(User, Option<i64>)>, String>;
    /// The active account with this email and its password hash, for a
    /// sign-in to check.
    fn credentials(&self, email: &str) -> Result<Option<(User, Option<String>)>, String>;
    /// The active account's password hash: `None` for no active account,
    /// `Some(None)` for one with no password.
    fn password_hash(&self, user_id: &str) -> Result<Option<Option<String>>, String>;
    /// Whether the account, active or not, has a password at all.
    fn has_password(&self, user_id: &str) -> Result<bool, String>;
    /// Whether the account is active; `None` when there is no such account.
    fn is_active(&self, user_id: &str) -> Result<Option<bool>, String>;
    fn set_password_hash(&self, user_id: &str, hash: &str) -> Result<(), String>;
    /// Disables the account at `at`, or enables it with `None`. Answers the
    /// account's id, or `None` when no account has this email.
    fn set_disabled(&self, email: &str, at: Option<i64>) -> Result<Option<String>, String>;
    /// Every account, by email.
    fn list_users(&self) -> Result<Vec<UserRow>, String>;

    // --- identities at providers ---

    /// Ties a provider identity to an account, replacing any earlier tie.
    fn link_identity(&self, provider: &str, provider_id: &str, user_id: &str) -> Result<(), String>;
    /// The active account a provider identity is tied to.
    fn user_by_identity(&self, provider: &str, provider_id: &str) -> Result<Option<User>, String>;
    /// The providers an account is tied to, by name.
    fn identities_for(&self, user_id: &str) -> Result<Vec<String>, String>;

    // --- sessions ---

    /// Stores a session: a site session when `scope` is `None`, else one
    /// app's.
    fn insert_session(&self, hash: &str, user_id: &str, expires_at: i64, scope: Option<&str>) -> Result<(), String>;
    /// The active account behind a live session of exactly this scope.
    /// Sweeps expired sessions on the way.
    fn session_user(&self, hash: &str, scope: Option<&str>, now: i64) -> Result<Option<User>, String>;
    /// The active account behind a live *site* session, and when it ends.
    fn site_session(&self, hash: &str, now: i64) -> Result<Option<(User, i64)>, String>;
    /// Removes a session and every app session of its account.
    fn end_session(&self, hash: &str) -> Result<(), String>;
    /// Removes every session of an account, but the one `except` names.
    fn delete_sessions_for(&self, user_id: &str, except: Option<&str>) -> Result<(), String>;

    // --- invitations ---

    /// Replaces an account's invitation with this one.
    fn replace_invite(&self, user_id: &str, hash: &str, expires_at: i64) -> Result<(), String>;
    /// The active account a live invitation is for, without spending it.
    fn invited(&self, hash: &str, now: i64) -> Result<Option<User>, String>;
    /// Spends a live invitation of an active account: of any number of
    /// callers with one invitation, exactly one gets the account.
    fn take_invite(&self, hash: &str, now: i64) -> Result<Option<User>, String>;

    // --- per-app grants ---

    fn set_grant(&self, user_id: &str, app: &str, role: &str) -> Result<(), String>;
    fn delete_grant(&self, user_id: &str, app: &str) -> Result<(), String>;
    fn grant_of(&self, user_id: &str, app: &str) -> Result<Option<String>, String>;
    /// Every grant: app, email, role; by app then email.
    fn list_grants(&self) -> Result<Vec<(String, String, String)>, String>;

    // --- platform scopes ---

    /// Sets the account's scope at a prefix, replacing the one there.
    fn set_scope(&self, user_id: &str, prefix: &str, scope: &str, granted_by: Option<&str>, now: i64) -> Result<(), String>;
    /// Removes the account's scope at a prefix. Answers the rows removed.
    fn delete_scope(&self, user_id: &str, prefix: &str) -> Result<usize, String>;
    /// The account's scopes, as prefix and word, by prefix.
    fn scopes_for(&self, user_id: &str) -> Result<Vec<(String, String)>, String>;
    /// Every scope row, by prefix then email.
    fn list_scopes(&self) -> Result<Vec<ScopeRow>, String>;
    /// Moves the rows at exactly `from` to `to`; a row already at `to`
    /// gives way to the one that moves.
    fn rename_scope(&self, from: &str, to: &str) -> Result<(), String>;
    /// Moves every row at `from` or below it to the same place under `to`,
    /// in one transaction, with the same giving way. Answers the rows moved.
    fn move_scope_tree(&self, from: &str, to: &str) -> Result<usize, String>;
    /// Removes every row at `path` or below it. Answers the rows removed.
    fn remove_scope_tree(&self, path: &str) -> Result<usize, String>;
    /// Removes, in one transaction, the rows at `path` or below and every
    /// grant on `app`, and answers what they were:
    /// `{app, path, access: [{email, path, scope}], grants: [{email, role}]}`.
    fn forget_app(&self, app: &str, path: &str) -> Result<serde_json::Value, String>;

    // --- pinned app tools ---

    fn pins_for(&self, user_id: &str) -> Result<Vec<String>, String>;
    fn set_pin(&self, user_id: &str, app: &str, pinned: bool, now: i64) -> Result<(), String>;

    // --- two-step sign-in ---

    /// `Some(true)` when two-step sign-in is on, `Some(false)` while setup
    /// waits, `None` when neither.
    fn mfa_enabled(&self, user_id: &str) -> Result<Option<bool>, String>;
    /// Recovery codes not yet used.
    fn recovery_left(&self, user_id: &str) -> Result<usize, String>;
    /// The sealed secret and last spent step: of the enabled secret when
    /// `setup_by` is `None`, else of the setup the session or pending
    /// sign-in with this digest began.
    fn mfa_secret(&self, user_id: &str, setup_by: Option<&str>) -> Result<Option<(String, i64)>, String>;
    /// Begins setup with a sealed secret, replacing a setup someone else
    /// began. Never touches an enabled secret.
    fn begin_mfa(&self, user_id: &str, sealed: &str, begun_by: &str) -> Result<(), String>;
    fn cancel_mfa_setup(&self, user_id: &str) -> Result<(), String>;
    /// Turns on the setup that waits. Whether this call was the one that did.
    fn enable_mfa(&self, user_id: &str, now: i64) -> Result<bool, String>;
    /// Spends `step` if it is later than the last spent one: of any number
    /// of callers with one step, exactly one is answered `true`.
    fn advance_step(&self, user_id: &str, step: i64) -> Result<bool, String>;
    /// Replaces the account's recovery codes with these digests.
    fn replace_recovery_codes(&self, user_id: &str, hashes: &[String]) -> Result<(), String>;
    /// Spends an unused recovery code: of any number of callers with one
    /// code, exactly one is answered `true`.
    fn spend_recovery_code(&self, user_id: &str, hash: &str, now: i64) -> Result<bool, String>;
    /// Removes the secret, the recovery codes and pending sign-ins.
    fn remove_mfa(&self, user_id: &str) -> Result<(), String>;
    /// Wrong codes counted against the account after `since`.
    fn failures_since(&self, user_id: &str, since: i64) -> Result<i64, String>;
    /// Counts a wrong code at `at`, forgetting every count from
    /// `forget_before` or earlier.
    fn record_failure(&self, user_id: &str, at: i64, forget_before: i64) -> Result<(), String>;
    /// Stores a pending sign-in, sweeping those expired before `now`.
    fn insert_pending(&self, hash: &str, user_id: &str, stage: &str, next: &str, expires_at: i64, now: i64) -> Result<(), String>;
    /// A live pending sign-in of an active account.
    fn pending(&self, hash: &str, now: i64) -> Result<Option<PendingRow>, String>;
    /// Counts one more wrong code against a pending sign-in; answers the
    /// count, or `None` when it is gone.
    fn fail_pending(&self, hash: &str) -> Result<Option<i64>, String>;
    fn delete_pending(&self, hash: &str) -> Result<(), String>;
    /// Removes every pending sign-in of an account.
    fn delete_pending_for(&self, user_id: &str) -> Result<(), String>;
}

/// The account store this site keeps accounts in. Cheap: it holds a path,
/// and opens nothing until a method runs. On either backend for now, since
/// accounts have not moved to Postgres yet.
pub fn of(config: &Config) -> Box<dyn AccountStore> {
    Box::new(sqlite::SqliteAccounts::new(config))
}

#[cfg(test)]
pub(crate) mod conformance;
