//! Tickets: short-lived capabilities minted on one request and presented on
//! another. Upload URLs, settings links, inline uploads in progress, browser
//! upload URLs, provider sign-ins in flight, preview sign-ins and subdomain
//! handoff codes are all this one shape, and all live here.
//!
//! What the store holds never opens anything by itself. A ticket's id is
//! the credential, so only its SHA-256 digest is kept, and the payload is
//! sealed with a key the store never sees, bound to that digest so a row
//! copied onto another id does not open. A dump of the table, or of this
//! process's memory map, has no id to present and no session token to use.
//!
//! The rules a ticket lives by are the store's, enforced in one step: an
//! expired ticket is not returned, and `take` hands a single-use ticket to
//! exactly one caller however many ask at once (one `delete ... returning`
//! on Postgres, one lock in memory).
//!
//! File mode keeps tickets in this process's memory, as it always did, under
//! a key made at boot. Postgres mode shares them between runners, under a
//! key derived from `TOOLSITE_SECRET_KEY`, so any runner can mint or redeem.

use async_trait::async_trait;
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use deadpool_postgres::Pool;
use rand::Rng;
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

/// The HKDF label the ticket key is derived from the site key with.
pub const KEY_LABEL: &str = "toolsite tickets v1";

/// What a ticket is for. Part of its digest, so an id minted as one kind
/// is never found as another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An upload URL: reusable until it expires.
    Upload,
    /// A link a person opens to type an app's settings in: reusable.
    SettingsLink,
    /// An upload arriving in chunks over MCP: updated per chunk, taken at
    /// the finish.
    InlineUpload,
    /// A browser upload URL a handler minted: single use.
    BlobUpload,
    /// A provider sign-in in flight, by its `state`: single use.
    Login,
    /// A headless browser's one-time sign-in: single use.
    Preview,
    /// A subdomain handoff code: single use.
    Handoff,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Upload => "upload",
            Kind::SettingsLink => "settings-link",
            Kind::InlineUpload => "inline-upload",
            Kind::BlobUpload => "blob-upload",
            Kind::Login => "login",
            Kind::Preview => "preview",
            Kind::Handoff => "handoff",
        }
    }
}

/// One ticket as a backend holds it: the digest of its id, its sealed
/// payload and when it stops counting. Nothing in it is a credential.
#[derive(Clone, Debug)]
pub struct Row {
    pub kind: String,
    pub digest: String,
    pub sealed: Vec<u8>,
    pub expires_at: i64,
}

/// A change to one ticket's sealed payload, run while the backend holds
/// it: `Some` stores the new payload, `None` leaves it as it was, an error
/// leaves it and is returned.
pub type Edit<'a> = Box<dyn FnOnce(&[u8]) -> Result<Option<Vec<u8>>, String> + Send + 'a>;

/// Where tickets are kept. Sees digests and sealed bytes only; expiry and
/// single use are enforced here, atomically, so no caller can race them.
/// Times are Unix seconds.
#[async_trait]
pub trait TicketStore: Send + Sync {
    /// Stores a new ticket, sweeping every expired one on the way.
    async fn insert(&self, row: Row, now: i64) -> Result<(), String>;
    /// A live ticket's payload, left in place.
    async fn get(&self, kind: &str, digest: &str, now: i64) -> Result<Option<Vec<u8>>, String>;
    /// A live ticket's payload, removed in the same step: of any number of
    /// callers at once, exactly one gets it.
    async fn take(&self, kind: &str, digest: &str, now: i64) -> Result<Option<Vec<u8>>, String>;
    /// Runs `edit` on a live ticket's payload with the ticket held, so two
    /// edits at once both land. False when there is no live ticket.
    async fn update(&self, kind: &str, digest: &str, now: i64, edit: Edit<'_>) -> Result<bool, String>;
    /// Live tickets of a kind.
    async fn count(&self, kind: &str, now: i64) -> Result<usize, String>;
    /// Ends a ticket's life now, as if its time had run out.
    async fn expire(&self, digest: &str) -> Result<bool, String>;
    /// Everything held, live or not: what a dump of the store would show.
    async fn rows(&self) -> Result<Vec<Row>, String>;
}

/// The handle every layer mints and redeems tickets through. Hashes ids,
/// seals payloads and keeps the clock; the backend keeps the rows.
#[derive(Clone)]
pub struct Tickets {
    store: Arc<dyn TicketStore>,
    key: Arc<[u8; 32]>,
}

impl Default for Tickets {
    fn default() -> Self {
        Tickets::memory()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// A ticket's id as the store knows it: SHA-256 over the kind and the id.
/// Ids are long random tokens, so a plain digest is enough; there is
/// nothing short here to guess.
pub fn digest(kind: Kind, id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.name().as_bytes());
    hasher.update([0]);
    hasher.update(id.as_bytes());
    data_encoding::HEXLOWER.encode(&hasher.finalize())
}

impl Tickets {
    /// This process's memory, under a key made now. Tickets do not outlive
    /// the process, so neither need the key.
    pub fn memory() -> Tickets {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        Tickets::with_store(Arc::new(Memory::default()), &key)
    }

