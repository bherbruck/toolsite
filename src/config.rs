use crate::{
    accounts::providers::{PendingLogin, Provider},
    platform::{github, upload::UploadTicket},
    runtime::blobs::{self, Blobs},
};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

/// A few gigabytes: room for real data without a single app being able to
/// fill the volume by accident. Zero, from the environment, means no ceiling.
pub const DEFAULT_MAX_DB_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub struct Config {
    pub data_dir: PathBuf,
    pub base_url: Option<String>,
    /// Stand-in for `base_url` when it isn't configured, so upload URLs handed
    /// to an agent are still something it can actually curl.
    pub local_base: String,
    /// Static tokens accepted at `/mcp` as they are. Signing in through the
    /// OAuth server needs none of these; it is on whenever `base_url` is.
    pub valid_tokens: Vec<String>,
    pub uploads: Mutex<HashMap<String, UploadTicket>>,
    /// Ceiling on any one SQLite file. Zero means none.
    pub max_db_bytes: u64,
    /// Where apps' files go, and how big one may be.
    pub blobs: Blobs,
    /// Browser uploads a handler has minted and nobody has spent yet.
    pub blob_uploads: Mutex<HashMap<String, blobs::UploadTicket>>,
    /// Ways to sign in besides a password, from the environment.
    pub providers: Vec<Provider>,
    /// Provider sign-ins begun and not yet answered, by state.
    pub logins: Mutex<HashMap<String, PendingLogin>>,
    /// The GitHub App this site speaks as, when one is configured. Lets an
    /// app live in a repository and deploy from it.
    pub github: Option<github::App>,
}

impl Config {
    /// The OAuth server needs an issuer URL to put in its metadata, so it
    /// exists exactly when the deployment knows its own address.
    pub fn oauth_enabled(&self) -> bool {
        self.base_url.is_some()
    }

    /// A standalone Config carrying only what a background task needs: the
    /// data directory and where URLs point.
    pub fn clone_for_task(&self) -> Config {
        Config {
            data_dir: self.data_dir.clone(),
            base_url: self.base_url.clone(),
            local_base: self.local_base.clone(),
            valid_tokens: Vec::new(),
            uploads: Mutex::new(HashMap::new()),
            max_db_bytes: self.max_db_bytes,
            // A task that needs the bucket gets a config that knows it; the
            // scheduler runs handlers, which may well store files.
            blobs: self.blobs.clone_settings(),
            blob_uploads: Mutex::new(HashMap::new()),
            providers: Vec::new(),
            logins: Mutex::new(HashMap::new()),
            github: None,
        }
    }

    /// A bearer-only instance backed by `data_dir`. Used by tests and by
    /// anything embedding the server with no public address, and so no
    /// OAuth server.
    pub fn local(data_dir: PathBuf, token: impl Into<String>) -> Self {
        Self {
            data_dir,
            base_url: None,
            local_base: "http://localhost:8080".to_string(),
            valid_tokens: vec![token.into()],
            uploads: Mutex::new(HashMap::new()),
            max_db_bytes: DEFAULT_MAX_DB_BYTES,
            blobs: Blobs::local(DEFAULT_MAX_BLOB_BYTES),
            blob_uploads: Mutex::new(HashMap::new()),
            providers: Vec::new(),
            logins: Mutex::new(HashMap::new()),
            github: None,
        }
    }
}
