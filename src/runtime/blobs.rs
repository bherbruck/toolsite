//! Files an app keeps: uploads, images, exports — anything too large or too
//! opaque for a row. One namespace per app, resolved by the host from the
//! slug exactly as the database is, so a guest can neither name another app's
//! files nor reach the filesystem to find them.
//!
//! Two backends behind one surface. **Local** keeps files under the app's own
//! directory in `.blobs/`, which no slug can name and no bundle can write.
//! **S3** puts them in one bucket under `<app>/`, for a deployment whose disk
//! is not where large things should live. Which one is in use is a deployment
//! choice an app never sees.
//!
//! Bytes do not have to pass through a guest. A handler hands a browser an
//! upload URL and the platform receives the file; a handler answers with
//! `x-toolsite-blob: <key>` and the platform streams the file out. The guest
//! API here is for small things and for deciding.

use crate::{config::Config, content::slug::valid_asset_path};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

/// The most `get` will copy into a guest's memory. Anything bigger is served
/// with the response header instead, which never touches the guest at all.
pub const MAX_GET_BYTES: u64 = 16 * 1024 * 1024;
/// Entries one `list` returns, so a prefix with a million files under it
/// cannot blow up the caller.
pub const MAX_LIST: usize = 1_000;
/// A browser upload URL is minted for one request, made soon.
pub const UPLOAD_TTL: Duration = Duration::from_secs(900);
const MAX_KEY_LEN: usize = 512;
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";
/// How long a presigned S3 request stays valid — long enough for a slow
/// multi-gigabyte transfer, and nothing else ever sees the URL.
const S3_SIGN_TTL: Duration = Duration::from_secs(3600);
const S3_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    NotFound,
    InvalidKey(String),
    /// Bigger than the guest ceiling; carries the actual size.
    TooLarge(u64),
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "no such blob"),
            Error::InvalidKey(why) => write!(f, "invalid key: {why}"),
            Error::TooLarge(size) => write!(f, "blob is {size} bytes, over the limit"),
            Error::Failed(why) => write!(f, "{why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    pub size: u64,
    pub content_type: String,
}

/// Credentials for an S3-compatible bucket. Railway's buckets hand out
/// exactly these five values.
pub struct S3 {
    bucket: Bucket,
    credentials: Credentials,
}

impl S3 {
    /// `path_style` is for buckets that predate virtual-hosted URLs; the
    /// credentials tab says which a bucket needs.
    pub fn new(
        endpoint: &str,
        bucket: &str,
        region: &str,
        access_key_id: &str,
        secret_access_key: &str,
        path_style: bool,
    ) -> Result<Self, String> {
        let endpoint = endpoint
            .parse()
            .map_err(|e| format!("blob endpoint {endpoint:?} is not a URL: {e}"))?;
        let style = if path_style { UrlStyle::Path } else { UrlStyle::VirtualHost };
        let bucket = Bucket::new(endpoint, style, bucket.to_string(), region.to_string())
            .map_err(|e| format!("blob bucket: {e}"))?;
        Ok(Self {
            bucket,
            credentials: Credentials::new(access_key_id, secret_access_key),
        })
    }
}

pub enum Backend {
    Local,
    S3(S3),
}

/// Where an app's files go, and how big one may be. Part of `Config`.
pub struct Blobs {
    pub backend: Backend,
    /// Ceiling on one blob. Zero means none.
    pub max_bytes: u64,
}

impl Blobs {
    pub fn local(max_bytes: u64) -> Self {
        Self {
            backend: Backend::Local,
            max_bytes,
        }
    }

    /// The same backend and ceiling for another `Config`; a bucket handle
    /// is cheap to duplicate.
    pub fn clone_settings(&self) -> Self {
        Self {
            backend: match &self.backend {
                Backend::Local => Backend::Local,
                Backend::S3(s3) => Backend::S3(S3 {
                    bucket: s3.bucket.clone(),
                    credentials: s3.credentials.clone(),
                }),
            },
            max_bytes: self.max_bytes,
        }
    }

    pub fn describe(&self) -> &'static str {
        match self.backend {
            Backend::Local => "local",
            Backend::S3(_) => "s3",
        }
    }
}

