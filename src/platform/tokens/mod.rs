//! Tokens minted for one app: export tokens (`GET /export/<app>.sqlite`),
//! deploy tokens (`PUT /deploy/<app>`) and device tokens (a device's
//! `auth.check-token`), kept in one store. What each opens, and the HTTP in
//! front of it, stays in `export`, `deploy` and `devices`; this is where
//! they are minted, listed, checked and revoked.
//!
//! A token is stored only as the SHA-256 of what was handed out, beside a
//! short public id, a label and when it was made and last used. A check
//! compares the digest against each of the app's tokens of that kind in
//! constant time, so the time taken says nothing about how close a guess
//! came or which token matched.
//!
//! Recording use never rewrites a token list: on files the list changes
//! under a lock per sidecar and is renamed into place, so a token minted
//! while another is used is never lost; on Postgres each token is a row and
//! `last_used` is one conditional update. Either way `last_used` is written
//! only when it is older than the kind's resolution, so a device checking
//! its token on every message does not write on every message.
//!
//! Two implementations: `files`, `<app>.exports`, `<app>.deploys` and
//! `<app>.devices` beside the app, as they always were; and `postgres`,
//! `platform.app_tokens`.

pub mod files;
pub mod postgres;

#[cfg(test)]
mod conformance;

use crate::{
    config::Config,
    content::slug::{random_token, valid_slug},
    state::Backend,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Export,
    Deploy,
    Device,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Export, Kind::Deploy, Kind::Device];

    /// What a plain token of this kind starts with: recognisable at a
    /// glance in a config field, and in a leak.
    pub const fn prefix(self) -> &'static str {
        match self {
            Kind::Export => "tse_",
            Kind::Deploy => "tsd_",
            Kind::Device => "tsv_",
        }
    }

    /// The name stored on Postgres.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Export => "export",
            Kind::Deploy => "deploy",
            Kind::Device => "device",
        }
    }

    /// The sidecar's extension on files.
    pub fn extension(self) -> &'static str {
        match self {
            Kind::Export => "exports",
            Kind::Deploy => "deploys",
            Kind::Device => "devices",
        }
    }

    /// How stale `last_used` may be before a use writes it again. Export
    /// and deploy tokens are used a few times a day, and a listing says
    /// "last used" to the second; a device may check on every message.
    pub fn resolution(self) -> u64 {
        match self {
            Kind::Export | Kind::Deploy => 0,
            Kind::Device => 60,
        }
    }
}

/// One token, as a listing shows it. The digest stays in the store.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Token {
    /// Short, public, names the token in a listing and a revocation.
    pub id: String,
    /// What holds the token. For a device token, what `auth.check-token`
    /// returns.
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_used: Option<u64>,
    pub created_at: u64,
}

