# Building on toolsite

How this platform works, for whoever is building on it. Fetch it any time:

    curl <server>/guide

Notes stored with an app (`app_notes`) are about *that one app*: what it is,
where things live, why it was built that way, what is half-finished. Read the
ones belonging to the app you are changing.

They are not the place for platform behaviour or for friction you hit getting
something working. Written there it goes stale the moment the platform
changes, and the next session reads a fixed bug as a live one. That is this
document, and this document stays current.

## Check your environment first

Before you promise an app, find out what you can run and reach:

    npm --version; cargo --version; curl -sI <server>/guide

A React or wasm build needs npm or cargo. Uploading needs a route to this
host, or the inline upload tools when there is none. Some sandboxes have
neither; ChatGPT's regular chat is one today. Without a build tool you can
still publish plain HTML with `push_page`, run SQL, and manage accounts,
access, exports, repositories and notes. Say what you have before you start.

## Choosing how to build it

If the thing has state, forms, or more than one screen, build it as a real
front-end project and upload the output. If it is a static document or a
readout, one HTML file is right.

One hand-written `index.html` is cheaper only for the first version. After
that every change is string replacement against markup with no build, no
components and no types, which is how a session ends up rewriting an entire
file to move a button. The scaffold removes all of it:

    toolsite init myapp --react --handler
    cd myapp && toolsite deploy

That writes Vite + React + Tailwind with `base` already set to `/p/myapp/`,
which is the setting that otherwise produces a blank page. `deploy` runs the
install and the build, so those two commands are the whole flow.

Without the CLI, the same thing by hand:

    npm create vite@latest myapp -- --template react-ts
    cd myapp && npm install && npm install -D tailwindcss @tailwindcss/vite
    # vite.config.ts: add base: '/p/myapp/' and tailwindcss() to plugins
    # src/index.css: replace everything with  @import "tailwindcss";
    npm run build && tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle&spa'

A site may serve each app from a host of its own, `<label>.apps.<domain>`
(subdomain mode), still at the path `/p/<app>/`. The same build works on
both kinds of site if it never names the site's address: use relative URLs,
or `window.location.origin`, for its own API, sockets and connector link.
`create_upload` and `list_pages` print the app's real URL; the main site's
`/p/<app>/` redirects there. On such a site a POST from another origin's
page is refused, so an app cannot take form posts from pages elsewhere.

## Start from an example

Working apps live at `<server>/examples`, each with a README that says what
it shows and where to look. Start from one, or copy the part you need:

    toolsite init <name> --example kitchen-sink
    curl -fsS '<server>/examples/kitchen-sink.tar.gz?slug=<name>' | tar -xz

Either way the slug, the base path and the package names are set for
`<name>`, so it deploys as it is.

- `kitchen-sink`: every capability, one screen each. SQL and migrations,
  row-level policies, files, settings, outbound fetch, identity and roles,
  a scheduled job, route rules, app tools.
- `orders`: orders with lines, totals on the server, status rules in the
  handler, an approval role, four app tools.
- `static-report`: one HTML file, no build.
- `blob-gallery`: browser uploads straight to storage, thumbnails, serving
  with `x-toolsite-blob`.
- `inventory-policies`: tables and policies only, with the `as_user` steps
  that prove them.
- `live-board`: a shared board over a declared socket. Changes by request,
  then `publish`; the board as the first message; presence; a nudge to one
  person through `user:<id>`; a front end that reconnects.
- `mqtt-broker`: an MQTT broker (a fork of rumqttd) running resident. A TCP
  port and a WebSocket with the subprotocol `mqtt`, device tokens as MQTT
  passwords, `on_tick` for keep alive, status saved for requests to read.
- `tcp-chat`: a line chat over a TCP port, for `nc`. A device token as the
  first line, framing with a partial line kept in per-connection state, a
  4 KB line cap, a topic, and a plain HTML page.