/// One browser upload, minted by a handler. The credential is the URL; it is
/// scoped to one app and one key, and spent when the upload begins.
pub struct UploadTicket {
    pub app: String,
    pub key: String,
    pub max_bytes: u64,
    pub expires_at: Instant,
}

// --- keys and names --------------------------------------------------------

/// Same rules as a bundle asset path: segments of letters, digits, `-`, `_`
/// and `.`, none empty and none starting with `.`, which rules out `..` and
/// dotfiles at once. The key becomes a path on the local backend, so this is
/// the whole traversal defence and is checked before anything else.
pub fn valid_key(key: &str) -> Result<(), Error> {
    if key.len() > MAX_KEY_LEN {
        return Err(Error::InvalidKey(format!("longer than {MAX_KEY_LEN} bytes")));
    }
    if !valid_asset_path(key) {
        return Err(Error::InvalidKey(
            "must be path segments of letters, digits, '-', '_' or '.', none starting with '.'"
                .into(),
        ));
    }
    Ok(())
}

/// A prefix is a key or the start of one, so it may be empty or end in `/`.
fn valid_prefix(prefix: &str) -> bool {
    prefix.is_empty() || valid_asset_path(prefix.trim_end_matches('/'))
}

/// A content type travels in a response header, so it is kept to something a
/// header can carry; anything odd falls back to bytes.
pub fn clean_content_type(content_type: &str) -> String {
    let trimmed = content_type.trim();
    let plausible = !trimmed.is_empty()
        && trimmed.len() <= 200
        && trimmed.contains('/')
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_graphic() || b == b' ');
    if plausible {
        trimmed.to_string()
    } else {
        DEFAULT_CONTENT_TYPE.to_string()
    }
}

fn app_root(config: &Config, app: &str) -> Result<PathBuf, Error> {
    if !crate::content::slug::valid_slug(app) || app.contains('/') {
        return Err(Error::Failed("invalid app name".into()));
    }
    Ok(config.data_dir.join(app).join(".blobs"))
}

fn local_paths(config: &Config, app: &str, key: &str) -> Result<(PathBuf, PathBuf), Error> {
    valid_key(key)?;
    let root = app_root(config, app)?;
    Ok((root.join("data").join(key), root.join("meta").join(key)))
}

fn s3_key(app: &str, key: &str) -> String {
    format!("{app}/{key}")
}

fn s3_client() -> Result<reqwest::blocking::Client, Error> {
    reqwest::blocking::Client::builder()
        .timeout(S3_TIMEOUT)
        .build()
        .map_err(|e| Error::Failed(format!("http client: {}", reason(&e))))
}

fn s3_async_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .connect_timeout(S3_TIMEOUT)
        .build()
        .map_err(|e| Error::Failed(format!("http client: {}", reason(&e))))
}

/// `Display` on a reqwest error drops the cause; walk to it.
fn reason(error: &reqwest::Error) -> String {
    let mut out = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

fn s3_failure(what: &str, status: reqwest::StatusCode) -> Error {
    if status == reqwest::StatusCode::NOT_FOUND {
        Error::NotFound
    } else {
        Error::Failed(format!("{what}: bucket answered {status}"))
    }
}

// --- blocking API, for guests ---------------------------------------------

pub fn put(
    config: &Config,
    app: &str,
    key: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), Error> {
    valid_key(key)?;
    if config.blobs.max_bytes > 0 && body.len() as u64 > config.blobs.max_bytes {
        return Err(Error::TooLarge(body.len() as u64));
    }
    let content_type = clean_content_type(content_type);
    match &config.blobs.backend {
        Backend::Local => {
            let (data, meta) = local_paths(config, app, key)?;
            write_local(&data, &meta, &content_type, |file| {
                std::io::Write::write_all(file, body)
            })
        }
        Backend::S3(s3) => {
            let object = s3_key(app, key);
            let mut action = s3.bucket.put_object(Some(&s3.credentials), &object);
            action.headers_mut().insert("content-type", content_type.clone());
            let url = action.sign(S3_SIGN_TTL);
            let response = s3_client()?
                .put(url)
                .header("content-type", content_type)
                .body(body.to_vec())
                .send()
                .map_err(|e| Error::Failed(format!("put: {}", reason(&e))))?;
            if !response.status().is_success() {
                return Err(s3_failure("put", response.status()));
            }
            Ok(())
        }
    }
}

/// Writes data then the sidecar, through a temp file and a rename so a reader
/// never sees half a blob.
fn write_local(
    data: &Path,
    meta: &Path,
    content_type: &str,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), Error> {
    let io = |e: std::io::Error| Error::Failed(format!("write: {e}"));
    for path in [data, meta] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
    }
    let temp = data.with_extension(format!("{}.part", crate::content::slug::random_token(8)));
    let mut file = std::fs::File::create(&temp).map_err(io)?;
    if let Err(e) = fill(&mut file).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&temp);
        return Err(io(e));
    }
    drop(file);
    std::fs::rename(&temp, data).map_err(io)?;
    std::fs::write(meta, content_type).map_err(io)?;
    Ok(())
}

