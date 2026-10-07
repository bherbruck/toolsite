# mqtt-broker

An MQTT broker that runs as a toolsite app, for devices on a TCP port and
browsers on a WebSocket. It is rumqttd, forked so that its router, sessions
and QoS state machines stay as upstream wrote them, with toolsite's
connection events in place of its network layer.

## What it shows

- Resident mode: `[resident]` in toolsite.toml gives the app one long-lived
  instance, so the broker's sessions, subscriptions, retained messages and
  queued messages stay in memory between events.
- A TCP port (`[[socket]] protocol = "tcp"`, port 1883) and a WebSocket
  (`[[socket]] path = "/mqtt"`) on one handler. Both arrive as the same
  `connect`, `message` and `close` events, and both reach the same broker,
  so a browser and a device talk to each other.
- A WebSocket subprotocol: `subprotocols = ["mqtt"]`. MQTT.js and every
  browser MQTT client ask for `mqtt` and give up on a socket that does not
  agree.
- Device tokens as MQTT passwords, checked with `auth.check-token` at
  CONNECT and again every 10 seconds. A wrong token gets CONNACK code 4
  (bad user name or password), no password gets 5 (not authorized), and a
  revoked token takes its device off within 10 seconds.
- A signed-in person on the WebSocket needs no token: the app's gate
  (`restricted`) already decided they may connect.
- `on-tick` for keep alive, CONNECT timeouts and delayed wills.
- Status from a fresh instance: requests do not run on the resident
  instance, so the broker saves a summary row on tick and `/api/status`
  reads it.
- A vendored fork with its changes listed: [FORK.md](FORK.md).

MQTT 3.1.1 and MQTT 5 both work on either transport: QoS 0, 1 and 2,
retained messages, wills, persistent sessions (while the instance runs)
and keep alive. Topics are this app's alone: each app is its own broker.

## Start it

```sh
toolsite init my-broker --example mqtt-broker
cd my-broker
toolsite deploy
```

Grant the people who may use the page and the WebSocket. Open the app: the
page connects over MQTT itself, so you can subscribe to `#` and publish to
see messages go round.

## Give it a TCP port

A declared port opens nothing until the site's owner maps it to the app:

```
TOOLSITE_PORTS=1883=my-broker
```

Set it in the server's environment and restart the server once. The port
number in the mapping is the one in `toolsite.toml`.

**On Railway** a service has one public HTTP port. Add a TCP proxy for the
broker: the service's Settings, Networking, TCP Proxy, with the container
port 1883. Railway gives back a `host:port` (for example
`shuttle.proxy.rlwy.net:41234`); devices connect there. Without a TCP proxy
only browsers can reach the broker, over the WebSocket.

## Connect a device

Mint a token for each device on the app's **Connections** tab on `/admin`,
or over MCP with `app_device_tokens("my-broker", "create", "pump-7")`. The
token is shown once. The device uses it as its MQTT password; the user name
is free, and the broker shows the token's label.

```sh
mosquitto_sub -h HOST -p PORT -u pump-7 -P tsv_... -t 'plant/#' -v
mosquitto_pub -h HOST -p PORT -u pump-7 -P tsv_... -t plant/pump-7/temp -m 21.5
```

TLS is not offered on the port: put a TLS proxy in front of it if devices
cross the internet, or use the WebSocket, which is HTTPS on a deployed site.

## Connect a browser

From a page on the same site, as someone with access to the app:

```js
import mqtt from 'mqtt'
const url = new URL('mqtt', document.baseURI)   // https://site/p/my-broker/mqtt
url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
const client = mqtt.connect(url.href, { protocolVersion: 4 })
client.on('connect', () => client.subscribe('plant/#'))
```

No user name or password: the person's toolsite session is the credential.
A page on another site cannot open the socket. A program that is not a
browser can still use the WebSocket with a device token as its password,
when the app's gate admits it.

## Limits

- One broker per app, in one server process. Everything in it is lost on a
  restart, a redeploy or a server restart, persistent sessions included;
  clients reconnect and subscribe again.
- The broker has `memory_mb = 128`. Each subscription filter keeps at most
  1 MB of messages (4 segments of 256 KB), and a packet may be at most
  128 KB. A client that sends more is disconnected.
- toolsite's limits on connections apply: per app, per IP address, and
  messages per app per second (`TOOLSITE_*` in the server's README). The
  broker sends each connection one message per event with everything it
  has for it, which keeps fan-out inside the rate.
- Client ids are shared by every client of the app. A client that connects
  with an id already in use takes over its session, as MQTT says.
- rumqttd forwards a message at the subscriber's QoS, and refuses a keep
  alive of 0. See FORK.md for the rest of upstream's behavior.

## Where to look

- `handler/src/lib.rs`: the glue between connection events and the broker,
  the token check and the saved status.
- `rumqttd/src/step.rs`: the broker's network side as a step function.
- `rumqttd/src/router/`: upstream's router, unchanged but for `step`.
- `src/App.tsx`: the page, an MQTT.js client plus the saved status.
- `tests/examples.rs` in the toolsite repository drives all of this with a
  real MQTT client over TCP and the WebSocket.