- `duckdb-report`: heavy analytics in the browser. A job writes a Parquet
  file with the streaming writer, SQLite keeps one row per file, the handler
  serves the file to people with a grant, and the page queries it with
  DuckDB-wasm and draws a chart.
- `syslog`: a UDP receiver. RFC 5424 and RFC 3164 parsed by hand into
  SQLite, a source allow list in a setting checked at `connect`, a live tail
  over a WebSocket, and a job that prunes old rows.

## Publishing

An upload URL comes from `create_upload`. It is a capability scoped to one
slug, good for 15 minutes, and it takes flags:

| Flag | Body |
|---|---|
| *(none)* | one HTML page, published at the slug |
| `?bundle` | gzipped tar of a built site; `&spa` serves index.html for unknown paths |
| `?handler` | a wasm component, rejected here if it is not one |
| `?migrations` | gzipped tar of numbered `.sql` files |
| `?manifest` | `toolsite.toml` |
| `?icon` | an image |
| `?source` | gzipped tar of the project; also `GET` to fetch it back |

Every app gets a favicon from its icon unless the bundle ships its own
`favicon.ico`, `favicon.svg` or links one in its `<head>`.

Any other flag is refused rather than guessed at. Order matters: migrations
and the manifest first, so an app is never briefly live without its tables or
its gate.

If curl cannot reach this host from your sandbox, send the file through the
tools instead: `upload_begin(slug, kind)` opens an upload for the same kinds
(`page`, `bundle`, `handler`, `migrations`, `manifest`, `source`, `icon`,
`blob`), `upload_chunk(id, index, data)` carries it in standard base64 chunks
of at most 768 KB decoded, in any order, and `upload_finish(id, chunks)`
stores it with the same rules and the same reply as the upload URL. Bytes
through a tool call cost tokens, so this is the fallback, not the default.
`push_page` and `push_app` remain for plain HTML.

## What a visitor can reach

Only what the bundle contained. The project, the notes, the settings, the
schema and the metadata all live beside the app and are served by nothing.

Requests resolve in a fixed order:

1. `/p/<app>/api/...`: the app's handler, always. The prefix is reserved.
2. an exact file from the bundle: static, no wasm runs.
3. no file but a handler exists: the handler, so it can render its own routes.
4. no file, no handler, `spa` set: the app's `index.html`.
5. otherwise 404.

A handler sees the path relative to its app **with `/api` still attached**, so
strip that prefix yourself.

## Handlers

A wasm component built for `wasm32-wasip2` against `<server>/wit/toolsite.wit`.
Start from `<server>/scaffold/<app>`, which is a crate that builds unmodified.

It gets five capabilities and nothing else:

- `db.query`: this app's own SQLite. Parameters are bound; there is no
  string-building entry point.
- `blobs`: this app's own files. See Files, below.
- `identity.current-user` / `current-role`: established by the host from a
  verified session. A guest cannot forge either.
- `secrets.get`: settings the owner entered. Never in the bundle.
- `fetch.send`: only hosts the app declared in `allow_http`.

No filesystem. No environment. No sockets beyond that allowlist. **And no
clock**: `std::time` will not link. Take timestamps from SQLite instead:

    select cast(strftime('%s','now') as integer)

Every request runs in a fresh instance with a fuel ceiling, a memory cap and a
wall-clock deadline. State must live in the database. A global does not
survive the request that set it.

Defaults: a request gets 5 s, 64 MB and 1,000 rows a query; a job 60 s and
128 MB. Heavy work asks for more in toolsite.toml, up to the site's ceilings
(by default 60 s a request, 900 s a job, 50,000 rows, 1024 MB); a value past
a ceiling is clamped and the deploy says so:

```toml
[limits]
request_seconds = 20
job_seconds = 600
query_rows = 10000
memory_mb = 512        # also request_fuel, job_fuel
```