#[derive(Debug)]
pub struct Blob {
    pub content_type: String,
    pub body: Vec<u8>,
}

/// The whole blob, for something small enough to hold. Refuses anything over
/// `MAX_GET_BYTES` before reading it.
pub fn get(config: &Config, app: &str, key: &str) -> Result<Blob, Error> {
    let entry = stat(config, app, key)?.ok_or(Error::NotFound)?;
    if entry.size > MAX_GET_BYTES {
        return Err(Error::TooLarge(entry.size));
    }
    match &config.blobs.backend {
        Backend::Local => {
            let (data, _) = local_paths(config, app, key)?;
            let body = std::fs::read(&data).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Error::NotFound,
                _ => Error::Failed(format!("read: {e}")),
            })?;
            Ok(Blob {
                content_type: entry.content_type,
                body,
            })
        }
        Backend::S3(s3) => {
            let url = s3
                .bucket
                .get_object(Some(&s3.credentials), &s3_key(app, key))
                .sign(S3_SIGN_TTL);
            let response = s3_client()?
                .get(url)
                .send()
                .map_err(|e| Error::Failed(format!("get: {}", reason(&e))))?;
            if !response.status().is_success() {
                return Err(s3_failure("get", response.status()));
            }
            let body = response
                .bytes()
                .map_err(|e| Error::Failed(format!("get: {}", reason(&e))))?
                .to_vec();
            Ok(Blob {
                content_type: entry.content_type,
                body,
            })
        }
    }
}

/// Size and type without the bytes. `Err` is a bad key or a backend fault;
/// `Ok(None)` is simply "nothing there".
pub fn stat(config: &Config, app: &str, key: &str) -> Result<Option<Entry>, Error> {
    valid_key(key)?;
    match &config.blobs.backend {
        Backend::Local => {
            let (data, meta) = local_paths(config, app, key)?;
            let Ok(metadata) = std::fs::metadata(&data) else {
                return Ok(None);
            };
            if !metadata.is_file() {
                return Ok(None);
            }
            let content_type = std::fs::read_to_string(&meta)
                .map(|s| clean_content_type(&s))
                .unwrap_or_else(|_| DEFAULT_CONTENT_TYPE.to_string());
            Ok(Some(Entry {
                key: key.to_string(),
                size: metadata.len(),
                content_type,
            }))
        }
        Backend::S3(s3) => {
            let url = s3
                .bucket
                .head_object(Some(&s3.credentials), &s3_key(app, key))
                .sign(S3_SIGN_TTL);
            let response = s3_client()?
                .head(url)
                .send()
                .map_err(|e| Error::Failed(format!("stat: {}", reason(&e))))?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !response.status().is_success() {
                return Err(s3_failure("stat", response.status()));
            }
            Ok(Some(entry_from_headers(key, response.headers())))
        }
    }
}

fn entry_from_headers(key: &str, headers: &reqwest::header::HeaderMap) -> Entry {
    let size = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(clean_content_type)
        .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_string());
    Entry {
        key: key.to_string(),
        size,
        content_type,
    }
}

