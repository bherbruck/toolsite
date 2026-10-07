//! Fork addition: the network side of the broker as a step function.
//!
//! Upstream, each connection is a tokio task (`server::broker::remote`) that
//! owns a socket (`link::network::Network`) and talks to the router thread
//! through a `RemoteLink`. This module does the same work with no socket,
//! no task and no thread: whoever owns the sockets calls [`Broker::open`],
//! [`Broker::read`], [`Broker::closed`] and [`Broker::tick`], and then
//! writes or closes what [`Broker::take_outputs`] returns. Each call runs
//! the router with [`Router::step`] until it has nothing left to do.
//!
//! What it keeps from upstream, in the same order:
//!
//! - `mqtt_connect`: the first packet must be CONNECT, within the
//!   connection timeout; a keep alive of zero is refused; an empty client
//!   id needs a clean session and is then assigned `rumqtt-<uuid>`.
//! - `remote`: a session that ends without DISCONNECT tells the router, and
//!   its will is published after the will delay, or at once when the same
//!   client id connects again with a clean session (cancelled when it
//!   connects with a persistent one).
//! - `RemoteLink`: packets read go to the link's buffer in batches of at
//!   most `max_connection_buffer_len`; notifications come back as packets,
//!   and an `Unschedule` marks the link ready again once they are written.
//! - `Network`: a connection that sends nothing for one and a half keep
//!   alive periods is closed.
//!
//! What differs: the protocol is chosen per connection from the CONNECT's
//! protocol level (5 is v5, anything else goes to v4, which refuses levels
//! it does not speak) rather than per listener, and a CONNECT the
//! authenticator refuses is answered with a CONNACK carrying its code
//! before the connection is closed (upstream closes without one).

use std::collections::{HashMap, VecDeque};

use bytes::{Buf, BytesMut};
use flume::Sender;
use tracing::{debug, info, warn};

use crate::link::local::{LinkBuilder, LinkRx, LinkTx};
use crate::protocol::{
    v4::V4, v5::V5, ConnAck, ConnectReturnCode, Login, Packet, Protocol, QoS,
};
use crate::router::{Event, Notification};
use crate::{ConnectionId, Router, RouterConfig};

/// The caller's name for one network connection.
pub type Key = u64;

/// How connections behave, as upstream's `ConnectionSettings`.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The CONNECT must arrive within this long of the connection opening.
    pub connection_timeout_ms: u64,
    /// The largest packet a client may send.
    pub max_payload_size: usize,
    /// Packets handed to the router in one batch.
    pub max_connection_buffer_len: usize,
    pub dynamic_filters: bool,
    /// The largest single write asked of the caller. Longer output is split.
    pub max_write_bytes: usize,
}

/// What the network should do after a call.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// Send these bytes to the connection, in order.
    Write(Key, Vec<u8>),
    /// Close the connection. The broker has already forgotten it, so
    /// `closed` for it afterwards does nothing.
    Close(Key),
}

/// What an authenticator sees of a CONNECT.
pub struct Hello<'a> {
    pub key: Key,
    pub client_id: &'a str,
    pub login: Option<&'a Login>,
}

/// A publish the broker received, for a caller that shows recent traffic.
#[derive(Debug, Clone)]
pub struct Seen {
    pub at_ms: u64,
    pub client_id: String,
    pub topic: String,
    pub qos: u8,
    pub retain: bool,
    pub size: usize,
    /// The first bytes of the payload, at most `SEEN_PREVIEW`.
    pub preview: Vec<u8>,
}

/// Recent publishes kept for [`Broker::recent`].
pub const SEEN_KEPT: usize = 50;
/// Payload bytes kept per recent publish.
pub const SEEN_PREVIEW: usize = 256;

/// Rounds of step-and-write per call while links keep asking to be
/// rescheduled. What is left runs on the next call.
const MAX_FLUSH_ROUNDS: usize = 16;

enum Proto {
    V4(V4),
    V5(V5),
}

impl Proto {
    fn read_mut(&mut self, stream: &mut BytesMut, max: usize) -> Result<Packet, crate::protocol::Error> {
        match self {
            Proto::V4(p) => p.read_mut(stream, max),
            Proto::V5(p) => p.read_mut(stream, max),
        }
    }

