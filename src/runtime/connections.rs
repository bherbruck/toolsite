//! Live connections: every open socket, keyed by app, and what a handler
//! may do to them.
//!
//! A handler runs one event in a fresh sandbox and must answer and exit, so
//! it never owns a connection. Toolsite keeps each one here instead, with
//! the topics it joined and a little state, and the handler acts on it by
//! id: send, close, subscribe, publish. Every call names the app from the
//! host side, so one app can never reach another app's connections, even
//! with an id it guessed.
//!
//! Everything bound for one connection goes through one queue, in the
//! order it was sent. Until its `connect` event returns, only that event
//! reaches it: what other events send or publish to it meanwhile is held,
//! and follows once it is accepted. So a handler can join a topic, read a
//! snapshot and send it, and know the snapshot arrives before anything
//! published on the topic, with nothing published in between lost.
//!
//! Delivery is best effort: nothing is stored and nothing is replayed. A
//! connection that falls behind is cut off rather than buffered without
//! bound; its browser reconnects and asks the app for what it missed.
//!
//! Nothing here knows about HTTP or about which transport carries the
//! frames. `platform::connections` runs the socket and hands this module a
//! queue per connection.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// The most one message may carry, either way.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// The most state one connection may keep, keys and values together.
pub const MAX_STATE_BYTES: usize = 64 * 1024;
/// Topics one connection may join at once.
pub const MAX_TOPICS: usize = 32;
/// Messages waiting for one slow connection before it is cut off.
const QUEUE: usize = 64;

/// The ceilings a deployment sets, read from the environment like the others.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Open connections on one app at once.
    pub per_app: usize,
    /// Open connections one account holds at once, across apps.
    pub per_person: usize,
    /// Open connections across every app at once. Anonymous visitors of a
    /// public app have no account to count against, so this is what stops
    /// a flood spread over many public apps from exhausting the server.
    pub total: usize,
    /// Messages one app may send or publish in a second; the rest are
    /// refused.
    pub rate_per_app: u32,
    /// How often each connection is pinged and its person's access checked
    /// again.
    pub check_every: Duration,
    /// TCP and UDP connections on one app at once. A device has no account
    /// to count against, so this and `per_ip` are what bound it.
    pub raw_per_app: usize,
    /// TCP and UDP connections from one IP address at once, across apps.
    pub per_ip: usize,
    /// A TCP connection that sends nothing for this long is closed.
    pub tcp_idle: Duration,
    /// A UDP remote that sends nothing for this long is forgotten: its
    /// connection closes, and its next datagram opens a new one.
    pub udp_idle: Duration,
    /// Datagrams one IP address may send to a port in a second. The rest
    /// are dropped.
    pub udp_per_second: u32,
    /// Bytes of datagrams waiting for the handler on one UDP port, across
    /// its remotes. Past it, datagrams are dropped.
    pub udp_queued_bytes: usize,
    /// A TCP peer that takes none of a reply for this long is closed, and
    /// its place freed, rather than holding its connection open unread.
    pub tcp_send_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_app: 500,
            per_person: 20,
            total: 5000,
            rate_per_app: 100,
            check_every: Duration::from_secs(30),
            raw_per_app: 500,
            per_ip: 50,
            tcp_idle: Duration::from_secs(300),
            udp_idle: Duration::from_secs(60),
            udp_per_second: 100,
            udp_queued_bytes: 8 * 1024 * 1024,
            tcp_send_timeout: Duration::from_secs(10),
        }
    }
}

/// One frame, either way.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
}