pub fn delete(config: &Config, app: &str, key: &str) -> Result<(), Error> {
    valid_key(key)?;
    match &config.blobs.backend {
        Backend::Local => {
            let (data, meta) = local_paths(config, app, key)?;
            match std::fs::remove_file(&data) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound),
                Err(e) => return Err(Error::Failed(format!("delete: {e}"))),
            }
            let _ = std::fs::remove_file(&meta);
            Ok(())
        }
        Backend::S3(s3) => {
            let url = s3
                .bucket
                .delete_object(Some(&s3.credentials), &s3_key(app, key))
                .sign(S3_SIGN_TTL);
            let response = s3_client()?
                .delete(url)
                .send()
                .map_err(|e| Error::Failed(format!("delete: {}", reason(&e))))?;
            if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
                return Err(s3_failure("delete", response.status()));
            }
            Ok(())
        }
    }
}

/// Everything under `prefix`, sorted by key, at most `MAX_LIST` of them.
pub fn list(config: &Config, app: &str, prefix: &str) -> Result<Vec<Entry>, Error> {
    if !valid_prefix(prefix) {
        return Err(Error::InvalidKey("prefix is not the start of a valid key".into()));
    }
    match &config.blobs.backend {
        Backend::Local => {
            let root = app_root(config, app)?.join("data");
            let mut keys = Vec::new();
            walk(&root, &root, &mut keys);
            keys.retain(|key| key.starts_with(prefix));
            keys.sort();
            keys.truncate(MAX_LIST);
            let entries = keys
                .iter()
                .filter_map(|key| stat(config, app, key).ok().flatten())
                .collect();
            Ok(entries)
        }
        Backend::S3(s3) => {
            let scope = s3_key(app, "");
            let mut action = s3.bucket.list_objects_v2(Some(&s3.credentials));
            action.with_prefix(format!("{scope}{prefix}"));
            action.with_max_keys(MAX_LIST);
            let url = action.sign(S3_SIGN_TTL);
            let response = s3_client()?
                .get(url)
                .send()
                .map_err(|e| Error::Failed(format!("list: {}", reason(&e))))?;
            if !response.status().is_success() {
                return Err(s3_failure("list", response.status()));
            }
            let text = response
                .text()
                .map_err(|e| Error::Failed(format!("list: {}", reason(&e))))?;
            let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&text)
                .map_err(|e| Error::Failed(format!("list: could not parse the bucket's answer: {e}")))?;
            // A listing does not carry content types; a caller that needs
            // one asks `stat`. Saying so beats a round trip per entry.
            Ok(parsed
                .contents
                .into_iter()
                .filter_map(|object| {
                    object.key.strip_prefix(&scope).map(|key| Entry {
                        key: key.to_string(),
                        size: object.size,
                        content_type: DEFAULT_CONTENT_TYPE.to_string(),
                    })
                })
                .collect())
        }
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(relative) = path.strip_prefix(root) {
            let key = relative.to_string_lossy().replace('\\', "/");
            // A `.part` file is a write in progress, not a blob.
            if valid_asset_path(&key) && !key.ends_with(".part") {
                out.push(key);
            }
        }
    }
}

// --- tickets ---------------------------------------------------------------

/// Mints a browser upload for `key`, good once, for fifteen minutes. The URL
/// is the credential, so it goes to whoever the handler chooses to hand it
/// to — the platform does not ask again.
pub fn issue_upload(config: &Config, app: &str, key: &str, max_bytes: u64) -> Result<String, Error> {
    valid_key(key)?;
    let ceiling = config.blobs.max_bytes;
    let max_bytes = match (max_bytes, ceiling) {
        (0, c) => c,
        (m, 0) => m,
        (m, c) => m.min(c),
    };
    let ticket = crate::content::slug::random_token(32);
    let now = Instant::now();
    let mut tickets = config.blob_uploads.lock().unwrap();
    tickets.retain(|_, t| t.expires_at > now);
    tickets.insert(
        ticket.clone(),
        UploadTicket {
            app: app.to_string(),
            key: key.to_string(),
            max_bytes,
            expires_at: now + UPLOAD_TTL,
        },
    );
    Ok(upload_url(config, app, &ticket))
}