    fn write(&self, packet: Packet, out: &mut BytesMut) -> Result<usize, crate::protocol::Error> {
        match self {
            Proto::V4(p) => p.write(packet, out),
            Proto::V5(p) => p.write(packet, out),
        }
    }
}

struct Live {
    protocol: Proto,
    read: BytesMut,
    link_tx: LinkTx,
    link_rx: LinkRx,
    connection_id: ConnectionId,
    client_id: String,
    /// One and a half keep alive periods, as `Network::set_keepalive`.
    keepalive_ms: u64,
    last_read_ms: u64,
    will_delay_ms: u64,
    /// Another connection took over this client id. Its will went with it.
    superseded: bool,
    notifications: VecDeque<Notification>,
}

enum Link {
    Connecting { read: BytesMut, opened_ms: u64 },
    Live(Box<Live>),
}

/// A will waiting out its delay after its session ended.
struct AwaitingWill {
    due_ms: u64,
    connection_id: ConnectionId,
}

/// One broker: the router and every connection's link to it.
pub struct Broker {
    router: Router,
    router_tx: Sender<(ConnectionId, Event)>,
    settings: Settings,
    links: HashMap<Key, Link>,
    wills: HashMap<String, AwaitingWill>,
    outputs: Vec<Output>,
    recent: VecDeque<Seen>,
    publishes: u64,
}

impl Broker {
    pub fn new(router: RouterConfig, settings: Settings) -> Broker {
        let router = Router::new(0, router);
        let router_tx = router.link();
        Broker {
            router,
            router_tx,
            settings,
            links: HashMap::new(),
            wills: HashMap::new(),
            outputs: Vec::new(),
            recent: VecDeque::with_capacity(SEEN_KEPT),
            publishes: 0,
        }
    }

    /// A network connection opened. Nothing is sent until its CONNECT.
    pub fn open(&mut self, key: Key, now_ms: u64) {
        self.links.insert(
            key,
            Link::Connecting {
                read: BytesMut::with_capacity(1024),
                opened_ms: now_ms,
            },
        );
    }

    /// Bytes arrived on a connection. `authenticate` decides a CONNECT:
    /// `Err(code)` refuses it with that CONNACK code.
    pub fn read(
        &mut self,
        key: Key,
        bytes: &[u8],
        now_ms: u64,
        authenticate: impl FnOnce(&Hello) -> Result<(), ConnectReturnCode>,
    ) {
        match self.links.get_mut(&key) {
            None => {}
            Some(Link::Connecting { read, .. }) => {
                read.extend_from_slice(bytes);
                self.try_connect(key, now_ms, authenticate);
            }
            Some(Link::Live(live)) => {
                live.read.extend_from_slice(bytes);
                live.last_read_ms = now_ms;
            }
        }
        if matches!(self.links.get(&key), Some(Link::Live(_))) {
            self.forward(key, now_ms);
        }
        self.flush(now_ms);
    }

    /// The network connection ended, from either side.
    pub fn closed(&mut self, key: Key, now_ms: u64) {
        self.end(key, now_ms, true);
        self.flush(now_ms);
    }

    /// Time passed: CONNECT and keep alive timeouts, and delayed wills.
    pub fn tick(&mut self, now_ms: u64) {
        let mut expired = Vec::new();
        for (key, link) in &self.links {
            match link {
                Link::Connecting { opened_ms, .. } => {
                    if now_ms.saturating_sub(*opened_ms) > self.settings.connection_timeout_ms {
                        debug!(key, "no CONNECT in time");
                        expired.push(*key);
                    }
                }
                Link::Live(live) => {
                    if now_ms.saturating_sub(live.last_read_ms) > live.keepalive_ms {
                        info!(client_id = live.client_id, "keep alive timeout");
                        expired.push(*key);
                    }
                }
            }
        }
        for key in expired {
            self.end(key, now_ms, true);
            self.outputs.push(Output::Close(key));
        }

        let due: Vec<String> = self
            .wills
            .iter()
            .filter(|(_, will)| will.due_ms <= now_ms)
            .map(|(client_id, _)| client_id.clone())
            .collect();
        for client_id in due {
            if let Some(will) = self.wills.remove(&client_id) {
                self.publish_will(client_id, will.connection_id);
            }
        }
        self.flush(now_ms);
    }