    /// Shared through Postgres, under a key derived from the site key, so
    /// every runner opens what any runner sealed.
    pub fn postgres(pool: Pool, site_key: &[u8; 32]) -> Tickets {
        Tickets::with_store(Arc::new(Postgres { pool }), &crate::seal::derive(site_key, KEY_LABEL))
    }

    pub fn with_store(store: Arc<dyn TicketStore>, key: &[u8; 32]) -> Tickets {
        Tickets { store, key: Arc::new(*key) }
    }

    fn seal(&self, digest: &str, plain: &[u8]) -> Result<Vec<u8>, String> {
        let cipher = XChaCha20Poly1305::new(&(*self.key).into());
        let mut nonce = [0u8; 24];
        rand::rng().fill_bytes(&mut nonce);
        let sealed = cipher
            .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad: digest.as_bytes() })
            .map_err(|_| "could not seal a ticket".to_string())?;
        let mut stored = nonce.to_vec();
        stored.extend_from_slice(&sealed);
        Ok(stored)
    }

    fn open<T: DeserializeOwned>(&self, digest: &str, stored: &[u8]) -> Result<T, String> {
        if stored.len() < 24 {
            return Err("a ticket's payload is too short to be sealed".into());
        }
        let (nonce, sealed) = stored.split_at(24);
        let nonce: [u8; 24] = nonce.try_into().map_err(|_| "a ticket's nonce is malformed".to_string())?;
        let cipher = XChaCha20Poly1305::new(&(*self.key).into());
        let plain = cipher
            .decrypt(&XNonce::from(nonce), Payload { msg: sealed, aad: digest.as_bytes() })
            .map_err(|_| "a ticket's payload did not open with this site's key".to_string())?;
        serde_json::from_slice(&plain).map_err(|e| format!("a ticket's payload is not what was expected: {e}"))
    }

    fn sealed<T: Serialize>(&self, digest: &str, payload: &T) -> Result<Vec<u8>, String> {
        let plain = serde_json::to_vec(payload).map_err(|e| format!("could not encode a ticket: {e}"))?;
        self.seal(digest, &plain)
    }

    /// Mints a ticket good for `ttl` and returns its id: the credential,
    /// which is handed out and never stored.
    pub async fn put<T: Serialize>(&self, kind: Kind, ttl: Duration, payload: &T) -> Result<String, String> {
        let id = crate::content::slug::random_token(40);
        let digest = digest(kind, &id);
        let sealed = self.sealed(&digest, payload)?;
        let at = now();
        let expires_at = at + ttl.as_secs() as i64 + i64::from(ttl.subsec_nanos() > 0);
        self.store
            .insert(Row { kind: kind.name().to_string(), digest, sealed, expires_at }, at)
            .await?;
        Ok(id)
    }

