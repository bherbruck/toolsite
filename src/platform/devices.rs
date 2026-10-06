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
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Recognisable in a device's config, and in a leak.
pub const PREFIX: &str = "tsv_";
const MAX_LABEL: usize = 60;
/// Longer than any token this mints; anything past it is not one, and is
/// refused before it is hashed.
const MAX_TOKEN: usize = 256;
/// How stale `last_used` may be before a check writes it again. A device
/// that checks its token on every message would otherwise rewrite the file
/// on every message.
const USE_RESOLUTION: u64 = 60;

/// Every change to a `.devices` file is read, changed and written under
/// this, so a check recording use can never write back a token that a
/// revocation running beside it just removed, nor drop one just minted.
static WRITING: Mutex<()> = Mutex::new(());

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
    // Through a rename, so a check reading at the same moment sees the old
    // list or the new one and never half of one, which would read as none.
    // Under `.tmp/`, which no URL can name, while it is half written.
    let spool = config.data_dir.join(".tmp");
    std::fs::create_dir_all(&spool).map_err(|e| e.to_string())?;
    let partial = spool.join(format!("{app}.devices.{}", random_token(8)));
    std::fs::write(&partial, text).map_err(|e| e.to_string())?;
    std::fs::rename(&partial, &path).map_err(|e| e.to_string())
}

/// Equal without saying, by how long it took, how much of it was.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut tokens = read(config, app);
    tokens.push(entry.clone());
    write(config, app, &tokens)?;
    Ok((entry, token))
}

pub fn list(config: &Config, app: &str) -> Vec<DeviceToken> {
    read(config, app)
}

pub fn revoke(config: &Config, app: &str, id: &str) -> Result<(), String> {
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
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
    if !presented.starts_with(PREFIX) || presented.len() > MAX_TOKEN {
        return None;
    }
    let wanted = hash(presented);
    // Every entry is compared, so the time taken says nothing about which.
    let tokens = read(config, app);
    let found = tokens
        .iter()
        .enumerate()
        .fold(None, |found, (at, token)| if same(&token.hash, &wanted) { Some(at) } else { found })?;
    let label = tokens[found].label.clone();
    if tokens[found].last_used.is_none_or(|at| now().saturating_sub(at) >= USE_RESOLUTION) {
        // Read again under the lock: the token may have been revoked since.
        let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let mut tokens = read(config, app);
        let token = tokens.iter_mut().find(|token| same(&token.hash, &wanted))?;
        token.last_used = Some(now());
        let _ = write(config, app, &tokens);
    }
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

    #[test]
    fn a_check_recording_use_never_brings_back_a_token_revoked_beside_it() {
        let (_dir, config) = config();
        let config = std::sync::Arc::new(config);
        for _ in 0..200 {
            let (_, keep) = create(&config, "broker", "keep").unwrap();
            let (doomed, doomed_token) = create(&config, "broker", "doomed").unwrap();
            // A fresh token's first check writes its use; the revocation
            // writes at the same moment.
            let checking = {
                let (config, keep) = (config.clone(), keep.clone());
                std::thread::spawn(move || check(&config, "broker", &keep))
            };
            revoke(&config, "broker", &doomed.id).unwrap();
            assert!(checking.join().unwrap().is_some());
            assert_eq!(check(&config, "broker", &doomed_token), None, "a revoked token came back");
        }
    }

    #[test]
    fn a_token_minted_while_devices_check_is_never_lost() {
        let (_dir, config) = config();
        let config = std::sync::Arc::new(config);
        let (_, first) = create(&config, "broker", "first").unwrap();
        let minting: Vec<_> = (0..8)
            .map(|n| {
                let config = config.clone();
                std::thread::spawn(move || {
                    (0..20).map(|m| create(&config, "broker", &format!("d{n}-{m}")).unwrap().1).collect::<Vec<_>>()
                })
            })
            .collect();
        let checking: Vec<_> = (0..4)
            .map(|_| {
                let (config, first) = (config.clone(), first.clone());
                std::thread::spawn(move || (0..100).all(|_| check(&config, "broker", &first).is_some()))
            })
            .collect();
        let minted: Vec<String> = minting.into_iter().flat_map(|t| t.join().unwrap()).collect();
        assert!(checking.into_iter().all(|t| t.join().unwrap()), "a check missed a live token mid-write");
        for token in &minted {
            assert!(check(&config, "broker", token).is_some(), "a minted token was lost");
        }
        assert_eq!(list(&config, "broker").len(), minted.len() + 1);
    }

    #[test]
    fn checking_a_token_on_every_message_does_not_rewrite_the_file_every_time() {
        let (dir, config) = config();
        let (_, token) = create(&config, "broker", "chatty").unwrap();
        check(&config, "broker", &token).unwrap();
        let path = dir.path().join("broker.devices");
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..50 {
            check(&config, "broker", &token).unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), written);
    }

    #[test]
    fn a_token_that_is_too_long_or_off_by_one_is_refused() {
        let (_dir, config) = config();
        let (_, token) = create(&config, "broker", "x").unwrap();
        let mut near = token.clone().into_bytes();
        let last = near.len() - 1;
        near[last] = if near[last] == b'a' { b'b' } else { b'a' };
        assert_eq!(check(&config, "broker", &String::from_utf8(near).unwrap()), None);
        assert_eq!(check(&config, "broker", &format!("{token}{}", " ".repeat(10))).as_deref(), Some("x"));
        assert_eq!(check(&config, "broker", &format!("{token}{}", "a".repeat(MAX_TOKEN))), None);
        assert!(!same("abc", "abd") && !same("abc", "ab") && same("abc", "abc"));
    }
}
