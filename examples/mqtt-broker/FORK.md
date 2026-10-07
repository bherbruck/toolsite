# The rumqttd fork

`rumqttd/` is rumqttd from [bytebeamio/rumqtt](https://github.com/bytebeamio/rumqtt)
at the tag **`rumqttd-0.20.0`** (commit `c03ba8bb`), Apache-2.0. Its LICENSE
and a NOTICE of what changed are beside it.

The goal is to keep rumqttd's MQTT behavior exactly as upstream wrote it:
the router, the subscription and retained logs, sessions, QoS 1 and 2 state
and the v4 and v5 codecs are unchanged. Only the network layer is replaced,
because in toolsite the platform holds the sockets and the app gets
connection events, and because a wasm32-wasip2 guest has no threads.

## What changed, and why

| Change | Where | Why |
| --- | --- | --- |
| Removed the server: listeners, TLS, WebSocket, the per-connection tokio tasks | `src/server/`, `src/main.rs` | toolsite accepts TCP and WebSocket connections and hands the bytes to the handler. A guest has no sockets. |
| Removed `RemoteLink` and `Network` | `src/link/remote.rs`, `src/link/network.rs` | They read and write a socket inside an async task. `src/step.rs` does their work from events. |
| Removed the bridge, the console, the metrics timer, the replicator | `src/link/bridge.rs`, `console.rs`, `timer.rs`, `src/replicator/` | They open sockets or HTTP servers, or spawn timers. The replicator was not built upstream either. |
| Removed tokio, rustls, native-tls, tungstenite, axum, metrics, prometheus, clap, config, tracing-subscriber, subtle | `Cargo.toml` | Only the removed parts used them, and several do not build for wasm32-wasip2. The features that chose them went too. `validate-tenant-prefix` no longer implies `verify-client-cert`, which took tenant ids from client certificates. |
| Removed the `Elapsed` error variants | `src/link/local.rs`, `alerts.rs`, `meters.rs` | They wrapped tokio's timeout error, which nothing raises now. |
| Removed the reload handle from `ConsoleSettings` | `src/lib.rs` | It was tracing-subscriber's, for the removed console. |
| Allowed `dead_code` and `mismatched_lifetime_syntaxes` crate-wide | `src/lib.rs` | Upstream items that only the removed network layer called stay in place, so the diff stays small; without this every build of the handler prints their warnings. |
| `Router::spawn`, `run` and `run_inner` replaced by `Router::step` | `src/router/routing.rs` | `run_inner` blocks on the event channel when nothing is ready, on its own thread. `step` does the same work (drain events, serve the ready queue) until neither has any left, and returns instead of blocking. It runs at most 64 rounds per call: a shared subscription can keep a connection ready while it waits for its turn, which the threaded loop spun on. |
| `Router::link` made public | `src/router/routing.rs` | The owner of the router needs the event sender without spawning. |
| Added `LinkBuilder::connect` and `PendingLink::try_finish` | `src/link/local.rs` | `build` sends the connect event and then blocks for the router's answer. Split in two, the caller steps the router in between. `try_finish` answers `None` when the router never answered, which is how upstream's router turns a connection away. |
| Added `LinkRx::try_exchange` | `src/link/local.rs` | `exchange` waits for a wake-up. This takes what the router left without waiting, and says when the router has dropped the link. |
| Added `step::Broker` | `src/step.rs` | The network side of upstream's broker, as calls: `open`, `read(bytes)`, `closed`, `tick`, then `take_outputs` for what to write and close. It follows `mqtt_connect`, `RemoteLink` and `server::broker::remote` line for line where it can; the differences are below. |

## How the handler drives it

`handler/src/lib.rs` holds one `step::Broker` in the resident instance's
memory. A connection's `connect` event opens it, each `message` event is
`read`, `close` is `closed`, and `on-tick` is `tick`. After each call the
handler sends what the broker wrote with `connections.send` and closes what
it closed. Toolsite runs every connection event of a resident app on one
instance, one at a time and in order, which is the ordering the router's
own thread gave upstream.

## Differences from upstream behavior

Made on purpose, all in `src/step.rs`:

- **A refused CONNECT gets a CONNACK.** Upstream closes the connection
  without one when authentication fails. Here the authenticator returns a
  code (bad user name or password, not authorized) and the client gets it.
- **One port speaks v4 and v5.** Upstream picks the codec by listener. Here
  it is picked per connection from the CONNECT's protocol level: 5 is v5,
  anything else goes to the v4 codec, which refuses levels it does not
  speak.
- **Keep alive is checked on tick.** Upstream times out a read after one
  and a half keep alive periods. Here `tick` closes a connection silent for
  that long, so the check is as fine as `tick_ms` (1 second).
- **A will delay is checked on tick** too, where upstream used a timer.

Upstream behavior kept as it is, though it differs from the MQTT
specification or from other brokers:

- A message is forwarded at the subscription's QoS, not at the lower of the
  publish's and the subscription's.
- A keep alive of 0 is refused (upstream: "not good in distributed broker
  context").
- Acknowledgements must arrive in order; at most 100 messages are in flight
  per connection.
- The router keeps one will per client id, so when a client id is taken
  over, the new connection's will replaces the old one's.
- A persistent session (`clean_session = false`) lives in the router's
  memory, so it lasts as long as the resident instance. A restart, a
  redeploy or a server restart ends every session, as restarting upstream's
  broker does.

## Nothing that needed a rewrite

The single-threaded step model did not need any change to the router's
logic: `events` and `consume` are called exactly as `run_inner` called them.
The upstream unit tests (`cd rumqttd && cargo test`) still pass, and
`src/step.rs` adds tests of the step loop itself.

## Updating from upstream

Copy `rumqttd/` from a newer tag, delete the files listed above, and carry
over the changes in `src/router/routing.rs`, `src/link/local.rs`,
`src/lib.rs` and `Cargo.toml`, and `src/step.rs`. Then run the fork's own
tests and `scripts/build-examples.sh mqtt-broker`, and `cargo test --test
examples mqtt` at the repository root.
