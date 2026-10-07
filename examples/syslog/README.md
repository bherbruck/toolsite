# syslog

A syslog receiver: devices send to a UDP port, each message is stored in
SQLite, and a page shows the log with filters and a live tail.

## What it shows

- A UDP port: `[[socket]] protocol = "udp"`, port 5514. Each datagram
  arrives as a `message`. The first datagram from a new address comes after
  a `connect`, where the app may refuse the address.
- Parsing RFC 5424 and RFC 3164 by hand (`handler/src/parse.rs`), with no
  dependency. A datagram that is neither is stored whole, with severity
  unknown.
- A source allow list in a setting, `ALLOWED_SOURCES`, checked against
  `connections.remote` at `connect`.
- A WebSocket and a UDP port on one handler. Each stored line is published
  to the topic `log`, and the page's WebSocket `/tail` is on that topic.
- A scheduled job, `prune`, that deletes rows older than 30 days.
- A Vite, React and Tailwind page with filters by severity and host.

## Start it

```sh
toolsite init my-syslog --example syslog
cd my-syslog
toolsite deploy
```

Grant the people who may open the page.

## Give it a UDP port

A declared port opens nothing until the site's owner maps it to the app:

```
TOOLSITE_PORTS=5514/udp=my-syslog
```

Set it in the server's environment and restart the server once. With
Docker, also publish the port: `-p 5514:5514/udp`.

**UDP syslog has no authentication.** Anyone who can reach the port can
write to the log, and a UDP source address can be forged. Map this port
only on a private network: a LAN, a VPN, or a host firewall that admits
only your devices. Do not open it to the internet.

To limit the senders further, set `ALLOWED_SOURCES` to their IP addresses,
comma-separated, for example `10.0.0.5, 10.0.0.6`. Empty or not set means
every source. Set it with `toolsite secret my-syslog --link`, or on the
app's Settings tab on `/admin`. The list is checked when an address sends
its first datagram, so a change reaches an address already sending only
after it was quiet for the UDP idle timeout.

**Railway does not route UDP** from outside its private network. On
Railway, only other services in the same project can send to the port.
Use a host that routes UDP for devices outside it.

## Try it

With `logger` from util-linux:

```sh
logger --server HOST --port 5514 --udp "disk 91% full"
logger --server HOST --port 5514 --udp --rfc3164 -p local0.err -t backup "backup failed"
```

Or with `nc`:

```sh
echo '<11>1 2026-10-07T09:05:00Z pump-7 modbusd 812 - - lost contact' | nc -u -w1 HOST 5514
echo '<14>Oct  7 09:05:00 router-1 dnsmasq[33]: DHCPACK 10.0.0.9' | nc -u -w1 HOST 5514
```

Open the app: the lines are there, and new ones appear as they arrive.

To send a device's log here, point its remote syslog at `HOST:5514` over
UDP. For rsyslog: `*.* @HOST:5514`.

## API

- `GET api/logs?severity=4&host=pump-7&before=123`: the newest 200 rows,
  newest first. `severity` keeps that severity and worse (0 is emergency,
  7 is debug); `before` pages back by id.
- `GET api/hosts`: every host seen.
- WebSocket `tail`: each new row as JSON, as it is stored.

## Files

- `handler/src/lib.rs`: the allow list, storing and publishing, the API and
  the prune job.
- `handler/src/parse.rs`: RFC 5424 and RFC 3164.
- `src/App.tsx`: the page and the live tail.
- `migrations/001_initial.sql`: the `logs` table.
- `toolsite.toml`: the gate, the port, the socket and the job.