/// On the app's own host in subdomain mode, so the page that asked for it
/// PUTs to its own origin.
pub fn upload_url(config: &Config, app: &str, ticket: &str) -> String {
    format!("{}/blob/{ticket}", crate::content::origins::app_base(config, app))
}

/// Spends a ticket. Nothing for one that is unknown or expired, and the same
/// ticket cannot be spent twice, so an upload URL that leaked after use is
/// worthless.
pub fn take_upload(config: &Config, ticket: &str) -> Option<UploadTicket> {
    let now = Instant::now();
    let mut tickets = config.blob_uploads.lock().unwrap();
    tickets.retain(|_, t| t.expires_at > now);
    tickets.remove(ticket)
}

// --- streaming API, for the platform --------------------------------------

pub type ByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Receives a body of unknown length and stores it under `key`, never holding
/// more than a buffer of it in memory. `max_bytes` of zero means unbounded.
/// The body is spooled to disk first: the local backend renames it into
/// place, and S3 wants a length before it will take a single byte.
pub async fn receive<S, E>(
    config: &Config,
    app: &str,
    key: &str,
    content_type: &str,
    max_bytes: u64,
    body: S,
) -> Result<u64, Error>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    valid_key(key)?;
    let content_type = clean_content_type(content_type);
    let spool_dir = config.data_dir.join(".tmp");
    tokio::fs::create_dir_all(&spool_dir)
        .await
        .map_err(|e| Error::Failed(format!("spool: {e}")))?;
    let spool = spool_dir.join(format!("blob-{}.part", crate::content::slug::random_token(16)));

    let outcome = spool_body(&spool, max_bytes, body).await;
    let size = match outcome {
        Ok(size) => size,
        Err(error) => {
            let _ = tokio::fs::remove_file(&spool).await;
            return Err(error);
        }
    };

    let result = match &config.blobs.backend {
        Backend::Local => {
            let (data, meta) = match local_paths(config, app, key) {
                Ok(paths) => paths,
                Err(error) => {
                    let _ = tokio::fs::remove_file(&spool).await;
                    return Err(error);
                }
            };
            let io = |e: std::io::Error| Error::Failed(format!("write: {e}"));
            async {
                for path in [&data, &meta] {
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(io)?;
                    }
                }
                tokio::fs::rename(&spool, &data).await.map_err(io)?;
                tokio::fs::write(&meta, &content_type).await.map_err(io)?;
                Ok(())
            }
            .await
        }
        Backend::S3(s3) => {
            let object = s3_key(app, key);
            let mut action = s3.bucket.put_object(Some(&s3.credentials), &object);
            action.headers_mut().insert("content-type", content_type.clone());
            let url = action.sign(S3_SIGN_TTL);
            async {
                let file = tokio::fs::File::open(&spool)
                    .await
                    .map_err(|e| Error::Failed(format!("spool: {e}")))?;
                let stream = tokio_util::io::ReaderStream::new(file);
                let response = s3_async_client()?
                    .put(url)
                    .header("content-type", &content_type)
                    .header("content-length", size)
                    .body(reqwest::Body::wrap_stream(stream))
                    .send()
                    .await
                    .map_err(|e| Error::Failed(format!("put: {}", reason(&e))))?;
                if !response.status().is_success() {
                    return Err(s3_failure("put", response.status()));
                }
                Ok(())
            }
            .await
        }
    };
    let _ = tokio::fs::remove_file(&spool).await;
    result.map(|()| size)
}

async fn spool_body<S, E>(spool: &Path, max_bytes: u64, mut body: S) -> Result<u64, Error>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut file = tokio::fs::File::create(spool)
        .await
        .map_err(|e| Error::Failed(format!("spool: {e}")))?;
    let mut size: u64 = 0;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| Error::Failed(format!("upload body: {e}")))?;
        size += chunk.len() as u64;
        if max_bytes > 0 && size > max_bytes {
            return Err(Error::TooLarge(size));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::Failed(format!("spool: {e}")))?;
    }
    file.flush()
        .await
        .map_err(|e| Error::Failed(format!("spool: {e}")))?;
    Ok(size)
}

