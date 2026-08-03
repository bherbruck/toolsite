use crate::{oauth::OAuth, upload::UploadTicket};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

pub(crate) struct Config {
    pub(crate) data_dir: PathBuf,
    pub(crate) base_url: Option<String>,
    /// Stand-in for `base_url` when it isn't configured, so upload URLs handed
    /// to an agent are still something it can actually curl.
    pub(crate) local_base: String,
    pub(crate) valid_tokens: Vec<String>,
    pub(crate) oauth: Option<OAuth>,
    /// Per-app SQLite, opt-in until the surrounding wasm layer exists.
    pub(crate) databases: bool,
    pub(crate) uploads: Mutex<HashMap<String, UploadTicket>>,
}