    /// What the network should do, in order, since the last call.
    pub fn take_outputs(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.outputs)
    }

    /// Connections past CONNECT, by key and client id.
    pub fn clients(&self) -> impl Iterator<Item = (Key, &str)> {
        self.links.iter().filter_map(|(key, link)| match link {
            Link::Live(live) => Some((*key, live.client_id.as_str())),
            Link::Connecting { .. } => None,
        })
    }

    /// The most recent publishes received, oldest first.
    pub fn recent(&self) -> impl DoubleEndedIterator<Item = &Seen> {
        self.recent.iter()
    }

    /// Publishes received since the broker started.
    pub fn publishes(&self) -> u64 {
        self.publishes
    }

    /// Upstream's `mqtt_connect` and `RemoteLink::new`, once the CONNECT has
    /// arrived whole.
    fn try_connect(
        &mut self,
        key: Key,
        now_ms: u64,
        authenticate: impl FnOnce(&Hello) -> Result<(), ConnectReturnCode>,
    ) {
        let Some(Link::Connecting { read, .. }) = self.links.get_mut(&key) else {
            return;
        };
        let mut protocol = match protocol_level(read) {
            None => return,
            Some(5) => Proto::V5(V5),
            Some(_) => Proto::V4(V4),
        };
        let packet = match protocol.read_mut(read, self.settings.max_payload_size) {
            Ok(packet) => packet,
            Err(crate::protocol::Error::InsufficientBytes(_)) => return,
            Err(e) => {
                warn!(key, error = ?e, "unreadable CONNECT");
                self.drop_connecting(key);
                return;
            }
        };
        let rest = std::mem::take(read);

        let Packet::Connect(connect, props, lastwill, lastwill_props, login) = packet else {
            warn!(key, "first packet is not CONNECT");
            self.drop_connecting(key);
            return;
        };

        if let Err(code) = authenticate(&Hello {
            key,
            client_id: &connect.client_id,
            login: login.as_ref(),
        }) {
            info!(key, client_id = connect.client_id, ?code, "CONNECT refused");
            self.refuse(key, &protocol, code);
            return;
        }

        // When keep_alive feature is disabled client can live forever, which
        // is not good in distributed broker context so currenlty we don't
        // allow it.
        if connect.keep_alive == 0 {
            info!(key, "zero keep alive refused");
            self.drop_connecting(key);
            return;
        }

        if connect.client_id.is_empty() && !connect.clean_session {
            self.refuse(key, &protocol, ConnectReturnCode::ClientIdentifierNotValid);
            return;
        }

        let mut client_id = connect.client_id.clone();
        let mut assigned_client_id = None;
        if client_id.is_empty() {
            let uuid = uuid::Uuid::new_v4().simple();
            client_id = format!("rumqtt-{uuid}");
            assigned_client_id = Some(client_id.clone());
        }
        let clean_session = connect.clean_session;

        // A will still waiting from this client id's last session fires now
        // for a clean session, and is dropped for a persistent one.
        if let Some(will) = self.wills.remove(&client_id) {
            if clean_session {
                self.publish_will(client_id.clone(), will.connection_id);
            }
        }
        // A live connection with this client id is about to be dropped by
        // the router, which keeps one will per client id: this one's.
        for link in self.links.values_mut() {
            if let Link::Live(live) = link {
                if live.client_id == client_id {
                    live.superseded = true;
                }
            }
        }

        let topic_alias_max = props.as_ref().and_then(|p| p.topic_alias_max);
        let session_expiry = props.as_ref().and_then(|p| p.session_expiry_interval).unwrap_or(0);
        let delay_interval = lastwill_props.as_ref().and_then(|f| f.delay_interval).unwrap_or(0);
        // The Server delays publishing the Client's Will Message until the
        // Will Delay Interval has passed or the Session ends, whichever
        // happens first
        let will_delay_interval = session_expiry.min(delay_interval);

        let pending = LinkBuilder::new(&client_id, self.router_tx.clone())
            .clean_session(clean_session)
            .last_will(lastwill)
            .last_will_properties(lastwill_props)
            .dynamic_filters(self.settings.dynamic_filters)
            .topic_alias_max(topic_alias_max.unwrap_or(0))
            .connect();
        let finished = pending.and_then(|pending| {
            self.step();
            pending.try_finish()
        });
        let (link_tx, link_rx, notification) = match finished {
            Ok(Some(link)) => link,
            Ok(None) => {
                warn!(client_id, "router did not take the connection");
                self.drop_connecting(key);
                return;
            }
            Err(e) => {
                warn!(client_id, error = ?e, "router link failed");
                self.drop_connecting(key);
                return;
            }
        };

        let mut out = BytesMut::new();
        if let Some(mut packet) = Option::<Packet>::from(notification) {
            if let Packet::ConnAck(_ack, props) = &mut packet {
                let mut new_props = props.clone().unwrap_or_default();
                new_props.assigned_client_identifier = assigned_client_id;
                *props = Some(new_props);
                if protocol.write(packet, &mut out).is_err() {
                    self.drop_connecting(key);
                    return;
                }
            }
        }
        self.write(key, out);

        let connection_id = link_rx.id();
        let keepalive_ms = connect.keep_alive as u64 * 1500;
        self.links.insert(
            key,
            Link::Live(Box::new(Live {
                protocol,
                read: rest,
                link_tx,
                link_rx,
                connection_id,
                client_id,
                keepalive_ms,
                last_read_ms: now_ms,
                will_delay_ms: will_delay_interval as u64 * 1000,
                superseded: false,
                notifications: VecDeque::new(),
            })),
        );
    }

    /// `RemoteLink::start`'s network branch: every whole packet read goes
    /// to the router, in batches.
    fn forward(&mut self, key: Key, now_ms: u64) {
        loop {
            let Some(Link::Live(live)) = self.links.get_mut(&key) else {
                return;
            };
            let mut failed = None;
            let mut seen = Vec::new();
            let count = {
                let mut buffer = live.link_tx.buffer();
                let before = buffer.len();
                while buffer.len() < self.settings.max_connection_buffer_len {
                    match live.protocol.read_mut(&mut live.read, self.settings.max_payload_size) {
                        Ok(packet) => {
                            if let Packet::Publish(publish, _) = &packet {
                                seen.push(Seen {
                                    at_ms: now_ms,
                                    client_id: live.client_id.clone(),
                                    topic: String::from_utf8_lossy(&publish.topic).to_string(),
                                    qos: match publish.qos {
                                        QoS::AtMostOnce => 0,
                                        QoS::AtLeastOnce => 1,
                                        QoS::ExactlyOnce => 2,
                                    },
                                    retain: publish.retain,
                                    size: publish.payload.len(),
                                    preview: publish.payload.chunk()
                                        [..publish.payload.len().min(SEEN_PREVIEW)]
                                        .to_vec(),
                                });
                            }
                            buffer.push_back(packet);
                        }
                        Err(crate::protocol::Error::InsufficientBytes(_)) => break,
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                }
                buffer.len() - before
            };
            let connection_id = live.connection_id;
            for s in seen {
                self.publishes += 1;
                if self.recent.len() == SEEN_KEPT {
                    self.recent.pop_front();
                }
                self.recent.push_back(s);
            }
            if count > 0 {
                let _ = self.router_tx.send((connection_id, Event::DeviceData));
                self.step();
            }
            if let Some(e) = failed {
                warn!(key, error = ?e, "unreadable packet");
                self.end(key, now_ms, true);
                self.outputs.push(Output::Close(key));
                return;
            }
            if count == 0 {
                return;
            }
            // Write what that batch produced before reading the next, as the
            // select! loop upstream alternates.
            self.flush(now_ms);
        }
    }

    /// Forgets a connection. `network_ended` is upstream's `send_disconnect`:
    /// the router still holds it and must be told.
    fn end(&mut self, key: Key, now_ms: u64, network_ended: bool) {
        let Some(link) = self.links.remove(&key) else {
            return;
        };
        let Link::Live(live) = link else {
            return;
        };
        if network_ended {
            let _ = self.router_tx.send((live.connection_id, Event::Disconnect));
        }
        if live.superseded {
            return;
        }
        if live.will_delay_ms == 0 {
            self.publish_will(live.client_id, live.connection_id);
        } else {
            self.wills.insert(
                live.client_id,
                AwaitingWill {
                    due_ms: now_ms + live.will_delay_ms,
                    connection_id: live.connection_id,
                },
            );
        }
        self.step();
    }

    fn publish_will(&mut self, client_id: String, connection_id: ConnectionId) {
        let _ = self
            .router_tx
            .send((connection_id, Event::PublishWill((client_id, None))));
        self.step();
    }

    fn step(&mut self) {
        if let Err(e) = self.router.step() {
            warn!(error = ?e, "router step failed");
        }
    }

    /// `RemoteLink::start`'s router branch for every link: what the router
    /// left becomes packets to write, a link it unscheduled is marked ready
    /// again, and a link it dropped is closed.
    fn flush(&mut self, now_ms: u64) {
        for _ in 0..MAX_FLUSH_ROUNDS {
            let mut rescheduled = false;
            let mut dropped = Vec::new();
            let mut writes = Vec::new();
            for (key, link) in self.links.iter_mut() {
                let Link::Live(live) = link else { continue };
                let alive = live.link_rx.try_exchange(&mut live.notifications);
                let mut out = BytesMut::new();
                let mut unscheduled = false;
                for notification in live.notifications.drain(..) {
                    match Option::<Packet>::from(notification) {
                        Some(packet) => {
                            if let Err(e) = live.protocol.write(packet, &mut out) {
                                warn!(client_id = live.client_id, error = ?e, "unwritable packet");
                            }
                        }
                        None => unscheduled = true,
                    }
                }
                if !out.is_empty() {
                    writes.push((*key, out));
                }
                if !alive {
                    dropped.push(*key);
                } else if unscheduled {
                    let _ = live.link_rx.ready();
                    rescheduled = true;
                }
            }
            for (key, out) in writes {
                self.write(key, out);
            }
            for key in dropped {
                self.end(key, now_ms, false);
                self.outputs.push(Output::Close(key));
            }
            if !rescheduled {
                return;
            }
            self.step();
        }
    }

    fn write(&mut self, key: Key, out: BytesMut) {
        for chunk in out.chunks(self.settings.max_write_bytes.max(1)) {
            self.outputs.push(Output::Write(key, chunk.to_vec()));
        }
    }

    fn refuse(&mut self, key: Key, protocol: &Proto, code: ConnectReturnCode) {
        let mut out = BytesMut::new();
        let ack = ConnAck {
            session_present: false,
            code,
        };
        if protocol.write(Packet::ConnAck(ack, None), &mut out).is_ok() {
            self.write(key, out);
        }
        self.drop_connecting(key);
    }

    fn drop_connecting(&mut self, key: Key) {
        self.links.remove(&key);
        self.outputs.push(Output::Close(key));
    }
}

