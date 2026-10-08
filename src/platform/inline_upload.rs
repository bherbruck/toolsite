//! An upload that arrives in base64 chunks over MCP.
//!
//! The upload URL is the default: an agent writes a file and curls it, and
//! the bytes never pass through the conversation. Some sandboxes cannot
//! reach this host at all, and for them the only channel is the tool call.
//! This is that channel: `upload_begin` names what is coming, `upload_chunk`
//! brings it a piece at a time, `upload_finish` hands the whole to the same
//! store the upload URL uses. The kinds, the limits, the slug rules and the
//! editor check at arrival are the same code, so nothing is looser here.
//!
//! The upload itself (what, for whom, which chunks have arrived) is a
//! ticket in `state::Tickets`, updated per chunk with the ticket held, so
//! two chunks at once both count. The bytes are spooled to this runner's
//! disk under `.tmp/inline/<digest>/`, one file per index, so a 64 MB bundle
//! never sits in memory twice, and a client that stops halfway leaves a
//! directory the next `upload_begin` sweeps. The spool is named by the
//! ticket's digest, never its id, and stays per runner until content moves
//! to the bucket.
//!
//! Every function here blocks (on the spool, and on the store through
//! `state::wait`), so callers run them on a blocking thread.

use crate::{
    config::Config,
    platform::upload::{SourceMeta, UploadKind, MAX_UPLOAD_BYTES},
    state::tickets::{self, Kind},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

/// The most one chunk may hold once decoded. Base64 adds a third, so a
/// chunk is about 1 MB on the wire, which every MCP client carries.
pub const CHUNK_BYTES: usize = 768 * 1024;
/// As long as an upload ticket lives: enough for a slow client, short
/// enough that an abandoned spool does not stay.
pub const INLINE_TTL: Duration = Duration::from_secs(900);

/// One upload in flight: what it is, who began it, and which chunks have
/// arrived. Only the index-to-size map lives here; the bytes are on disk.
/// Its expiry is the ticket's.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct InlineUpload {
    pub slug: String,
    pub kind: UploadKind,
    pub meta: SourceMeta,
    /// The account that began it, when one was signed in. Checked again at
    /// the finish, as the upload URL checks when the file arrives.
    pub user: Option<String>,
    /// The folder a new app lands in.
    pub project: Option<String>,
    pub chunks: BTreeMap<u32, u64>,
}

fn spool_root(config: &Config) -> PathBuf {
    config.data_dir.join(".tmp").join("inline")
}

/// An upload's spool, by the digest of its id: a hex name whatever the id
/// says, so no id reaches the path, and the id is not left on disk.
fn spool_dir(config: &Config, id: &str) -> PathBuf {
    spool_root(config).join(tickets::digest(Kind::InlineUpload, id))
}

