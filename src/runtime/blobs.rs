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
    time::Duration,
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
#[derive(Clone)]
pub struct S3 {
    pub(crate) bucket: Bucket,
    pub(crate) credentials: Credentials,
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
    /// Ceiling on one file a guest writes in pieces, which may be set
    /// higher than `max_bytes` for a deployment whose jobs write large
    /// exports. Zero means none.
    pub max_write_bytes: u64,
}

impl Blobs {
    pub fn local(max_bytes: u64) -> Self {
        Self {
            backend: Backend::Local,
            max_bytes,
            max_write_bytes: max_bytes,
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
            max_write_bytes: self.max_write_bytes,
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
/// scoped to one app and one key, and spent when the upload begins. Kept in
/// `state::Tickets`, which holds its expiry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UploadTicket {
    pub app: String,
    pub key: String,
    pub max_bytes: u64,
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
pub(crate) fn reason(error: &reqwest::Error) -> String {
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
pub async fn issue_upload(config: &Config, app: &str, key: &str, max_bytes: u64) -> Result<String, Error> {
    valid_key(key)?;
    let ceiling = config.blobs.max_bytes;
    let max_bytes = match (max_bytes, ceiling) {
        (0, c) => c,
        (m, 0) => m,
        (m, c) => m.min(c),
    };
    let ticket = UploadTicket { app: app.to_string(), key: key.to_string(), max_bytes };
    let ticket = config
        .stores
        .tickets
        .put(crate::state::tickets::Kind::BlobUpload, UPLOAD_TTL, &ticket)
        .await
        .map_err(Error::Failed)?;
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
pub async fn take_upload(config: &Config, ticket: &str) -> Option<UploadTicket> {
    match config.stores.tickets.take(crate::state::tickets::Kind::BlobUpload, ticket).await {
        Ok(ticket) => ticket,
        Err(why) => {
            tracing::error!(%why, "browser upload ticket could not be spent");
            None
        }
    }
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

// --- writing in pieces, for guests -------------------------------------------

/// Writers one call may hold open at once. Each may keep a part's worth of
/// bytes in memory on S3, so this bounds what a call can make the host hold.
pub const MAX_OPEN_WRITERS: usize = 4;
/// S3 takes a multipart upload in parts of at least 5 MiB, all but the last.
/// Ten thousand parts of this size is 80 GB, past any ceiling a deployment
/// would set.
const S3_PART_BYTES: usize = 8 * 1024 * 1024;
const S3_MAX_PARTS: usize = 10_000;
/// A temp file older than this was left by a process that died mid-write.
const STALE_TEMP: Duration = Duration::from_secs(24 * 3600);

/// A file a guest writes in pieces. Nothing appears under the key until
/// `finish`: on the volume the bytes go to a temp file under the app's
/// hidden `.blobs/tmp/` and are renamed into place; on S3 they go up as a
/// multipart upload, completed at the end. Dropped unfinished, it throws
/// away what it wrote, so a call that traps leaves nothing behind.
pub struct Writer {
    key: String,
    content_type: String,
    written: u64,
    cap: u64,
    /// `None` once finished or abandoned.
    sink: Option<Sink>,
}

enum Sink {
    Local {
        file: std::fs::File,
        temp: PathBuf,
        data: PathBuf,
        meta: PathBuf,
    },
    S3 {
        s3: Box<S3>,
        object: String,
        /// Begun when the first full part is ready. A file smaller than a
        /// part is one plain PUT at the end and never starts one.
        upload: Option<String>,
        etags: Vec<String>,
        buffer: Vec<u8>,
    },
}

impl Writer {
    pub fn open(config: &Config, app: &str, key: &str, content_type: &str) -> Result<Writer, Error> {
        valid_key(key)?;
        let content_type = clean_content_type(content_type);
        let sink = match &config.blobs.backend {
            Backend::Local => {
                let (data, meta) = local_paths(config, app, key)?;
                let temp_dir = app_root(config, app)?.join("tmp");
                std::fs::create_dir_all(&temp_dir).map_err(|e| Error::Failed(format!("write: {e}")))?;
                sweep_stale(&temp_dir);
                let temp = temp_dir.join(format!("{}.part", crate::content::slug::random_token(16)));
                let file = std::fs::File::create(&temp).map_err(|e| Error::Failed(format!("write: {e}")))?;
                Sink::Local { file, temp, data, meta }
            }
            Backend::S3(s3) => Sink::S3 {
                s3: Box::new(s3.clone()),
                object: s3_key(app, key),
                upload: None,
                etags: Vec::new(),
                buffer: Vec::new(),
            },
        };
        Ok(Writer {
            key: key.to_string(),
            content_type,
            written: 0,
            cap: config.blobs.max_write_bytes,
            sink: Some(sink),
        })
    }

    /// Adds bytes. Past the ceiling, or on any failure, the writer is
    /// abandoned and what it wrote is gone.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.sink.is_none() {
            return Err(Error::Failed("this writer is closed".into()));
        }
        let after = self.written.saturating_add(bytes.len() as u64);
        if self.cap > 0 && after > self.cap {
            self.abort();
            return Err(Error::TooLarge(after));
        }
        let outcome = match self.sink.as_mut().expect("checked above") {
            Sink::Local { file, .. } => {
                std::io::Write::write_all(file, bytes).map_err(|e| Error::Failed(format!("write: {e}")))
            }
            Sink::S3 { s3, object, upload, etags, buffer } => {
                buffer.extend_from_slice(bytes);
                let mut outcome = Ok(());
                while buffer.len() >= S3_PART_BYTES {
                    let rest = buffer.split_off(S3_PART_BYTES);
                    let part = std::mem::replace(buffer, rest);
                    if let Err(error) = s3_upload_part(s3, object, &self.content_type, upload, etags, part) {
                        outcome = Err(error);
                        break;
                    }
                }
                outcome
            }
        };
        match outcome {
            Ok(()) => {
                self.written = after;
                Ok(())
            }
            Err(error) => {
                self.abort();
                Err(error)
            }
        }
    }

    /// Puts the whole file under its key at once.
    pub fn finish(mut self) -> Result<Entry, Error> {
        let Some(sink) = self.sink.take() else {
            return Err(Error::Failed("this writer is closed".into()));
        };
        let entry = Entry {
            key: self.key.clone(),
            size: self.written,
            content_type: self.content_type.clone(),
        };
        match sink {
            Sink::Local { file, temp, data, meta } => {
                let io = |e: std::io::Error| Error::Failed(format!("write: {e}"));
                let placed = (|| {
                    file.sync_all().map_err(io)?;
                    drop(file);
                    for path in [&data, &meta] {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).map_err(io)?;
                        }
                    }
                    std::fs::rename(&temp, &data).map_err(io)?;
                    std::fs::write(&meta, &self.content_type).map_err(io)
                })();
                if placed.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                placed.map(|()| entry)
            }
            Sink::S3 { s3, object, mut upload, mut etags, buffer } => {
                let outcome = if upload.is_none() {
                    s3_put_whole(&s3, &object, &self.content_type, buffer)
                } else {
                    s3_upload_part(&s3, &object, &self.content_type, &mut upload, &mut etags, buffer)
                        .and_then(|()| s3_complete(&s3, &object, upload.as_deref().unwrap_or_default(), &etags))
                };
                if outcome.is_err()
                    && let Some(upload) = upload
                {
                    s3_abort(&s3, &object, &upload);
                }
                outcome.map(|()| entry)
            }
        }
    }

    /// Throws away what was written. Safe to call twice.
    pub fn abort(&mut self) {
        match self.sink.take() {
            Some(Sink::Local { file, temp, .. }) => {
                drop(file);
                let _ = std::fs::remove_file(&temp);
            }
            Some(Sink::S3 { s3, object, upload: Some(upload), .. }) => {
                // reqwest's blocking client may not run on an async worker,
                // which is where a store could in principle be dropped.
                if tokio::runtime::Handle::try_current().is_ok() {
                    std::thread::spawn(move || s3_abort(&s3, &object, &upload));
                } else {
                    s3_abort(&s3, &object, &upload);
                }
            }
            Some(Sink::S3 { upload: None, .. }) | None => {}
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Removes temp files a dead process left. Best effort: a failure here
/// costs disk, never a write.
fn sweep_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE_TEMP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn s3_put_whole(s3: &S3, object: &str, content_type: &str, body: Vec<u8>) -> Result<(), Error> {
    let mut action = s3.bucket.put_object(Some(&s3.credentials), object);
    action.headers_mut().insert("content-type", content_type.to_string());
    let response = s3_client()?
        .put(action.sign(S3_SIGN_TTL))
        .header("content-type", content_type)
        .body(body)
        .send()
        .map_err(|e| Error::Failed(format!("put: {}", reason(&e))))?;
    if !response.status().is_success() {
        return Err(s3_failure("put", response.status()));
    }
    Ok(())
}

/// Sends one part, starting the multipart upload first if this is the
/// first.
fn s3_upload_part(
    s3: &S3,
    object: &str,
    content_type: &str,
    upload: &mut Option<String>,
    etags: &mut Vec<String>,
    part: Vec<u8>,
) -> Result<(), Error> {
    if etags.len() >= S3_MAX_PARTS {
        return Err(Error::TooLarge((etags.len() * S3_PART_BYTES + part.len()) as u64));
    }
    let client = s3_client()?;
    if upload.is_none() {
        let mut action = s3.bucket.create_multipart_upload(Some(&s3.credentials), object);
        action.headers_mut().insert("content-type", content_type.to_string());
        let response = client
            .post(action.sign(S3_SIGN_TTL))
            .header("content-type", content_type)
            .send()
            .map_err(|e| Error::Failed(format!("begin upload: {}", reason(&e))))?;
        if !response.status().is_success() {
            return Err(s3_failure("begin upload", response.status()));
        }
        let text = response.text().map_err(|e| Error::Failed(format!("begin upload: {}", reason(&e))))?;
        let parsed = rusty_s3::actions::CreateMultipartUpload::parse_response(&text)
            .map_err(|e| Error::Failed(format!("begin upload: could not parse the bucket's answer: {e}")))?;
        *upload = Some(parsed.upload_id().to_string());
    }
    let upload_id = upload.as_deref().expect("set above");
    let number = (etags.len() + 1) as u16;
    let url = s3.bucket.upload_part(Some(&s3.credentials), object, number, upload_id).sign(S3_SIGN_TTL);
    let response = client
        .put(url)
        .body(part)
        .send()
        .map_err(|e| Error::Failed(format!("upload part: {}", reason(&e))))?;
    if !response.status().is_success() {
        return Err(s3_failure("upload part", response.status()));
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::Failed("upload part: the bucket gave no ETag".into()))?;
    etags.push(etag.to_string());
    Ok(())
}

fn s3_complete(s3: &S3, object: &str, upload_id: &str, etags: &[String]) -> Result<(), Error> {
    let action = s3.bucket.complete_multipart_upload(
        Some(&s3.credentials),
        object,
        upload_id,
        etags.iter().map(String::as_str),
    );
    let url = action.sign(S3_SIGN_TTL);
    let response = s3_client()?
        .post(url)
        .body(action.body())
        .send()
        .map_err(|e| Error::Failed(format!("complete upload: {}", reason(&e))))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    // S3 can answer 200 and put the failure in the body.
    if !status.is_success() || text.contains("<Error>") {
        return Err(Error::Failed(format!("complete upload: bucket answered {status}")));
    }
    Ok(())
}

fn s3_abort(s3: &S3, object: &str, upload_id: &str) {
    let url = s3.bucket.abort_multipart_upload(Some(&s3.credentials), object, upload_id).sign(S3_SIGN_TTL);
    let outcome = s3_client().and_then(|client| {
        client.delete(url).send().map_err(|e| Error::Failed(reason(&e)))
    });
    match outcome {
        Ok(response) if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND => {}
        Ok(response) => tracing::warn!(object, status = %response.status(), "could not abandon a multipart upload; a bucket lifecycle rule will have to"),
        Err(error) => tracing::warn!(object, %error, "could not abandon a multipart upload; a bucket lifecycle rule will have to"),
    }
}

/// The writers one guest call holds, by handle. Lives in the call's store,
/// so a handle means nothing to another call or another app; emptied, and
/// so every writer abandoned, when the call ends.
#[derive(Default)]
pub struct Writers {
    open: std::collections::HashMap<u64, Writer>,
}

impl Writers {
    pub fn open(&mut self, config: &Config, app: &str, key: &str, content_type: &str) -> Result<u64, Error> {
        if self.open.len() >= MAX_OPEN_WRITERS {
            return Err(Error::Failed(format!("at most {MAX_OPEN_WRITERS} writers may be open at once")));
        }
        let writer = Writer::open(config, app, key, content_type)?;
        // Random rather than counted, so a handle carried from another call
        // cannot happen to name one of this call's writers.
        let mut handle = rand::random::<u64>();
        while handle == 0 || self.open.contains_key(&handle) {
            handle = rand::random::<u64>();
        }
        self.open.insert(handle, writer);
        Ok(handle)
    }

    pub fn append(&mut self, handle: u64, bytes: &[u8]) -> Result<(), Error> {
        let writer = self.open.get_mut(&handle).ok_or_else(unknown_writer)?;
        let outcome = writer.append(bytes);
        if outcome.is_err() {
            self.open.remove(&handle);
        }
        outcome
    }

    pub fn finish(&mut self, handle: u64) -> Result<Entry, Error> {
        self.open.remove(&handle).ok_or_else(unknown_writer)?.finish()
    }

    pub fn abort(&mut self, handle: u64) {
        self.open.remove(&handle);
    }

    /// Abandons every writer still open: the end of a call.
    pub fn abort_all(&mut self) {
        self.open.clear();
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}

fn unknown_writer() -> Error {
    Error::Failed("no such writer in this call".into())
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
            let issued = tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(issue_upload(&config, "app", key, 0));
            assert!(matches!(issued, Err(Error::InvalidKey(_))));
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

    #[tokio::test]
    async fn an_upload_ticket_is_spent_once_and_capped_by_the_platform() {
        let (_dir, config) = config();
        let config = Config {
            blobs: Blobs::local(100),
            ..config
        };
        let url = issue_upload(&config, "app", "up.bin", 1_000).await.unwrap();
        let ticket = url.rsplit('/').next().unwrap();
        let taken = take_upload(&config, ticket).await.expect("a fresh ticket");
        assert_eq!((taken.app.as_str(), taken.key.as_str(), taken.max_bytes), ("app", "up.bin", 100));
        assert!(take_upload(&config, ticket).await.is_none(), "spent twice");
        assert!(take_upload(&config, "nope").await.is_none());
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
                max_write_bytes: 0,
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

    // --- writing in pieces ---------------------------------------------------

    fn temps(dir: &tempfile::TempDir, app: &str) -> usize {
        std::fs::read_dir(dir.path().join(app).join(".blobs/tmp")).map(|d| d.count()).unwrap_or(0)
    }

    #[test]
    fn a_file_written_in_pieces_appears_whole_and_only_at_finish() {
        let (dir, config) = config();
        let mut writer = Writer::open(&config, "app", "out/report.csv", "text/csv").unwrap();
        writer.append(b"a,b\n").unwrap();
        writer.append(b"1,2\n").unwrap();
        writer.append(b"").unwrap();
        assert!(stat(&config, "app", "out/report.csv").unwrap().is_none(), "visible before finish");
        assert!(list(&config, "app", "").unwrap().is_empty(), "listed before finish");
        let entry = writer.finish().unwrap();
        assert_eq!((entry.key.as_str(), entry.size, entry.content_type.as_str()), ("out/report.csv", 8, "text/csv"));
        let blob = get(&config, "app", "out/report.csv").unwrap();
        assert_eq!((blob.body.as_slice(), blob.content_type.as_str()), (&b"a,b\n1,2\n"[..], "text/csv"));
        assert_eq!(temps(&dir, "app"), 0, "temp left behind");
    }

    #[test]
    fn finishing_a_writer_replaces_the_file_under_its_key_at_once() {
        let (_dir, config) = config();
        put(&config, "app", "data.bin", "text/plain", b"old").unwrap();
        let mut writer = Writer::open(&config, "app", "data.bin", "text/plain").unwrap();
        writer.append(b"new and longer").unwrap();
        assert_eq!(get(&config, "app", "data.bin").unwrap().body, b"old", "the old file changed mid-write");
        writer.finish().unwrap();
        assert_eq!(get(&config, "app", "data.bin").unwrap().body, b"new and longer");
    }

    #[test]
    fn an_abandoned_writer_leaves_no_file_and_no_temp() {
        let (dir, config) = config();
        let mut aborted = Writer::open(&config, "app", "a.txt", "text/plain").unwrap();
        aborted.append(b"never stored").unwrap();
        aborted.abort();
        aborted.abort();
        assert!(aborted.append(b"more").is_err(), "an aborted writer took more bytes");
        {
            let mut dropped = Writer::open(&config, "app", "b.txt", "text/plain").unwrap();
            dropped.append(b"never stored either").unwrap();
        }
        assert!(stat(&config, "app", "a.txt").unwrap().is_none());
        assert!(stat(&config, "app", "b.txt").unwrap().is_none());
        assert_eq!(temps(&dir, "app"), 0, "temp left behind");
    }

    #[test]
    fn the_write_ceiling_stops_a_writer_and_throws_its_bytes_away() {
        let (dir, config) = config();
        let config = Config {
            blobs: Blobs { max_write_bytes: 10, ..Blobs::local(0) },
            ..config
        };
        let mut writer = Writer::open(&config, "app", "big.bin", "application/octet-stream").unwrap();
        writer.append(b"123456").unwrap();
        assert_eq!(writer.append(b"789012").unwrap_err(), Error::TooLarge(12));
        assert!(writer.append(b"1").is_err(), "a writer past its ceiling took more bytes");
        assert!(writer.finish().is_err(), "a writer past its ceiling finished");
        assert!(stat(&config, "app", "big.bin").unwrap().is_none());
        assert_eq!(temps(&dir, "app"), 0, "temp left behind");
    }

    #[test]
    fn the_write_ceiling_may_be_set_above_the_blob_ceiling() {
        let (_dir, config) = config();
        let config = Config {
            blobs: Blobs { max_write_bytes: 100, ..Blobs::local(4) },
            ..config
        };
        assert_eq!(put(&config, "app", "put.bin", "text/plain", b"12345").unwrap_err(), Error::TooLarge(5));
        let mut writer = Writer::open(&config, "app", "written.bin", "text/plain").unwrap();
        writer.append(&[1u8; 50]).unwrap();
        assert_eq!(writer.finish().unwrap().size, 50);
    }

    #[test]
    fn a_writer_key_cannot_leave_the_apps_blob_directory() {
        let (dir, config) = config();
        for key in ["../victim/pwned", "../../etc/passwd", "/etc/passwd", ".hidden", "a/.b", "a//b", "", "a\\b", "tmp/../../x"] {
            assert!(matches!(Writer::open(&config, "app", key, "text/plain"), Err(Error::InvalidKey(_))), "{key:?} was accepted");
        }
        assert!(Writer::open(&config, "../victim", "x", "text/plain").is_err(), "an app name escaped");
        assert!(!dir.path().join("victim").exists());
        assert_eq!(temps(&dir, "app"), 0);
    }

    #[test]
    fn a_call_holds_a_few_writers_and_ending_it_abandons_them_all() {
        let (dir, config) = config();
        let mut writers = Writers::default();
        let handles: Vec<u64> = (0..MAX_OPEN_WRITERS)
            .map(|n| writers.open(&config, "app", &format!("w{n}.txt"), "text/plain").unwrap())
            .collect();
        assert!(matches!(writers.open(&config, "app", "one-too-many.txt", "text/plain"), Err(Error::Failed(_))));
        for handle in &handles {
            writers.append(*handle, b"partial").unwrap();
        }
        assert_eq!(temps(&dir, "app"), MAX_OPEN_WRITERS);
        // A number this call never handed out names nothing.
        let stranger = (1..).find(|n| !handles.contains(n)).unwrap();
        assert!(writers.append(stranger, b"x").is_err());
        assert!(writers.finish(stranger).is_err());
        writers.abort_all();
        assert!(writers.is_empty());
        assert_eq!(temps(&dir, "app"), 0, "temps outlived the call");
        for n in 0..MAX_OPEN_WRITERS {
            assert!(stat(&config, "app", &format!("w{n}.txt")).unwrap().is_none());
        }
        // A finished handle is spent.
        let handle = writers.open(&config, "app", "done.txt", "text/plain").unwrap();
        writers.finish(handle).unwrap();
        assert!(writers.finish(handle).is_err(), "a handle finished twice");
    }

    // --- writing in pieces to a bucket -------------------------------------

    /// Enough of S3 to take objects and multipart uploads, in this process
    /// on a loopback port, recording what it was asked.
    #[derive(Default)]
    struct FakeBucket {
        objects: std::collections::HashMap<String, (String, Vec<u8>)>,
        uploads: std::collections::HashMap<String, (String, std::collections::BTreeMap<u16, Vec<u8>>)>,
        part_sizes: Vec<usize>,
        aborted: usize,
        next: usize,
    }

    type Shared = std::sync::Arc<std::sync::Mutex<FakeBucket>>;

    async fn fake_object(
        axum::extract::State(state): axum::extract::State<Shared>,
        method: axum::http::Method,
        axum::extract::Path(key): axum::extract::Path<String>,
        axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
        headers: axum::http::HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        use axum::{http::StatusCode, response::IntoResponse};
        const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
        let mut bucket = state.lock().unwrap();
        let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        match (method.as_str(), query.get("uploadId")) {
            ("POST", None) if query.contains_key("uploads") => {
                bucket.next += 1;
                let id = format!("upload-{}", bucket.next);
                bucket.uploads.insert(id.clone(), (content_type, Default::default()));
                format!("<InitiateMultipartUploadResult xmlns=\"{NS}\"><Bucket>b</Bucket><Key>{key}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>").into_response()
            }
            ("PUT", Some(id)) => {
                let number: u16 = query["partNumber"].parse().unwrap();
                bucket.part_sizes.push(body.len());
                let Some((_, parts)) = bucket.uploads.get_mut(id) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                parts.insert(number, body.to_vec());
                ([("etag", format!("\"part-{number}\""))], "").into_response()
            }
            ("POST", Some(id)) => {
                let text = String::from_utf8_lossy(&body).to_string();
                let Some((content_type, parts)) = bucket.uploads.remove(id) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                for number in parts.keys() {
                    assert!(text.contains(&format!("part-{number}")), "part {number} missing from {text}");
                }
                let whole: Vec<u8> = parts.into_values().flatten().collect();
                bucket.objects.insert(key.clone(), (content_type, whole));
                format!("<CompleteMultipartUploadResult xmlns=\"{NS}\"><Key>{key}</Key></CompleteMultipartUploadResult>").into_response()
            }
            ("DELETE", Some(id)) => {
                bucket.uploads.remove(id);
                bucket.aborted += 1;
                StatusCode::NO_CONTENT.into_response()
            }
            ("PUT", None) => {
                bucket.objects.insert(key, (content_type, body.to_vec()));
                StatusCode::OK.into_response()
            }
            _ => StatusCode::NOT_IMPLEMENTED.into_response(),
        }
    }

    fn fake_bucket() -> (Shared, S3) {
        let state: Shared = Default::default();
        let app = axum::Router::new().route("/b/{*key}", axum::routing::any(fake_object))
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(state.clone());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        let s3 = S3::new(&format!("http://{addr}"), "b", "auto", "key", "secret", true).unwrap();
        (state, s3)
    }

    fn bucket_config(s3: S3, max_write_bytes: u64) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            blobs: Blobs { backend: Backend::S3(s3), max_bytes: 0, max_write_bytes },
            ..Config::local(dir.path().to_path_buf(), "t")
        };
        (dir, config)
    }

    #[test]
    fn a_small_file_written_in_pieces_goes_to_the_bucket_as_one_put_at_finish() {
        let (bucket, s3) = fake_bucket();
        let (_dir, config) = bucket_config(s3, 0);
        let mut writer = Writer::open(&config, "app", "small.txt", "text/plain").unwrap();
        writer.append(b"hello ").unwrap();
        writer.append(b"world").unwrap();
        assert!(bucket.lock().unwrap().objects.is_empty(), "visible before finish");
        assert_eq!(writer.finish().unwrap().size, 11);
        let state = bucket.lock().unwrap();
        assert_eq!(state.objects["app/small.txt"], ("text/plain".to_string(), b"hello world".to_vec()));
        assert!(state.part_sizes.is_empty() && state.uploads.is_empty(), "a small file started a multipart upload");
    }

    #[test]
    fn a_large_file_goes_up_in_parts_and_appears_in_the_bucket_only_when_completed() {
        let (bucket, s3) = fake_bucket();
        let (_dir, config) = bucket_config(s3, 0);
        let mut writer = Writer::open(&config, "app", "big.parquet", "application/vnd.apache.parquet").unwrap();
        let mut expected = Vec::new();
        for n in 0..20u8 {
            let chunk = vec![n; 1_000_000];
            writer.append(&chunk).unwrap();
            expected.extend_from_slice(&chunk);
        }
        {
            let state = bucket.lock().unwrap();
            assert!(state.objects.is_empty(), "visible before finish");
            assert_eq!(state.uploads.len(), 1, "no multipart upload under way");
            assert_eq!(state.part_sizes, [S3_PART_BYTES, S3_PART_BYTES], "parts went up before finish, at the part size");
        }
        assert_eq!(writer.finish().unwrap().size, 20_000_000);
        let state = bucket.lock().unwrap();
        let (content_type, body) = &state.objects["app/big.parquet"];
        assert_eq!(content_type, "application/vnd.apache.parquet");
        assert!(body == &expected, "the parts did not make the file");
        assert_eq!(state.part_sizes.len(), 3);
        assert!(state.uploads.is_empty());
    }

    #[test]
    fn an_abandoned_bucket_writer_aborts_its_upload_and_stores_nothing() {
        let (bucket, s3) = fake_bucket();
        let (_dir, config) = bucket_config(s3, 0);
        {
            let mut writer = Writer::open(&config, "app", "half.bin", "application/octet-stream").unwrap();
            writer.append(&vec![1u8; S3_PART_BYTES + 10]).unwrap();
            assert_eq!(bucket.lock().unwrap().uploads.len(), 1);
            // Dropped here, as when the call that held it traps.
        }
        let state = bucket.lock().unwrap();
        assert_eq!(state.aborted, 1, "the upload was not abandoned");
        assert!(state.uploads.is_empty() && state.objects.is_empty());
    }

    #[test]
    fn the_write_ceiling_stops_a_bucket_writer_and_aborts_its_upload() {
        let (bucket, s3) = fake_bucket();
        let (_dir, config) = bucket_config(s3, (S3_PART_BYTES + 100) as u64);
        let mut writer = Writer::open(&config, "app", "big.bin", "application/octet-stream").unwrap();
        writer.append(&vec![1u8; S3_PART_BYTES + 50]).unwrap();
        assert!(matches!(writer.append(&[1u8; 51]), Err(Error::TooLarge(_))));
        let state = bucket.lock().unwrap();
        assert_eq!(state.aborted, 1);
        assert!(state.uploads.is_empty() && state.objects.is_empty());
    }
}
