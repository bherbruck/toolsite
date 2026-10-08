//! What the OAuth server remembers: registered clients, codes in flight, and
//! the tokens it has handed out. In file mode that is `.site/oauth.db`
//! (`sqlite.rs`); on Postgres it is the `oauth` schema (`postgres.rs`). The
//! rules of the flow live in `client_oauth`; this module only keeps rows.
//!
//! Everything a client presents is stored hashed, so a copy of either store
//! is not a set of live credentials. Rows name a user by the id accounts gave
//! them and never join across to that store; the caller resolves the id
//! through the accounts API, which is also where "still active" is decided.
//!
//! Single use is the store's promise, on both backends: a code is redeemed
//! and a refresh token rotated at most once, however many requests (or
//! runners) present it at the same moment.

mod postgres;
mod sqlite;

#[cfg(test)]
mod conformance;

use crate::{config::Config, state};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub use postgres::PostgresOAuth;
pub use sqlite::SqliteOAuth;

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

/// The consent a person just gave, waiting to be exchanged.
pub struct Grant<'a> {
    pub client_id: &'a str,
    pub user_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    /// The resource the client asked for, normalised, if it named one.
    pub resource: Option<&'a str>,
}

/// What a redeemed code was issued for. The caller still has to check that
/// the client, redirect URI and PKCE verifier presented match.
#[derive(Debug)]
pub struct Redeemed {
    pub client_id: String,
    pub user_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub resource: Option<String>,
}

#[derive(Debug)]
pub struct Issued {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// The rows the OAuth server keeps, on whichever backend the site runs.
/// The same names as the free functions below, which are this trait called
/// from a blocking thread.
#[async_trait::async_trait]
pub trait OAuthStore: Send + Sync {
    /// Records a client and hands back its id. Registration proves nothing
    /// and grants nothing: a token still needs a person to sign in and
    /// consent, and the redirect URIs are pinned here so that consent can
    /// only be sent where the client said up front. Sweeps first.
    async fn register_client(&self, name: Option<&str>, redirect_uris: &[String]) -> Result<Client, String>;

    async fn client(&self, id: &str) -> Option<Client>;

    /// Mints a one-time code for the grant.
    async fn issue_code(&self, grant: &Grant<'_>) -> Result<String, String>;

    /// Consumes a code: whatever the outcome of the checks that follow, the
    /// same code cannot be tried twice. Nothing for a code that is unknown or
    /// expired; an expired code is spent all the same.
    async fn redeem_code(&self, code: &str) -> Option<Redeemed>;

    /// A fresh access/refresh pair for this person and client, bound to the
    /// resource the grant named.
    async fn issue_tokens(&self, client_id: &str, user_id: &str, resource: Option<&str>) -> Result<Issued, String>;

    /// Trades a refresh token for a new pair, retiring the old one in the
    /// same transaction: a refresh token works exactly once, so a copy that
    /// surfaces later is refused rather than quietly minting a second
    /// connection. Nothing if the token is unknown, expired, or was issued to
    /// a different client. The new pair is for the same resource.
    async fn rotate_refresh(&self, client_id: &str, refresh_token: &str) -> Option<Issued>;

    /// The holder, the client and the resource a live access token was
    /// issued for. Whether that account may still act is the accounts API's
    /// answer, not this store's. Sweeps first.
    async fn access_token_grant(&self, token: &str) -> Option<(String, String, Option<String>)>;

    /// Ends every connection an account's clients hold: its access and
    /// refresh tokens and any code not yet exchanged. Returns how many rows
    /// went.
    async fn revoke_for_user(&self, user_id: &str) -> Result<usize, String>;

    /// Expired rows go opportunistically rather than on a timer, as sessions
    /// do: expired tokens and codes, and clients idle past their lifetime
    /// that hold neither.
    async fn sweep(&self);
}

/// The store for this site's backend. Cheap: the SQLite store opens its file
/// per call, as it always has, and the Postgres one shares the pool.
pub fn of(config: &Config) -> Arc<dyn OAuthStore> {
    match &config.stores.backend {
        state::Backend::Files => Arc::new(SqliteOAuth::of(config)),
        state::Backend::Postgres(pg) => Arc::new(PostgresOAuth::new(pg.pool.clone())),
    }
}

// The same calls for code that is already on a blocking thread: an account
// function, a `toolsite user` command, a test helper. File mode runs the
// SQLite code directly, exactly as before the trait; Postgres goes through
// `state::wait`, which refuses an async worker thread loudly.

fn on_postgres(config: &Config) -> Option<PostgresOAuth> {
    match &config.stores.backend {
        state::Backend::Files => None,
        state::Backend::Postgres(pg) => Some(PostgresOAuth::new(pg.pool.clone())),
    }
}

pub fn register_client(config: &Config, name: Option<&str>, redirect_uris: &[String]) -> Result<Client, String> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).register_client(name, redirect_uris),
        Some(pg) => state::wait(pg.register_client(name, redirect_uris)),
    }
}

pub fn client(config: &Config, id: &str) -> Option<Client> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).client(id),
        Some(pg) => state::wait(pg.client(id)),
    }
}

pub fn issue_code(config: &Config, grant: &Grant<'_>) -> Result<String, String> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).issue_code(grant),
        Some(pg) => state::wait(pg.issue_code(grant)),
    }
}

pub fn redeem_code(config: &Config, code: &str) -> Option<Redeemed> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).redeem_code(code),
        Some(pg) => state::wait(pg.redeem_code(code)),
    }
}

pub fn issue_tokens(config: &Config, client_id: &str, user_id: &str, resource: Option<&str>) -> Result<Issued, String> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).issue_tokens(client_id, user_id, resource),
        Some(pg) => state::wait(pg.issue_tokens(client_id, user_id, resource)),
    }
}

pub fn rotate_refresh(config: &Config, client_id: &str, refresh_token: &str) -> Option<Issued> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).rotate_refresh(client_id, refresh_token),
        Some(pg) => state::wait(pg.rotate_refresh(client_id, refresh_token)),
    }
}

/// Who an access token was issued to, if it is live: which account and
/// which client.
pub fn access_token_holder(config: &Config, token: &str) -> Option<(String, String)> {
    access_token_grant(config, token).map(|(user, client, _)| (user, client))
}

pub fn access_token_grant(config: &Config, token: &str) -> Option<(String, String, Option<String>)> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).access_token_grant(token),
        Some(pg) => state::wait(pg.access_token_grant(token)),
    }
}

/// What turning on two-step sign-in, a new password and an admin's reset do,
/// so a client connected before has to sign in again.
pub fn revoke_for_user(config: &Config, user_id: &str) -> Result<usize, String> {
    match on_postgres(config) {
        None => SqliteOAuth::of(config).revoke_for_user(user_id),
        Some(pg) => state::wait(pg.revoke_for_user(user_id)),
    }
}

/// What the conformance suite needs and the trait must not offer: moving a
/// row's clock back, and looking at what is stored.
#[cfg(test)]
#[async_trait::async_trait]
pub(crate) trait Backdoor {
    /// Moves expiry (codes, tokens) or registration (clients) back by `by`
    /// seconds, or forward when it is negative.
    async fn age(&self, rows: Aged, by: i64);
    /// Every token hash stored, with its kind and user.
    async fn stored_tokens(&self) -> Vec<(String, String, String)>;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Aged {
    Codes,
    Tokens,
    Clients,
}

#[cfg(test)]
mod tests;
