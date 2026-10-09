//! Objects in the site's bucket by their whole key: the platform's own
//! content under `.toolsite/`, which `content::files` keeps there on a
//! Postgres site. An app's files go through `blobs` instead, which scopes
//! every key to its app; nothing here scopes or judges a key, so only a
//! caller that built the key from validated parts may use it.
//!
//! Async, on a client kept per thread, so a page of a bundle's files goes
//! up over connections already open. Synchronous callers wait on it through
//! `state::wait`.

use super::blobs::{reason, ByteStream, S3};
use bytes::Bytes;
use futures_util::TryStreamExt;
use rusty_s3::S3Action;
use std::time::{Duration, SystemTime};

/// How long a signed request stays valid: long enough for a slow upload of
/// the largest bundle file, and nothing else ever sees the URL.
const SIGN_TTL: Duration = Duration::from_secs(3600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Keys one listing request returns; a listing asks again until done.
const PAGE: usize = 1_000;

/// What the bucket says about one object.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// One listing: the objects under a prefix, and with a delimiter the
/// "directories" one level down, each ending in `/`.
#[derive(Debug, Default)]
pub struct Listing {
    pub objects: Vec<(String, Object)>,
    pub prefixes: Vec<String>,
}

thread_local! {
    /// One client per thread, so connections are reused call to call. Not
    /// one per process: a pooled connection lives on the runtime that
    /// opened it, and a thread only ever serves one runtime, where the
    /// process may hold several (a command, a test).
    static CLIENT: Result<reqwest::Client, String> = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {}", reason(&e)));
}

fn client() -> Result<reqwest::Client, String> {
    CLIENT.with(Clone::clone)
}

fn failed(what: &str, key: &str, status: reqwest::StatusCode) -> String {
    format!("{what} {key}: the bucket answered {status}")
}

fn sent(what: &'static str) -> impl Fn(reqwest::Error) -> String {
    move |e| format!("{what}: {}", reason(&e))
}

fn http_date(headers: &reqwest::header::HeaderMap) -> Option<SystemTime> {
    let text = headers.get(reqwest::header::LAST_MODIFIED)?.to_str().ok()?;
    chrono::DateTime::parse_from_rfc2822(text).ok().map(SystemTime::from)
}

fn object_from(headers: &reqwest::header::HeaderMap) -> Object {
    let size = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Object { size, modified: http_date(headers) }
}

impl S3 {
    /// Stores `body` at `key`, replacing whatever was there.
    pub async fn object_put(&self, key: &str, body: Bytes) -> Result<(), String> {
        let url = self.bucket.put_object(Some(&self.credentials), key).sign(SIGN_TTL);
        let response = client()?.put(url).body(body).send().await.map_err(sent("put"))?;
        if !response.status().is_success() {
            return Err(failed("put", key, response.status()));
        }
        Ok(())
    }

    /// Stores `size` bytes from `body` at `key` without holding them: S3
    /// takes a stream only with its length known first.
    pub async fn object_put_stream(&self, key: &str, size: u64, body: ByteStream) -> Result<(), String> {
        let url = self.bucket.put_object(Some(&self.credentials), key).sign(SIGN_TTL);
        let response = client()?
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(reqwest::Body::wrap_stream(body))
            .send()
            .await
            .map_err(sent("put"))?;
        if !response.status().is_success() {
            return Err(failed("put", key, response.status()));
        }
        Ok(())
    }

    /// The object at `key`, opened for streaming; nothing when there is none.
    pub async fn object_get(&self, key: &str) -> Result<Option<(Object, ByteStream)>, String> {
        let url = self.bucket.get_object(Some(&self.credentials), key).sign(SIGN_TTL);
        let response = client()?.get(url).send().await.map_err(sent("get"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(failed("get", key, response.status()));
        }
        let object = object_from(response.headers());
        let stream = response.bytes_stream().map_err(|e| std::io::Error::other(reason(&e)));
        Ok(Some((object, Box::pin(stream))))
    }

    /// The whole object at `key`, for something small.
    pub async fn object_read(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let Some((_, stream)) = self.object_get(key).await? else { return Ok(None) };
        let parts: Vec<Bytes> = stream.try_collect().await.map_err(|e| format!("get {key}: {e}"))?;
        Ok(Some(parts.concat()))
    }

    /// Size and time of the object at `key`, without its bytes.
    pub async fn object_head(&self, key: &str) -> Result<Option<Object>, String> {
        let url = self.bucket.head_object(Some(&self.credentials), key).sign(SIGN_TTL);
        let response = client()?.head(url).send().await.map_err(sent("head"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(failed("head", key, response.status()));
        }
        Ok(Some(object_from(response.headers())))
    }

    /// Removes the object at `key`. One that is not there is not an error.
    pub async fn object_delete(&self, key: &str) -> Result<(), String> {
        let url = self.bucket.delete_object(Some(&self.credentials), key).sign(SIGN_TTL);
        let response = client()?.delete(url).send().await.map_err(sent("delete"))?;
        if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(failed("delete", key, response.status()));
        }
        Ok(())
    }

    /// Moves an object: copied through this process, then the original
    /// removed, so a failure part way leaves a copy, never neither. Whether
    /// there was anything at `from`.
    pub async fn object_move(&self, from: &str, to: &str) -> Result<bool, String> {
        let Some((object, stream)) = self.object_get(from).await? else { return Ok(false) };
        self.object_put_stream(to, object.size, stream).await?;
        self.object_delete(from).await?;
        Ok(true)
    }

    /// Every object under `prefix`, every page of it; with `delimited`,
    /// only those directly under it, and the next level's prefixes beside.
    pub async fn object_list(&self, prefix: &str, delimited: bool) -> Result<Listing, String> {
        let mut listing = Listing::default();
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
            action.with_prefix(prefix);
            action.with_max_keys(PAGE);
            if delimited {
                action.with_delimiter("/");
            }
            if let Some(token) = &token {
                action.with_continuation_token(token.clone());
            }
            let url = action.sign(SIGN_TTL);
            let response = client()?.get(url).send().await.map_err(sent("list"))?;
            if !response.status().is_success() {
                return Err(failed("list", prefix, response.status()));
            }
            let text = response.text().await.map_err(sent("list"))?;
            let page = rusty_s3::actions::ListObjectsV2::parse_response(&text)
                .map_err(|e| format!("list {prefix}: could not parse the bucket's answer: {e}"))?;
            for object in page.contents {
                let modified = chrono::DateTime::parse_from_rfc3339(&object.last_modified).ok().map(SystemTime::from);
                listing.objects.push((object.key, Object { size: object.size, modified }));
            }
            listing.prefixes.extend(page.common_prefixes.into_iter().map(|p| p.prefix));
            match page.next_continuation_token {
                Some(next) if !next.is_empty() => token = Some(next),
                _ => break,
            }
        }
        Ok(listing)
    }

    /// Makes the bucket, when a scratch server has none yet. A bucket that
    /// is already there is not an error.
    pub async fn ensure_bucket(&self) -> Result<(), String> {
        let url = self.bucket.create_bucket(&self.credentials).sign(SIGN_TTL);
        let response = client()?.put(url).send().await.map_err(sent("create bucket"))?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::CONFLICT {
            return Ok(());
        }
        Err(format!("create bucket: the bucket answered {status}"))
    }
}