impl Message {
    pub fn len(&self) -> usize {
        match self {
            Message::Text(text) => text.len(),
            Message::Binary(bytes) => bytes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What the transport is told to do with a connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Message(Message),
    Close,
}

struct Connection {
    user: Option<String>,
    /// Where a TCP or UDP connection comes from. None for a WebSocket.
    remote: Option<SocketAddr>,
    topics: HashSet<String>,
    /// None once the connection is cut off for falling behind. It stays
    /// registered, and counted, until its transport lets go of the socket:
    /// freeing its place sooner would let a peer that never reads hold
    /// sockets past every ceiling.
    tx: Option<mpsc::Sender<Outgoing>>,
    /// Some while its `connect` event runs: what others send it waits here,
    /// counted against the same queue.
    held: Option<Vec<Outgoing>>,
    state: HashMap<String, String>,
}

impl Connection {
    fn state_bytes(&self) -> usize {
        self.state.iter().map(|(k, v)| k.len() + v.len()).sum()
    }
}

struct AppEntry {
    connections: HashMap<String, Connection>,
    window_start: Instant,
    sent_in_window: u32,
    last_warning: Option<Instant>,
}

impl AppEntry {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
            window_start: Instant::now(),
            sent_in_window: 0,
            last_warning: None,
        }
    }

    /// Counts one message against the app's rate, or refuses it.
    fn spend(&mut self, app: &str, limit: u32) -> Result<(), String> {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.sent_in_window = 0;
        }
        if self.sent_in_window >= limit {
            // One line a second, not one per dropped message.
            if self.last_warning.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
                self.last_warning = Some(now);
                tracing::warn!(app, limit, "connection messages refused: the app sends faster than its limit per second");
            }
            return Err(format!("the app has sent its limit of {limit} messages this second; this one was refused"));
        }
        self.sent_in_window += 1;
        Ok(())
    }
}

#[derive(Default)]
struct Inner {
    apps: HashMap<String, AppEntry>,
    per_person: HashMap<String, usize>,
    per_ip: HashMap<IpAddr, usize>,
    total: usize,
}

impl Inner {
    /// Frees what one removed connection held: its place in the total, in
    /// its person's count and in its address's count.
    fn release(&mut self, connection: Connection) {
        self.total = self.total.saturating_sub(1);
        if let Some(remote) = connection.remote
            && let Some(count) = self.per_ip.get_mut(&remote.ip())
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_ip.remove(&remote.ip());
            }
        }
        self.release_person(connection.user);
    }

    fn release_person(&mut self, user: Option<String>) {
        if let Some(user) = user
            && let Some(count) = self.per_person.get_mut(&user)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_person.remove(&user);
            }
        }
    }
}

/// Every app's connections. One per process: a message reaches the sockets
/// this toolsite instance holds, which is all of them while it runs as one.
pub struct Hub {
    pub limits: Limits,
    inner: Mutex<Inner>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

/// A connection's place in the registry. Dropping it removes the
/// connection, its topics and its state, whichever way it ended.
pub struct Registration {
    hub: Arc<Hub>,
    pub app: String,
    pub id: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.forget(&self.app, &self.id);
    }
}

