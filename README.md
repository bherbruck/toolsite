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

## The CLI

```
cargo install --path cli
export TOOLSITE_URL=https://yourdomain.com TOOLSITE_TOKEN=<TOOLSITE_MCP_TOKEN>
```

| Command | What it does |
|---|---|
| `toolsite init <name> [--react] [--spa] [--handler]` | Scaffolds an app with its base path already right. `--react` writes a Vite + React + Tailwind project that builds unmodified; `--handler` adds a wasm handler with its own database. |
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
| `projects(action, path?, name?, app?, email?, scope?)` | Projects and who may act in them: `list`, `create` (admin at the parent), `move` an app (admin at both ends, the target must exist), `permissions`, `grant`, `revoke` (admin there, never more than you hold). The same rules as the app browser. |
| `app_migrations`, `app_jobs`, `app_settings`, `app_notes`, `app_exports` | An app's schema, schedule, settings, notes and export tokens, each described below. |
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

**What a handler can and cannot do.** It gets five imports and nothing else:
`db.query`, bound to its own app's database with parameters bound rather than
interpolated; `blobs`, its own files; `identity.current-user` and
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
ceiling).

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
`0 0 3 * * *` is 03:00 daily. A bad expression is refused when you set it
rather than silently never firing. The app's Jobs tab on `/admin` shows the
same list with the last run and its status, and a Run now button.

The handler sees an `x-toolsite-scheduled` header naming the job, so a route
can behave differently when nobody is waiting on the other end. A job that
missed its turn while the server was down fires once when it comes back, not
once per missed interval, and a job still running when its next turn arrives
is skipped rather than stacked.

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

**What it declares, it owns.** Routes and jobs are replaced wholesale, so
deleting a line removes the thing; no drift between the file and the server.
What it does not mention is left alone, so hiding an app by hand survives the
next deploy. A job whose schedule did not change keeps its history. A manifest
with a mistake anywhere is rejected whole rather than half-applied.

Commands still work and are right for a one-off (`toolsite gate`,
`toolsite job`). The manifest is for anything meant to outlive the session
that set it.

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
`target` and `.git` are left out; build output is kept, because a project with
no build step has nothing else.

Nothing stored beside an app is reachable under `/p/`: not `.source`, not
`.notes`, not `.meta`, not `.exports`, not `.blobs/`. If a visitor should be
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
| `/admin/accounts/<email>` | One account: the apps they may open (add with a searchable picker, revoke), a fresh setup link shown once, disable or enable. |
| `/admin/accounts` | Accounts with role and status; disable or re-enable; New account is its own page. |
| `/admin/exports` | Every export token, by app and label. |

Anything that removes or disables asks first. Every action comes back to the
page it was made on with one line saying what happened. Disabling ends the
account's live sessions immediately rather than waiting for them to expire,
and destroys nothing; enabling restores the same password.

### Your own account

Everyone signed in has `/account`, reached from their email in the sidebar:
how they sign in (a password, a provider, or both) and, for an account with
a password, a form to change it. Changing it signs out every other session
of that account. There is no mailer, so there is no reset email: someone who
has forgotten their password asks an admin, who issues a new setup link from
the account's page in the admin.

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

An app that has not chosen follows the site default, `TOOLSITE_DEFAULT_ACCESS`,
which is `public` unless you set it. An internal deployment sets it to
`restricted` or `authenticated` once, and every app is closed from the moment it
is published; an app that should be open says `public` itself. The admin
page marks apps that follow the default, and `toolsite gate <app> default`
puts one back on it.

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
session, in a cookie scoped to `/p/<app>/`, is the only thing that satisfies a
gate. `/auth/handoff` mints the second from the first, and refuses to do so
for anything the browser reports as a background fetch.

**Scope note.** Every app shares one origin, so cookie `Path` decides which
requests carry a session, not which page asked. That contains accidents
between apps but does not stop a deliberate one: a script can navigate the
visitor through the handoff and then use the resulting cookie. This is a fine
trade when every app is one you deployed, and it is the reason to reach for a
subdomain per app if that ever stops being true.

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
  same way.
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
| `POST /me/mcp` | any account's sign-in | A regular account's MCP server: `my_apps` and `query` over the data apps share. |
| `GET /.well-known/oauth-authorization-server`, `POST /register`, `GET\|POST /authorize`, `POST /token` | public | The OAuth server MCP clients sign in through. Present when `TOOLSITE_BASE_URL` is set. |
| `PUT /upload/<ticket>[/<page>]` | ticket | Write a page. `?icon` stores an icon, `?bundle` unpacks a tar, `&spa` marks it client-routed, `?handler` installs a wasm component, `?migrations`, `?manifest`, `?source`, `?blob=<key>`. 64 MB. |
| `ANY /p/<slug>` | gate | The page, a bundle asset, or the app's handler. An app root redirects to `/p/<slug>/` so relative links resolve. |
| `GET /icon/<slug>` | public | A page's icon, if set. |
| `PUT /blob/<ticket>` | ticket | A visitor's file, streamed to the app's storage. Minted by the app's handler. |
| `GET /export/<app>.sqlite` | export token | A snapshot of that app's database, for a reporting tool. |
| `GET /auth/login`, `/auth/login/<slug>`, `/auth/callback/<slug>`, `/auth/logout`, `/auth/setup`, `/auth/handoff`, `/auth/me` | public | Visitor sign-in: password, provider, one-time setup link, the app handoff, and who am I. |
| `GET /settings/<token>` | link | Where someone pastes an app's settings in. |
| `GET /admin/...` | admin account | The admin pages. |
| `GET /guide` | public | How the platform works, for an agent about to build on it. |
| `GET /wit/toolsite.wit` | public | The contract a handler compiles against. |
| `GET /scaffold/<app>` | public | A gzipped tar of a handler crate ready to build. |
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
| `TOOLSITE_MCP_TOKEN` | if clients don't sign in | Static token an MCP client sends to `/mcp`. |
| `TOOLSITE_DATA_DIR` | no (default `/data`) | Where everything is stored. |
| `TOOLSITE_LOGIN_<SLUG>_CLIENT_ID` / `_CLIENT_SECRET` | no | A sign-in provider. Presets `GOOGLE`, `GITHUB`, `MICROSOFT`, `ENTRA` (needs `_TENANT`); any other slug needs `_ISSUER`. Optional `_NAME` and `_ALLOW_DOMAIN`. See Accounts. |
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
| `TOOLSITE_SECRET_KEY` | no | Base64, 32 bytes. Encrypts app settings. Generated beside the data when unset, which is weaker; see Settings. |
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
.site/auth.db             accounts and sessions
.site/oauth.db            MCP clients' tokens
.trash/                   what remove_page moved aside
```

Slugs are restricted to letters, numbers, `-`, `_` and `/`, so a slug can
never escape the data directory. Bundle paths and blob keys additionally
allow `.` inside a filename but never at the start of a segment, which rules
out `..` and dotfiles in one stroke; that is also why nothing under `.site/`,
`.trash/` or `.blobs/` can ever be named by a URL.