    /// A reusable ticket's payload, while it lives.
    pub async fn get<T: DeserializeOwned>(&self, kind: Kind, id: &str) -> Result<Option<T>, String> {
        let digest = digest(kind, id);
        match self.store.get(kind.name(), &digest, now()).await? {
            Some(stored) => self.open(&digest, &stored).map(Some),
            None => Ok(None),
        }
    }

    /// A single-use ticket's payload, spent by this call. Unknown, expired
    /// and already spent are all `None`.
    pub async fn take<T: DeserializeOwned>(&self, kind: Kind, id: &str) -> Result<Option<T>, String> {
        let digest = digest(kind, id);
        match self.store.take(kind.name(), &digest, now()).await? {
            Some(stored) => self.open(&digest, &stored).map(Some),
            None => Ok(None),
        }
    }

    /// Changes a live ticket's payload in place: `edit` runs with the
    /// ticket held, and what it returns comes back. `None` when there is no
    /// live ticket; `edit`'s error leaves the ticket as it was.
    pub async fn update<T, R, F>(&self, kind: Kind, id: &str, edit: F) -> Result<Option<R>, String>
    where
        T: Serialize + DeserializeOwned,
        R: Send,
        F: FnOnce(&mut T) -> Result<R, String> + Send,
    {
        let digest = digest(kind, id);
        let mut outcome: Option<Result<R, String>> = None;
        let slot = &mut outcome;
        let this = self;
        let at = &digest;
        let found = self
            .store
            .update(
                kind.name(),
                &digest,
                now(),
                Box::new(move |stored: &[u8]| {
                    let mut value: T = this.open(at, stored)?;
                    match edit(&mut value) {
                        Ok(result) => {
                            let sealed = this.sealed(at, &value)?;
                            *slot = Some(Ok(result));
                            Ok(Some(sealed))
                        }
                        Err(why) => {
                            *slot = Some(Err(why));
                            Ok(None)
                        }
                    }
                }),
            )
            .await?;
        match (found, outcome) {
            (false, _) | (true, None) => Ok(None),
            (true, Some(result)) => result.map(Some),
        }
    }

    /// How many tickets of a kind are live. For tests and diagnostics.
    pub async fn live(&self, kind: Kind) -> Result<usize, String> {
        self.store.count(kind.name(), now()).await
    }

    /// Ends a ticket now, as its time running out would. For tests that
    /// prove an expired ticket is refused without waiting for one to be.
    #[doc(hidden)]
    pub async fn expire(&self, kind: Kind, id: &str) -> Result<bool, String> {
        self.store.expire(&digest(kind, id)).await
    }

    /// Everything the backend holds, as a dump would show it.
    #[doc(hidden)]
    pub async fn rows(&self) -> Result<Vec<Row>, String> {
        self.store.rows().await
    }
}

// --- memory -----------------------------------------------------------------

/// File mode's backend: a map behind one lock, so `take` and `update` are
/// whole steps.
#[derive(Default)]
pub struct Memory {
    rows: Mutex<HashMap<String, Row>>,
}

impl Memory {
    fn rows(&self) -> std::sync::MutexGuard<'_, HashMap<String, Row>> {
        self.rows.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl TicketStore for Memory {
    async fn insert(&self, row: Row, now: i64) -> Result<(), String> {
        let mut rows = self.rows();
        rows.retain(|_, row| row.expires_at > now);
        rows.insert(row.digest.clone(), row);
        Ok(())
    }

    async fn get(&self, kind: &str, digest: &str, now: i64) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .rows()
            .get(digest)
            .filter(|row| row.kind == kind && row.expires_at > now)
            .map(|row| row.sealed.clone()))
    }

    async fn take(&self, kind: &str, digest: &str, now: i64) -> Result<Option<Vec<u8>>, String> {
        let mut rows = self.rows();
        if !rows.get(digest).is_some_and(|row| row.kind == kind && row.expires_at > now) {
            return Ok(None);
        }
        Ok(rows.remove(digest).map(|row| row.sealed))
    }

    async fn update(&self, kind: &str, digest: &str, now: i64, edit: Edit<'_>) -> Result<bool, String> {
        let mut rows = self.rows();
        let Some(row) = rows.get_mut(digest).filter(|row| row.kind == kind && row.expires_at > now) else {
            return Ok(false);
        };
        if let Some(sealed) = edit(&row.sealed)? {
            row.sealed = sealed;
        }
        Ok(true)
    }

    async fn count(&self, kind: &str, now: i64) -> Result<usize, String> {
        Ok(self.rows().values().filter(|row| row.kind == kind && row.expires_at > now).count())
    }

    async fn expire(&self, digest: &str) -> Result<bool, String> {
        Ok(self.rows().get_mut(digest).map(|row| row.expires_at = 0).is_some())
    }

    async fn rows(&self) -> Result<Vec<Row>, String> {
        Ok(self.rows().values().cloned().collect())
    }
}

