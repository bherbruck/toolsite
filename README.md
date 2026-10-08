# toolsite

A place for the things an AI assistant builds for you, running on a server you
own. Ask Claude for a dashboard, a sign-up form, a small internal tool, and
instead of a file you have to find somewhere to host, you get a link. The link
is on your domain, behind your sign-in if you want it, with its own database,
its own files, and a page where you decide who may open it.

It is one program: an MCP server that agents publish through, and the web
host that serves what they published, in a single binary with no database to
run beside it. Ship it as a container or run the binary on a machine you
already have. Nothing an agent builds passes through the conversation. It
asks for an upload URL, writes the file, and sends it.

## What you get

- **A URL for everything.** A single page, a multi-page site, or a built
  React app, served at `https://yourdomain.com/p/<name>/`.
- **Apps that do things.** An app can ship server-side code (a wasm
  component) with its own SQLite database, files, settings, outbound HTTP to
  hosts you allow, and scheduled jobs.
- **Your sign-in, your rules.** Accounts you create, or Google, Microsoft,
  GitHub, an Entra tenant, or any OpenID Connect provider. Each app is public,
  for anyone signed in, or for the people you name. No public signup, ever.
- **One admin page.** Apps, accounts, access, and export tokens, with a page
  per app for its gate, route rules, grants, settings, jobs and notes.
- **Reporting on your data.** A reporting tool pulls an app's database
  with a token that opens that app and nothing else.
- **Nothing destroys data.** Taking a page down is a flag. Removing an app
  moves its files to `.trash/`. Disabling an account keeps it.

## How it is used

**Publishing.** You connect Claude (claude.ai, Claude Code, ChatGPT) to
`https://yourdomain.com/mcp`. It sends you to sign in with your admin account
and asks to be allowed; no token to paste. From then on you ask for things.
The agent calls `create_upload`, gets a short-lived URL, writes the page or
the built bundle to disk and `curl`s it up. It hands you back the link.

**Building something real.** For anything with state, the agent scaffolds a
project (`toolsite init <name> --react --handler`), writes migrations for the
schema, a Rust handler compiled to wasm for the API, and a `toolsite.toml`
that says who may reach which paths. `toolsite deploy` sends all of it, keeps
the source with the app, and fetches the page back to check it works.

**People using it.** Visitors sign in at `/auth/login` with a password you
invited them to set, or with a provider button. A gated app sends them there
and back. The handler knows who they are and what role you granted; the
platform never interprets the role.

**Running it.** `/admin` lists apps, accounts, grants and export tokens. Each
app has its own page: Overview, Access, Exports, Settings, Jobs, Notes.
Anything that removes or disables asks first; every action reports back on
the page you were on.

**Reading the data elsewhere.** On an app's Exports tab you mint a token. A
reporting tool pulls `GET /export/<app>.sqlite` with it and gets a consistent
snapshot of the database.

**Keeping it in GitHub.** On an app's Repo tab you create a repository from
the app's stored source, or import one you already have. The repository
holds the source and its history: toolsite pushes a commit when you publish
the source and pulls the branch back when someone pushes. Building and
publishing happen wherever the agent or the CLI runs, in seconds; nothing
runs in GitHub. The Repo tab shows the newest commits and whether the live
app is the repository's head.

## Deploy it

You need a domain pointing at the server and a persistent directory for its
data, `/data` by default. Everything lives there as plain files; losing it
loses every app.

**Railway.** Deploy the repository as a service, attach a Volume at `/data`
(the Dockerfile has no `VOLUME` line on purpose; Railway's builder rejects
it), and set:

```
TOOLSITE_BASE_URL=https://your-service.up.railway.app
```

That is enough for clients to sign in. Add `TOOLSITE_MCP_TOKEN` if you also
want a static token for scripts, and a Railway Bucket if you want apps' files
in object storage instead of on the volume (see Files). Then create your
first admin from the service's shell:

```
toolsite user add you@example.com --admin
```

It prints a one-time link to choose a password. Open it, then connect Claude.

**Docker.**

```
docker build -t toolsite .                              # about 220 MB, screenshots off
docker build --build-arg WITH_BROWSER=1 -t toolsite .   # about 660 MB, with a browser for screenshots

docker run -d -p 8080:8080 -v ./data:/data \
  -e TOOLSITE_BASE_URL=https://yourdomain.com \
  toolsite

docker exec -it <container> toolsite user add you@example.com --admin
```

`compose.yml` does the same with a named volume; it reads `.env` and
currently requires `TOOLSITE_MCP_TOKEN` to be set there.

**Locally, with cargo.** `cp .env.example .env`, set `TOOLSITE_MCP_TOKEN` or
`TOOLSITE_BASE_URL`, then `cargo run --release`. `.env` is loaded at startup
and is gitignored. The site is at `http://localhost:8080`.

**Locally, over stdio.** A client on the same machine can speak MCP to the
binary directly, with no network and no token:

```json
{ "mcpServers": { "toolsite": {
    "command": "/path/to/toolsite",
    "args": ["--stdio"],
    "env": { "TOOLSITE_DATA_DIR": "/path/to/data" }
} } }
```

The web server keeps running alongside, so uploads still have somewhere to go
and pages are viewable. HTTP `/mcp` still refuses everything without a token
or a sign-in. Logs go to stderr, since stdout is the protocol.

Set `TOOLSITE_BASE_URL` to where the outside world reaches the server.
Without it, the upload URLs handed to an agent point at the server's own
local port, which nothing outside can use, and clients cannot sign in. Boot logs the
effective configuration, so a misconfigured deploy is visible without a client
to test against:

```
INFO toolsite: auth configuration bearer_auth=true oauth_auth=true base_url="https://host.com"
INFO toolsite: storage configuration blobs="local" max_db_mb=4096 max_blob_mb=4096
```

### Subdomain mode

By default every app is served at `<site>/p/<app>/`, on one origin with
every other app and with toolsite's own pages (path mode). That needs no
DNS, but the browser treats all of it as one origin: a script in one app can
send requests to another app as the visitor. See "Two modes: what each
protects" under Gates.

Subdomain mode gives each app an origin of its own. Set:

```
TOOLSITE_BASE_URL=https://example.com
TOOLSITE_APPS_DOMAIN=apps.example.com
```

and each app is served from `https://<label>.apps.example.com/p/<app>/`.
The path does not change, so a build made with base `/p/<app>/` works as it
is. The label is the app's name when that is already a DNS label (lower
case letters, digits and `-`, at most 63 characters); any other name is
lowered, `_` becomes `-`, and a short hash of the exact name is added, so
`Orders` and `orders` never share a host. A label is stored in the app's
meta the first time it is used, and does not change when the app moves to
another project.

The main host then serves toolsite and no app content. A link to
`<site>/p/<app>/...` from before still works: a page navigation is sent to
the same path and query on the app's host. Anything else under `/p/` on the
main host (a POST, a socket, an MCP call) is refused. An app host serves
only its own app; toolsite's pages opened there go to the main host, and
everything else is 404, as is any host that is neither the site nor one of
its apps.