fn plain_topic(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn user_topic_id(name: &str) -> Option<&str> {
    let id = name.strip_prefix("user:")?;
    ((1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then_some(id)
}

/// Whether the app may publish on `topic`. The app may address any
/// person's `user:` topic: it is the app's own code.
pub fn valid_topic(topic: &str) -> Result<(), String> {
    if plain_topic(topic) || user_topic_id(topic).is_some() {
        Ok(())
    } else {
        Err(format!(
            "topic {topic:?} is not a valid name: use lower-case letters, digits, '-' and '_', or user:<id>"
        ))
    }
}

/// Whether a connection held by `holder` may join `topic`. A `user:` topic
/// is only ever its holder's own, whatever the handler asks for.
pub fn may_join(topic: &str, holder: Option<&str>) -> Result<(), String> {
    valid_topic(topic)?;
    match (user_topic_id(topic), holder) {
        (None, _) => Ok(()),
        (Some(id), Some(me)) if id == me => Ok(()),
        (Some(_), _) => Err(format!("topic {topic} belongs to another person")),
    }
}

impl Hub {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Adds a connection for `app`, or says which ceiling stops it. The
    /// receiver is what the transport sends from.
    pub fn register(
        self: &Arc<Self>,
        app: &str,
        user: Option<&str>,
    ) -> Result<(Registration, mpsc::Receiver<Outgoing>), String> {
        self.register_from(app, user, None)
    }

    /// `register` for a connection that comes from an address rather than a
    /// browser: a TCP connection or a UDP remote. It counts against the
    /// app's TCP and UDP ceiling and its address's.
    pub fn register_from(
        self: &Arc<Self>,
        app: &str,
        user: Option<&str>,
        remote: Option<SocketAddr>,
    ) -> Result<(Registration, mpsc::Receiver<Outgoing>), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.total >= self.limits.total {
            return Err(format!("the server has its maximum of {} open connections", self.limits.total));
        }
        let open_here = inner.apps.get(app).map_or(0, |entry| entry.connections.len());
        if open_here >= self.limits.per_app {
            return Err(format!("this app has its maximum of {} open connections", self.limits.per_app));
        }
        if let Some(remote) = remote {
            let raw_here = inner
                .apps
                .get(app)
                .map_or(0, |entry| entry.connections.values().filter(|c| c.remote.is_some()).count());
            if raw_here >= self.limits.raw_per_app {
                return Err(format!(
                    "this app has its maximum of {} open TCP and UDP connections",
                    self.limits.raw_per_app
                ));
            }
            if inner.per_ip.get(&remote.ip()).copied().unwrap_or(0) >= self.limits.per_ip {
                return Err(format!("{} has its maximum of {} open connections", remote.ip(), self.limits.per_ip));
            }
        }
        if let Some(user) = user {
            let held = inner.per_person.get(user).copied().unwrap_or(0);
            if held >= self.limits.per_person {
                return Err(format!("this account has its maximum of {} open connections", self.limits.per_person));
            }
            *inner.per_person.entry(user.to_string()).or_default() += 1;
        }
        if let Some(remote) = remote {
            *inner.per_ip.entry(remote.ip()).or_default() += 1;
        }
        inner.total += 1;
        let id = crate::content::slug::random_token(24);
        let (tx, rx) = mpsc::channel(QUEUE);
        inner.apps.entry(app.to_string()).or_insert_with(AppEntry::new).connections.insert(
            id.clone(),
            Connection {
                user: user.map(str::to_string),
                remote,
                topics: HashSet::new(),
                tx: Some(tx),
                held: Some(Vec::new()),
                state: HashMap::new(),
            },
        );
        Ok((
            Registration {
                hub: self.clone(),
                app: app.to_string(),
                id,
            },
            rx,
        ))
    }

    fn forget(&self, app: &str, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        let removed = inner.apps.get_mut(app).and_then(|entry| entry.connections.remove(id));
        if let Some(connection) = removed {
            inner.release(connection);
        }
        if inner.apps.get(app).is_some_and(|entry| entry.connections.is_empty()) {
            inner.apps.remove(app);
        }
    }

    /// Queues `out` for each id, without waiting. `from` is the connection
    /// whose `connect` event is sending, if any; a connection still
    /// connecting holds what anyone else sends it. A connection whose queue
    /// is full is cut off: dropping its sender ends its transport loop.
    fn deliver(inner: &mut Inner, app: &str, ids: &[String], out: &Outgoing, from: Option<&str>) -> u32 {
        let Some(entry) = inner.apps.get_mut(app) else {
            return 0;
        };
        let mut reached = 0;
        for id in ids {
            if let Some(connection) = entry.connections.get_mut(id)
                && let Some(tx) = &connection.tx
            {
                let queued = match &mut connection.held {
                    Some(held) if from != Some(id.as_str()) => {
                        let room = held.len() < tx.capacity();
                        if room {
                            held.push(out.clone());
                        }
                        room
                    }
                    _ => tx.try_send(out.clone()).is_ok(),
                };
                if queued {
                    reached += 1;
                } else {
                    tracing::warn!(app, "a connection fell behind and was closed");
                    connection.tx = None;
                    connection.held = None;
                }
            }
        }
        reached
    }

    /// Marks one of `app`'s connections accepted: its `connect` event
    /// returned, so what was held for it follows what that event sent.
    pub fn accept(&self, app: &str, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        let Some(connection) = inner.apps.get_mut(app).and_then(|entry| entry.connections.get_mut(id)) else {
            return;
        };
        let held = connection.held.take().unwrap_or_default();
        let Some(tx) = &connection.tx else {
            return;
        };
        for out in held {
            if tx.try_send(out).is_err() {
                tracing::warn!(app, "a connection fell behind and was closed");
                connection.tx = None;
                return;
            }
        }
    }

    fn check_message(message: &Message) -> Result<(), String> {
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "the message is {} bytes; the most one message may carry is {MAX_MESSAGE_BYTES}",
                message.len()
            ));
        }
        Ok(())
    }

    /// Sends one message to one of `app`'s connections. `from` is the
    /// connection whose `connect` event is calling, if any.
    pub fn send(&self, app: &str, id: &str, message: Message, from: Option<&str>) -> Result<(), String> {
        Self::check_message(&message)?;
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .apps
            .get_mut(app)
            .filter(|entry| entry.connections.contains_key(id))
            .ok_or_else(|| format!("no open connection {id}"))?;
        entry.spend(app, self.limits.rate_per_app)?;
        match Self::deliver(&mut inner, app, &[id.to_string()], &Outgoing::Message(message), from) {
            1 => Ok(()),
            _ => Err(format!("connection {id} fell behind and was closed")),
        }
    }

    /// Ends one of `app`'s connections. Its transport sends the close, and
    /// the handler still gets the connection's `close` event.
    pub fn close(&self, app: &str, id: &str, from: Option<&str>) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let open = inner.apps.get(app).is_some_and(|entry| entry.connections.contains_key(id));
        if !open {
            return Err(format!("no open connection {id}"));
        }
        Self::deliver(&mut inner, app, &[id.to_string()], &Outgoing::Close, from);
        Ok(())
    }

    pub fn subscribe(&self, app: &str, id: &str, topic: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let connection = inner
            .apps
            .get_mut(app)
            .and_then(|entry| entry.connections.get_mut(id))
            .ok_or_else(|| format!("no open connection {id}"))?;
        may_join(topic, connection.user.as_deref())?;
        if !connection.topics.contains(topic) && connection.topics.len() >= MAX_TOPICS {
            return Err(format!("a connection may join at most {MAX_TOPICS} topics"));
        }
        connection.topics.insert(topic.to_string());
        Ok(())
    }

    pub fn unsubscribe(&self, app: &str, id: &str, topic: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let connection = inner
            .apps
            .get_mut(app)
            .and_then(|entry| entry.connections.get_mut(id))
            .ok_or_else(|| format!("no open connection {id}"))?;
        connection.topics.remove(topic);
        Ok(())
    }

    /// Sends `message` to `app`'s connections on `topic`, and returns how
    /// many it reached. Called from a handler, so it never waits. `from` is
    /// as for `send`.
    pub fn publish(&self, app: &str, topic: &str, message: Message, from: Option<&str>) -> Result<u32, String> {
        valid_topic(topic)?;
        Self::check_message(&message)?;
        let mut inner = self.inner.lock().unwrap();
        let Some(entry) = inner.apps.get_mut(app) else {
            return Ok(0);
        };
        entry.spend(app, self.limits.rate_per_app)?;
        let ids: Vec<String> = entry
            .connections
            .iter()
            .filter(|(_, connection)| connection.topics.contains(topic))
            .map(|(id, _)| id.clone())
            .collect();
        Ok(Self::deliver(&mut inner, app, &ids, &Outgoing::Message(message), from))
    }

    /// Where one of `app`'s connections comes from, as "ip:port", when it is
    /// a TCP connection or a UDP remote.
    pub fn remote(&self, app: &str, id: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner.apps.get(app)?.connections.get(id)?.remote.map(|addr| addr.to_string())
    }

    pub fn state_get(&self, app: &str, id: &str, key: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner.apps.get(app)?.connections.get(id)?.state.get(key).cloned()
    }

    pub fn state_set(&self, app: &str, id: &str, key: &str, value: Option<String>) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let connection = inner
            .apps
            .get_mut(app)
            .and_then(|entry| entry.connections.get_mut(id))
            .ok_or_else(|| format!("no open connection {id}"))?;
        match value {
            None => {
                connection.state.remove(key);
            }
            Some(value) => {
                let current = connection.state.get(key).map_or(0, |old| key.len() + old.len());
                let after = connection.state_bytes() - current + key.len() + value.len();
                if after > MAX_STATE_BYTES {
                    return Err(format!(
                        "a connection may keep at most {MAX_STATE_BYTES} bytes of state; this would make it {after}"
                    ));
                }
                connection.state.insert(key.to_string(), value);
            }
        }
        Ok(())
    }

    /// Ends every connection `app` holds: the app was hidden, removed, or
    /// replaced by a new app at its name. Each still gets its `close` event
    /// from whatever handler is there; a gone app's handler is not run.
    pub fn close_app(&self, app: &str) {
        let mut inner = self.inner.lock().unwrap();
        let ids: Vec<String> = inner.apps.get(app).map(|entry| entry.connections.keys().cloned().collect()).unwrap_or_default();
        if !ids.is_empty() {
            tracing::info!(app, open = ids.len(), "closing the app's connections");
            Self::deliver(&mut inner, app, &ids, &Outgoing::Close, None);
        }
    }

    /// Open connections on one app.
    pub fn open(&self, app: &str) -> usize {
        self.inner.lock().unwrap().apps.get(app).map_or(0, |entry| entry.connections.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Message {
        Message::Text(s.to_string())
    }

    /// A connection whose `connect` event has returned.
    fn accepted(hub: &Arc<Hub>, app: &str, user: Option<&str>) -> (Registration, mpsc::Receiver<Outgoing>) {
        let (registration, rx) = hub.register(app, user).unwrap();
        hub.accept(app, &registration.id);
        (registration, rx)
    }

    fn drain(rx: &mut mpsc::Receiver<Outgoing>) -> Vec<Outgoing> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn what_connect_sends_arrives_in_the_order_it_was_sent() {
        let hub = Arc::new(Hub::new(Limits { rate_per_app: 10_000, ..Limits::default() }));
        let (conn, mut rx) = hub.register("board", None).unwrap();
        let me = Some(conn.id.as_str());
        hub.subscribe("board", &conn.id, "who").unwrap();
        hub.send("board", &conn.id, text("1"), me).unwrap();
        assert_eq!(hub.publish("board", "who", text("2"), me).unwrap(), 1);
        hub.send("board", &conn.id, text("3"), me).unwrap();
        hub.accept("board", &conn.id);
        let got = drain(&mut rx);
        assert_eq!(got, ["1", "2", "3"].map(|n| Outgoing::Message(text(n))));
    }

    #[test]
    fn what_others_publish_during_connect_follows_what_connect_sent() {
        let hub = Arc::new(Hub::new(Limits { rate_per_app: 10_000, ..Limits::default() }));
        let (old, mut old_rx) = accepted(&hub, "board", None);
        let (conn, mut rx) = hub.register("board", None).unwrap();
        let me = Some(conn.id.as_str());
        hub.subscribe("board", &conn.id, "who").unwrap();
        // Another event, such as the close of this person's last tab,
        // publishes between the join and the snapshot.
        assert_eq!(hub.publish("board", "who", text("from elsewhere"), None).unwrap(), 1);
        assert!(rx.try_recv().is_err(), "a connection still connecting heard another event");
        hub.send("board", &conn.id, text("snapshot"), me).unwrap();
        hub.accept("board", &conn.id);
        assert_eq!(drain(&mut rx), ["snapshot", "from elsewhere"].map(|m| Outgoing::Message(text(m))));
        // An accepted connection hears others at once, and nothing twice.
        hub.subscribe("board", &old.id, "who").unwrap();
        hub.publish("board", "who", text("later"), None).unwrap();
        assert_eq!(drain(&mut rx), [Outgoing::Message(text("later"))]);
        assert_eq!(drain(&mut old_rx), [Outgoing::Message(text("later"))]);
    }

    #[test]
    fn what_is_held_during_connect_counts_against_the_queue_and_goes_if_it_is_refused() {
        let hub = Arc::new(Hub::new(Limits { rate_per_app: 10_000, ..Limits::default() }));
        let (conn, mut rx) = hub.register("board", None).unwrap();
        hub.subscribe("board", &conn.id, "a").unwrap();
        hub.send("board", &conn.id, text("own"), Some(&conn.id)).unwrap();
        for n in 1..QUEUE {
            assert_eq!(hub.publish("board", "a", text(&n.to_string()), None).unwrap(), 1);
        }
        // The queue is full counting what is held: the next one cuts it off.
        assert_eq!(hub.publish("board", "a", text("one too many"), None).unwrap(), 0);
        hub.accept("board", &conn.id);
        assert_eq!(drain(&mut rx), [Outgoing::Message(text("own"))], "a cut connection got what was held");

        // A refused connect: what was held goes with it.
        let (refused, mut rx) = hub.register("board", None).unwrap();
        hub.subscribe("board", &refused.id, "a").unwrap();
        hub.publish("board", "a", text("x"), None).unwrap();
        drop(refused);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_user_topic_is_only_ever_its_holders_own() {
        assert!(may_join("orders", None).is_ok());
        assert!(may_join("user:abc", Some("abc")).is_ok());
        assert!(may_join("user:abc", Some("xyz")).is_err());
        assert!(may_join("user:abc", None).is_err());
        assert!(may_join("Orders", None).is_err());
        assert!(may_join("", None).is_err());
        assert!(may_join(&"a".repeat(65), None).is_err());
        assert!(valid_topic("user:anyone").is_ok());
        assert!(valid_topic("user:").is_err());
    }

    #[tokio::test]
    async fn a_publish_reaches_its_topic_on_its_app_and_nowhere_else() {
        let hub = Arc::new(Hub::default());
        let (orders, mut orders_rx) = accepted(&hub, "shop", None);
        let (_stock, mut stock_rx) = accepted(&hub, "shop", None);
        let (other, mut other_rx) = accepted(&hub, "ledger", None);
        hub.subscribe("shop", &orders.id, "orders").unwrap();
        hub.subscribe("ledger", &other.id, "orders").unwrap();
        assert_eq!(hub.publish("shop", "orders", text("hello"), None).unwrap(), 1);
        assert_eq!(orders_rx.try_recv().unwrap(), Outgoing::Message(text("hello")));
        assert!(stock_rx.try_recv().is_err());
        assert!(other_rx.try_recv().is_err());
        // Another app's id is no handle on this one.
        assert!(hub.send("ledger", &orders.id, text("x"), None).is_err());
        assert!(hub.close("ledger", &orders.id, None).is_err());
        assert!(hub.subscribe("ledger", &orders.id, "a").is_err());
        assert_eq!(hub.state_get("ledger", &orders.id, "k"), None);
    }

    #[test]
    fn a_message_over_the_ceiling_is_refused() {
        let hub = Arc::new(Hub::default());
        let (conn, _rx) = hub.register("shop", None).unwrap();
        let big = text(&"x".repeat(MAX_MESSAGE_BYTES + 1));
        assert!(hub.publish("shop", "orders", big.clone(), None).unwrap_err().contains("bytes"));
        assert!(hub.send("shop", &conn.id, big, None).unwrap_err().contains("bytes"));
    }

    #[test]
    fn the_connection_ceilings_hold_and_free_up_when_one_closes() {
        let hub = Arc::new(Hub::new(Limits { per_app: 2, per_person: 1, ..Limits::default() }));
        let first = hub.register("shop", Some("u1")).unwrap();
        assert!(hub.register("shop", Some("u1")).is_err(), "per person");
        let _second = hub.register("shop", Some("u2")).unwrap();
        assert!(hub.register("shop", Some("u3")).is_err(), "per app");
        drop(first);
        assert!(hub.register("shop", Some("u1")).is_ok());
    }

    #[test]
    fn the_address_and_tcp_ceilings_hold_and_free_up_when_one_closes() {
        let hub = Arc::new(Hub::new(Limits { per_ip: 2, raw_per_app: 3, ..Limits::default() }));
        let near: SocketAddr = "10.0.0.1:4000".parse().unwrap();
        let other: SocketAddr = "10.0.0.2:4000".parse().unwrap();
        let first = hub.register_from("broker", None, Some(near)).unwrap();
        // The address counts across apps, whatever its port.
        let _second = hub.register_from("syslog", None, Some("10.0.0.1:4001".parse().unwrap())).unwrap();
        assert!(hub.register_from("broker", None, Some(near)).err().unwrap().contains("10.0.0.1"));
        let _third = hub.register_from("broker", None, Some(other)).unwrap();
        // A browser does not count against the TCP and UDP ceiling.
        let _browser = hub.register("broker", None).unwrap();
        assert_eq!(hub.remote("broker", &first.0.id).as_deref(), Some("10.0.0.1:4000"));
        drop(first);
        let _again = hub.register_from("broker", None, Some(near)).unwrap();
        let _fourth = hub.register_from("broker", None, Some("10.0.0.3:1".parse().unwrap())).unwrap();
        assert!(hub.register_from("broker", None, Some("10.0.0.4:1".parse().unwrap())).err().unwrap().contains("TCP"));
    }

    #[test]
    fn the_server_wide_ceiling_holds_for_anonymous_visitors_across_apps() {
        let hub = Arc::new(Hub::new(Limits { total: 2, ..Limits::default() }));
        let first = hub.register("one", None).unwrap();
        let _second = hub.register("two", None).unwrap();
        assert!(hub.register("three", None).err().unwrap().contains("server"));
        drop(first);
        assert!(hub.register("three", None).is_ok());
    }

    #[tokio::test]
    async fn closing_an_app_ends_every_connection_it_holds_and_no_other_apps() {
        let hub = Arc::new(Hub::default());
        let (_a, mut a_rx) = accepted(&hub, "shop", None);
        let (_b, mut b_rx) = accepted(&hub, "shop", None);
        let (_c, mut c_rx) = accepted(&hub, "ledger", None);
        hub.close_app("shop");
        assert_eq!(a_rx.try_recv().unwrap(), Outgoing::Close);
        assert_eq!(b_rx.try_recv().unwrap(), Outgoing::Close);
        assert!(c_rx.try_recv().is_err());
    }

    #[test]
    fn messages_past_the_rate_are_refused() {
        let hub = Arc::new(Hub::new(Limits { rate_per_app: 2, ..Limits::default() }));
        let (conn, _rx) = hub.register("shop", None).unwrap();
        assert!(hub.publish("shop", "a", text("1"), None).is_ok());
        assert!(hub.send("shop", &conn.id, text("2"), None).is_ok());
        assert!(hub.publish("shop", "a", text("3"), None).is_err());
    }

    #[test]
    fn a_connection_that_falls_behind_is_cut_off_but_counted_until_its_socket_is_let_go() {
        let hub = Arc::new(Hub::new(Limits { rate_per_app: 10_000, per_person: 1, ..Limits::default() }));
        let (slow, _rx) = hub.register("shop", Some("u1")).unwrap();
        hub.subscribe("shop", &slow.id, "a").unwrap();
        for n in 0..QUEUE {
            hub.publish("shop", "a", text(&n.to_string()), None).unwrap();
        }
        assert_eq!(hub.publish("shop", "a", text("one too many"), None).unwrap(), 0);
        assert!(hub.send("shop", &slow.id, text("x"), None).is_err(), "a cut connection still takes messages");
        // Still holding its socket until the transport lets go, so it still
        // counts against the ceilings.
        assert_eq!(hub.open("shop"), 1);
        assert!(hub.register("shop", Some("u1")).is_err(), "the cut connection's place was given away early");
        drop(slow);
        assert_eq!(hub.open("shop"), 0);
    }

    #[test]
    fn state_is_capped_per_connection_and_goes_with_it() {
        let hub = Arc::new(Hub::default());
        let (conn, _rx) = hub.register("shop", None).unwrap();
        hub.state_set("shop", &conn.id, "k", Some("v".into())).unwrap();
        assert_eq!(hub.state_get("shop", &conn.id, "k").as_deref(), Some("v"));
        assert!(hub.state_set("shop", &conn.id, "big", Some("x".repeat(MAX_STATE_BYTES))).is_err());
        // Replacing a value counts the new one, not both.
        hub.state_set("shop", &conn.id, "k", Some("x".repeat(MAX_STATE_BYTES - 1))).unwrap();
        let id = conn.id.clone();
        drop(conn);
        assert_eq!(hub.state_get("shop", &id, "k"), None);
    }
}
