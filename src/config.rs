use crate::{accounts::providers::Provider, platform::github, runtime::blobs::Blobs};
use std::path::PathBuf;

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
    /// Ceiling on any one SQLite file. Zero means none.
    pub max_db_bytes: u64,
    /// Where apps' files go, and how big one may be.
    pub blobs: Blobs,
    /// Ways to sign in besides a password, from the environment.
    pub providers: Vec<Provider>,
    /// The gate an app has until it says otherwise. "public" for a site on
    /// the open internet; "granted" or "authenticated" for an internal one.
    pub default_gate: String,
    /// The GitHub App this site speaks as, when one is configured. Lets an
    /// app live in a repository and deploy from it.
    pub github: Option<github::App>,
    /// What takes screenshots, when anything does: a local browser or a
    /// sidecar. Chosen once at boot.
    pub renderer: Option<std::sync::Arc<dyn crate::platform::screenshot::Renderer>>,
    /// The base a renderer opens the one-time preview URL on. This server's
    /// own port unless `TOOLSITE_PREVIEW_BASE` names an address a sidecar
    /// can reach.
    pub preview_base: String,
    /// Live connections: every app's open sockets, one registry per
    /// process. Shared with every task's copy of this config, so a
    /// scheduled job's publish reaches the same sockets a request's does.
    pub connections: std::sync::Arc<crate::runtime::connections::Hub>,
    /// TCP and UDP ports the site's owner mapped to apps, from
    /// `TOOLSITE_PORTS`. Empty means no port beyond HTTP is opened.
    pub ports: crate::platform::ports::PortMap,
    /// Apps that run resident: one long-lived instance each, shared like
    /// `connections` so every copy of this config reaches the same ones.
    /// Also carries the memory default and ceiling, from
    /// `TOOLSITE_RESIDENT_MEMORY_MB` and `TOOLSITE_RESIDENT_MAX_MB`.
    pub residents: std::sync::Arc<crate::runtime::resident::Residents>,
    /// Subdomain mode, from `TOOLSITE_APPS_DOMAIN`: each app on a host of
    /// its own under this domain. `None` is path mode, every app under
    /// `/p/` on the main host.
    pub apps: Option<crate::content::origins::AppsDomain>,
    /// Two-step sign-in: who must have it, from `TOOLSITE_REQUIRE_MFA` and
    /// `TOOLSITE_MFA_FOR_PROVIDERS`, and the clock codes are checked by.
    pub mfa: crate::accounts::mfa::Settings,
    /// The most an app's `[limits]` may ask for, from the
    /// `TOOLSITE_MAX_*` variables.
    pub limits: crate::runtime::limits::Ceilings,
    /// Jobs in progress and queued, shared like `connections` so the
    /// scheduler, a person and an app all see one run per job.
    pub jobs: std::sync::Arc<crate::platform::schedule::Jobs>,
    /// Where platform state lives: files, or Postgres when `DATABASE_URL`
    /// is set. Chosen once in `main`; every copy shares the same handles.
    pub stores: crate::state::Stores,
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
            max_db_bytes: self.max_db_bytes,
            // A task that needs the bucket gets a config that knows it; the
            // scheduler runs handlers, which may well store files.
            blobs: self.blobs.clone_settings(),
            default_gate: self.default_gate.clone(),
            providers: Vec::new(),
            github: None,
            renderer: self.renderer.clone(),
            preview_base: self.preview_base.clone(),
            connections: self.connections.clone(),
            ports: self.ports.clone(),
            residents: self.residents.clone(),
            apps: self.apps.clone(),
            mfa: self.mfa.clone(),
            limits: self.limits.clone(),
            jobs: self.jobs.clone(),
            stores: self.stores.clone(),
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
            max_db_bytes: DEFAULT_MAX_DB_BYTES,
            blobs: Blobs::local(DEFAULT_MAX_BLOB_BYTES),
            providers: Vec::new(),
            default_gate: "public".to_string(),
            github: None,
            renderer: None,
            preview_base: "http://127.0.0.1:8080".to_string(),
            connections: std::sync::Arc::new(crate::runtime::connections::Hub::default()),
            ports: Default::default(),
            residents: Default::default(),
            apps: None,
            mfa: crate::accounts::mfa::Settings::off(),
            limits: Default::default(),
            jobs: Default::default(),
            stores: Default::default(),
        }
    }
}