DNS: a wildcard record `*.apps.example.com` pointing at the service, beside
the record for the main domain. The main domain must not be under the apps
domain; the server refuses to start if it is. On Railway, add
`example.com` and `*.apps.example.com` as two custom domains on the service
and create the records it shows for each (for the wildcard, a CNAME and the
certificate's `_acme-challenge` CNAME). A `*.up.railway.app` address cannot
carry a wildcard, so subdomain mode needs a domain of your own.

The scheme comes from `TOOLSITE_BASE_URL`, and so does the port when it
names one; `TOOLSITE_APPS_PORT` overrides the port. To try it on one
machine, `*.localhost` names resolve to loopback in Chrome and Firefox:

```
TOOLSITE_BASE_URL=http://localhost:8080
TOOLSITE_APPS_DOMAIN=apps.localhost
```

A request whose `Host` is not the main host, a loopback name or an app's
host is answered with 404, and one that names two hosts (two `Host`
headers, or a `Host` that disagrees with the request's authority) with
400. The one exception is `GET /healthz`, which answers `ok` on any
host in either mode, so a platform health check passes whatever `Host` it
sends: on Railway, set the service's health check path to `/healthz`.

A label once issued stays with its app, even after the app is removed:
another app published later never takes over that host, its bookmarks or
what the browser stored for it. Publishing again under the same name, or
putting an app back from `.trash/`, brings its label back. Issued labels are
kept in `.site/labels.json`. A name spelled the way DNS spells Unicode
(`xn--...`, or any name with `--` in its third and fourth places) is never
its own label, since a browser would show that host as some other name.

In subdomain mode a screenshot browser opens the app's own host, by the
same name a visitor uses, so it must be able to resolve and reach it;
`TOOLSITE_PREVIEW_BASE` is not used. Connectors for an app's tools are at
`https://<label>.apps.example.com/p/<app>/mcp`, and a token issued for the
path-mode address is not accepted there; reconnect such a connector once.
Switching modes signs everyone out once, since the cookies change name.

Every MCP request also leaves one line: the method, the tool, the client,
the protocol version, who, and the status. Never the arguments.

```
INFO toolsite::platform::mcp_log: mcp path=/mcp method=tools/call tool=list_pages client=- protocol=- user_agent="openai-mcp/1.0.0 (ChatGPT)" email=you@example.com status=200
```

---

# Reference

## Connecting a client

The MCP endpoint is `POST /mcp`. Streamable HTTP transport, so **no `/sse`
suffix.** Responses are SSE-framed, but the path is still `/mcp`.

- **claude.ai**: Settings, Connectors, Add custom connector. URL
  `https://yourdomain.com/mcp`, nothing else. Claude registers itself, sends
  you to sign in with your admin account, and asks you to allow it. (A
  "Request headers" field still takes `Authorization: Bearer
  <TOOLSITE_MCP_TOKEN>` if you would rather.)
- **Claude Code**: `claude mcp add --transport http toolsite
  https://yourdomain.com/mcp`, then `/mcp` to sign in; the browser opens the
  same consent screen. Or add it with a bearer token header.
- **ChatGPT**: add a connector with the same URL and sign in the same way.
  Outside Developer Mode, ChatGPT uses two tools, `search` and `fetch`, to
  read the apps and pages the account may open; with Developer Mode on it
  gets every tool.

Signing in needs `TOOLSITE_BASE_URL` set, and an admin account to sign in
with (see Accounts). A visitor account is told no: a connected client
publishes with the account's full standing, which is an admin's.

The transport holds no session. A client on MCP 2026-07-28 asks
`server/discover` and then calls what it needs, each request naming its
protocol version; an older client may still `initialize` first. Neither is
given a session id. ChatGPT speaks the newer lifecycle.

`GET /guide` is a short, current description of the platform written for an
agent about to build a handler, a schema or a gate.

`GET /examples` lists working example apps, and `GET /examples/<name>.tar.gz`
hands one over (`?slug=` renames it and sets its base path). They live in
[`examples/`](examples/): `kitchen-sink` uses every capability, one screen
each; `orders`, `static-report`, `blob-gallery`, `inventory-policies`,
`live-board`, `mqtt-broker`, `tcp-chat`, `syslog` and `duckdb-report` are smaller and focused. The same apps are test fixtures: `tests/examples.rs`
publishes each one through the router and checks what its README claims.

## The CLI

```
cargo install --path cli
export TOOLSITE_URL=https://yourdomain.com TOOLSITE_TOKEN=<TOOLSITE_MCP_TOKEN>
```

| Command | What it does |
|---|---|
| `toolsite init <name> [--react] [--spa] [--handler]` | Scaffolds an app with its base path already right. `--react` writes a Vite + React + Tailwind project that builds unmodified; `--handler` adds a wasm handler with its own database. |
| `toolsite init <name> --example <example>` | Starts from one of the server's example apps, named and with its base path set. `curl <site>/examples` lists them. Needs `--url` or `TOOLSITE_URL`, not a token. |
| `toolsite deploy [dir] [--slug s]` | Runs the project's build, applies migrations and `toolsite.toml`, uploads the bundle, handler, notes and source, then fetches the page to check it. |
| `toolsite fetch` | Unpacks the project a previous deploy kept with the app, so a later session carries on. |
| `toolsite sql <app> "<sql>" [--param v]` | Runs SQL against that app's database. Values are bound. |
| `toolsite list [--all]` | What is published, newest first. |
| `toolsite hide <slug>` / `unhide` | Reversible takedown. |
| `toolsite remove <slug> [--page-only]` | Takes it down for good; files move to `.trash/`. |
| `toolsite notes <slug> [--file notes.md]` | Read or write the notes kept with an app. |
| `toolsite secret <app> [NAME --value v] [--link]` | List setting names, set one, or print a link for someone to paste values into. |
| `toolsite job <app> [name] [--schedule c --path p] [--now] [--remove]` | List, set, run or remove a scheduled job. |
| `toolsite user add <email> [--password p] [--admin]` | Create an account. Reads `TOOLSITE_PASSWORD` if the flag is omitted. |
| `toolsite gate <app> <public\|authenticated\|restricted\|default> [--path /prefix]` | Decide who may reach an app, or one path within it. |
| `toolsite grant <app> <email> [--role r]` / `revoke` | Access for a `restricted` app. |
| `toolsite user disable <email>` / `enable` | Stop an account signing in and end its live sessions. Reversible. |

`deploy` warns when `index.html` references `/assets/…` from the domain root,
which is the mistake that ships a blank page while looking like a success.

## Publishing

| Tool | Use |
|---|---|
| `create_upload(slug?)` | **The default.** Returns a short-lived upload URL to `curl` files to. Handles single pages, multi-page apps, bundles, handlers, migrations, manifests, source, files. |
| `run_sql(app, sql, params?)` | Schema and seed work against an app's own database. MCP only; never reachable from a published page. |
| `list_pages(include_all?)` | What already exists: slug, title, URL, last modified, visibility. Newest first. |
| `set_visibility(slug, hidden?, listed?, gate?, path?)` | Take a page down, hide it from the index, or set its gate. Reversible; nothing is deleted. |
| `set_icon(slug, icon)` | An emoji, inline `<svg>`, or `data:` URI. Optional. |
| `projects(action, path?, name?, parent?, app?, email?, scope?)` | Projects and who may act in them: `list`, `create` (admin at the parent), `move` an app (admin at both ends, the target must exist), `rename` a project (admin at its parent), `move_project` (admin at the project, where it is and where it goes), `remove` an empty project (admin at its parent), `permissions`, `grant`, `revoke` (admin there, never more than you hold). The same rules as the app browser. |
| `app_migrations`, `app_jobs`, `app_settings`, `app_notes`, `app_exports` | An app's schema, schedule, settings, notes and export tokens, each described below. |
| `app_device_tokens` | Tokens for devices that connect to an app over TCP or UDP, checked by the app with `auth.check-token`. See TCP and UDP. |
| `create_user`, `set_user_active`, `set_access` | Accounts and grants, as on the admin page. |
| `push_page(html, slug?)` | Fallback for clients with no shell; HTML inline. |
| `push_app(app, pages)` | Fallback, multi-page. A page named `index` also serves at the app root. |
| `upload_begin` / `upload_chunk` / `upload_finish` | Fallback for a sandbox that cannot reach the upload URL: any kind the URL takes, sent inline as base64 chunks of at most 768 KB decoded. Same rules, same reply. |
| `screenshot(slug, path?, as_user?, width?, full_page?)` | A picture of the page from a real browser on the server, as nobody or as an account (site admins only). Say what you see before you say it works. |
| `pull_page(slug)` / `pull_app(app)` | Read a page back for editing. With a shell, `curl` the public URL instead. |
| `remove_page(slug, confirm)` | Takes a slug down for good. Files move to `.trash/` on the server rather than being deleted. |

Prefer `set_visibility`; it retracts without removing anything. `remove_page`
is for junk: a probe published as a page, an app nobody wants. Pass
`page_only` to clear a single page that is shadowing an app of the same name,
which is what an accidental upload leaves behind.

Every app gets a favicon from its icon unless it ships one. Toolsite serves
`/p/<app>/favicon.svg`, `favicon.ico` and `apple-touch-icon.png` when the
bundle has no file of that name, and adds the three `<link>` tags to an
app's HTML when its `<head>` names no icon of its own. A page that declares
an icon is never changed. An uploaded image is resized, an emoji stays an
emoji in the SVG, and an app with no icon gets the same badge of its name as
the index shows. The favicon has the same access as the app.

### Looking at what you built

`screenshot` loads a page in a real browser and returns the image, at most
1280 pixels wide. A gated page renders as the account you name, with its
data, through a one-time sign-in that works for one load. The picture is
taken after the page has loaded and its requests have finished, so a page
that fetches its data on load shows the data, not a spinner. `toolsite shot
<slug> -o page.png` does the same from a shell.

Screenshots are off unless the deployment turns them on. There are two ways,
and the tool works the same with either:

- **A browser in the image.** Build with `WITH_BROWSER=1`. The image then
  carries Google's chrome-headless-shell, about 440 MB more than without.
  On Railway, add a service variable `WITH_BROWSER=1`; Railway passes it
  into the build.
- **A browser sidecar.** Keep the small image and run the browser as a second
  service. On Railway:
  1. Add a service from the Docker image `ghcr.io/browserless/chromium`. Its
     private address is then `<name>.railway.internal`, port 3000.
  2. On the toolsite service, set
     `TOOLSITE_BROWSER_URL=ws://<browser-service>.railway.internal:3000` and
     `TOOLSITE_PREVIEW_BASE=http://<toolsite-service>.railway.internal:8080`,
     so the browser opens the one-time link over the private network, not
     the internet.
  3. Redeploy. The boot log says `screenshots available renderer=browser
     sidecar at ...`.

  Any browser that speaks the Chrome DevTools Protocol works as the sidecar:
  Browserless, a chrome-headless-shell container started with
  `--remote-debugging-address=0.0.0.0`, or Playwright's Chromium with remote
  debugging. With a token in the URL (`?token=...`), the log shows the
  address without it.

With neither, the tool says what to set.

### Upload tickets

`create_upload` returns a URL carrying a one-off ticket that is valid for 15
minutes and writes only to its own slug. The server's real token is never
handed to the agent.

```
curl -fT page.html  <upload-url>            # single page
curl -fT index.html <upload-url>/index      # a page of an app
curl -fT about.html <upload-url>/about
curl -fT logo.png   '<upload-url>?icon'     # this page's index icon
```

Every flag the URL takes: `?bundle`, `?spa`, `?handler`, `?migrations`,
`?manifest`, `?icon`, `?source`, `?blob=<key>`. No flag publishes the body as
a page. Anything else is refused rather than guessed at.

### Bundles

A built front-end goes up whole, as a gzipped tar of the `dist` folder:

```
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle'        # static
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle&spa'    # client router
```

Both `tar -czf - -C dist .` and `tar -czf - dist` work; a single shared top
level directory is stripped. Files serve from `/p/<app>/…` with a content type
derived from the extension (JS, CSS, JSON, wasm, fonts, images). With `&spa`,
paths matching no file fall back to the app's `index.html`; without it, they
404.

**Set the base path before building.** Apps are served from `/p/<slug>/`,
never the domain root, so a default config emits `/assets/…` URLs that 404:
the page loads and renders blank. `create_upload` prints these with the real
slug filled in:

```
vite.config:  base: '/p/<slug>/'
next.config:  basePath: '/p/<slug>', assetPrefix: '/p/<slug>/'
CRA:          "homepage": "/p/<slug>/"
router:       basename: '/p/<slug>'
```

Relative (`base: './'`) also works for a static multi-page bundle but breaks
on deep client-side routes, so prefer the absolute form for SPAs.

Limits: 64 MB compressed, 128 MB unpacked, 2000 files. Paths containing `..`
or a leading `/` abort the upload. Symlinks and dotfiles are skipped, and the
response says so rather than silently shipping less.

## Server-side code

An app can ship a wasm component that answers requests. The server hands out
everything needed to build one, so an agent with a shell needs nothing from
this repository:

```
curl -s https://yourdomain.com/scaffold/myapp | tar xz && cd myapp-handler
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
curl -f -T target/wasm32-wasip2/release/*.wasm '<upload-url>?handler'
```

The scaffold is a complete crate: the contract vendored into `wit/`, a
`Cargo.toml` with the right crate type, and a handler that already reads and
writes its own database. `create_upload` prints these commands with the real
slug filled in.

The contract on its own is at `GET /wit/toolsite.wit`, and mirrors
[`wit/toolsite.wit`](wit/toolsite.wit).

Requests are then resolved in a fixed order:

1. `/p/<app>/api/...`: the handler, always. The prefix is reserved so a file
   cannot shadow it.
2. an exact file on disk: served statically, no wasm involved.
3. no file, but a handler exists: the handler, so it can render its own
   routes server-side.
4. no file, no handler, `spa` set: the app's `index.html`.
5. otherwise 404.

The guest sees the path relative to its app (`/api/echo`, not
`/p/myapp/api/echo`), so a handler never needs to know where it is mounted.

**What a handler can and cannot do.** It gets these imports and nothing else:
`db.query` and `db.batch`, bound to its own app's database with parameters
bound rather than interpolated; `blobs`, its own files; `jobs.run`, its own
declared jobs; `identity.current-user` and
`current-role`, which it cannot forge; `secrets.get`, settings the owner
entered; and `fetch.send`, only to hosts the app declared. It gets no
filesystem, no environment, no sockets, and no clock beyond what the world
imports. wasi is linked because a `wasm32-wasip2` guest imports it through
std, so the sandbox is the context, which grants nothing, and the test suite
proves each denial.

Every request runs in a fresh instance with a fuel ceiling, a memory cap and a
wall-clock deadline. A handler that loops forever is killed and returns 500;
the server keeps serving. Because instances are never reused, state must live
in the database.

### Limits

What one call may use, and what an app gets without asking:

| | Request | Job | Most an app may ask for |
|---|---|---|---|
| Wall clock | 5 s | 60 s | 60 s a request, 900 s a job |
| Fuel (instructions) | 200,000,000 | 2,000,000,000 | 2,000,000,000 a request, 100,000,000,000 a job |
| Memory | 64 MB | 128 MB | 1024 MB |
| Rows one query returns | 1,000 | 1,000 | 50,000 |

An app that needs more says so in its toolsite.toml:

```toml
[limits]
request_seconds = 20
job_seconds = 600
job_fuel = 50000000000
query_rows = 10000
memory_mb = 512
```

Every key is optional; one left out keeps its default. A value past the site's
ceiling is clamped, not refused, and the deploy says so: `[limits]
job_seconds: asked for 3600, the site allows 900`. The site's owner sets the
ceilings with `TOOLSITE_MAX_REQUEST_FUEL`, `TOOLSITE_MAX_REQUEST_SECONDS`,
`TOOLSITE_MAX_JOB_FUEL`, `TOOLSITE_MAX_JOB_SECONDS`, `TOOLSITE_MAX_QUERY_ROWS`
and `TOOLSITE_MAX_MEMORY_MB`; a ceiling below a default lowers the default
too, and a fuel ceiling of `none` meters no fuel, leaving the wall clock as
the only time limit. What the app asked is stored, and clamped per call, so a
raised ceiling applies without a redeploy. `memory_mb` covers requests and
jobs; a resident instance has its own under `[resident]`. `query_rows`
applies to `query` and `query-scoped` alike. The app's admin page and its
`fetch` metadata (`limits`) show what it runs under now.

### Writing many rows: db.batch

`db.batch(statements)` runs a list of statements, each `{ sql, params }` with
its own bound parameters, in one transaction: all of them, or, if any fails,
none. It answers the rows each statement changed, in order. It has
`db.query`'s refusals (no `attach`, no `pragma`) and its deadline, and
refuses `begin`, `commit` and `savepoint` inside, since the host holds the
transaction. One statement per entry. A statement that returns rows (a
`select`, or `returning`) runs and its rows are discarded; read with `query`.
At most 10,000 statements and 4 MB of SQL text a batch.
`db.batch-scoped` is the same as the visitor, held to the app's declared
access like `query-scoped`: a write the policy forbids fails the whole batch.

## An app's schema

Put numbered `.sql` files in `migrations/` beside the source. `toolsite
deploy` sends them and applies them before the app is reachable:

```
migrations/001_initial.sql
migrations/002_add_note.sql
```

```
notes: 2 migration(s) stored, 1 applied, now at version 2
```

Each runs once, in order, in a transaction, tracked by SQLite's own
`user_version` on that app's database. **Add a file for the next change
rather than editing an old one**; a database that already ran it will never
run it again.

This is what `create table if not exists` in a handler cannot do: the table
already exists, so adding a column silently does nothing and the failure
arrives later as "no such column" against real rows. A broken migration is
rolled back whole, so a failed deploy leaves the database as it was.

Without the CLI: `curl -f -T migrations.tar.gz '<upload-url>?migrations'`, or
`app_migrations(app, files)` over MCP. `toolsite sql` remains for looking
around and one-off fixes.

The platform never reads what migrations create. Tables, columns and their
meaning are entirely the app's business; only the ladder is shared. Any one
database may grow to `TOOLSITE_MAX_DB_MB` (4 GB by default, `0` for no
ceiling); past that a write fails its own statement instead of filling the
volume.

## Files

An app keeps files the way it keeps rows: in its own namespace, reached only
through its handler. Keys look like paths (`photos/cat.jpg`) and obey the
bundle rules, so `..` and dotfiles are refused before storage is touched.

Bytes never pass through the guest, whose request body is capped at 8 MB:

- **In.** The handler calls `blobs::upload_url(key, max_bytes)` and hands the
  URL to the browser, which `PUT`s the file there. The URL works once and
  dies in fifteen minutes. The platform streams the body to storage.
- **Out.** The handler answers with `x-toolsite-blob: <key>` and an empty
  body; the platform streams the file in its place, with the stored content
  type unless the handler set one, and keeps the handler's other headers. By
  answering, the handler has decided the visitor may have it.
- **Small things.** `put`, `get`, `stat`, `list`, `delete` from inside the
  handler; `get` refuses anything over 16 MB.
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
- **From a shell.** `curl -f -T file '<upload-url>?blob=<key>'`, 64 MB per
  PUT, typed by the key's extension.

Where the bytes live is the deployment's choice, not the app's:

- **The volume**, by default: `<app>/.blobs/` beside the app's database.
  Removing the app trashes them with it.
- **An S3-compatible bucket**, when `TOOLSITE_BLOB_S3_ENDPOINT` and
  `TOOLSITE_BLOB_S3_BUCKET` are set (with the key id, secret and region),
  under `<app>/` in that bucket. A Railway bucket injects `ENDPOINT`,
  `BUCKET`, `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY` and `REGION` by reference,
  and those unprefixed names are accepted as they are, so pointing at one is
  five variable references. Set `TOOLSITE_BLOB_S3_PATH_STYLE=1` for a bucket
  whose credentials tab says path-style.

One file may be up to `TOOLSITE_MAX_BLOB_MB` (4 GB by default, `0` for no
ceiling). A file a handler writes in pieces is held to the same ceiling
unless `TOOLSITE_MAX_BLOB_WRITE_BYTES` says otherwise, in bytes (`0` for
none), for a deployment whose jobs write larger exports than an upload
should be.

How a file written in pieces stays invisible until it is finished: on the
volume the bytes go to a temp file under the app's hidden `.blobs/tmp/` and
are renamed into place; a temp file a crashed process left is swept after a
day. On a bucket they go up as a multipart upload in 8 MB parts, completed
at finish and aborted when the writer is abandoned; a file smaller than one
part is a single PUT at finish. An upload a crashed process never aborted
stays in the bucket until something removes it, so give the bucket a
lifecycle rule that aborts incomplete multipart uploads after a day.

### Heavy analytics: files plus DuckDB in the browser

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

## Reaching other services

A handler can make HTTP requests, but only to hosts its app named:

```toml
allow_http = ["api.github.com", "*.example.com"]
roles = ["viewer", "editor"]   # the roles the handler checks; a hint for whoever grants
```

```rust
let response = fetch::send(&fetch::Request {
    method: "GET".into(),
    url: format!("https://api.github.com/repos/{repo}"),
    headers: vec![("authorization".into(), format!("Bearer {token}"))],
    body: vec![],
})?;
```

Off by default; an empty list is no capability at all. `*.example.com`
covers subdomains but not the bare name.

**Naming a host is not enough.** Every address is resolved and checked first,
and anything inside this server's own network is refused whatever the
allowlist says: loopback, private ranges, link-local, including
`169.254.169.254`, which on most clouds hands out credentials. IPv4 addresses
written as IPv6 are unwrapped and checked the same way, redirects are followed
by hand so each hop is checked rather than trusted, and only `http` and
`https` are fetched at all.

A `user-agent` is sent unless the handler sets its own, since several APIs,
GitHub among them, answer 403 without one. Responses are capped at 8 MB with
a 10 second timeout. Pair it with a setting for the key: `secrets::get("API_KEY")`
and `allow_http` are the two halves of calling somebody's API.

## Scheduled work

An app can do things nobody asked for: refresh a cache, pull from an API,
tidy a table. A job is a cron expression and a path, and when it fires the
host calls the app's own handler exactly as a request would: same sandbox,
same limits, same database, no signed-in user.

```
toolsite job myapp refresh --schedule '0 */5 * * * *' --path /api/refresh
toolsite job myapp refresh --now      # run it immediately
toolsite job myapp                    # what is scheduled, and how each went
toolsite job myapp refresh --remove
```

Six cron fields, seconds first: `0 */5 * * * *` is every five minutes,
`*/10 * * * * *` every ten seconds, `0 0 3 * * *` is 03:00 daily. The
scheduler sleeps until the next job is due, to the second, and wakes when
jobs change, so a schedule fires when it says. A bad expression is refused
when you set it rather than silently never firing. The app's Jobs tab on
`/admin` shows the same list with when each last started, how long it took,
its status and whether it is running, and to someone who manages the app, a
Run now button. Running a job by hand, there or with `app_jobs` `run_now`,
takes Manage on the app; scheduling one takes Edit.

The handler sees an `x-toolsite-scheduled` header naming the job, so a route
can behave differently when nobody is waiting on the other end. A job that
missed its turn while the server was down fires once when it comes back, not
once per missed interval. A job runs once at a time: one still running when
its next turn arrives is skipped rather than stacked, and the skip recorded.
Jobs run under the job limits (see Limits).

**Starting a job from the app.** `jobs.run(name)` starts one of the app's
declared jobs now, in the background, as the schedule would: no signed-in
user, the job's limits, recorded as a run. It answers `started`. Asked while
that job is running, from a request or from the job itself, it queues one
more run for the moment the current one finishes and answers `queued`; more
asks before then are the same one run. So a job that works in stages asks
for itself at the end of each, and the next stage starts with no gap. An app
may start 600 a minute (`TOOLSITE_JOB_STARTS_PER_MINUTE`), queued runs
included; it can never name another app's job.

## Settings

An app's handler can read values its bundle must not contain: API keys,
endpoints. They are stored encrypted, beside the app, and nothing the platform
serves ever returns one: listings give names, the source archive omits them,
and no URL exposes them.

The good way to set them is a link, so a secret never enters a conversation
with an agent:

```
toolsite secret myapp --link     # prints a URL to hand over
```

Whoever holds the credentials opens it and pastes them, one `NAME=value` per
line; a `.env` file works as-is, `export` prefixes, quotes and `#` comments
included. For scripting there is still `toolsite secret myapp API_KEY --value
…`, and `toolsite secret myapp` lists the names. The app's Settings tab on
`/admin` lists the names and mints the same link.

A handler reads them through the `secrets` import:

```rust
let key = secrets::get("API_KEY").ok_or("API_KEY is not set")?;
```

**Encryption at rest.** Values are sealed with XChaCha20-Poly1305. The key
comes from `TOOLSITE_SECRET_KEY` (base64, 32 bytes) when set, which is worth
doing, since then a copy of the data volume is not a copy of the secrets.
Without it one is generated at `.site/secret.key` beside them, which protects
a stray backup of the database file and no more; the log says so at startup.

## toolsite.toml

An app says what it needs in one file that travels with its source, so
`toolsite fetch` brings back the intent along with the code and a redeploy
reproduces it:

```toml
slug = "board"
spa  = false
gate = "public"
icon = "📋"

# Anyone can submit; only signed-in people read the pile.
[[route]]
path = "/triage"
gate = "authenticated"

[[job]]
name = "rollup"
schedule = "0 0 3 * * *"
path = "/api/rollup"
```

`toolsite deploy` sends it first, before the bundle or handler, so a private
app is never briefly public. One deploy does the lot: build, schema, config,
bundle, handler, notes, source. Without the CLI it is
`curl -f -T toolsite.toml '<upload-url>?manifest'`.

**What it declares, it owns.** Routes, jobs, tools and sockets are replaced wholesale, so
deleting a line removes the thing; no drift between the file and the server.
What it does not mention is left alone, so hiding an app by hand survives the
next deploy. A job whose schedule did not change keeps its history. A manifest
with a mistake anywhere is rejected whole rather than half-applied.

Commands still work and are right for a one-off (`toolsite gate`,
`toolsite job`). The manifest is for anything meant to outlive the session
that set it.

## App tools

An app offers MCP tools by declaring handler routes in `toolsite.toml`:

```toml
[[tool]]
name = "log_production"
title = "Log production"
description = "Record a day's egg count for a house."
path = "/api/tools/log_production"
idempotent = true
input = { type = "object", properties = { house = { type = "string" }, eggs = { type = "integer" } }, required = ["house", "eggs"] }
```

`input` and `output` are JSON Schemas, inline or a file in the stored
source. The platform does the auth: a person signs in with their toolsite
account, sees only the tools of apps (and route rules) that admit them, and
each call runs the route as them, with `x-toolsite-tool` set by the host and
the body `{"tool", "arguments"}`. The handler's identity and the app's
row-level policies apply as on any request. Someone without access gets "no
such tool", the same as for an app that does not exist.

Three ways in, one code path:

- **`/p/<app>/mcp`**: a connector with that app's tools, unprefixed. Any
  account's OAuth token; a static token calls as nobody. Its
  protected-resource metadata is at
  `/.well-known/oauth-protected-resource/p/<app>/mcp`. `mcp` under an app is
  reserved, like `api`.
- **`app_tools` and `call_app_tool`** on `/mcp` and `/me/mcp`, for any app.
  A site admin may pass `as_user` to call as someone.
- **Pinned apps**: `pin_app`, or Pin tools in the app browser's menu, lists
  an app's tools on the person's `/mcp` or `/me/mcp` as typed tools named
  `<app>__<name>`, titled `<App>: <Tool>`, with annotations and `_meta`
  keys `io.toolsite/app`, `io.toolsite/project` and `io.toolsite/tool`.

The app's admin page has a Tools tab with the connector URL and the
declared tools, and the app browser's menu has Copy connector link. The
guide tells an agent building an app with tools to put the same link in the
app itself.

## Live connections

An app can take WebSockets. Declare where in `toolsite.toml`:

```toml
[[socket]]
path = "/live/ws"
```

A browser connects with `new WebSocket("wss://<host>/p/<app>/live/ws")`.
Toolsite holds the socket; the app's handler gets what happens on it as
events, in the same component that answers requests:

- **`connect`**: the socket path, the query, the upgrade's headers and the
  person, as `identity.current-user()`. Return ok to accept; return an
  error to refuse, and the browser gets HTTP 403 with that reason.
- **`message`**: a text or binary frame, at most 64 KB.
- **`close`**: the connection ended, from either side.

Events for one connection run one at a time, in order. Each runs in a fresh
instance like a request, with the database, files, settings and identity
as usual. The `connections` import acts on connections by id: `send`,
`close`, `subscribe`, `unsubscribe`, `publish` to a topic, and
`state-get` / `state-set` for up to 64 KB kept per connection between its
events. These imports work from an ordinary request and from a scheduled
job too, so an API write can tell the open sockets about it.

Everything a `connect` event sends or publishes reaches the connection
before anything else does: what other events send it meanwhile follows in
order once it is accepted (and is dropped if it is refused), so a snapshot
sent after `subscribe` is never overtaken and nothing published in between
is lost.

The rules:

- Only a declared path takes an upgrade; anywhere else is 404. A plain
  request to the same path is served as always. At most 16 sockets per
  app, declared wholesale like routes and tools. `/mcp` cannot be one.
- A socket agrees to a WebSocket subprotocol only when it declares it:
  `subprotocols = ["mqtt"]` on its `[[socket]]`, at most 8, in order of
  preference. The upgrade answers with the first one declared that the
  client offers, and with none when nothing matches. A browser that asked
  for one then gives up on the socket, so a client library that insists on
  a subprotocol (MQTT.js asks for `mqtt`) needs it declared.
- The upgrade passes the app's gate for that path, route rules and project
  locks included, so a route rule opens or closes a socket.
- A browser's upgrade must come from a page on this site. An `Origin` from
  any other site gets 403, so a page elsewhere cannot open a socket with the
  visitor's cookies. A client that sends no `Origin` is not a browser.
- Every 30 seconds each connection is pinged and its person's access is
  decided again. A disabled account, a withdrawn socket or a gate that no
  longer admits them closes it then. Hiding or removing the app closes its
  sockets at once.
- A topic is lower-case letters, digits, `-` and `_`, or `user:<id>`. A
  connection may join `user:<id>` only when it is that person's; the app
  may publish to any.
- The handler never sees toolsite's own cookies (`ts_session`, `ts_app_*`)
  in the upgrade's headers or in a request's, and a `Set-Cookie` from a
  handler that names one is dropped. The app sees and sets only its own.
- Limits: open connections per app, per person and in total, and messages
  per app per second (see Environment variables). A connection that falls more
  than 64 messages behind is closed, and so is one whose browser sends
  faster than the handler answers.
- Nothing is stored or replayed. A page that reconnects asks the app's API
  for what it missed.
- A handler opts in by building for the `app-with-connections` world, which
  adds the `on-connection` export to `app`. A handler built for `app` keeps
  working, and its app refuses upgrades with 501. To keep state in memory
  across events, see Resident mode below.

Connections live in one server process. Toolsite runs as one instance, so
that is all of them; a deployment with several instances would need a shared
bus first.

[`examples/live-board`](examples/live-board/) puts this together: a shared
board where changes go through the API and then `publish`, the board is the
first message of each connection, presence lives on a topic and a nudge
reaches one person through `user:<id>`.

### TCP and UDP

An app can also take raw TCP connections and UDP datagrams, for devices
that do not speak HTTP: an MQTT broker, a syslog sink, a sensor protocol.
Declare the port with a protocol:

```toml
[[socket]]
protocol = "tcp"     # or "udp"; "websocket" is the default
port = 1883          # 1024 to 65535
```

A declaration opens nothing. A port belongs to the server, so the site's
owner gives it to one app with `TOOLSITE_PORTS`:

```
TOOLSITE_PORTS=1883=mqtt-broker,5514/udp=syslog
```

The protocol suffix is optional and defaults to tcp. A port is live only
while it is mapped to an app that declares it, and two apps can never be
given one port: the server refuses to start. Every mapped port is bound at
boot, so publishing the app later needs no restart. A deploy that declares
a port nobody mapped says so in its output.

The same handler gets the same events:

- **TCP**: each accepted connection is `connect` (socket `"tcp:1883"`, no
  person), then a `message` with the bytes for each read, at most 64 KB,
  in order and one at a time, then `close` when either side ends it. A read
  is not a boundary the sender chose; framing is the app's protocol. While
  the handler is behind, toolsite stops reading, so a fast sender meets
  TCP's own flow control.
- **UDP**: one `message` per datagram. Toolsite keeps one connection per
  remote address, opened by its first datagram (`connect` then that
  `message`) and closed after it is quiet for the idle timeout. So
  `send(conn, data)` replies to that address and per-connection state works.
  A datagram over 64 KB, past its address's rate, arriving while its
  connection's queue or the port's is full, or opening a new remote while
  the app already holds its most TCP and UDP connections is dropped. A UDP
  source address can be forged, so replies to a remote are capped by what it
  sent: a 128-byte allowance, then three bytes out per byte in; past that a
  reply is dropped. A device that keeps talking keeps getting answers.
- A handler that traps, or runs out of time or fuel, on an event ends that
  connection; the others carry on. A TCP peer that takes none of a reply for
  `TOOLSITE_TCP_SEND_SECONDS` is closed, and it counts against the limits
  until it is.
- `connections.remote(conn)` says where a TCP connection or UDP remote comes
  from, as `"ip:port"`. `send`, `close`, `subscribe`, `publish` and the
  state functions work as for a WebSocket, so a device's reading can be
  published straight to the browsers watching it.

There is no gate in front of a port. A device carries no cookie, so the
platform cannot decide who it is: the app does. Mint a **device token**
(`tsv_...`) for each device with `app_device_tokens(app, "create",
label)` or on the app's Connections tab on `/admin`, and have the device
present it however the protocol carries one: a first line, an MQTT
password, a field in each datagram. The handler calls
`auth.check-token(token)`, which returns the token's label for a live token
of this app and nothing for anything else, another app's token included.
Tokens are stored hashed in `<app>.devices`, which is never served and goes
to the trash with the app. A revoked token stops passing the check at once;
a connection the app already let in stays open until the app closes it.

Hiding or removing the app closes its TCP connections at once and turns new
ones away; withdrawing the port from `toolsite.toml` or unmapping it closes
them at the next 30-second check. Limits are per app and per address (see
Environment variables): open TCP and UDP connections per app and per IP
address, the TCP idle timeout, and UDP datagrams per second per address.

On Railway a service has one public HTTP port. Each TCP port needs a TCP
proxy of its own (service settings, Networking, TCP Proxy, pointing at the
container port), which hands out a `host:port` of Railway's choosing for the
devices to use. Railway does not route UDP from outside, so a UDP port is
reachable only from inside its private network, or on a host that routes
UDP.

Two small examples use the ports. [`examples/tcp-chat`](examples/tcp-chat/)
is a line chat for `nc`: a device token as the first line, the partial line
and the nickname in per-connection state, and lines of at most 4 KB, since a
read is not a line. [`examples/syslog`](examples/syslog/) parses RFC 5424
and RFC 3164 datagrams into SQLite, refuses sources outside a setting at
`connect`, and publishes each line to a WebSocket tail.

### Resident mode

Each event normally runs in a fresh instance, so a handler keeps nothing in
memory between events. That suits most apps, and the database is the place
for state. Some server software is built around state in memory: a broker's
sessions and subscriptions, a game's world, a protocol's state machine.
For that, an app can run **resident**:

```toml
[resident]
enabled = true
memory_mb = 256   # optional; the default is TOOLSITE_RESIDENT_MEMORY_MB
tick_ms = 1000    # optional; calls on-tick this often, 100 to 60000
```

The app then gets one long-lived instance, started by its first connection
event. Every connection event of the app, from every connection and every
transport, runs on that instance, one at a time, in the order they arrive.
Statics and the heap last from one event to the next, so a counter, a map
of sessions or a subscription tree just stays in memory.

- Only connection events reach it. Requests and jobs still run in fresh
  instances. They share the database and files with the resident instance,
  not its memory.
- Each call gets the usual fuel and wall-clock deadline. The memory cap
  holds for the whole life of the instance: `memory_mb`, at most
  `TOOLSITE_RESIDENT_MAX_MB`.
- The instance has exactly the imports a fresh one has. The connection
  limits, the rate limits and the order rule above all still apply.
- If a call traps, runs out of time or fuel, or grows memory past the cap,
  the instance is dropped and every connection it held is closed, since
  what it knew about them is gone. Clients must reconnect. The next
  connection starts a fresh instance after a pause of 1 second, which
  doubles while it keeps failing, up to 60 seconds. Republishing does not
  end a pause that is running.
- Publishing a new handler, changing `[resident]`, hiding or removing the
  app drops the instance and closes its connections too.
- **Memory is lost** on every restart, redeploy and server restart. Save to
  the database what must survive, and rebuild from it on start.
- One instance per app, in one server process. It does not scale out, and
  one slow event delays every connection of the app. At most
  `TOOLSITE_RESIDENT_QUEUE` events wait for it; one more is refused and its
  connection closed.
- A site runs at most `TOOLSITE_RESIDENT_MAX` instances at once, whose
  memory caps add up to at most `TOOLSITE_RESIDENT_TOTAL_MB`. An instance
  that would pass either does not start, and its connection is refused.
- The wall clock holds inside host calls too: a wasi sleep, a query or a
  `fetch` ends when the call's time does.
- `on-tick(now-ms)` is for keepalive timeouts and retries. It runs on the
  same instance, between events, every `tick_ms`, only when `tick_ms` is
  set and the handler exports it. Build for the `app-resident` world to
  export it; a handler built for `app-with-connections` runs resident
  without ticks. The wasi clocks work as well, so `Instant::now()` and
  `SystemTime::now()` are fine in a handler.

`[resident]` needs a handler that exports `on-connection`. A deploy is
refused if the handler on the server does not, and a handler without it is
refused for an app that runs resident. The app's Connections tab on
`/admin` shows the instance: running since, memory used, restarts and the
last failure. The MCP `fetch` tool reports the same in its metadata.

[`examples/mqtt-broker`](examples/mqtt-broker/) runs resident: a fork of
the rumqttd MQTT broker whose router and session state live in the
instance, with devices on a TCP port presenting device tokens as their
MQTT password and signed-in browsers on a WebSocket with the subprotocol
`mqtt`. Its FORK.md lists what changed from upstream to run on events.

## Notes for the next session

A published app is a rendered page; its source does not come back out of it.
So each app can carry markdown written for whoever works on it next: the
schema, why something is the way it is, what is half-finished.

`NOTES.md` (or `AGENTS.md`) in the project is sent on every deploy, so notes
travel with the source instead of being a command someone remembers. The
commands still work for reading or setting them directly:

```
toolsite notes myapp                    # read
toolsite notes myapp --file NOTES.md    # write
```

Over MCP that is `app_notes(slug, notes?)`, reading when `notes` is omitted,
and the app's Notes tab on `/admin` edits the same text. They are stored
beside the app rather than inside the bundle, so they are never served to a
visitor and need no place in the build. The same applies to the `.meta` and
`.icon` sidecars: none of the three is reachable under `/p/`.

## Source, and what a visitor can see

A visitor only ever sees what the bundle contained: the built output. The
project that produced it is stored separately and is never served:

```
toolsite deploy            # uploads the bundle, and keeps the project with it
toolsite fetch             # a later session unpacks the project and carries on
```

Over HTTP that is `PUT <upload-url>?source` and `GET <upload-url>?source`: the
same ticket, both directions, scoped to the same slug. `node_modules`,
`target` and `.git` are left out at any depth, and so is a `dist` beside a
`package.json`, which a build makes again. A `dist` with no `package.json`
beside it is kept, because a project with no build step has nothing else.

Nothing stored beside an app is reachable under `/p/`: not `.source`, not
`.notes`, not `.meta`, not `.exports`, not `.devices`, not `.blobs/`. If a visitor should be
able to read a file, put it in the bundle; that is the whole rule.

## GitHub

An app's source can live in a GitHub repository, with its history. The
repository is a mirror: toolsite pushes to it when you publish the source and
pulls from it when someone pushes. Building and publishing happen wherever the
agent or the CLI runs, in seconds. Nothing is built or run in GitHub, and the
server stays one binary with no toolchain in it.

**Connecting.** Register a GitHub App once (below), set the variables, and
install it on your account or organisation from `/admin/github`. GitHub sends
you back to `/github/setup`, which records the installation.

**Create.** On an app's Repo tab, "Create a repository": toolsite makes the
repository (private unless you say otherwise, named `toolsite-<app>` unless
you say otherwise, tagged `toolsite`) and commits the app's stored source in
one commit, with a README when the project brings none. The app must have had
its source published (`?source`, which `toolsite deploy` does); without it
there is nothing to put in the repository and the tab says so.

**Import.** "Import a repository", on the Repo tab or on the GitHub page for
an app that does not exist yet: pick a repository the installation can reach
(the picker searches; it never lists everything), optionally a branch and a
subdirectory. Toolsite links it and pulls the branch into the app's source
archive. An agent then fetches it with `curl '<upload-url>?source' | tar xz`,
builds, and publishes.

**Discovery.** The GitHub page lists every repository the App can reach that
carries the `toolsite` topic and is not yet connected, with the app name
proposed from the repository name (`toolsite-shop` proposes `shop`) and an
Import button per row. Nothing is imported until you click.
`app_repo(any, "discover")` gives an agent the same list.

**Push on publish.** Publishing the source of a linked app (`?source` through
an upload ticket, or `toolsite deploy`) commits it to the branch: files in the
archive are written, files the project dropped are removed, `.github/` and a
README toolsite wrote are left alone, and an unchanged project makes no
commit. Say why with `?source&message=<text>` or the `X-Toolsite-Message`
header (`toolsite deploy --message "..."`); the first line is the subject,
capped at 200 characters, and every such commit ends with the trailer
"Published from toolsite". An archive that arrives through `PUT /deploy/<app>`
with a deploy token is stored and never pushed, since it came from a pipeline.

**Pull on push.** The webhook at `/github/webhook` hears a push to the linked
branch and stores the branch as the app's source archive, so the next session
starts from what is in GitHub. Toolsite's own pushes are recognised and not
pulled again. "Pull from repository" on the Repo tab does the same on demand,
as does `app_repo(app, "pull")`.

**Drift.** The link remembers which commit the live app came from: toolsite's
own push sets it, and a publish may name it with `&commit=<sha>` or the
`X-Toolsite-Commit` header (`toolsite deploy` sends `git rev-parse HEAD` when
the project is a checkout, `--commit` overrides). The Repo tab and
`app_repo(app, "status")` then say "The live app is the repository's head" or
"The repository is N commits ahead of the live app", with the newest five
commits under it. When it is ahead, pull the source and run `toolsite deploy`,
or ask the agent to.

**Deploy tokens** are per app and independent of GitHub: `tsd_…`, hashed in
`<app>.deploys`, accepting exactly the flags an upload ticket accepts at
`PUT /deploy/<app>`, refused for any other app and never the publish token.
They are for a CI system of your own. `app_deploy_tokens` mints and revokes
them over MCP; the Repo tab does the same.

Disconnecting forgets the link; the repository and the app are left alone.

### Registering the GitHub App

Settings → Developer settings → GitHub Apps → New GitHub App, then:

| Field | Value |
|---|---|
| Homepage URL | `https://<host>` |
| Setup URL | `https://<host>/github/setup`, with "Redirect on update" ticked |
| Webhook URL | `https://<host>/github/webhook` |
| Webhook secret | what you will set as `TOOLSITE_GITHUB_WEBHOOK_SECRET` |
| Repository permissions | Contents: read and write · Administration: read and write · Metadata: read |
| Subscribe to events | Push |
| Where can it be installed | Any account, or only yours |

Generate a private key, then set `TOOLSITE_GITHUB_APP_ID` (the App ID on
that page), `TOOLSITE_GITHUB_APP_PRIVATE_KEY` (the PEM, or base64 of it if
your dashboard eats newlines), `TOOLSITE_GITHUB_APP_SLUG` (the name in the
App's URL) and `TOOLSITE_GITHUB_WEBHOOK_SECRET`. `TOOLSITE_BASE_URL` must be
set; the README toolsite writes into a repository names it.

Creating a repository on a personal account needs the App installed on that
account with Administration permission; an organisation works the same way.
If GitHub refuses to create one, import an empty repository you made by hand
instead.

## Accounts

Visitors are separate from publishing: a token, or an admin signing a client
in, says who may deploy; an account says who may look. There is no public
signup. Every account is created by the owner or arrives through a provider
you configured, so there is nothing to abuse.

From a shell on the machine itself, with no token and no network, which is
how the first account gets created:

```
toolsite user add you@example.com --admin
  created you@example.com as an admin

  Open this to choose a password (48 hours, one use):
  https://yourdomain.com/auth/setup?token=…

toolsite user list
toolsite user invite someone@example.com     # a fresh link
toolsite user disable someone@example.com
toolsite user reset-mfa someone@example.com  # lost phone and recovery codes
```

Or remotely, with the CLI against a running server:

```
toolsite user add someone@example.com        # prints the same link
toolsite gate reports restricted
toolsite grant reports someone@example.com
```

A password is never typed by whoever does the inviting: the account is created
without one, and the link is the only way to set it. `--password` exists for
scripts, at the cost of putting it in shell history.

### Signing in with a provider

Google, Microsoft, an Entra tenant, GitHub, or anything that speaks OpenID
Connect (Keycloak, Okta, Auth0) can be a sign-in button. Each is a group of
environment variables under one slug; the slug is the URL and the default
button text:

```env
TOOLSITE_LOGIN_GOOGLE_CLIENT_ID=…               presets: issuer known
TOOLSITE_LOGIN_GOOGLE_CLIENT_SECRET=…
TOOLSITE_LOGIN_GITHUB_CLIENT_ID=…
TOOLSITE_LOGIN_GITHUB_CLIENT_SECRET=…
TOOLSITE_LOGIN_MICROSOFT_CLIENT_ID=…            personal and any work account
TOOLSITE_LOGIN_MICROSOFT_CLIENT_SECRET=…
TOOLSITE_LOGIN_ENTRA_TENANT=…                   one tenant
TOOLSITE_LOGIN_ENTRA_CLIENT_ID=…
TOOLSITE_LOGIN_ENTRA_CLIENT_SECRET=…
TOOLSITE_LOGIN_KEYCLOAK_ISSUER=https://sso.example.com/realms/main
TOOLSITE_LOGIN_KEYCLOAK_CLIENT_ID=…             any other slug: say its issuer
TOOLSITE_LOGIN_KEYCLOAK_CLIENT_SECRET=…
TOOLSITE_LOGIN_KEYCLOAK_NAME="Company SSO"      optional button text
TOOLSITE_LOGIN_ENTRA_ALLOW_DOMAIN=example.com   optional, per provider
```

Register `https://yourdomain.com/auth/callback/<slug>` (lowercase) as the
redirect URI at the provider. `TOOLSITE_BASE_URL` must be set.

There is still no public signup. A provider login signs in the account that
already holds that email and links the identity, so a later rename at the
provider lands on the same account. With `ALLOW_DOMAIN`, an unknown email
under that domain gets an account made on first sign-in, never an admin one.
Anyone else is told to ask an admin. A disabled account is refused however
it arrives.

Under the hood: authorization code with PKCE, a single-use state that lasts
ten minutes, and a nonce checked against the id token, which is verified
against the provider's published keys (asymmetric algorithms only). An email
the provider marks unverified is refused. GitHub has no id token, so its
primary verified email is read from the API.

### The admin pages

An admin account sees `/admin`. It is a platform route rather than a
published app because an app cannot read the account database; that isolation
is what the rest of the security rests on, so an admin app could only exist by
breaking it.

| Page | What is there |
|---|---|
| `/admin/apps` | The apps you manage, with their gate and whether they ship a handler. Each row opens the app's page. Projects and their permissions are run from the app browser. |
| `/admin/apps/<app>` | Overview (title, database size, outbound hosts, visibility), then tabs: Access (who may open it, route rules, people with access), Exports, Settings, Jobs, Notes. |
| `/admin/accounts/<email>` | One account: the apps they may open (add with a searchable picker, revoke), whether two-step sign-in is on and a reset for it, a fresh setup link shown once, disable or enable. |
| `/admin/accounts` | Accounts with role, status and two-step sign-in; disable or re-enable; New account is its own page. |
| `/admin/exports` | Every export token, by app and label. |

Anything that removes or disables asks first. Every action comes back to the
page it was made on with one line saying what happened. Disabling ends the
account's live sessions immediately rather than waiting for them to expire,
and destroys nothing; enabling restores the same password.

### Your own account

Everyone signed in has `/account`, reached from their email in the sidebar:
how they sign in (a password, a provider, or both) and, for an account with
a password, a form to change it. Changing it signs out every other session
of that account, ends any sign-in still waiting for its two-step code, and
revokes the account's OAuth tokens. There is no mailer, so there is no reset email: someone who
has forgotten their password asks an admin, who issues a new setup link from
the account's page in the admin. Using a setup link also signs out every
session of the account, since it is how a leaked password is replaced. The
same page turns two-step sign-in on and off.

### Two-step sign-in

An account can add a code from an authenticator app (Google Authenticator,
Microsoft Authenticator, 1Password and the like) to its password. It is TOTP
(RFC 6238): six digits, a new code every 30 seconds.

- **Turn it on** from `/account`: scan the QR code, or type the key into the
  app, and enter the first code and your password (a stolen session cookie
  alone cannot put someone else's phone on the account). The QR code is
  shown only to the session that began setup; a setup begun anywhere else
  starts over with a new secret, so a secret someone else saw is never the
  one you scan. Ten recovery codes are shown one time, with
  copy and download. Each recovery code works one time. Turning it on keeps
  the session you are using, signs out every other session of the account
  (app sessions too), and revokes the account's OAuth tokens, so a connected
  MCP client must sign in again and give a code.
- **Sign in**: after a correct password, there is no session yet. A pending
  sign-in, good for 5 minutes and held in its own cookie, leads to a page
  that takes a code from the app or a recovery code. Only then is the
  session made. This applies to the sign-in page, to a setup link, to the
  consent screen an MCP client sends you to, and to the subdomain handoff,
  which sends a pending sign-in back to sign in rather than open an app.
- **Limits**: a code works for the current 30 seconds and one step either
  side, and each step works one time per account. Five wrong codes end a
  pending sign-in. Ten wrong codes for one account in 15 minutes, across
  sign-ins, stop that account taking codes until the 15 minutes pass. Each
  refusal is logged at `warn`, without the code.
- **Recovery**: from `/account`, get ten new recovery codes (needs a code
  from the app), or turn it off (needs a code from the app or a recovery
  code). Someone who lost the phone and the recovery codes asks an admin,
  whose "Reset two-step sign-in" on the account's page removes it and signs
  out every session of that person and revokes their OAuth tokens. Only a
  site admin may reset it (Manage over the whole tree is not enough); one
  admin may reset another, and every reset is logged at `warn` with both
  accounts. On the server, `toolsite user reset-mfa <email>` does the same,
  which is the way back in for the only admin.
- **Live connections**: a WebSocket belongs to the app session it was opened
  with, and closes at the next check once that session ends, whatever ended
  it: sign-out, a new password, turning two-step sign-in on, or a reset.
- **At rest**: the shared secret is sealed with the same key as app settings
  (`TOOLSITE_SECRET_KEY`), and recovery codes are stored as digests keyed
  with it. Neither is logged, and the secret is not shown again after setup.

**Policy.** `TOOLSITE_REQUIRE_MFA` says who must have it: `admins` (the
default: site admins), `everyone`, or `off`. Someone the policy covers who
has not set it up is sent to a setup page after the password and gets no
session until the first code confirms it, so an existing admin is not locked
out and sets it up at the next sign-in. A session from before the policy keeps
working on `/account` and nowhere an admin's powers are used: the admin pages send it to `/account` to set two-step
sign-in up, and the consent screen will not connect a new MCP client for it.
Under the policy, turning it off on `/account` is refused.

Whoever has the password of an account the policy holds for setup can set it
up first, with their own phone; that is the nature of a first sign-in, so
issue admin accounts by setup link and have the person sign in promptly. They
can also restart a setup the owner has begun, which makes the owner sign in
again, but never see the owner's secret.

**Providers.** A sign-in through Google, Entra, GitHub or an OIDC issuer
skips toolsite's code, since the provider asks for its own. Set
`TOOLSITE_MFA_FOR_PROVIDERS=1` to ask for the code (and for setup, under the
policy) after the provider too. With it off, an admin who signs in through a
provider and has no password owes nothing under `admins`; one who also has a
password is sent to set it up before using the admin pages.

**What it does not cover.** Two-step sign-in protects interactive sign-ins
and new OAuth consents. Credentials that skip sign-in stay as they are: the
MCP bearer token, per-app export, deploy and device tokens, and OAuth tokens
issued before. Existing OAuth tokens keep working until they are revoked
(an access token lasts a day); turning two-step sign-in on, a new
password and a reset all revoke those of that account, refresh tokens
included. In path mode an app's script shares the site's origin and can
plant a cookie of its own named like the pending sign-in's; the code page
names the account it is for, and such a sign-in cannot be finished without
that account's code. Subdomain mode over HTTPS names the cookie
`__Host-ts_mfa`, which a sibling host cannot set.

### Projects and scopes

Apps sit in projects, and projects nest: `ops`, `ops/yard`. The tree is the
platform's; an app's URL is its slug wherever it sits, so moving an app
changes who manages it, not where visitors find it. Projects are made, apps
are moved and permissions are set in the app browser at `/` (see The app
browser below), or by an agent with the `projects` tool, which follows the
same rules. The code and the API call a project a folder.

A scope says what an account may do to the platform from a folder down.
Unlike a grant's role, which only the app reads, the platform acts on it.

| Scope | May |
|---|---|
| `viewer` | open the apps under the folder |
| `editor` | also publish and change apps there, and remove apps it created |
| `admin` | also set access and route rules, give and take scopes at or below the folder, and manage exports and repositories there |

Scopes only add. The strongest scope held on an app's folder or any folder
above it applies, so a scope given on `ops` applies to `ops/yard` and every
app in both. A person may hold many scopes: editor under `ops/yard`, viewer
under `finance/reports`, admin under `labs`. A site admin is admin at the
root and so everywhere. A grant on an app counts as viewer on that app.

The part that pays for it: an editor connects Claude to `/mcp` and publishes,
but only under the folders it holds. A tool that is asked to touch something
else refuses and names the folder and the scope it would take. A static
`TOOLSITE_MCP_TOKEN` keeps every power, as the CLI relies on it. An account
with only viewer scopes is turned away at the consent page.

#### Permissions

Access is set in one table of rules, the same on a project's Permissions tab in
the app browser and on an app's Access tab, the way Tableau's permissions dialog
works. A rule is one person and a level: **View** (viewer), **Edit** (editor) or
**Manage** (admin). The levels add up, so Edit includes View.

The table lists only the people who hold a rule here or above, never the whole
directory, so a site with hundreds of accounts stays readable. Past 50 rules it
gets a filter and pages.

- **Add a person** is a line of controls above the table, so the column
  headers label the rules only: type a name or email, choose from at most
  ten matches, pick a level, press Add. The row appears in place and the search
  keeps the focus, so several people go in one after another. People who
  already hold a rule here are not offered again.
- **A rule row** has a level select and the View, Edit and Manage cells shaded
  to match. Change the select, or click a cell, to set that level.
- **Grey rows** are rules held from a project above. They name the project and
  link to it; change them there.
- **Click a name** to see what that person may do here and why, from the same
  function every request is checked with.
- **Remove** is the cross at the end of a row. It asks "Remove?" in the row.

On an app whose `toolsite.toml` declares `roles`, a **Role in app** column picks
the role the app reads; it is still only a hint.

Each project is **Customizable** (the default) or **Locked**. Customizable: apps
and projects inside follow its permissions and can add their own. Locked: only
the permissions set on it and above it apply inside. Rows set inside are kept,
but ignored while it is locked, and their grids say "Locked by ops". Access given
on an app is ignored under a lock too. Unlocking brings everything back.

Access given on one app with `set_access`, `toolsite grant` or the grid is a
View row on that app; grants from before the grid became such rows on the
first start. A manager changes access only at its own project and below, and
never gives more than it holds. An account's page lists what it holds,
grouped by project, each linking to the grid where it is set.

An admin who is not a site admin opens the admin pages and sees only the
folders and apps it holds scope on.

### Gates

An app's gate is one of:

| Gate | Who gets in |
|---|---|
| `public` | anyone |
| `authenticated` | any signed-in account |
| `restricted` | people given access: a grant on the app, or any scope on it or a project above it |

`restricted` was called `granted` before; the old word still works everywhere a level is typed, and toolsite stores and reports `restricted`. The pages show the three levels as Public, Signed in and Restricted.

An admin passes every gate: they can grant themselves anything from the
admin page, so asking them to do it app by app would only add a step.

General access is decided in three steps, the first one that says
something wins:

1. The app's own setting (and its route rules, for their paths).
2. The nearest project at or above the app with a setting, set on the
   project's Permissions tab or with `projects(action: "access")`.
3. The site default, `TOOLSITE_DEFAULT_ACCESS`, which is `public` unless you
   set it.

An internal deployment sets the site default to `restricted` once, and every
app is closed from the moment it is published; a project or an app that
should be open says `public` itself. Under a locked project, steps 1 and 2
skip everything inside the lock: the locked project's setting, or what it
inherits, applies, and route rules inside are ignored. The admin pages say
where an app's access comes from ("follows ops: Signed in"), and
`toolsite gate <app> default` puts an app back on what it inherits.

A gate can cover one part of an app instead of all of it, which is how a
public page and a private one live in the same bundle:

```
toolsite gate board public                          # the app
toolsite gate board authenticated --path /triage    # this corner of it
toolsite gate board authenticated --path /api/all
```

Longest matching prefix wins, so `/admin` can be closed while `/admin/help`
stays open. The arrangement works in reverse too: a `restricted` app with
`--path / --gate public` has a front page anyone can read.

Beyond that, what a signed-in caller may *do* is the app's decision. A grant
carries a role (`toolsite grant board someone@example.com --role editor`)
and the handler reads it with `identity::current-role()`. The platform never
interprets it.

Gates are per app, so public and gated apps sit side by side on one instance;
each is decided on its own. A gate covers the app's handler and its assets,
not just its pages, and keeps it off the index of anyone who cannot open it.
Signing in happens at `/auth/login`; a handler sees the visitor through
`identity.current-user` and cannot forge it.

Sessions come in two tiers. The site session proves who someone is; an app
session, in a cookie scoped to `/p/<app>/` (in subdomain mode, a host-only
cookie of the app's host), is the only thing that satisfies a gate.
`/auth/handoff` mints the second from the first, and refuses to do so for
anything the browser reports as a background fetch.

### Two modes: what each protects

Anyone who can publish an app, including a deploy-token holder or someone
who can push to an app's repository, can put a script in it. What that
script can reach depends on the mode.

**Path mode** (the default). Every app is served under `/p/<app>/` on the
same origin as every other app and as `/admin`, `/account`, the consent
screen and the settings entry form. The browser treats them as one origin.
Protected:

- toolsite's own pages (`/`, `/browse`, `/admin`, `/account`, `/authorize`,
  `/settings`, `/auth/setup`) are handed out only to a navigation in a tab,
  as the browser's fetch metadata reports it, or to a script that already
  shows the visitor's form token. An app's script cannot fetch them to read
  a form token, data or a secret.
- Those pages refuse to be framed (`X-Frame-Options: DENY`,
  `frame-ancestors 'none'`).
- Those pages and app pages never share a browsing context group
  (`Cross-Origin-Opener-Policy`), so a window an app opens on `/admin` is
  one its script cannot reach into.
- Form tokens are derived with a key only the server holds
  (`.site/form.key`), never from anything an app can learn.
- App code never sees toolsite's cookies, cannot set them, and a socket
  upgrade from another site is refused.

Not protected in path mode: a script in one app can send requests to
another app's `/p/<other>/...` as the visitor, because each app's cookie is
scoped by path on one origin and the browser attaches it to any request to
that path. It can also walk the visitor through the handoff to get that
cookie. In path mode, treat every app on a site as trusted with every
visitor's access to every other app on it.

**Subdomain mode** (`TOOLSITE_APPS_DOMAIN`). Each app has its own origin,
and the main host serves no app content. On top of everything above:

- An app's script cannot read another app's pages, storage or responses,
  or toolsite's: each is another origin, and none of them allows it.
- Each app host has its own session cookie, host-only (no `Domain`),
  `HttpOnly`, `SameSite=Lax`, and `Secure` with the `__Host-` prefix over
  https. The browser never sends it to another host, and a sibling host
  cannot plant one, since a `__Host-` cookie cannot be set for a parent
  domain. The main host's session cookie is `__Host-ts_session` for the same
  reason.
- Sign-in on an app host goes through the main host: the app host sends the
  visitor to `/auth/handoff` with a nonce it also keeps in its own cookie;
  the main host mints the app session and sends a one-time code, good for a
  minute, to that app's host, built from the configuration and the app's
  stored label, never from the request. The app host takes the code only
  with the matching nonce, so a code cannot land on another app or another
  host, or in a browser that did not ask for it.
- Hosts under one apps domain are still one *site* to the browser, so a
  sibling app's script could send a credentialed POST and the cookie would
  go with it. An app host refuses any request other than GET, HEAD or
  OPTIONS whose `Origin` is present and is not its own, and a socket upgrade
  from any origin but its own. With no `Origin`, a request whose
  `Sec-Fetch-Site` says `same-site` or `cross-site` is refused the same way.
- A handler's `Set-Cookie` naming one of toolsite's cookies is dropped under
  any spelling, an empty name included (`=ts_app=x`, which a browser sends
  back as `ts_app=x`). So are `Service-Worker-Allowed`, which would let a
  worker claim more than the app's own path, and `Clear-Site-Data`, which
  would sign the visitor out of every app; in both modes.
- Toolsite adds no CORS headers. A handler may answer another origin's
  request with its own, which is the app's choice.
- An app session for one app is refused by every other app, and sign-out on
  the main host ends every app host's session with it.

Not protected in subdomain mode: an app that changes state on a GET can
still be made to do so by a sibling's script (it cannot read the answer).
An app that wants POSTs from pages on other sites (a form embedded
elsewhere) has them refused. On plain http under a name that is not
`localhost`, browsers do not keep `Secure` cookies, so the `__Host-` prefix
is not used and a sibling host can set cookies for the parent domain,
`ts_app` included, which signs a visitor in to another app as someone else:
run subdomain mode over https. Even over https a sibling's script can set
an app's *own* cookies (any name not toolsite's) for the parent domain, and,
when the main site shares a registrable domain with the apps domain
(`example.com` and `apps.example.com`), cookies the main host reads that
carry no prefix, such as the admin pages' one-time notice. Putting the apps
domain on the Public Suffix List, or under a registrable domain of its own,
would make each app host a site of its own, which closes these; toolsite
does not need it.

## Row-level access

An app decides who may see which rows by declaring it, and the platform
enforces it. On every connection to an app's database the host binds three
SQL functions: `current_user()` is the signed-in account's id,
`current_email()` its email, `current_role()` the role of its grant on this
app, or NULL when nobody is signed in. A policy in `toolsite.toml` names a
table and a `where` over them:

```toml
[[access.table]]
table = "orders"
where = "owner_id = current_user()"
owner = "owner_id"      # optional: filled with current_user() on insert when NULL
write = true            # optional, default false
```

From it the platform generates the view `my_orders` (or the `view` you name)
and, with `write = true`, three `instead of` triggers that carry an insert,
update or delete through to `orders` and abort when the result would be a row
the person cannot see. An insert for someone else fails; an update or delete
of someone else's row changes nothing.

Scoping by site, team or department reads the attribute from another table:

```toml
[[access.table]]
table = "records"
where = "location = (select location from members where user_id = current_user())"
write = true
```

The `where` may reference any table or view in the app's own database. It
must prepare as `select 1 from <table> where (<where>)`, may not contain a
semicolon or a bound parameter, and must name a table that exists. A
`without rowid` table cannot take `write = true`. Hand-written views are
shared read only with `[access] views = ["my_summary"]`; a declared view
must be a view, not a table, and should read from tables directly, because
a view it reads through is not reachable unless it is declared too.

Rules the policy's author must know:

- A `where` compared against a NULL identity matches nothing, never
  everything: `owner_id = current_user()` is NULL for a scheduled job or an
  anonymous visitor, and NULL is not true. A policy written as
  `owner_id = current_user() or current_user() is null` opens every row to
  nobody in particular; do not write that.
- A row whose owner column is NULL belongs to nobody and is visible to
  nobody through an owner policy.
- `insert or replace`, `replace into`, upserts and `update or replace`
  cannot remove a row the person cannot see: a write that would collide with
  an existing primary key or unique index is aborted before it runs, under
  every conflict clause.
- Changing a row's primary key through the view is refused.
- A delete through the view runs the table's own `on delete cascade`, which
  is the app's schema doing what it says.

The generated objects are the platform's: every declared name becomes a
one-line view over an inner view whose name carries a random part, and the
triggers carry it too. Names starting with `ts_` are reserved for them. They
are rebuilt whenever the manifest or the schema changes, so a new column
reaches the view, and dropped when the policy goes; a declared hand-written
view gets its own definition back. The app's Access tab lists them. If a
migration breaks a policy, say by dropping its column, the migration fails
and says so rather than leaving the table open.

What a person's SQL may do, whether over `/me/mcp`, through `query-scoped`
or as `run_sql as_user`: one statement, SELECT on the declared views, writes
on writable ones, no transaction, no `explain`, no pragma, no schema change,
no attach, nothing from the filesystem. One statement may run for ten
seconds, carry 256 KB of SQL and produce values of up to 16 MB; past that it
is interrupted. Every refusal reads "not authorized".

Three callers see the boundary:

- **A regular account's Claude**, connected to `https://yourdomain.com/me/mcp`.
  Any active account can connect; the consent page says what the client may
  do. Two tools: `my_apps` lists the apps the account may open and the views
  each shares, `query(app, sql)` runs one statement as the account. Reads
  work on every shared view, writes only on a writable one, and the SQL
  cannot reach anything else in the file. An admin's publishing client still
  uses `/mcp`; a non-admin token there is refused with a pointer to `/me/mcp`.
- **The app's own handler**, through `db::query-scoped(sql, params)`, for an
  app that lets its users type SQL. The handler's plain `db::query` stays
  unrestricted and can read `current_user()` for its own filtering.
- **An admin proving a policy**: `run_sql(app, sql, as_user: "a@x")` from
  MCP, or `toolsite sql <app> "<sql>" --as a@x`, runs exactly as that account
  would through `/me/mcp`.

## Exporting a database

A reporting tool that pulls SQLite over HTTP can read an
app's database with a token minted for that app alone:

```
GET https://yourdomain.com/export/<app>.sqlite
Authorization: Bearer tse_…
```

The answer is a consistent snapshot of the whole file, taken with
`VACUUM INTO`, never the live WAL set. Mint one with `app_exports(app,
"create", label)` from an MCP client, or on the app's Exports tab, which shows
the token once and lists tokens by label afterwards. Each token opens one app
and nothing else; the publish token is refused there. Revoke from either place
and the tool gets 401 on its next pull. Tokens live hashed in
`<app>.exports`, so removing the app takes them with it.

In the reporting tool: a connection of type `sqlite` with that URL and the token as its
bearer token. It downloads the file on each sync.

## The app browser

`GET /` is the top level of a browser over projects and apps, like a file
browser over folders and files. `GET /browse/<project path>`, for example
`/browse/ops/yard`, is one project's level, with breadcrumbs back up. Old
links of the form `/?project=ops` are redirected to the path.

Each level lists its projects first, then its apps. Two views, chosen with
the icon buttons by the title and kept in the browser for every level:

- **List**: one row per project and app. The chevron on a project row opens
  it in place, so its subprojects and apps show indented beneath it; the
  name goes into the project. Open rows are kept in the address as
  `?open=yard,yard/north`, relative to the level, so a shared link opens the
  same way. The browser also keeps them per level, so a level opens the way
  you left it.
- **Cards**: the current level as tiles, project tiles first.

A search covers everything below the current level and shows each result
with its project path.

Nothing is listed that the viewer could not open. A gated app's title is as
sensitive as its contents, so signing in changes what the browser shows. A
project appears only when the viewer may open something in it, or holds
access on it or below. A project that is not there for the viewer answers
404, the same as one that does not exist.

What a person may do depends on the access they hold:

| Access | Sees |
|---|---|
| none | the apps they may open |
| `editor` on an app | an actions menu on the app: **Open** and **Settings** |
| `admin` on an app | also **Permissions** (the Access tab in Settings) and **Move to project**, to a project where they hold admin |
| `admin` at a project | an actions menu on the project (**Open**, **Permissions**, **New project inside**), **New project** by the title, and the **Permissions** tab |
| `admin` where a project sits | also **Rename**, **Move to project** and, for an empty project, **Remove** in the project's menu |

Renaming or moving a project carries everything with it: the apps inside
and below, the access set there and below, and the lock. The old path keeps
working: `/browse/<old path>` is redirected to the new one, until another
project takes that path. Only an empty project can be removed; one with
anything inside is refused with what is inside.

The actions menu opens from the **⋯** button at the end of a row or tile, or
with a right-click (or Shift+F10) on the row. It is a small panel next to
the button, with no backdrop; Escape or a click outside closes it, and the
arrow keys move between its items. Move to project and New project open as
fields inside the menu, not as a separate window. The menus are HTML
popovers, so they open and close with no script.

Every action goes to the admin action that checks it again, and comes back
to the browser with one line saying what happened.

- **Title**: from the page's own `<title>` (first 8 KB scanned). Pages
  without one are listed by slug.
- **Icon**: in priority order, an uploaded image (`?icon`), an emoji, inline
  SVG or `data:` URI from `set_icon`, or a generated badge on a
  hash-derived colour, stable forever: the first letters of the title's
  first two words, or of the slug when there is no title. The app's favicon
  uses the same badge. Projects have a muted
  folder in the same place.

## Endpoints

| Route | Auth | Purpose |
|---|---|---|
| `POST /mcp` | admin sign-in or token | The MCP server for publishing. |
| `POST /me/mcp` | any account's sign-in | A regular account's MCP server: `my_apps` and `query` over the data apps share, and app tools. |
| `POST /p/<app>/mcp` | any account's sign-in or token | That app's declared tools, as a connector of their own. |
| `GET /.well-known/oauth-authorization-server`, `POST /register`, `GET\|POST /authorize`, `POST /token` | public | The OAuth server MCP clients sign in through. Present when `TOOLSITE_BASE_URL` is set. |
| `PUT /upload/<ticket>[/<page>]` | ticket | Write a page. `?icon` stores an icon, `?bundle` unpacks a tar, `&spa` marks it client-routed, `?handler` installs a wasm component, `?migrations`, `?manifest`, `?source`, `?blob=<key>`. 64 MB. |
| `ANY /p/<slug>` | gate | The page, a bundle asset, or the app's handler. An app root redirects to `/p/<slug>/` so relative links resolve. |
| `GET /icon/<slug>` | public | A page's icon, if set. |
| `PUT /blob/<ticket>` | ticket | A visitor's file, streamed to the app's storage. Minted by the app's handler. |
| `GET /export/<app>.sqlite` | export token | A snapshot of that app's database, for a reporting tool. |
| `GET /auth/login`, `/auth/login/<slug>`, `/auth/callback/<slug>`, `/auth/logout`, `/auth/setup`, `/auth/handoff`, `/auth/me` | public | Visitor sign-in: password, provider, one-time setup link, the app handoff, and who am I. |
| `GET\|POST /auth/mfa`, `GET\|POST /auth/mfa/setup` | pending sign-in | The two-step code page, and the setup the policy requires before a session. |
| `POST /account/mfa/...` | session and form token | Turn two-step sign-in on or off, and new recovery codes. |
| `GET /settings/<token>` | link | Where someone pastes an app's settings in. |
| `GET /admin/...` | admin account | The admin pages. |
| `GET /guide` | public | How the platform works, for an agent about to build on it. |
| `GET /wit/toolsite.wit` | public | The contract a handler compiles against. |
| `GET /scaffold/<app>` | public | A gzipped tar of a handler crate ready to build. |
| `GET /examples` | public | The example apps, one line each. |
| `GET /examples/<name>.tar.gz?slug=<app>` | public | One example's source, renamed for `<app>`. |
| `GET /`, `GET /browse/<path>` | public | The app browser: a level of projects and apps, what the viewer may open. |
| `GET /admin/projects/search?q=` | session | Up to ten projects the caller may move an app into, for a picker. |

## Auth

Two independent modes for `/mcp`; use either, or both at once. At least one
is required to serve HTTP.

- **Sign in**: set `TOOLSITE_BASE_URL`. The server is then an OAuth 2.1
  authorization server for its own `/mcp`: a client registers itself
  (RFC 7591), the person signs in with an admin account and consents on a
  screen that names where the answer is going, and the client gets a token
  that is theirs. Clients are public and PKCE S256 is required; codes are
  single-use and a minute long; access tokens last a day and refresh tokens
  a month, rotating on every use. Every request re-checks the account, so
  disabling it ends its clients' access on their next call. Tokens live
  hashed in `.site/oauth.db`.
- **Bearer token**: set `TOOLSITE_MCP_TOKEN`. Sent as
  `Authorization: Bearer <token>`; `x-api-key: <token>` is also accepted,
  since clients differ. For scripts and the CLI, or a client with a headers
  field. Rejected requests are logged at `warn` with the headers that
  arrived (never the token itself), so a client stuck on 401 is diagnosable
  from the deploy log.

## Environment variables

| Variable | Required | Description |
|---|---|---|
| `TOOLSITE_BASE_URL` | if clients sign in | Base URL of the deployment, e.g. `https://host.com`. Turns the OAuth server on and is what upload URLs are built from. A bare host gets `https://` prepended; stray quotes are stripped. Without it, published URLs come back relative. |
| `TOOLSITE_APPS_DOMAIN` | no | Subdomain mode: each app on its own host, `<label>.<domain>`, e.g. `apps.example.com`. Needs `TOOLSITE_BASE_URL` (for the scheme) and a wildcard DNS record. See Subdomain mode. |
| `TOOLSITE_APPS_PORT` | no | The port app hosts are reached on, when it is not the base URL's. For local testing. |
| `TOOLSITE_MCP_TOKEN` | if clients don't sign in | Static token an MCP client sends to `/mcp`. |
| `TOOLSITE_DATA_DIR` | no (default `/data`) | Where everything is stored. |
| `TOOLSITE_LOGIN_<SLUG>_CLIENT_ID` / `_CLIENT_SECRET` | no | A sign-in provider. Presets `GOOGLE`, `GITHUB`, `MICROSOFT`, `ENTRA` (needs `_TENANT`); any other slug needs `_ISSUER`. Optional `_NAME` and `_ALLOW_DOMAIN`. See Accounts. |
| `TOOLSITE_REQUIRE_MFA` | no (default `admins`) | Who must have two-step sign-in: `admins` (site admins), `everyone`, or `off`. Someone covered without it sets it up after the password, before any session. See Two-step sign-in. |
| `TOOLSITE_MFA_FOR_PROVIDERS` | no (default off) | `1`: a sign-in through a provider also asks for toolsite's two-step code. Off, the provider's own second step counts. |
| `TOOLSITE_DEFAULT_ACCESS` | no (default `public`) | The gate an app has until it sets its own: `public`, `authenticated` or `restricted` (`granted`, the old name, still works). Set `restricted` for an internal site. |
| `TOOLSITE_MAX_DB_MB` | no (default `4096`) | Ceiling on any one SQLite file, in MB. `0` means none. SQLite enforces it, so a runaway insert fails its own statement instead of filling the volume. |
| `TOOLSITE_MAX_BLOB_MB` | no (default `4096`) | Ceiling on any one stored file, in MB. `0` means none. |
| `TOOLSITE_GITHUB_APP_ID` / `TOOLSITE_GITHUB_APP_PRIVATE_KEY` | no | A GitHub App, so an app's source can live in a repository. The key is the PEM, raw or base64. Both or neither. See GitHub. |
| `TOOLSITE_GITHUB_APP_SLUG` | no | The App's URL name, for the install link on `/admin/github`. |
| `TOOLSITE_GITHUB_WEBHOOK_SECRET` | no | Signs the pushes GitHub sends to `/github/webhook`. Without it the webhook is closed. |
| `TOOLSITE_GITHUB_API` | no | The API base, `https://api.github.com` unless you run GitHub Enterprise. |
| `TOOLSITE_BROWSER_URL` | no | A browser sidecar for `screenshot`: the `ws://` or `http://` address of its DevTools endpoint, e.g. `ws://browser.railway.internal:3000`. Wins over `TOOLSITE_BROWSER`. |
| `TOOLSITE_BROWSER` | no | Path to a Chromium or chrome-headless-shell binary in this container. An image built with `WITH_BROWSER=1` sets it. When unset, the usual names on `PATH` are tried; when none is found, screenshots are off and say so. |
| `TOOLSITE_PREVIEW_BASE` | no | The address a screenshot browser uses to reach this server, e.g. `http://toolsite.railway.internal:8080` for a sidecar. Default: this server's own port on `127.0.0.1`. |
| `TOOLSITE_BLOB_S3_ENDPOINT` | no | With `_BUCKET`, `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY` and `_REGION` (default `auto`): store apps' files in this S3-compatible bucket instead of on the volume. Railway's unprefixed `ENDPOINT`, `BUCKET`, `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY`, `REGION` are accepted too. `TOOLSITE_BLOB_S3_PATH_STYLE=1` for path-style buckets. |
| `TOOLSITE_SOCKETS_PER_APP` | no (default `500`) | Open live connections one app may have at once. See Live connections. |
| `TOOLSITE_SOCKETS_PER_PERSON` | no (default `20`) | Open live connections one account may hold at once, across apps. |
| `TOOLSITE_SOCKETS_TOTAL` | no (default `5000`) | Open live connections across every app at once. Stops a flood of anonymous visitors spread over many public apps. |
| `TOOLSITE_SOCKET_MESSAGES_PER_SECOND` | no (default `100`) | Messages one app may send or publish to its connections each second. The rest are refused, with one warning a second in the log. |
| `TOOLSITE_PORTS` | no | TCP and UDP ports given to apps: `1883=mqtt-broker,5514/udp=syslog`. Protocol defaults to tcp; 1024 to 65535; one app per port. See TCP and UDP. |
| `TOOLSITE_TCP_PER_APP` | no (default `500`) | Open TCP and UDP connections one app may have at once. |
| `TOOLSITE_TCP_PER_IP` | no (default `50`) | Open TCP and UDP connections from one IP address at once, across apps. |
| `TOOLSITE_TCP_IDLE_SECONDS` | no (default `300`) | A TCP connection that sends nothing for this long is closed. |
| `TOOLSITE_UDP_IDLE_SECONDS` | no (default `60`) | A UDP remote that sends nothing for this long is closed; its next datagram opens a new connection. |
| `TOOLSITE_UDP_PER_SECOND` | no (default `100`) | Datagrams one IP address may send to a port each second. The rest are dropped. |
| `TOOLSITE_UDP_QUEUED_BYTES` | no (default `8388608`) | Bytes of datagrams waiting for the handler on one UDP port, across its remotes. The rest are dropped. |
| `TOOLSITE_TCP_SEND_SECONDS` | no (default `10`) | A TCP peer that takes none of a reply for this long is closed. |
| `TOOLSITE_RESIDENT_MEMORY_MB` | no (default `128`) | Memory cap of a resident app's instance when its `[resident]` block does not set `memory_mb`. See Resident mode. |
| `TOOLSITE_RESIDENT_MAX_MB` | no (default `512`) | The most memory a resident app may ask for. A larger `memory_mb` is refused at deploy. |
| `TOOLSITE_RESIDENT_MAX` | no (default `20`) | The most resident instances that run at once. Each is a thread. |
| `TOOLSITE_RESIDENT_TOTAL_MB` | no (default `2048`) | The most memory the caps of all running resident instances may add up to. At least `TOOLSITE_RESIDENT_MAX_MB`. |
| `TOOLSITE_RESIDENT_QUEUE` | no (default `256`) | The connection events that may wait for one resident instance. One more is refused. |
| `TOOLSITE_MAX_REQUEST_SECONDS` | no (default `60`) | The most wall clock an app's `[limits]` may ask for a request. See Limits. |
| `TOOLSITE_MAX_REQUEST_FUEL` | no (default `2000000000`) | The most fuel a request may ask for. `none` meters no fuel for requests. |
| `TOOLSITE_MAX_JOB_SECONDS` | no (default `900`) | The most wall clock a job may ask for. |
| `TOOLSITE_MAX_JOB_FUEL` | no (default `100000000000`) | The most fuel a job may ask for. `none` meters no fuel for jobs. |
| `TOOLSITE_MAX_QUERY_ROWS` | no (default `50000`) | The most rows one `query` may return when an app asks. |
| `TOOLSITE_MAX_MEMORY_MB` | no (default `1024`) | The most memory a request or job may ask for. |
| `TOOLSITE_JOB_STARTS_PER_MINUTE` | no (default `600`) | Jobs one app may start through `jobs.run` in a minute, queued reruns included. |
| `TOOLSITE_SECRET_KEY` | no | Base64, 32 bytes. Encrypts app settings and two-step sign-in secrets. Generated beside the data when unset, which is weaker; see Settings. |
| `PORT` | no (default `8080`) | Port to listen on. Unprefixed because platforms inject it. |
| `RUST_LOG` | no (default `info`) | Log filter. Unprefixed because the Rust ecosystem owns it. |

`MCP` is in the token's name because it authenticates MCP *clients*, who may
publish, and nothing else.

Older names still answer (`TOOLSITE_TOKEN`, `BEARER_TOKEN`, `MCP_TOKEN`,
`PUBLIC_BASE_URL`, `DATA_DIR` and so on), so an existing deployment needs no
changes. `TOOLSITE_MCP_OAUTH_CLIENT_ID` / `_SECRET` configured an earlier
single-user OAuth shim that signing in replaces; the secret is still accepted
as a bearer token, so a connector made under it keeps working until you
reconnect it, after which both can go.

Every app gets a SQLite database; there is nothing to switch on. `db.query`
and `run_sql` always work. Files work the same way; a bucket is optional.

## On disk

Everything under `TOOLSITE_DATA_DIR` is plain files:

```
budget-2026.html          a single page          -> /p/budget-2026
budget-2026.icon          its icon               -> /icon/budget-2026
budget-2026.meta          {"listed":true,...}
myapp/index.html          app root               -> /p/myapp/
myapp/about.html          a page of the app      -> /p/myapp/about
myapp/assets/main.js      a bundle asset         -> /p/myapp/assets/main.js
myapp/handler.wasm        its server-side code   (runs, never served)
myapp/data.db             its SQLite database    (never served)
myapp/.blobs/data/<key>   a stored file          (only through its handler)
myapp.notes, .secrets, .jobs, .migrations, .source, .exports
                          sidecars               (never served)
.site/auth.db             accounts, sessions, two-step sign-in (secrets sealed)
.site/oauth.db            MCP clients' tokens
.trash/                   what remove_page moved aside
```

Slugs are restricted to letters, numbers, `-`, `_` and `/`, so a slug can
never escape the data directory. Bundle paths and blob keys additionally
allow `.` inside a filename but never at the start of a segment, which rules
out `..` and dotfiles in one stroke; that is also why nothing under `.site/`,
`.trash/` or `.blobs/` can ever be named by a URL.