Many writes: `db::batch(&[Statement { sql, params }, ...])` runs them in one
transaction, all or nothing, and returns rows changed per statement. Up to
10,000 statements and 4 MB of SQL; rows a select returns are discarded.
`db::batch_scoped` does the same through the app's declared access.

Work in stages: `jobs::run("name")` starts a declared job now, in the
background, with no user. Called while that job runs (from the job itself
too) it queues one more run right after, so a job chains its stages by
asking for itself at the end of each: `started` or `queued`.

The host sets `x-toolsite-scheduled` on a job run and `x-toolsite-tool` on an
app tool call. Client copies of any `x-toolsite-*` header are stripped, so
each means what it says.

## Files

Uploads, images, exports, datasets: anything too large or too opaque for a
row is a blob. One namespace per app, keyed like a path (`photos/cat.jpg`),
with the same rules as a bundle path: no segment may start with `.`, so
`..` is refused before anything touches storage.

The handler decides; the platform moves the bytes. A request body into a
handler is capped at 8 MB and a blob may be gigabytes, so neither direction
goes through guest memory:

- **Taking a file from a browser.** Call `blobs::upload_url(key, max_bytes)`
  and hand the URL to the page. The browser `PUT`s the file there with its
  content type; the URL works once and expires in fifteen minutes. Decide who
  gets a URL the way you decide anything else. The URL is the credential.
- **Sending one.** Answer with the header `x-toolsite-blob: <key>` and an
  empty body. The platform streams the file in its place, with the stored
  content type unless you set one, and keeps your other headers, so
  `content-disposition` and `cache-control` are yours to add. Gate it however
  the route is gated: answering is the permission. A `Range` request on a
  200 gets just those bytes (206), so DuckDB-WASM reads only the parts of a
  Parquet file it needs; set an `etag` to make `If-Range` work, or
  `accept-ranges: none` to always send the whole file.
- **Small things from inside.** `put`, `get`, `stat`, `list(prefix)` and
  `delete`. `get` refuses anything over 16 MB rather than truncating it;
  serve those with the header.
- **Made by the handler or a job.** For a file too big to build in memory, a
  Parquet file or a long export, write it in pieces:

  ```rust
  let handle = blobs::writer_open("reports/sales.parquet", "application/vnd.apache.parquet")?;
  for chunk in chunks {
      blobs::writer_append(handle, &chunk)?;
  }
  let entry = blobs::writer_finish(handle)?; // or blobs::writer_abort(handle)
  ```

  Nothing shows under the key until `writer_finish`, which puts the whole
  file there at once. A handle belongs to the call that opened it: when the
  call returns or traps, any writer not finished is thrown away with what it
  wrote. A call may hold four at once.

Seeding from a shell: `curl -f -T file '<upload-url>?blob=<key>'`, up to 64 MB
per PUT, typed by the key's extension.

The ceiling per file is the deployment's, a few GB by default. Where the
bytes live, the volume or a bucket, is not the app's concern.

## Heavy analytics: files plus DuckDB in the browser

SQLite is where an app keeps its configuration and metadata. It is not
where millions of rows of history should be scanned on every request, and a
handler has five seconds. For reports, keep the data in files and let the
browser do the work:

1. **A job writes the data as a file.** Parquet, written with the streaming
   writer a row group at a time, into the app's files. SQLite keeps one row
   per file: key, row count, size, when it was made.
2. **The page asks the handler for it.** `GET /p/<app>/api/files/<key>`. The
   handler checks who is asking and that the key is one it means to serve,
   then answers `x-toolsite-blob: <key>` with an empty body. The platform
   sends the whole file.
3. **The page queries it locally.** Read the response as an `ArrayBuffer`,
   register it with DuckDB-wasm, `db.registerFileBuffer('sales.parquet',
   new Uint8Array(buffer))`, and run SQL against `'sales.parquet'` in the tab.