// --- Postgres ---------------------------------------------------------------

/// Postgres mode's backend: `state.tickets`. Single use is one
/// `delete ... returning`; an edit holds the row with `for update`.
pub struct Postgres {
    pool: Pool,
}

impl Postgres {
    async fn client(&self) -> Result<deadpool_postgres::Client, String> {
        self.pool
            .get()
            .await
            .map_err(|e| format!("could not reach Postgres for tickets: {}", super::pg::chain(&e)))
    }
}

fn failed(what: &str) -> impl Fn(tokio_postgres::Error) -> String + '_ {
    move |e| format!("could not {what}: {}", super::pg::chain(&e))
}

#[async_trait]
impl TicketStore for Postgres {
    async fn insert(&self, row: Row, now: i64) -> Result<(), String> {
        let client = self.client().await?;
        client
            .execute(
                "with swept as (delete from state.tickets where expires_at <= $5)
                 insert into state.tickets (id_hash, kind, payload, expires_at) values ($1, $2, $3, $4)",
                &[&row.digest, &row.kind, &row.sealed, &row.expires_at, &now],
            )
            .await
            .map_err(failed("store a ticket"))?;
        Ok(())
    }

    async fn get(&self, kind: &str, digest: &str, now: i64) -> Result<Option<Vec<u8>>, String> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "select payload from state.tickets where id_hash = $1 and kind = $2 and expires_at > $3",
                &[&digest, &kind, &now],
            )
            .await
            .map_err(failed("read a ticket"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn take(&self, kind: &str, digest: &str, now: i64) -> Result<Option<Vec<u8>>, String> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "delete from state.tickets where id_hash = $1 and kind = $2 and expires_at > $3 returning payload",
                &[&digest, &kind, &now],
            )
            .await
            .map_err(failed("spend a ticket"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn update(&self, kind: &str, digest: &str, now: i64, edit: Edit<'_>) -> Result<bool, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin a ticket update"))?;
        let Some(row) = transaction
            .query_opt(
                "select payload from state.tickets where id_hash = $1 and kind = $2 and expires_at > $3 for update",
                &[&digest, &kind, &now],
            )
            .await
            .map_err(failed("read a ticket"))?
        else {
            return Ok(false);
        };
        let stored: Vec<u8> = row.get(0);
        if let Some(sealed) = edit(&stored)? {
            transaction
                .execute("update state.tickets set payload = $2 where id_hash = $1", &[&digest, &sealed])
                .await
                .map_err(failed("update a ticket"))?;
        }
        transaction.commit().await.map_err(failed("commit a ticket update"))?;
        Ok(true)
    }

    async fn count(&self, kind: &str, now: i64) -> Result<usize, String> {
        let client = self.client().await?;
        let row = client
            .query_one("select count(*) from state.tickets where kind = $1 and expires_at > $2", &[&kind, &now])
            .await
            .map_err(failed("count tickets"))?;
        Ok(row.get::<_, i64>(0) as usize)
    }

    async fn expire(&self, digest: &str) -> Result<bool, String> {
        let client = self.client().await?;
        let changed = client
            .execute("update state.tickets set expires_at = 0 where id_hash = $1", &[&digest])
            .await
            .map_err(failed("expire a ticket"))?;
        Ok(changed > 0)
    }

    async fn rows(&self) -> Result<Vec<Row>, String> {
        let client = self.client().await?;
        let rows = client
            .query("select kind, id_hash, payload, expires_at from state.tickets", &[])
            .await
            .map_err(failed("read tickets"))?;
        Ok(rows
            .iter()
            .map(|row| Row { kind: row.get(0), digest: row.get(1), sealed: row.get(2), expires_at: row.get(3) })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Payload {
        app: String,
        token: String,
    }

    #[tokio::test]
    async fn a_payload_sealed_for_one_id_does_not_open_under_another() {
        let tickets = Tickets::memory();
        let first = tickets
            .put(Kind::Handoff, Duration::from_secs(60), &Payload { app: "a".into(), token: "t".into() })
            .await
            .unwrap();
        let second = tickets
            .put(Kind::Handoff, Duration::from_secs(60), &Payload { app: "b".into(), token: "u".into() })
            .await
            .unwrap();
        // Swap the two sealed payloads, as someone with write access to the
        // table could: neither opens where it does not belong.
        let (a, b) = (digest(Kind::Handoff, &first), digest(Kind::Handoff, &second));
        let memory = Memory::default();
        let rows = tickets.rows().await.unwrap();
        let sealed = |d: &str| rows.iter().find(|r| r.digest == d).unwrap().sealed.clone();
        memory
            .insert(Row { kind: "handoff".into(), digest: a.clone(), sealed: sealed(&b), expires_at: i64::MAX }, 0)
            .await
            .unwrap();
        let moved = Tickets { store: Arc::new(memory), key: tickets.key.clone() };
        assert!(moved.take::<Payload>(Kind::Handoff, &first).await.is_err(), "a payload opened under another id");
    }

    #[tokio::test]
    async fn a_ticket_of_one_kind_is_not_found_as_another() {
        let tickets = Tickets::memory();
        let id = tickets.put(Kind::Upload, Duration::from_secs(60), &"app").await.unwrap();
        assert_eq!(tickets.get::<String>(Kind::SettingsLink, &id).await.unwrap(), None);
        assert_eq!(tickets.take::<String>(Kind::Preview, &id).await.unwrap(), None);
        assert_eq!(tickets.get::<String>(Kind::Upload, &id).await.unwrap().as_deref(), Some("app"));
    }

    /// One site key, three uses: what seals a ticket is neither the key that
    /// seals settings and two-step secrets nor the key form tokens come from,
    /// so a value made for one use never opens or forges another.
    #[tokio::test]
    async fn the_ticket_key_is_its_own_and_not_the_site_or_form_key() {
        let site = [42u8; 32];
        let ticket_key = crate::seal::derive(&site, KEY_LABEL);
        assert_ne!(ticket_key, site);
        assert_ne!(ticket_key, crate::seal::derive(&site, crate::seal::FORM_KEY_LABEL));
        assert_ne!(KEY_LABEL, crate::seal::FORM_KEY_LABEL);

        let store: Arc<dyn TicketStore> = Arc::new(Memory::default());
        let minted = Tickets::with_store(store.clone(), &ticket_key);
        let id = minted.put(Kind::Upload, Duration::from_secs(60), &"app").await.unwrap();
        for wrong in [site, crate::seal::derive(&site, crate::seal::FORM_KEY_LABEL)] {
            let other = Tickets::with_store(store.clone(), &wrong);
            assert!(other.get::<String>(Kind::Upload, &id).await.is_err(), "a ticket opened under another use's key");
        }
        assert_eq!(minted.get::<String>(Kind::Upload, &id).await.unwrap().as_deref(), Some("app"));
    }

    #[tokio::test]
    async fn a_ticket_sealed_under_another_key_does_not_open() {
        let one = Tickets::memory();
        let id = one.put(Kind::Upload, Duration::from_secs(60), &"app").await.unwrap();
        let other = Tickets { store: one.store.clone(), key: Arc::new([7u8; 32]) };
        assert!(other.get::<String>(Kind::Upload, &id).await.is_err());
    }
}
