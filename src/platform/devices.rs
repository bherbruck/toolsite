//! Letting a device in over TCP or UDP: tokens minted for one app, which the
//! app's own handler checks with `auth.check-token`.
//!
//! A port has no gate in front of it. A sensor or a broker client carries
//! no cookie and follows no redirect, so the platform cannot decide who it
//! is; the app can, by asking whether what the device presented is one of
//! its tokens. A device token is that answer: one app, a label that says
//! which device holds it, revocable on its own. It opens nothing over HTTP.
//!
//! Tokens live hashed in `<app>.devices` beside the app's other sidecars, so
//! removing the app takes them along and a copy of the data directory is not
//! a set of live credentials. Same shape as `export.rs`, for the same
//! reasons.

use crate::{
    config::Config,
    content::slug::random_token,
    platform::export::valid_app,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// Recognisable in a device's config, and in a leak.
pub const PREFIX: &str = "tsv_";
const MAX_LABEL: usize = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceToken {
    /// Short, public, names the token in a listing and a revocation.
    pub id: String,
    /// What `auth.check-token` returns: which device holds the token.
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_used: Option<u64>,
    pub created_at: u64,
    /// The token itself, hashed.
    hash: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn path(config: &Config, app: &str) -> Option<PathBuf> {
    valid_app(app).then(|| config.data_dir.join(format!("{app}.devices")))
}

fn read(config: &Config, app: &str) -> Vec<DeviceToken> {
    let Some(path) = path(config, app) else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(config: &Config, app: &str, tokens: &[DeviceToken]) -> Result<(), String> {
    let path = path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if tokens.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let text = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// Mints a token for `app`. The plain token is returned exactly once, here;
/// after this only its hash exists.
pub fn create(config: &Config, app: &str, label: &str) -> Result<(DeviceToken, String), String> {
    if !valid_app(app) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    let label = label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL {
        return Err(format!("label must be 1 to {MAX_LABEL} characters: say which device will hold the token"));
    }
    let token = format!("{PREFIX}{}", random_token(40));
    let entry = DeviceToken {
        id: random_token(8),
        label: label.to_string(),
        last_used: None,
        created_at: now(),
        hash: hash(&token),
    };
    let mut tokens = read(config, app);
    tokens.push(entry.clone());
    write(config, app, &tokens)?;
    Ok((entry, token))
}

pub fn list(config: &Config, app: &str) -> Vec<DeviceToken> {
    read(config, app)
}

pub fn revoke(config: &Config, app: &str, id: &str) -> Result<(), String> {
    let mut tokens = read(config, app);
    let before = tokens.len();
    tokens.retain(|token| token.id != id);
    if tokens.len() == before {
        return Err(format!("no device token {id} on {app}"));
    }
    write(config, app, &tokens)
}

/// The label of `presented` when it is a live device token for `app`, and
/// nothing otherwise. Marks it used, so a listing can say which devices
/// still connect.
pub fn check(config: &Config, app: &str, presented: &str) -> Option<String> {
    let presented = presented.trim();
    if !presented.starts_with(PREFIX) {
        return None;
    }
    let wanted = hash(presented);
    let mut tokens = read(config, app);
    let token = tokens.iter_mut().find(|token| token.hash == wanted)?;
    token.last_used = Some(now());
    let label = token.label.clone();
    let _ = write(config, app, &tokens);
    Some(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "publish-token");
        (dir, config)
    }

    #[test]
    fn a_device_token_is_shown_once_and_stored_only_as_a_hash() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "broker", "boiler sensor").unwrap();
        assert!(token.starts_with("tsv_"));
        let stored = std::fs::read_to_string(dir.path().join("broker.devices")).unwrap();
        assert!(!stored.contains(&token), "the plain token is on disk");
        assert!(stored.contains(&entry.id));
        assert_eq!(check(&config, "broker", &token).as_deref(), Some("boiler sensor"));
        assert!(list(&config, "broker")[0].last_used.is_some(), "use was not recorded");
    }

    #[test]
    fn a_device_token_names_one_app_and_no_other() {
        let (_dir, config) = config();
        let (_, token) = create(&config, "broker", "x").unwrap();
        assert_eq!(check(&config, "syslog", &token), None);
        assert_eq!(check(&config, "broker", "publish-token"), None, "the publish token was accepted");
        let (_, export) = crate::platform::export::create(&config, "broker", "x").unwrap();
        assert_eq!(check(&config, "broker", &export), None, "an export token was accepted");
        assert_eq!(check(&config, "../broker", &token), None);
    }

    #[test]
    fn revoking_a_device_token_ends_it_and_an_empty_list_leaves_no_file() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "broker", "x").unwrap();
        revoke(&config, "broker", &entry.id).unwrap();
        assert_eq!(check(&config, "broker", &token), None);
        assert!(!dir.path().join("broker.devices").exists());
        assert!(revoke(&config, "broker", &entry.id).is_err());
        for app in ["../x", "a/b", ".site", ""] {
            assert!(create(&config, app, "x").is_err(), "{app:?} accepted");
        }
    }
}