DuckDB never fetches a URL itself: no httpfs, no `read_parquet('<url>')`, no
`ATTACH '<url>'`. Every byte comes through the handler, which is where access
is decided. Bundle DuckDB with the app (`@duckdb/duckdb-wasm`, its wasm and
worker imported with `?url`) rather than loading it from a CDN.

**A file has no row-level policy.** Whoever is given the file has every row
in it. When people should see different rows, write one file per person or
per role (`reports/role-north/sales.parquet`, `reports/user-<id>.parquet`)
and have the handler serve only the key that belongs to the caller.

**A `.duckdb` file works the same way.** Build it locally, or in the browser
(DuckDB-wasm writes one), and upload it with `blobs::upload_url` or
`curl -f -T report.duckdb '<upload-url>?blob=reports/report.duckdb'`. Deliver
it through the handler like the Parquet file, register the buffer as
`report.duckdb`, then `ATTACH 'report.duckdb' AS report (READ_ONLY)`.

**Mind the job's budget.** A job runs with two billion instructions and a
minute. Encoding Parquet in wasm costs a few thousand instructions a row, so
one run writes a few hundred thousand rows. More than that is several files,
one per month say, each made by its own run.

The `duckdb-report` example is all of this: the job, the table, the route
and a React page with DuckDB and a chart.

## Schema

Numbered `.sql` files, applied once each, in order, in a transaction, before
the app answers anything. Add a file for the next change; never edit one that
has run, because a database that already applied it will not apply it again.

Do not write `create table if not exists` in a handler. Once the table exists
that statement does nothing, so a column added later never arrives and the
failure surfaces as "no such column" against real rows.

## toolsite.toml

What the app needs, beside its source, so a later session sees the intent
rather than a list of commands someone once ran.

```toml
slug = "myapp"
spa  = false
gate = "public"            # or authenticated, restricted
icon = "🧺"
allow_http = ["api.example.com"]
roles = ["viewer", "editor"]  # what the handler checks; a hint for whoever grants

[[route]]                  # note the singular; unknown keys are refused
path = "/admin"
gate = "restricted"

[[job]]                    # six cron fields, seconds first; */10 * * * * * is every 10 s
name = "refresh"
schedule = "0 */5 * * * *"
path = "/api/refresh"

[limits]                   # optional; clamped to the site's ceilings
job_seconds = 600
```

Routes, jobs and tools are replaced wholesale, so deleting a line removes the
thing. Anything the file does not mention is left alone. Tools: see App tools.

## App tools

An app can offer MCP tools. Each tool is a handler route you already write;
the platform signs the person in, decides whether they may open the app (its
access, its route rules, their permissions) and calls the route as them. So
`current-user`, `current-role`, `current_user()` in SQL and the app's
row-level policies apply to a tool call exactly as they apply to a page. The
app writes no auth code.

Declare them in `toolsite.toml`:

```toml
[[tool]]
name = "log_production"              # lower case, digits, single underscores
title = "Log production"             # optional; made from the name otherwise
description = "Record a day's egg count for a house."
path = "/api/tools/log_production"   # a handler route, under /api/
read_only = false                    # hints: read_only, destructive, idempotent, open_world
idempotent = true
input = { type = "object", properties = { house = { type = "string" }, eggs = { type = "integer" } }, required = ["house", "eggs"] }
# or: input = "tools/log_production.json", a file in the stored source (upload ?source first)
# output = "tools/log_production.out.json"   # optional output schema
```

Tools are replaced wholesale, like routes and jobs. `<app>__<name>` must fit
in 64 characters. A manifest with one bad tool applies nothing.

A call arrives as `POST <path>` with `content-type: application/json`, the
body `{"tool": "<name>", "arguments": {...}}`, and the header
`x-toolsite-tool: <name>`, which only the host can set. Answer JSON with a
2xx status and the model gets it as structured content; any other 2xx body
arrives as text; a 4xx or 5xx is a tool error carrying the first 2 KB of the
body, so say what went wrong in words.