/// Where tokens live. App names arrive validated; digests arrive computed.
#[async_trait]
pub trait Tokens: Send + Sync {
    /// Keeps a new token. `hash` is the digest of the plain token.
    async fn insert(&self, app: &str, kind: Kind, token: &Token, hash: &str) -> Result<(), String>;
    /// An app's tokens of a kind, oldest first.
    async fn list(&self, app: &str, kind: Kind) -> Result<Vec<Token>, String>;
    /// Every app's tokens of a kind, by app and then age.
    async fn list_all(&self, kind: Kind) -> Result<Vec<(String, Token)>, String>;
    /// Removes one token. Answers whether there was one.
    async fn revoke(&self, app: &str, kind: Kind, id: &str) -> Result<bool, String>;
    /// Removes every token of a kind for the app.
    async fn revoke_all(&self, app: &str, kind: Kind) -> Result<(), String>;
    /// The token of this app and kind whose digest is `hash`, if one is
    /// live, with its use recorded when `last_used` is older than `now`
    /// less the kind's resolution. Every stored digest is compared in
    /// constant time.
    async fn check(&self, app: &str, kind: Kind, hash: &str, now: u64) -> Result<Option<Token>, String>;
    /// `check` for a handler's host call, on a blocking thread.
    fn check_blocking(&self, app: &str, kind: Kind, hash: &str, now: u64) -> Result<Option<Token>, String>;
    /// Takes every token of `app` out, as a removal does, and answers each
    /// kind's as the sidecar it is on files: (extension, contents).
    fn retire_blocking(&self, app: &str, at: u64) -> Result<Vec<(&'static str, String)>, String>;
}

/// The tokens this site's backend keeps. Cheap: a path or a pool handle.
pub fn of(config: &Config) -> Arc<dyn Tokens> {
    match &config.stores.backend {
        Backend::Files => Arc::new(files::Files::new(config.data_dir.clone())),
        Backend::Postgres(postgres) => Arc::new(postgres::Postgres::new(postgres.pool.clone())),
    }
}

const MAX_LABEL: usize = 60;
/// Longer than any token this mints; anything past it is not one, and is
/// refused before it is hashed.
pub const MAX_TOKEN: usize = 256;

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// Equal without saying, by how long it took, how much of it was.
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An app here is a top-level directory, the first segment of any slug. A
/// nested name would make the sidecar path ambiguous.
pub fn valid_app(app: &str) -> bool {
    valid_slug(app) && !app.contains('/')
}

/// Mints a token for `app`. The plain token is returned exactly once, here;
/// after this only its digest exists. `holder` finishes the sentence "say
/// ..." in the refusal of a bad label.
pub async fn create(config: &Config, app: &str, kind: Kind, label: &str, holder: &str) -> Result<(Token, String), String> {
    if !valid_app(app) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    let label = label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL {
        return Err(format!("label must be 1 to {MAX_LABEL} characters: say {holder}"));
    }
    // A label is shown in listings and handed to a handler; a control
    // character in one is a mistake or a trick, and Postgres refuses a NUL.
    if label.chars().any(char::is_control) {
        return Err("label must not hold control characters".into());
    }
    let plain = format!("{}{}", kind.prefix(), random_token(40));
    let token = Token { id: random_token(8), label: label.to_string(), last_used: None, created_at: now() };
    of(config).insert(app, kind, &token, &hash(&plain)).await?;
    Ok((token, plain))
}

/// An app's tokens of a kind; none, logged, when they cannot be read.
pub async fn list(config: &Config, app: &str, kind: Kind) -> Vec<Token> {
    if !valid_app(app) {
        return Vec::new();
    }
    of(config).list(app, kind).await.unwrap_or_else(|why| {
        tracing::warn!(app, kind = kind.name(), %why, "tokens could not be listed");
        Vec::new()
    })
}

pub async fn list_all(config: &Config, kind: Kind) -> Vec<(String, Token)> {
    of(config).list_all(kind).await.unwrap_or_else(|why| {
        tracing::warn!(kind = kind.name(), %why, "tokens could not be listed");
        Vec::new()
    })
}

pub async fn revoke(config: &Config, app: &str, kind: Kind, id: &str) -> Result<(), String> {
    if valid_app(app) && of(config).revoke(app, kind, id).await? {
        return Ok(());
    }
    Err(format!("no {} token {id} on {app}", kind.name()))
}

pub async fn revoke_all(config: &Config, app: &str, kind: Kind) -> Result<(), String> {
    if !valid_app(app) {
        return Err(format!("invalid app name '{app}'"));
    }
    of(config).revoke_all(app, kind).await
}

/// What a presented token must look like before it is hashed: the kind's
/// prefix, and no longer than any token this mints.
fn plausible(kind: Kind, app: &str, presented: &str) -> bool {
    valid_app(app) && presented.starts_with(kind.prefix()) && presented.len() <= MAX_TOKEN
}

/// The live token `presented` is for `app`, if it is one, with its use
/// recorded. A store that cannot be read admits nobody.
pub async fn check(config: &Config, app: &str, kind: Kind, presented: &str) -> Option<Token> {
    if !plausible(kind, app, presented) {
        return None;
    }
    of(config).check(app, kind, &hash(presented), now()).await.unwrap_or_else(|why| {
        tracing::warn!(app, kind = kind.name(), %why, "a token could not be checked; it is refused");
        None
    })
}

/// `check` for a synchronous caller on a blocking thread.
pub fn check_blocking(config: &Config, app: &str, kind: Kind, presented: &str) -> Option<Token> {
    if !plausible(kind, app, presented) {
        return None;
    }
    of(config).check_blocking(app, kind, &hash(presented), now()).unwrap_or_else(|why| {
        tracing::warn!(app, kind = kind.name(), %why, "a token could not be checked; it is refused");
        None
    })
}

/// For "3 days ago" in a listing.
pub fn seconds_since(then: u64) -> u64 {
    now().saturating_sub(then)
}