/// The protocol level of a CONNECT at the start of `bytes`, once enough of
/// it has arrived. Anything that is not a CONNECT says level 4, so the v4
/// codec refuses it.
fn protocol_level(bytes: &[u8]) -> Option<u8> {
    let first = *bytes.first()?;
    if first >> 4 != 1 {
        return Some(4);
    }
    // The remaining length: one to four bytes, seven bits each.
    let mut at = 1;
    loop {
        let byte = *bytes.get(at)?;
        at += 1;
        if byte & 0x80 == 0 {
            break;
        }
        if at > 4 {
            return Some(4);
        }
    }
    let name_len = u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as usize;
    bytes.get(at + 2 + name_len).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Connect, Filter, LastWill, Publish, RetainForwardRule, Subscribe};
    use bytes::Bytes;

    #[test]
    fn the_protocol_level_is_read_from_a_connect_once_it_arrives() {
        // CONNECT, remaining length 12, "MQTT", level 4.
        let v4 = [0x10, 12, 0, 4, b'M', b'Q', b'T', b'T', 4, 2, 0, 60, 0, 0];
        assert_eq!(protocol_level(&v4), Some(4));
        let mut v5 = v4;
        v5[8] = 5;
        assert_eq!(protocol_level(&v5), Some(5));
        assert_eq!(protocol_level(&v4[..7]), None);
        assert_eq!(protocol_level(&[0x30, 0]), Some(4));
    }

    fn broker() -> Broker {
        let router = RouterConfig {
            max_connections: 100,
            max_outgoing_packet_count: 200,
            max_segment_size: 100 * 1024,
            max_segment_count: 10,
            ..RouterConfig::default()
        };
        let settings = Settings {
            connection_timeout_ms: 5000,
            max_payload_size: 64 * 1024,
            max_connection_buffer_len: 100,
            dynamic_filters: true,
            max_write_bytes: 64 * 1024,
        };
        Broker::new(router, settings)
    }

    fn encode(packet: Packet) -> Vec<u8> {
        let mut out = BytesMut::new();
        V4.write(packet, &mut out).unwrap();
        out.to_vec()
    }

    fn connect(client_id: &str, clean: bool, will: Option<LastWill>) -> Vec<u8> {
        let connect = Connect { keep_alive: 10, client_id: client_id.into(), clean_session: clean };
        let login = Login { username: "u".into(), password: "p".into() };
        encode(Packet::Connect(connect, None, will, None, Some(login)))
    }

    fn subscribe(filter: &str, qos: QoS) -> Vec<u8> {
        let filters = vec![Filter {
            path: filter.into(),
            qos,
            nolocal: false,
            preserve_retain: false,
            retain_forward_rule: RetainForwardRule::OnEverySubscribe,
        }];
        encode(Packet::Subscribe(Subscribe { pkid: 1, filters }, None))
    }

    fn publish(topic: &str, payload: &str, qos: QoS, retain: bool) -> Vec<u8> {
        let mut publish = Publish::new(topic.to_string(), payload.to_string(), retain);
        publish.qos = qos;
        publish.pkid = if qos == QoS::AtMostOnce { 0 } else { 7 };
        encode(Packet::Publish(publish, None))
    }

    /// Every packet written to `key` since the last call.
    fn packets(broker: &mut Broker, key: Key) -> Vec<Packet> {
        let mut bytes = BytesMut::new();
        for output in broker.take_outputs() {
            if let Output::Write(k, b) = output {
                if k == key {
                    bytes.extend_from_slice(&b);
                }
            }
        }
        let mut out = Vec::new();
        while let Ok(packet) = V4.read_mut(&mut bytes, 1 << 20) {
            out.push(packet);
        }
        out
    }

    fn allow(_: &Hello) -> Result<(), ConnectReturnCode> {
        Ok(())
    }

    fn connected(broker: &mut Broker, key: Key, client_id: &str, clean: bool, will: Option<LastWill>) {
        broker.open(key, 0);
        broker.read(key, &connect(client_id, clean, will), 0, allow);
        let acks = packets(broker, key);
        assert!(
            matches!(acks.first(), Some(Packet::ConnAck(ack, _)) if ack.code == ConnectReturnCode::Success),
            "{acks:?}"
        );
    }

    fn published_topics(packets: &[Packet]) -> Vec<(String, String)> {
        packets
            .iter()
            .filter_map(|p| match p {
                Packet::Publish(p, _) => Some((
                    String::from_utf8_lossy(&p.topic).to_string(),
                    String::from_utf8_lossy(&p.payload).to_string(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_publish_reaches_a_subscriber_at_qos_1_and_is_acked() {
        let mut b = broker();
        connected(&mut b, 1, "sub", true, None);
        connected(&mut b, 2, "pub", true, None);
        b.read(1, &subscribe("a/+", QoS::AtLeastOnce), 1, allow);
        assert!(matches!(packets(&mut b, 1).first(), Some(Packet::SubAck(..))));

        b.read(2, &publish("a/b", "hi", QoS::AtLeastOnce, false), 2, allow);
        let outputs = b.take_outputs();
        let to = |key| {
            let mut bytes = BytesMut::new();
            for o in &outputs {
                if let Output::Write(k, data) = o {
                    if *k == key {
                        bytes.extend_from_slice(data);
                    }
                }
            }
            let mut v = Vec::new();
            while let Ok(p) = V4.read_mut(&mut bytes, 1 << 20) {
                v.push(p);
            }
            v
        };
        assert!(to(2).iter().any(|p| matches!(p, Packet::PubAck(ack, _) if ack.pkid == 7)), "{:?}", to(2));
        assert_eq!(published_topics(&to(1)), vec![("a/b".into(), "hi".into())]);
        assert_eq!(b.publishes(), 1);
    }

    #[test]
    fn a_refused_connect_gets_its_code_and_is_closed() {
        let mut b = broker();
        b.open(1, 0);
        b.read(1, &connect("x", true, None), 0, |_| Err(ConnectReturnCode::BadUserNamePassword));
        let outputs = b.take_outputs();
        assert_eq!(outputs.last(), Some(&Output::Close(1)));
        let Output::Write(_, bytes) = &outputs[0] else { panic!("{outputs:?}") };
        let packet = V4.read_mut(&mut BytesMut::from(&bytes[..]), 1024).unwrap();
        assert!(matches!(packet, Packet::ConnAck(ack, _) if ack.code == ConnectReturnCode::BadUserNamePassword));
        assert_eq!(b.clients().count(), 0);
    }

    #[test]
    fn a_retained_message_reaches_a_later_subscriber() {
        let mut b = broker();
        connected(&mut b, 1, "pub", true, None);
        b.read(1, &publish("r/t", "kept", QoS::AtMostOnce, true), 1, allow);
        connected(&mut b, 2, "sub", true, None);
        b.read(2, &subscribe("r/#", QoS::AtMostOnce), 2, allow);
        assert_eq!(published_topics(&packets(&mut b, 2)), vec![("r/t".into(), "kept".into())]);
    }

    #[test]
    fn a_silent_client_is_closed_after_its_keep_alive_and_its_will_fires() {
        let mut b = broker();
        connected(&mut b, 1, "watcher", true, None);
        b.read(1, &subscribe("wills/#", QoS::AtMostOnce), 0, allow);
        packets(&mut b, 1);
        let will = LastWill { topic: Bytes::from("wills/dev"), message: Bytes::from("gone"), qos: QoS::AtMostOnce, retain: false };
        connected(&mut b, 2, "dev", true, Some(will));

        // Keep alive 10 s: closed after 15 s of silence, not before.
        b.read(1, &encode(Packet::PingReq(crate::protocol::PingReq)), 14_000, allow);
        b.tick(14_000);
        assert!(!b.take_outputs().contains(&Output::Close(2)));
        b.tick(15_001);
        let outputs = b.take_outputs();
        assert!(outputs.contains(&Output::Close(2)), "{outputs:?}");
        let mut bytes = BytesMut::new();
        for o in &outputs {
            if let Output::Write(1, data) = o {
                bytes.extend_from_slice(data);
            }
        }
        let mut seen = Vec::new();
        while let Ok(p) = V4.read_mut(&mut bytes, 1 << 20) {
            seen.push(p);
        }
        assert_eq!(published_topics(&seen), vec![("wills/dev".into(), "gone".into())]);
    }

    #[test]
    fn a_persistent_session_gets_what_was_published_while_it_was_away() {
        let mut b = broker();
        connected(&mut b, 1, "keeper", false, None);
        b.read(1, &subscribe("q/#", QoS::AtLeastOnce), 0, allow);
        packets(&mut b, 1);
        b.closed(1, 1);

        connected(&mut b, 2, "pub", true, None);
        b.read(2, &publish("q/1", "while away", QoS::AtLeastOnce, false), 2, allow);
        b.take_outputs();

        b.open(3, 3);
        b.read(3, &connect("keeper", false, None), 3, allow);
        let got = packets(&mut b, 3);
        assert!(matches!(got.first(), Some(Packet::ConnAck(ack, _)) if ack.session_present), "{got:?}");
        assert_eq!(published_topics(&got), vec![("q/1".into(), "while away".into())]);
    }
}