A full route, writing through a policy so a person logs only their own rows
(`serde_json` added to the crate):

```toml
[[access.table]]
table = "production"
where = "logged_by = current_user()"
owner = "logged_by"
write = true
```

```rust
("POST", "/tools/log_production") => {
    let Some(_who) = identity::current_user() else {
        return json(401, r#"{"error":"sign in to log production"}"#.into());
    };
    let call: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let args = &call["arguments"];
    let (Some(house), Some(eggs)) = (args["house"].as_str(), args["eggs"].as_i64()) else {
        return json(400, r#"{"error":"house and eggs are required"}"#.into());
    };
    // Through the view: the policy fills logged_by and refuses anyone else's row.
    match db::query_scoped(
        "insert into my_production (house, eggs, day) values (?, ?, date('now'))",
        &[db::Value::Text(house.into()), db::Value::Integer(eggs)],
    ) {
        Ok(_) => json(200, format!(r#"{{"logged":{eggs},"house":{house:?}}}"#)),
        Err(e) => json(403, format!(r#"{{"error":{:?}}}"#, format!("{e:?}"))),
    }
}
```

People reach the tools three ways, all with their toolsite account:

- `<site>/p/<app>/mcp`: a connector with this app's tools alone. Add it in
  Claude or ChatGPT; it signs in through the same OAuth server. `mcp` under
  an app is reserved for it, like `api`.
- `<site>/me/mcp` and `<site>/mcp`: `app_tools` lists apps with tools and
  their inputs, `call_app_tool` calls one. `pin_app` lists a chosen app's
  tools there as typed tools named `<app>__<name>`.
- The app browser's menu: Pin tools, and Copy connector link.

**Put the link in the app.** An app that declares tools gets a "Connect an
AI assistant" entry in its menu or settings that shows the connector URL
with a copy button. Build it from the page's own address, so it is right on
every site the app is deployed to:

```jsx
function ConnectAssistant() {
  const url = window.location.origin + import.meta.env.BASE_URL.replace(/\/$/, "") + "/mcp";
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center gap-2">
      <code className="truncate">{url}</code>
      <button onClick={() => navigator.clipboard.writeText(url).then(() => setCopied(true))}>
        {copied ? "Copied" : "Copy"}
      </button>
      <p className="text-sm">Add this as a connector in Claude or ChatGPT and sign in with your account.</p>
    </div>
  );
}
```

`BASE_URL` is Vite's `base`, which is `/p/<app>/`. Without Vite, take the
first two segments of `window.location.pathname`.

## Live connections

For anything live (a board that updates, a chat, a progress bar), the app
takes WebSockets. Toolsite holds the socket; your handler gets `connect`,
`message` and `close` as events and uses the database, files and settings
as usual.

1. Declare the path in `toolsite.toml`:

   ```toml
   [[socket]]
   path = "/live/ws"
   ```

2. Build the handler for the `app-with-connections` world and implement
   `on_connection` beside `handle`. This one puts each browser on the topic
   it asks for and broadcasts from an ordinary API route:

   ```rust
   wit_bindgen::generate!({ path: "wit", world: "app-with-connections" });

   use toolsite::app::connections::{self, Message};

   impl Guest for Handler {
       fn handle(req: Request) -> Response {
           // POST /api/notes saves a note, then tells everyone on "notes".
           // ... insert with db::query as usual ...
           let _ = connections::publish("notes", &Message::Text(r#"{"changed":"notes"}"#.into()));
           json(200, r#"{"ok":true}"#.into())
       }

       fn on_connection(conn: String, event: Event) -> Result<(), String> {
           match event {
               // ?topic=notes on the URL. Err refuses with HTTP 403.
               Event::Connect(info) => {
                   let topic = info.query.strip_prefix("topic=").unwrap_or("notes");
                   connections::subscribe(&conn, topic)
               }
               Event::Message(_) => Ok(()), // changes go through the API
               Event::Close => Ok(()),
           }
       }
   }
   ```

   `identity::current_user()` is the person on the socket for every event.
   `connections::send(conn, ...)` answers one connection;
   `state_get` / `state_set` keep up to 64 KB per connection between its
   events; `publish("user:<id>", ...)` reaches one person's connections.
   Everything `connect` sends or publishes reaches the connection before
   anything else does, so a snapshot sent after `subscribe` is never
   overtaken and nothing published in between is lost.

