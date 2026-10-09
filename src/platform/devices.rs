//! Letting a device in over TCP or UDP: tokens minted for one app, which the
//! app's own handler checks with `auth.check-token`.
//!
//! A port has no gate in front of it. A sensor or a broker client carries
//! no cookie and follows no redirect, so the platform cannot decide who it
//! is; the app can, by asking whether what the device presented is one of
//! its tokens. A device token is that answer: one app, a label that says
//! which device holds it, revocable on its own. It opens nothing over HTTP.
//!
//! Tokens live hashed in the `tokens` store (`<app>.devices` on files)
//! beside the app's other records, so removing the app takes them along and
//! a copy of the data directory is not a set of live credentials. Same shape
//! as `export.rs`, for the same reasons. A device may check its token on
//! every message, so its use is recorded at most once a minute.

use crate::{
    config::Config,
    platform::tokens::{self, Kind},
};

/// Recognisable in a device's config, and in a leak.
pub const PREFIX: &str = Kind::Device.prefix();

pub type DeviceToken = tokens::Token;

/// Mints a token for `app`. The plain token is returned exactly once, here;
/// after this only its hash exists.
pub async fn create(config: &Config, app: &str, label: &str) -> Result<(DeviceToken, String), String> {
    tokens::create(config, app, Kind::Device, label, "which device will hold the token").await
}

pub async fn list(config: &Config, app: &str) -> Vec<DeviceToken> {
    tokens::list(config, app, Kind::Device).await
}

pub async fn revoke(config: &Config, app: &str, id: &str) -> Result<(), String> {
    tokens::revoke(config, app, Kind::Device, id).await
}

/// The label of `presented` when it is a live device token for `app`, and
/// nothing otherwise. Marks it used, so a listing can say which devices
/// still connect. Called from a handler's host call, on a blocking thread.
pub fn check(config: &Config, app: &str, presented: &str) -> Option<String> {
    tokens::check_blocking(config, app, Kind::Device, presented.trim()).map(|token| token.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "publish-token");
        (dir, config)
    }

    #[tokio::test]
    async fn a_device_token_is_shown_once_and_stored_only_as_a_hash() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "broker", "boiler sensor").await.unwrap();
        assert!(token.starts_with("tsv_"));
        let stored = std::fs::read_to_string(dir.path().join("broker.devices")).unwrap();
        assert!(!stored.contains(&token), "the plain token is on disk");
        assert!(stored.contains(&entry.id));
        assert_eq!(check(&config, "broker", &token).as_deref(), Some("boiler sensor"));
        assert!(list(&config, "broker").await[0].last_used.is_some(), "use was not recorded");
    }

    #[tokio::test]
    async fn a_device_token_names_one_app_and_no_other() {
        let (_dir, config) = config();
        let (_, token) = create(&config, "broker", "x").await.unwrap();
        assert_eq!(check(&config, "syslog", &token), None);
        assert_eq!(check(&config, "broker", "publish-token"), None, "the publish token was accepted");
        let (_, export) = crate::platform::export::create(&config, "broker", "x").await.unwrap();
        assert_eq!(check(&config, "broker", &export), None, "an export token was accepted");
        assert_eq!(check(&config, "../broker", &token), None);
    }

    #[tokio::test]
    async fn revoking_a_device_token_ends_it_and_an_empty_list_leaves_no_file() {
        let (dir, config) = config();
        let (entry, token) = create(&config, "broker", "x").await.unwrap();
        revoke(&config, "broker", &entry.id).await.unwrap();
        assert_eq!(check(&config, "broker", &token), None);
        assert!(!dir.path().join("broker.devices").exists());
        assert!(revoke(&config, "broker", &entry.id).await.is_err());
        for app in ["../x", "a/b", ".site", ""] {
            assert!(create(&config, app, "x").await.is_err(), "{app:?} accepted");
        }
    }

    #[tokio::test]
    async fn checking_a_token_on_every_message_does_not_rewrite_the_file_every_time() {
        let (dir, config) = config();
        let (_, token) = create(&config, "broker", "chatty").await.unwrap();
        check(&config, "broker", &token).unwrap();
        let path = dir.path().join("broker.devices");
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..50 {
            check(&config, "broker", &token).unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), written);
    }

    #[tokio::test]
    async fn a_token_that_is_too_long_or_off_by_one_is_refused() {
        let (_dir, config) = config();
        let (_, token) = create(&config, "broker", "x").await.unwrap();
        let mut near = token.clone().into_bytes();
        let last = near.len() - 1;
        near[last] = if near[last] == b'a' { b'b' } else { b'a' };
        assert_eq!(check(&config, "broker", &String::from_utf8(near).unwrap()), None);
        assert_eq!(check(&config, "broker", &format!("{token}{}", " ".repeat(10))).as_deref(), Some("x"));
        assert_eq!(check(&config, "broker", &format!("{token}{}", "a".repeat(tokens::MAX_TOKEN))), None);
        assert!(!tokens::same("abc", "abd") && !tokens::same("abc", "ab") && tokens::same("abc", "abc"));
    }
}
