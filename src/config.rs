use crate::{oauth::OAuth, upload::UploadTicket};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

pub struct Config {
    pub data_dir: PathBuf,
    pub base_url: Option<String>,
    /// Stand-in for `base_url` when it isn't configured, so upload URLs handed
    /// to an agent are still something it can actually curl.
    pub local_base: String,
    pub valid_tokens: Vec<String>,
    pub oauth: Option<OAuth>,
    /// Per-app SQLite, opt-in until the surrounding wasm layer exists.
    pub databases: bool,
    pub uploads: Mutex<HashMap<String, UploadTicket>>,
}

impl Config {
    /// A bearer-only instance backed by `data_dir`. Used by tests and by
    /// anything embedding the server without the OAuth shim.
    pub fn local(data_dir: PathBuf, token: impl Into<String>, databases: bool) -> Self {
        Self {
            data_dir,
            base_url: None,
            local_base: "http://localhost:8080".to_string(),
            valid_tokens: vec![token.into()],
            oauth: None,
            databases,
            uploads: Mutex::new(HashMap::new()),
        }
    }
}