3. Connect from the page, reconnect with backoff, and refetch on every
   (re)connect, since nothing is replayed:

   ```tsx
   function useLive(topic: string, onChange: () => void) {
     useEffect(() => {
       let socket: WebSocket | undefined;
       let delay = 1000;
       let stopped = false;
       const open = () => {
         const url = new URL(`live/ws?topic=${topic}`, document.baseURI);
         url.protocol = url.protocol.replace("http", "ws");
         socket = new WebSocket(url);
         socket.onopen = () => { delay = 1000; onChange(); };
         socket.onmessage = () => onChange();
         socket.onclose = () => {
           if (!stopped) setTimeout(open, delay);
           delay = Math.min(delay * 2, 30000);
         };
       };
       open();
       return () => { stopped = true; socket?.close(); };
     }, [topic]);
   }
   ```

The socket passes the app's gate for its path, so a `[[route]]` rule can
open or close it. A signed-in person's connection closes within 30 seconds
of losing access. Only declared paths take an upgrade; plain requests to
the same path are served as usual. A client that asks for a WebSocket
subprotocol (MQTT.js asks for `mqtt`) gets it only when the socket declares
it: `subprotocols = ["mqtt"]` under its `[[socket]]`.

TCP and UDP work the same way for devices that do not speak HTTP. Declare
`[[socket]] protocol = "tcp"` (or `"udp"`) with `port = 1883`; the site's
owner makes it live by mapping the port to the app with `TOOLSITE_PORTS`.
TCP bytes arrive as `Event::Message(Message::Binary(..))` in order, in reads
of up to 64 KB that need not match the sender's writes, so frame them
yourself. UDP gives one message per datagram, under one connection per
remote address that closes after it is quiet for a minute.
`connections::remote(&conn)` returns `"ip:port"`. No gate stands in front
of a port: create a token per device with `app_device_tokens`, have the
device send it first, and check it with `auth::check_token(&token)`, which
returns the token's label or `None`:

```rust
Event::Message(Message::Binary(bytes)) => {
    if connections::state_get(&conn, "device").is_none() {
        let line = String::from_utf8_lossy(&bytes);
        let Some(label) = auth::check_token(line.trim()) else {
            return connections::close(&conn);
        };
        return connections::state_set(&conn, "device", Some(&label));
    }
    // ... a reading from a known device: store it, publish it ...
    Ok(())
}
```

### Resident mode: state in memory

Each event normally runs in a fresh instance, so statics reset every time.
For server software that keeps its state in memory (a broker, a game, a
protocol state machine), add `[resident]` to `toolsite.toml`. The app then
gets one long-lived instance for all its connection events, one at a time
in arrival order across every connection, so its memory lasts between them:

```toml
[resident]
enabled = true
memory_mb = 256   # optional cap; the site sets the default and the ceiling
tick_ms = 1000    # optional: call on_tick this often (100 to 60000)
```

```rust
wit_bindgen::generate!({ path: "wit", world: "app-resident" });

static SESSIONS: Mutex<BTreeMap<String, Session>> = Mutex::new(BTreeMap::new());

impl Guest for Handler {
    // handle(...) as usual: requests still run in fresh instances.
    fn on_connection(conn: String, event: Event) -> Result<(), String> {
        let mut sessions = SESSIONS.lock().unwrap();
        match event {
            Event::Connect(_) => { sessions.insert(conn, Session::default()); Ok(()) }
            Event::Message(message) => sessions.get_mut(&conn).map_or(Ok(()), |s| s.feed(message)),
            Event::Close => { sessions.remove(&conn); Ok(()) }
        }
    }

    fn on_tick(now_ms: u64) {
        // Drop sessions whose keepalive ran out.
        SESSIONS.lock().unwrap().retain(|_, s| s.alive_at(now_ms));
    }
}
```

