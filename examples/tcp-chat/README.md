# tcp-chat

A chat over raw TCP that you use with `nc`, and a plain HTML page that
shows the last 50 lines. The smallest app on a TCP port.

## What it shows

- A TCP port: `[[socket]] protocol = "tcp"`, port 7777. Each connection
  arrives as `connect`, then a `message` per read, then `close`.
- Framing. A TCP read is not a line: one read can hold half a line or three
  lines. The handler adds each read to a buffer and handles each line that
  ends in `\n`. A line is at most 4 KB; a client that sends a longer one is
  told so and closed.
- State per connection without resident mode. Each event runs in a fresh
  instance, so the partial line and the nickname live in the connection's
  state (`state-get` and `state-set`).
- A device token as the first line, checked with `auth.check-token`. The
  token's label is the first nickname (or `guest` when the label is not a
  valid one). A wrong token closes the connection.
- A topic: every signed-in connection joins `room`, and a message is
  published to it.
- `connections.remote` to tell a live connection from one that is gone, so
  `/who` drops the rows the server left behind when it stopped.
- Lines stored in SQLite and read by a page with no build step
  (`public/index.html`) through `/api/lines`.

## Start it

```sh
toolsite init my-chat --example tcp-chat
cd my-chat
toolsite deploy
```

Grant the people who may open the page.

## Give it a TCP port

A declared port opens nothing until the site's owner maps it to the app:

```
TOOLSITE_PORTS=7777=my-chat
```

Set it in the server's environment and restart the server once. With
Docker, also publish the port: `-p 7777:7777`.

**On Railway** a service has one public HTTP port. Add a TCP proxy (the
service's Settings, Networking, TCP Proxy, container port 7777). Railway
gives back a `host:port`; clients connect there.

## Try it

Mint a token for each client on the app's **Connections** tab on `/admin`,
or over MCP with `app_device_tokens("my-chat", "create", "ana")`. The token
is shown once. Then, in two terminals:

```
$ nc HOST 7777
Send: token <device-token>
token tsv_...
Welcome, ana. The commands are /nick name, /who and /quit.
* ana joined
hello
<ana> hello
/nick ana-desk
* ana is now ana-desk
/who
here: ana-desk, bo
/quit
bye
```

Each message goes to everyone in the room, the sender too. Open the app in
a browser to see the last 50 lines.

## Protocol

From the client, one line each, ended by `\n` (`\r\n` works too):

- `token <device-token>`: must be the first line.
- `/nick <name>`: 1 to 20 letters, digits, `-` or `_`.
- `/who`: who is here.
- `/quit`: says bye and closes.
- Anything else: a message.

From the server: `<nick> text` for a message, `* ...` for joins, leaves
and renames, `here: ...` for `/who`, and `error: ...` when something is
refused. There is no TLS on the port: put a TLS proxy in front of it if
clients cross the internet.

## Files

- `handler/src/lib.rs`: framing, sign-in, the commands and `/api/lines`.
- `public/index.html`: the page.
- `migrations/001_initial.sql`: lines and who is here.
- `toolsite.toml`: the gate and the port.