/// Removes every spool older than an upload lives. A spool is made when
/// its upload begins, which is when its ticket's clock starts, so one this
/// old belongs to an upload that has expired, here or after a restart.
pub fn sweep(config: &Config) {
    if let Ok(entries) = std::fs::read_dir(spool_root(config)) {
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > INLINE_TTL + Duration::from_secs(60));
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

const UNKNOWN: &str = "upload id unknown or expired; call upload_begin again";

/// The answer for an id with no live upload. Its spool, if this runner has
/// one, goes now rather than at the next sweep.
fn unknown(config: &Config, id: &str) -> String {
    let _ = std::fs::remove_dir_all(spool_dir(config, id));
    UNKNOWN.to_string()
}

/// Opens an upload and returns its id. The spool directory is created now,
/// so the first chunk has somewhere to go.
pub fn begin(
    config: &Config,
    slug: String,
    kind: UploadKind,
    meta: SourceMeta,
    user: Option<String>,
    project: Option<String>,
) -> Result<String, String> {
    sweep(config);
    let upload = InlineUpload { slug, kind, meta, user, project, chunks: BTreeMap::new() };
    let id = crate::state::wait(config.stores.tickets.put(Kind::InlineUpload, INLINE_TTL, &upload))?;
    std::fs::create_dir_all(spool_dir(config, &id)).map_err(|e| format!("could not open a spool: {e}"))?;
    Ok(id)
}

/// What a chunk left behind: the total so far and the indexes present.
#[derive(Debug)]
pub struct Progress {
    pub received: u64,
    pub present: Vec<u32>,
}

/// Stores one chunk. A repeat of an index replaces it. The whole may not
/// pass the upload ceiling, counted across every chunk present.
pub fn chunk(config: &Config, id: &str, index: u32, data: &str) -> Result<Progress, String> {
    chunk_within(config, id, index, data, MAX_UPLOAD_BYTES as u64)
}

pub(crate) fn chunk_within(config: &Config, id: &str, index: u32, data: &str, ceiling: u64) -> Result<Progress, String> {
    let bytes = STANDARD
        .decode(data.trim())
        .map_err(|_| "data is not standard base64".to_string())?;
    if bytes.len() > CHUNK_BYTES {
        return Err(format!(
            "a chunk holds at most {} bytes decoded; this one holds {}",
            CHUNK_BYTES,
            bytes.len()
        ));
    }
    if bytes.is_empty() {
        return Err("a chunk holds at least one byte".to_string());
    }
    let size = bytes.len() as u64;
    let mut over = false;
    let counted = crate::state::wait(config.stores.tickets.update(Kind::InlineUpload, id, |upload: &mut InlineUpload| {
        let others: u64 = upload
            .chunks
            .iter()
            .filter(|(i, _)| **i != index)
            .map(|(_, size)| *size)
            .sum();
        if others + size > ceiling {
            over = true;
            return Err(format!(
                "the upload would pass {} bytes, the ceiling for one upload; it is cancelled",
                ceiling
            ));
        }
        upload.chunks.insert(index, size);
        Ok(Progress {
            received: upload.chunks.values().sum(),
            present: upload.chunks.keys().copied().collect(),
        })
    }));
    let progress = match counted {
        Ok(Some(progress)) => progress,
        Ok(None) => return Err(unknown(config, id)),
        Err(why) => {
            if over {
                // Over the ceiling the upload is lost either way; say so and
                // clear the spool rather than hold it until expiry.
                let _ = crate::state::wait(config.stores.tickets.take::<InlineUpload>(Kind::InlineUpload, id));
                let _ = std::fs::remove_dir_all(spool_dir(config, id));
            }
            return Err(why);
        }
    };
    let dir = spool_dir(config, id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not spool the chunk: {e}"))?;
    std::fs::write(dir.join(format!("{index}.part")), &bytes).map_err(|e| format!("could not spool the chunk: {e}"))?;
    Ok(progress)
}

/// Closes the upload and returns it with its bytes in order, or the indexes
/// that never arrived. Either way the id is spent and the spool is gone.
pub fn finish(config: &Config, id: &str, count: u32) -> Result<(InlineUpload, Vec<u8>), String> {
    let upload = crate::state::wait(config.stores.tickets.take::<InlineUpload>(Kind::InlineUpload, id));
    let dir = spool_dir(config, id);
    let outcome = (|| {
        let upload = upload?.ok_or_else(|| UNKNOWN.to_string())?;
        if count == 0 {
            return Err("chunks must be at least 1".to_string());
        }
        let missing: Vec<String> = (0..count)
            .filter(|i| !upload.chunks.contains_key(i))
            .map(|i| i.to_string())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "chunks {} never arrived; the upload is cancelled, begin again",
                missing.join(", ")
            ));
        }
        let extra: Vec<String> = upload.chunks.keys().filter(|i| **i >= count).map(|i| i.to_string()).collect();
        if !extra.is_empty() {
            return Err(format!(
                "chunks {} are past the count of {count}; the upload is cancelled, begin again",
                extra.join(", ")
            ));
        }
        let total: u64 = upload.chunks.values().sum();
        let mut bytes = Vec::with_capacity(total as usize);
        for index in 0..count {
            let part = std::fs::read(dir.join(format!("{index}.part")))
                .map_err(|e| format!("chunk {index} could not be read back: {e}"))?;
            bytes.extend_from_slice(&part);
        }
        Ok((upload, bytes))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A site, and a runtime entered for the store calls these blocking
    /// functions wait on, as their blocking thread would have.
    fn config() -> (tempfile::TempDir, Config, tokio::runtime::Runtime) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        (dir, config, runtime)
    }

    fn meta() -> SourceMeta {
        SourceMeta {
            push: true,
            message: None,
            commit: None,
        }
    }

    fn b64(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    #[test]
    fn chunks_join_in_index_order_whatever_order_they_came() {
        let (_dir, config, runtime) = config();
        let _entered = runtime.enter();
        let id = begin(&config, "app".into(), UploadKind::Page, meta(), None, None).unwrap();
        chunk(&config, &id, 2, &b64(b"cc")).unwrap();
        chunk(&config, &id, 0, &b64(b"aa")).unwrap();
        let progress = chunk(&config, &id, 1, &b64(b"bb")).unwrap();
        assert_eq!(progress.received, 6);
        assert_eq!(progress.present, vec![0, 1, 2]);
        let (_, bytes) = finish(&config, &id, 3).unwrap();
        assert_eq!(bytes, b"aabbcc");
        assert!(!spool_dir(&config, &id).exists(), "the spool stayed");
        assert!(finish(&config, &id, 3).is_err(), "a finished id was still open");
    }

    #[test]
    fn a_missing_chunk_is_named_and_the_upload_is_cancelled() {
        let (_dir, config, runtime) = config();
        let _entered = runtime.enter();
        let id = begin(&config, "app".into(), UploadKind::Page, meta(), None, None).unwrap();
        chunk(&config, &id, 0, &b64(b"aa")).unwrap();
        chunk(&config, &id, 2, &b64(b"cc")).unwrap();
        let error = finish(&config, &id, 3).unwrap_err();
        assert!(error.contains("chunks 1 never arrived"), "{error}");
        assert!(!spool_dir(&config, &id).exists());
        assert_eq!(crate::state::wait(config.stores.tickets.live(Kind::InlineUpload)).unwrap(), 0);
    }

    #[test]
    fn a_repeated_index_replaces_rather_than_adds() {
        let (_dir, config, runtime) = config();
        let _entered = runtime.enter();
        let id = begin(&config, "app".into(), UploadKind::Page, meta(), None, None).unwrap();
        chunk(&config, &id, 0, &b64(b"wrong")).unwrap();
        let progress = chunk(&config, &id, 0, &b64(b"right")).unwrap();
        assert_eq!(progress.received, 5);
        let (_, bytes) = finish(&config, &id, 1).unwrap();
        assert_eq!(bytes, b"right");
    }

    #[test]
    fn a_chunk_past_the_ceiling_cancels_the_upload_and_clears_the_spool() {
        let (_dir, config, runtime) = config();
        let _entered = runtime.enter();
        let id = begin(&config, "app".into(), UploadKind::Page, meta(), None, None).unwrap();
        chunk_within(&config, &id, 0, &b64(&[1u8; 10]), 15).unwrap();
        let error = chunk_within(&config, &id, 1, &b64(&[2u8; 10]), 15).unwrap_err();
        assert!(error.contains("ceiling"), "{error}");
        assert!(!spool_dir(&config, &id).exists(), "the spool stayed");
        assert!(chunk(&config, &id, 2, &b64(b"x")).is_err(), "a cancelled upload took another chunk");
    }

    #[test]
    fn a_chunk_larger_than_the_chunk_size_or_not_base64_is_refused() {
        let (_dir, config, runtime) = config();
        let _entered = runtime.enter();
        let id = begin(&config, "app".into(), UploadKind::Page, meta(), None, None).unwrap();
        let big = vec![0u8; CHUNK_BYTES + 1];
        assert!(chunk(&config, &id, 0, &b64(&big)).unwrap_err().contains("at most"));
        assert!(chunk(&config, &id, 0, "not base64!!").unwrap_err().contains("base64"));
        assert!(chunk(&config, &id, 0, "").unwrap_err().contains("at least"));
    }

    #[test]
    fn an_expired_upload_is_gone_with_its_spool() {
        let (_dir, config, runtime) = config();
        let _entered = runtime.enter();
        let id = begin(&config, "app".into(), UploadKind::Page, meta(), None, None).unwrap();
        chunk(&config, &id, 0, &b64(b"aa")).unwrap();
        assert!(crate::state::wait(config.stores.tickets.expire(Kind::InlineUpload, &id)).unwrap());
        assert!(chunk(&config, &id, 1, &b64(b"bb")).unwrap_err().contains("expired"));
        assert!(!spool_dir(&config, &id).exists(), "the expired spool stayed");
        assert!(finish(&config, &id, 1).unwrap_err().contains("expired"));
    }

    #[test]
    fn a_spool_is_named_by_digest_so_no_id_reaches_the_path_or_the_disk() {
        let (dir, config, runtime) = config();
        let _entered = runtime.enter();
        let id = begin(&config, "app".into(), UploadKind::Page, meta(), None, None).unwrap();
        chunk(&config, &id, 0, &b64(b"aa")).unwrap();
        let names: Vec<String> = std::fs::read_dir(dir.path().join(".tmp/inline"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![tickets::digest(Kind::InlineUpload, &id)]);
        assert!(!names.iter().any(|name| name.contains(&id)));

        // An id that would climb out of the spool is only ever a digest.
        std::fs::write(dir.path().join("keep.html"), b"kept").unwrap();
        for hostile in ["..", "../..", "../../keep.html", "/"] {
            assert!(chunk(&config, hostile, 0, &b64(b"x")).is_err());
            assert!(finish(&config, hostile, 1).is_err());
        }
        assert!(dir.path().join("keep.html").exists(), "a hostile id removed a file outside the spool");
    }
}