The memory is lost on a crash, a redeploy and a server restart, so save to
the database what must survive. A trap, a call past its time or fuel, or
memory past the cap drops the instance and closes all its connections; the
next connection starts a fresh one after a short pause. Requests and jobs do
not see the resident instance's memory, only its database and files. A
handler built for `app-with-connections` also runs resident, without
`on_tick`. `std::time::Instant` and `SystemTime` work in any handler; a
sleep, a query or a fetch never outlasts the call's wall clock, so a sleep
longer than what is left wakes early and the call then ends.

## Access

People are given access in a grid of View, Edit and Manage on a project or an
app, and a project can be Locked so only its own permissions apply inside. As
an agent, use `projects(action: "grant")` or `set_access`; to see why someone
can or cannot open something, `projects(action: "permissions")` lists direct
and inherited rows.

Publishing may be limited to a folder. Apps sit in a project tree, and the
account behind your connection may hold editor on `ops/yard` and nothing
elsewhere. A tool that is asked to touch an app outside that refuses with the
folder and the scope it would take, for example "holds editor at ops/yard;
this needs admin". Ask the person for the scope, or publish under a folder you
hold; `list_pages` shows what you may open.

`projects(action: "list")` shows the project tree and what you hold at each
level. With admin there you can `create` a project, `move` an app into one
(admin where it is and where it goes; the project must exist), and read or
change its `permissions` with `grant` and `revoke`. You cannot give more than
you hold. `rename` and `move_project` carry the apps, the access and the lock
along and keep the old path working as a link; `remove` takes away only an
empty project.

A gate decides whether a request arrives:

| Gate | Who |
|---|---|
| `public` | anyone |
| `authenticated` | any signed-in account |
| `restricted` | people given access: a grant on the app, or a scope on it or a project above it |

`restricted` was called `granted` before; the old word still works everywhere a level is typed, and toolsite stores and reports `restricted`. The pages show the three levels as Public, Signed in and Restricted.

An admin account passes every gate. General access is the first of: the
app's own gate (and route rules for their paths), the nearest project above
it with one, the site default the owner set (on an internal site usually
`restricted`). A locked project's setting overrides everything inside it.
Say `gate = "public"` only when the app really should be open to anyone.

`[[route]]` applies a gate to a path prefix, longest match winning, so a
public page and a private one live in one app. Past the door it is the app's
call: read `identity::current-role()` and decide what "editor" means. The
platform never interprets a role. Declare the roles your handler checks
(`roles = ["viewer", "editor"]` in toolsite.toml) so whoever grants access
can pick the right word; it is a hint, and any role can still be granted.

There is no public signup. Accounts are created by the owner, and a person
sets their own password through a one-time link. Site admins (or everyone,
as the owner sets it) also sign in with a code from an authenticator app;
that is the platform's, and an app does nothing for it.

## Row-level access