/// Opens a blob for streaming out, with what a response needs to know first.
pub async fn open(config: &Config, app: &str, key: &str) -> Result<(Entry, ByteStream), Error> {
    valid_key(key)?;
    match &config.blobs.backend {
        Backend::Local => {
            let (data, _) = local_paths(config, app, key)?;
            // stat blocks, but only for one metadata call and one tiny read.
            let entry = stat(config, app, key)?.ok_or(Error::NotFound)?;
            let file = tokio::fs::File::open(&data).await.map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Error::NotFound,
                _ => Error::Failed(format!("open: {e}")),
            })?;
            let stream = tokio_util::io::ReaderStream::new(file);
            Ok((entry, Box::pin(stream)))
        }
        Backend::S3(s3) => {
            let url = s3
                .bucket
                .get_object(Some(&s3.credentials), &s3_key(app, key))
                .sign(S3_SIGN_TTL);
            let response = s3_async_client()?
                .get(url)
                .send()
                .await
                .map_err(|e| Error::Failed(format!("get: {}", reason(&e))))?;
            if !response.status().is_success() {
                return Err(s3_failure("get", response.status()));
            }
            let entry = entry_from_headers(key, response.headers());
            let stream = response
                .bytes_stream()
                .map_err(|e| std::io::Error::other(reason(&e)));
            Ok((entry, Box::pin(stream)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        (dir, config)
    }

    #[test]
    fn a_blob_comes_back_as_it_went_in() {
        let (_dir, config) = config();
        put(&config, "app", "photos/cat.jpg", "image/jpeg", b"JPEGDATA").unwrap();
        let blob = get(&config, "app", "photos/cat.jpg").unwrap();
        assert_eq!(blob.content_type, "image/jpeg");
        assert_eq!(blob.body, b"JPEGDATA");
        let entry = stat(&config, "app", "photos/cat.jpg").unwrap().unwrap();
        assert_eq!(entry.size, 8);
        assert!(stat(&config, "app", "photos/dog.jpg").unwrap().is_none());
    }

    #[test]
    fn a_key_cannot_leave_the_apps_blob_directory() {
        let (dir, config) = config();
        std::fs::create_dir_all(dir.path().join("victim")).unwrap();
        for key in [
            "../victim/pwned",
            "../../etc/passwd",
            "/etc/passwd",
            ".hidden",
            "a/.b",
            "a//b",
            "",
            "a\\b",
            "a b",
        ] {
            let error = put(&config, "app", key, "text/plain", b"x").unwrap_err();
            assert!(matches!(error, Error::InvalidKey(_)), "{key:?} was accepted: {error}");
            assert!(matches!(get(&config, "app", key), Err(Error::InvalidKey(_))));
            assert!(matches!(delete(&config, "app", key), Err(Error::InvalidKey(_))));
            assert!(matches!(issue_upload(&config, "app", key, 0), Err(Error::InvalidKey(_))));
        }
        assert!(!dir.path().join("victim/pwned").exists());
        assert!(!dir.path().join("etc").exists());
    }

    #[test]
    fn one_apps_blobs_are_invisible_to_another() {
        let (_dir, config) = config();
        put(&config, "alpha", "secret.txt", "text/plain", b"alpha's").unwrap();
        assert_eq!(get(&config, "beta", "secret.txt").unwrap_err(), Error::NotFound);
        assert!(list(&config, "beta", "").unwrap().is_empty());
        assert_eq!(delete(&config, "beta", "secret.txt").unwrap_err(), Error::NotFound);
        assert!(stat(&config, "alpha", "secret.txt").unwrap().is_some(), "beta's miss touched alpha");
    }

    #[test]
    fn listing_is_by_prefix_and_sorted() {
        let (_dir, config) = config();
        for key in ["b/2.txt", "a/1.txt", "b/1.txt", "c.txt"] {
            put(&config, "app", key, "text/plain", b"x").unwrap();
        }
        let all: Vec<String> = list(&config, "app", "").unwrap().into_iter().map(|e| e.key).collect();
        assert_eq!(all, ["a/1.txt", "b/1.txt", "b/2.txt", "c.txt"]);
        let under_b: Vec<String> = list(&config, "app", "b/").unwrap().into_iter().map(|e| e.key).collect();
        assert_eq!(under_b, ["b/1.txt", "b/2.txt"]);
        assert!(list(&config, "app", "../").is_err());
    }

    #[test]
    fn deleting_removes_the_blob_and_its_type() {
        let (dir, config) = config();
        put(&config, "app", "gone.bin", "application/x-thing", b"x").unwrap();
        delete(&config, "app", "gone.bin").unwrap();
        assert_eq!(get(&config, "app", "gone.bin").unwrap_err(), Error::NotFound);
        assert!(!dir.path().join("app/.blobs/meta/gone.bin").exists());
    }

    #[test]
    fn get_refuses_what_would_not_fit_in_a_guest() {
        let (dir, config) = config();
        put(&config, "app", "big.bin", "application/octet-stream", b"small").unwrap();
        // Grow the file behind the store's back rather than writing 16 MB.
        let path = dir.path().join("app/.blobs/data/big.bin");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_GET_BYTES + 1).unwrap();
        assert_eq!(get(&config, "app", "big.bin").unwrap_err(), Error::TooLarge(MAX_GET_BYTES + 1));
    }

    #[test]
    fn a_put_over_the_ceiling_is_refused_before_it_is_written() {
        let (dir, config) = config();
        let config = Config {
            blobs: Blobs::local(4),
            ..config
        };
        assert_eq!(put(&config, "app", "x", "text/plain", b"12345").unwrap_err(), Error::TooLarge(5));
        assert!(!dir.path().join("app/.blobs/data/x").exists());
        put(&config, "app", "x", "text/plain", b"1234").unwrap();
    }

    #[test]
    fn a_content_type_that_could_not_travel_in_a_header_becomes_bytes() {
        assert_eq!(clean_content_type("image/png"), "image/png");
        assert_eq!(clean_content_type(" text/html; charset=utf-8 "), "text/html; charset=utf-8");
        assert_eq!(clean_content_type(""), DEFAULT_CONTENT_TYPE);
        assert_eq!(clean_content_type("nonsense"), DEFAULT_CONTENT_TYPE);
        assert_eq!(clean_content_type("text/plain\r\nx-injected: 1"), DEFAULT_CONTENT_TYPE);
    }

    #[test]
    fn an_upload_ticket_is_spent_once_and_capped_by_the_platform() {
        let (_dir, config) = config();
        let config = Config {
            blobs: Blobs::local(100),
            ..config
        };
        let url = issue_upload(&config, "app", "up.bin", 1_000).unwrap();
        let ticket = url.rsplit('/').next().unwrap();
        let taken = take_upload(&config, ticket).expect("a fresh ticket");
        assert_eq!((taken.app.as_str(), taken.key.as_str(), taken.max_bytes), ("app", "up.bin", 100));
        assert!(take_upload(&config, ticket).is_none(), "spent twice");
        assert!(take_upload(&config, "nope").is_none());
    }

    #[tokio::test]
    async fn a_streamed_upload_lands_whole_and_the_spool_is_gone() {
        let (dir, config) = config();
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from_static(b"hello ")), Ok(Bytes::from_static(b"world"))];
        let size = receive(&config, "app", "greeting.txt", "text/plain", 0, futures_util::stream::iter(chunks))
            .await
            .unwrap();
        assert_eq!(size, 11);
        let blob = get(&config, "app", "greeting.txt").unwrap();
        assert_eq!(blob.body, b"hello world");
        assert_eq!(blob.content_type, "text/plain");
        let leftovers = std::fs::read_dir(dir.path().join(".tmp")).unwrap().count();
        assert_eq!(leftovers, 0, "spool file left behind");

        let (entry, stream) = open(&config, "app", "greeting.txt").await.unwrap();
        assert_eq!(entry.size, 11);
        let out: Vec<u8> = stream.try_collect::<Vec<Bytes>>().await.unwrap().concat();
        assert_eq!(out, b"hello world");
    }

    #[tokio::test]
    async fn a_streamed_upload_past_its_ceiling_stops_and_stores_nothing() {
        let (dir, config) = config();
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from_static(b"12345")), Ok(Bytes::from_static(b"67890"))];
        let error = receive(&config, "app", "big.bin", "text/plain", 7, futures_util::stream::iter(chunks))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::TooLarge(_)), "{error}");
        assert!(stat(&config, "app", "big.bin").unwrap().is_none());
        let leftovers = std::fs::read_dir(dir.path().join(".tmp")).unwrap().count();
        assert_eq!(leftovers, 0, "spool file left behind");
    }

    /// The real thing, against whatever bucket the environment names. Run
    /// with `cargo test -- --ignored` once `TOOLSITE_BLOB_S3_*` are set; a
    /// local MinIO does.
    #[test]
    #[ignore = "reaches the network: needs TOOLSITE_BLOB_S3_* pointing at a bucket"]
    fn the_s3_backend_round_trips_against_a_real_bucket() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} unset"));
        let s3 = S3::new(
            &var("TOOLSITE_BLOB_S3_ENDPOINT"),
            &var("TOOLSITE_BLOB_S3_BUCKET"),
            &std::env::var("TOOLSITE_BLOB_S3_REGION").unwrap_or_else(|_| "auto".into()),
            &var("TOOLSITE_BLOB_S3_ACCESS_KEY_ID"),
            &var("TOOLSITE_BLOB_S3_SECRET_ACCESS_KEY"),
            std::env::var("TOOLSITE_BLOB_S3_PATH_STYLE").is_ok_and(|v| v != "0"),
        )
        .unwrap();
        // A scratch server may not have the bucket yet; a real one answers
        // 409 here, which is fine.
        let create = s3.bucket.create_bucket(&s3.credentials).sign(S3_SIGN_TTL);
        let created = s3_client().unwrap().put(create).send().unwrap().status();
        assert!(created.is_success() || created == reqwest::StatusCode::CONFLICT, "create bucket: {created}");

        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            blobs: Blobs {
                backend: Backend::S3(s3),
                max_bytes: 0,
            },
            ..Config::local(dir.path().to_path_buf(), "t")
        };
        let app = format!("t{}", crate::content::slug::random_token(6));

        put(&config, &app, "dir/a.txt", "text/plain", b"alpha").unwrap();
        put(&config, &app, "dir/b.txt", "text/plain", b"beta").unwrap();
        let entry = stat(&config, &app, "dir/a.txt").unwrap().expect("stat");
        assert_eq!((entry.size, entry.content_type.as_str()), (5, "text/plain"));
        assert_eq!(get(&config, &app, "dir/a.txt").unwrap().body, b"alpha");
        let keys: Vec<String> = list(&config, &app, "dir/").unwrap().into_iter().map(|e| e.key).collect();
        assert_eq!(keys, ["dir/a.txt", "dir/b.txt"]);
        assert!(list(&config, "someone-else", "").unwrap().is_empty());

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let chunks: Vec<Result<Bytes, std::io::Error>> =
                vec![Ok(Bytes::from(vec![7u8; 1_000_000])), Ok(Bytes::from(vec![8u8; 500_000]))];
            let size = receive(&config, &app, "dir/streamed.bin", "application/octet-stream", 0, futures_util::stream::iter(chunks))
                .await
                .unwrap();
            assert_eq!(size, 1_500_000);
            let (entry, stream) = open(&config, &app, "dir/streamed.bin").await.unwrap();
            assert_eq!(entry.size, 1_500_000);
            let out: Vec<u8> = stream.try_collect::<Vec<Bytes>>().await.unwrap().concat();
            assert_eq!(out.len(), 1_500_000);
            assert_eq!(out[999_999], 7);
            assert_eq!(out[1_000_000], 8);
        });

        for key in ["dir/a.txt", "dir/b.txt", "dir/streamed.bin"] {
            delete(&config, &app, key).unwrap();
        }
        assert!(stat(&config, &app, "dir/a.txt").unwrap().is_none());
        assert_eq!(get(&config, &app, "dir/a.txt").unwrap_err(), Error::NotFound);
    }
}
