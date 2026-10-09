//! Tokens as files under `DATA_DIR`, as they always were: a JSON list per
//! app and kind in `<app>.exports`, `<app>.deploys` or `<app>.devices`,
//! each entry its id, label, times and digest.
//!
//! Every change to a list (a mint, a revocation, a use recorded) reads,
//! changes and writes it under a lock of its own in this process, and the
//! write is a dotted temporary file renamed into place. Without the lock a
//! use recorded beside a mint wrote back the list it read before the mint,
//! and the new token was gone; without the rename a check reading mid-write
//! saw half a list, which reads as none. An empty list leaves no file.

use super::{same, Kind, Token, Tokens};
use crate::content::catalog::files::write_aside;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

pub struct Files {
    data_dir: PathBuf,
}

/// One entry as the sidecar has always stored it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    id: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    last_used: Option<u64>,
    created_at: u64,
    /// The token itself, hashed.
    hash: String,
}

impl Stored {
    fn token(&self) -> Token {
        Token { id: self.id.clone(), label: self.label.clone(), last_used: self.last_used, created_at: self.created_at }
    }
}

/// One lock per token list this process has changed, by its path.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);

fn held<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A list as stored; empty when there is no file. A file that does not
/// parse is an error: a change written over it would drop every token, and
/// a check reading it as empty would only refuse, which the caller does.
fn read(path: &Path) -> Result<Vec<Stored>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("a token list could not be read: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("a token list could not be read: {e}")),
    }
}

fn write(path: &Path, tokens: &[Stored]) -> Result<(), String> {
    if tokens.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }
    write_aside(path, crate::platform::records::pretty(tokens)?.as_bytes())
}

/// The entry whose digest is `hash`. Every entry is compared, so the time
/// taken says nothing about which matched or how near a guess came.
fn find(tokens: &[Stored], hash: &str) -> Option<usize> {
    tokens
        .iter()
        .enumerate()
        .fold(None, |found, (at, token)| if same(&token.hash, hash) { Some(at) } else { found })
}

impl Files {
    pub fn new(data_dir: PathBuf) -> Files {
        Files { data_dir }
    }

    fn path(&self, app: &str, kind: Kind) -> PathBuf {
        self.data_dir.join(format!("{app}.{}", kind.extension()))
    }

    /// Runs `edit` on a list with it held, and writes the result.
    fn change<T>(&self, app: &str, kind: Kind, edit: impl FnOnce(&mut Vec<Stored>) -> T) -> Result<T, String> {
        let path = self.path(app, kind);
        let lock = held(&LOCKS).entry(path.clone()).or_default().clone();
        let _one_writer = held(&lock);
        let mut tokens = read(&path)?;
        let answer = edit(&mut tokens);
        write(&path, &tokens)?;
        Ok(answer)
    }

    fn check_now(&self, app: &str, kind: Kind, hash: &str, now: u64) -> Result<Option<Token>, String> {
        let tokens = read(&self.path(app, kind))?;
        let Some(found) = find(&tokens, hash) else {
            return Ok(None);
        };
        let token = tokens[found].token();
        if !token.last_used.is_none_or(|at| now.saturating_sub(at) >= kind.resolution()) {
            return Ok(Some(token));
        }
        // Again under the lock: the token may have been revoked since, and
        // a revoked token is not written back.
        self.change(app, kind, |tokens| {
            let found = find(tokens, hash)?;
            tokens[found].last_used = Some(now);
            Some(tokens[found].token())
        })
    }
}

#[async_trait]
impl Tokens for Files {
    async fn insert(&self, app: &str, kind: Kind, token: &Token, hash: &str) -> Result<(), String> {
        let entry = Stored {
            id: token.id.clone(),
            label: token.label.clone(),
            last_used: token.last_used,
            created_at: token.created_at,
            hash: hash.to_string(),
        };
        self.change(app, kind, |tokens| tokens.push(entry))
    }

    async fn list(&self, app: &str, kind: Kind) -> Result<Vec<Token>, String> {
        Ok(read(&self.path(app, kind))?.iter().map(Stored::token).collect())
    }

    async fn list_all(&self, kind: Kind) -> Result<Vec<(String, Token)>, String> {
        let Ok(entries) = std::fs::read_dir(&self.data_dir) else {
            return Ok(Vec::new());
        };
        let suffix = format!(".{}", kind.extension());
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(app) = name.strip_suffix(&suffix) else { continue };
            if !super::valid_app(app) {
                continue;
            }
            // One unreadable list does not hide every other app's.
            for token in read(&entry.path()).unwrap_or_default() {
                out.push((app.to_string(), token.token()));
            }
        }
        out.sort_by(|a, b| (&a.0, a.1.created_at).cmp(&(&b.0, b.1.created_at)));
        Ok(out)
    }

    async fn revoke(&self, app: &str, kind: Kind, id: &str) -> Result<bool, String> {
        self.change(app, kind, |tokens| {
            let before = tokens.len();
            tokens.retain(|token| token.id != id);
            tokens.len() != before
        })
    }

    async fn revoke_all(&self, app: &str, kind: Kind) -> Result<(), String> {
        self.change(app, kind, |tokens| tokens.clear())
    }

    async fn check(&self, app: &str, kind: Kind, hash: &str, now: u64) -> Result<Option<Token>, String> {
        self.check_now(app, kind, hash, now)
    }

    fn check_blocking(&self, app: &str, kind: Kind, hash: &str, now: u64) -> Result<Option<Token>, String> {
        self.check_now(app, kind, hash, now)
    }

    /// Nothing to take: the lists are files, and the trash moves them.
    fn retire_blocking(&self, _app: &str, _at: u64) -> Result<Vec<(&'static str, String)>, String> {
        Ok(Vec::new())
    }
}