Who may see which rows is declared, not written. Three functions are bound
on every connection by the host: `current_user()` (the account id),
`current_email()` and `current_role()` (the grant's role on this app). A
policy in `toolsite.toml` names a table and a `where` over them; the
platform generates a view and, with `write = true`, the triggers that carry
inserts, updates and deletes through to the table and abort when the result
would be a row the person cannot see.

```toml
[[access.table]]
table = "orders"
where = "owner_id = current_user()"
owner = "owner_id"      # filled with current_user() on insert when NULL
write = true            # default false: read only
```

The person's own rows are the simplest case. Scoping by site, team or
department reads the attribute from another table in the same database:

```toml
[[access.table]]
table = "records"
where = "location = (select location from members where user_id = current_user())"
write = true
```

The `where` may use any table or view in the app's database. It must prepare
as `select 1 from <table> where (<where>)` and may not contain `;` or a `?`.
The view is `my_<table>` unless `view = "..."` says otherwise. A table
declared `without rowid` cannot take `write = true`. Hand-written views go
under `[access] views = ["my_summary"]` and are read only; a declared view
must be a view that reads tables directly, since a view it reads through is
closed unless declared as well. Names starting with `ts_` are the
platform's.

Four facts to write policies by:

- Against a NULL identity a `where` matches nothing. `current_user()` is
  NULL for a job and for an anonymous visitor, and `owner_id = NULL` is not
  true. Never write `or current_user() is null` to "let the job see
  everything": it lets everyone with no identity see everything.
- A row with a NULL owner is nobody's.
- A column left out of an insert through the view takes its declared
  default, as in a direct insert. An explicit NULL into a column with a
  default takes the default too: the view cannot tell the two apart.
- `insert or replace`, upserts and `update or replace` cannot remove a row
  the person cannot see; a colliding key is refused before the write runs.
  Changing a primary key through the view is refused too.

What this buys: a regular account connects Claude to `<site>/me/mcp`, calls
`my_apps`, and queries the declared views as themselves. A handler can offer
the same inside the app with `db::query-scoped(sql, params)`, which runs the
visitor's SQL inside the declared views and nothing else. The handler's own
`db::query` is unrestricted and can still read `current_user()`.

Prove a policy before saying it holds: `run_sql(app, sql, as_user: "a@x")`
runs the statement exactly as that account would through `/me/mcp`. Run the
same query as two accounts and compare.

The generated objects are rebuilt when the manifest or the schema changes
and dropped when the policy goes. Do not edit them; edit the policy.

## A repository

An app's source can live in a GitHub repository, with its history. The
repository is a mirror: toolsite pushes when the source is published and
pulls when someone pushes. Nothing is built there; you build and publish from
where you run, as always.

- `app_repo(app, "create")` makes a repository out of the source you
  published with `?source`, named `toolsite-<app>` unless you say otherwise,
  tagged with the `toolsite` topic.
- `app_repo(app, "import", repo: "owner/name")` links a repository that
  already exists and pulls its branch into the app's source archive; fetch it
  with `curl '<upload-url>?source' | tar xz`, build, publish.
- Publishing the source of a linked app pushes a commit. Say why with
  `'<upload-url>?source&message=<url-encoded text>'`, and name the commit the
  build came from with `&commit=<sha>` so the Repo tab can tell whether the
  live app is the repository's head.
- `app_repo(app, "status")` says where the source is mirrored, the last
  push, whether the repository is ahead of the live app, and the newest
  commits. `app_repo(app, "pull")` pulls the branch again;
  `app_repo(any, "discover")` lists the repositories tagged `toolsite` that
  nobody has imported yet, each with its import call.

The site has to be configured with a GitHub App for any of this;
`app_repo(app, "installations")` says whether it is and on which accounts.

## Settings

`app_settings(app, name, value)` writes one; `link: true` returns a URL the
owner opens to paste values in themselves. Prefer the link: a secret that
never enters a conversation cannot leak from one. Values are encrypted at
rest and never come back out: listings give names only.

## Before saying it works

Fetch the thing. A page that returns 200 with its assets 404ing renders blank
and looks like a success. The usual cause is a build whose base path is not
`/p/<slug>/`.

    curl -I <page-url>/assets/<a-built-file>
    curl <page-url>/api/<a-route>

Then look at it. `screenshot(slug)` renders the page in a real browser on the
server and returns the picture; pass `as_user` to see a gated page as that
person, with their data. Describe what you see, with the data in it, before
you say the page works. A blank image or a sign-in page is a finding, not a
success.
