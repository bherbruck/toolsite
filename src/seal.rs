//! Sealing values at rest with the site's key: app settings, and the shared
//! secret behind a person's two-step sign-in.
//!
//! One key and one cipher for every caller, so a deployment that moves its
//! key out of the volume with `TOOLSITE_SECRET_KEY` protects all of it at
//! once. Lives beside `config` because every layer may need it and it needs
//! nothing above it.

use crate::config::Config;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use hmac::Mac;
use rand::Rng;
use std::path::PathBuf;

/// Where the key lives when the environment does not supply one. Under the
/// dot-directory no slug can name, like the account database.
fn key_path(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("secret.key")
}

/// The key values are encrypted with.
///
/// `TOOLSITE_SECRET_KEY` is the honest option: the key lives somewhere the
/// data volume is not, so a copy of the volume is not a copy of the secrets.
/// Without it one is generated beside them, which still protects a backup
/// that loses only the database file, and is stated plainly rather than
/// pretended to be more.
///
/// On Postgres there is no file to fall back on: every runner must open
/// every value, so the key is the environment's or nothing (the boot guard
/// makes it the environment's).
fn key(config: &Config) -> Result<[u8; 32], String> {
    if let Ok(configured) = std::env::var("TOOLSITE_SECRET_KEY") {
        return parse_key(&configured);
    }
    if config.stores.is_postgres() {
        return Err("TOOLSITE_SECRET_KEY is required on Postgres".to_string());
    }

    let path = key_path(config);
    if let Ok(existing) = std::fs::read(&path)
        && let Ok(key) = <[u8; 32]>::try_from(existing.as_slice())
    {
        return Ok(key);
    }

    let mut fresh = [0u8; 32];
    rand::rng().fill_bytes(&mut fresh);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, fresh).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!("generated a key for sealed values at .site/secret.key");
    Ok(fresh)
}

/// `TOOLSITE_SECRET_KEY` as bytes. The errors never repeat the value.
pub fn parse_key(configured: &str) -> Result<[u8; 32], String> {
    let decoded = BASE64
        .decode(configured.trim())
        .map_err(|_| "TOOLSITE_SECRET_KEY must be base64".to_string())?;
    decoded
        .try_into()
        .map_err(|_| "TOOLSITE_SECRET_KEY must decode to 32 bytes".to_string())
}

/// What form tokens are derived with on Postgres, as an HKDF label.
pub const FORM_KEY_LABEL: &str = "toolsite form key v1";

/// A key for one purpose, derived from the site key with HKDF-SHA256, so
/// one variable serves every runner and a key for one use never opens
/// another's.
pub fn derive(key: &[u8; 32], label: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(None, key)
        .expand(label.as_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// The form-token key on Postgres: derived from the site key rather than
/// kept in `.site/form.key`, which only one runner would have.
pub fn form_key(config: &Config) -> Result<[u8; 32], String> {
    Ok(derive(&key(config)?, FORM_KEY_LABEL))
}

/// Encrypts a value. The same value sealed twice looks different, since
/// each gets a fresh nonce.
pub fn seal(config: &Config, value: &str) -> Result<String, String> {
    let cipher = XChaCha20Poly1305::new(&key(config)?.into());
    let mut nonce = [0u8; 24];
    rand::rng().fill_bytes(&mut nonce);
    let sealed = cipher
        .encrypt(XNonce::from_slice(&nonce), value.as_bytes())
        .map_err(|_| "could not encrypt".to_string())?;

    let mut stored = nonce.to_vec();
    stored.extend_from_slice(&sealed);
    Ok(BASE64.encode(stored))
}

/// The value back, or nothing if it was not sealed with this key.
pub fn open(config: &Config, stored: &str) -> Option<String> {
    let raw = BASE64.decode(stored).ok()?;
    if raw.len() < 24 {
        return None;
    }
    let (nonce, sealed) = raw.split_at(24);
    let cipher = XChaCha20Poly1305::new(&key(config).ok()?.into());
    let plain = cipher.decrypt(XNonce::from_slice(nonce), sealed).ok()?;
    String::from_utf8(plain).ok()
}

/// A digest of `value` keyed with the site key, for values too short to
/// store under a plain hash: someone with the database alone cannot try
/// every recovery code against it. `purpose` keeps one use's digests from
/// matching another's.
pub fn keyed_hash(config: &Config, purpose: &str, value: &str) -> Result<String, String> {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key(config)?).map_err(|e| e.to_string())?;
    mac.update(purpose.as_bytes());
    mac.update(&[0]);
    mac.update(value.as_bytes());
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_value_opens_only_with_the_key_it_was_sealed_with() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        let sealed = seal(&config, "JBSWY3DPEHPK3PXP").unwrap();
        assert!(!sealed.contains("JBSWY3DPEHPK3PXP"));
        assert_eq!(open(&config, &sealed).as_deref(), Some("JBSWY3DPEHPK3PXP"));
        let other = tempfile::tempdir().unwrap();
        let elsewhere = Config::local(other.path().to_path_buf(), "t");
        if std::env::var("TOOLSITE_SECRET_KEY").is_err() {
            assert!(open(&elsewhere, &sealed).is_none(), "another site's key opened it");
        }
    }

    #[test]
    fn the_derived_form_key_is_stable_for_one_secret_and_different_for_two() {
        let one = [7u8; 32];
        let two = [8u8; 32];
        assert_eq!(derive(&one, FORM_KEY_LABEL), derive(&one, FORM_KEY_LABEL));
        assert_ne!(derive(&one, FORM_KEY_LABEL), derive(&two, FORM_KEY_LABEL));
        // Not the secret itself, and not another purpose's key.
        assert_ne!(derive(&one, FORM_KEY_LABEL), one);
        assert_ne!(derive(&one, FORM_KEY_LABEL), derive(&one, "toolsite bus key v1"));
    }

    #[test]
    fn a_secret_key_is_32_bytes_of_base64() {
        assert_eq!(parse_key(" AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8= ").unwrap()[31], 31);
        assert!(parse_key("c2hvcnQ=").is_err());
        assert!(parse_key("not base64!").is_err());
    }

    #[test]
    fn a_keyed_hash_depends_on_the_purpose() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        let a = keyed_hash(&config, "recovery", "abcdefgh").unwrap();
        assert_eq!(a, keyed_hash(&config, "recovery", "abcdefgh").unwrap());
        assert_ne!(a, keyed_hash(&config, "other", "abcdefgh").unwrap());
    }
}
