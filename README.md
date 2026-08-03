# toolsite

A single container that is both an MCP server and a web host. An agent
publishes a page — a hand-written HTML file, a multi-page app, or a compiled
front-end bundle — and gets back a URL that just works.

The point of the design: **page contents never pass through the model.** The
agent asks for an upload URL, writes the file to disk, and `curl`s it up.

## Run it

Locally: `cp .env.example .env`, set a `BEARER_TOKEN`, then
`cargo run --release`. `.env` is loaded at startup and is gitignored.

```
docker build -t toolsite .

docker run -d -p 8080:8080 -v ./data:/data \
  -e BEARER_TOKEN=<random-secret> \
  -e PUBLIC_BASE_URL=https://yourdomain.com \
  toolsite
```

On Railway: attach a Volume at `/data` (the Dockerfile deliberately has no
`VOLUME` line — Railway's builder rejects it) and set the variables below.

## The CLI

```
cargo install --path cli
export TOOLSITE_URL=https://yourdomain.com TOOLSITE_TOKEN=<BEARER_TOKEN>
```

| Command | What it does |
|---|---|
| `toolsite init <name> [--spa] [--handler]` | Scaffolds an app with its base path already right, optionally with a wasm handler. |
| `toolsite deploy [dir] [--slug s] [--spa]` | Builds the handler if there is one, tars `dist/`, uploads both, then fetches the page to check it. |
| `toolsite sql <app> "<sql>" [--param v]` | Runs SQL against that app's database. Values are bound. |
| `toolsite list [--all]` | What is published, newest first. |
| `toolsite hide <slug>` / `unhide` | Reversible takedown. |
| `toolsite user add <email> [--password p] [--admin]` | Create an account. Reads `TOOLSITE_PASSWORD` if the flag is omitted. |
| `toolsite gate <app> <public\|authenticated\|granted>` | Decide who may reach an app. |
| `toolsite grant <app> <email>` / `revoke` | Access for a `granted` app. |
| `toolsite user disable <email>` / `enable` | Stop an account signing in and end its live sessions. Reversible. |

`deploy` warns when `index.html` references `/assets/…` from the domain root,
which is the mistake that ships a blank page while looking like a success.

## Connecting a client

The MCP endpoint is `POST /mcp` — Streamable HTTP transport, so **no `/sse`
suffix.** Responses are SSE-framed, but the path is still `/mcp`.

- **claude.ai** — Settings → Connectors → Add custom connector. URL
  `https://yourdomain.com/mcp`. If a "Request headers" field is offered, use
  `Authorization: Bearer <BEARER_TOKEN>`; otherwise fill in the OAuth Client
  ID / Secret you configured.
- **ChatGPT** — enable Developer Mode, add a connector with the same URL and
  token auth. OAuth will *not* work: `/authorize` only permits redirects back
  to `claude.ai`, so use `BEARER_TOKEN`.
- **Claude Code** — add it as a remote MCP server with a bearer token.

### Locally, over stdio

`toolsite --stdio` (or `MCP_STDIO=1`) speaks MCP on stdin/stdout instead of
requiring a network round trip, which is how a local client like Claude Code
can use it with no token and no OAuth:

```json
{ "mcpServers": { "toolsite": {
    "command": "/path/to/toolsite",
    "args": ["--stdio"],
    "env": { "DATA_DIR": "/path/to/data", "DATABASES": "on" }
} } }
```

The web server keeps running alongside, so uploads still have somewhere to go
and pages are viewable at `http://localhost:8080`. No token is needed in this
mode — the client already owns the process — but HTTP `/mcp` still refuses
everything without one. Logs go to stderr, since stdout is the protocol.

`GET /mcp` on its own returns `400 Session ID is required`. That's normal for
Streamable HTTP — the session is issued by `initialize`.

## Publishing

| Tool | Use |
|---|---|
| `create_upload(slug?)` | **The default.** Returns a short-lived upload URL to `curl` files to. Handles single pages, multi-page apps, bundles, and handlers. |
| `run_sql(app, sql, params?)` | Schema and seed work against an app's own database. MCP only — never reachable from a published page. |
| `list_pages(include_all?)` | What already exists: slug, title, URL, last modified, visibility. Newest first. |
| `set_visibility(slug, hidden?, listed?)` | Take a page down or hide it from the index. Reversible; nothing is deleted. |
| `set_icon(slug, icon)` | An emoji, inline `<svg>`, or `data:` URI. Optional. |
| `push_page(html, slug?)` | Fallback for clients with no shell — HTML inline. |
| `push_app(app, pages)` | Fallback, multi-page. A page named `index` also serves at the app root. |
| `pull_page(slug)` / `pull_app(app)` | Read a page back for editing. With a shell, `curl` the public URL instead. |

There is deliberately **no delete tool**. `set_visibility` covers retraction
without destroying anything.

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

### Bundles

A built front-end goes up whole, as a gzipped tar of the `dist` folder:

```
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle'        # static
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle&spa'    # client router
```

Both `tar -czf - -C dist .` and `tar -czf - dist` work — a single shared top
level directory is stripped. Files serve from `/p/<app>/…` with a content type
derived from the extension (JS, CSS, JSON, wasm, fonts, images). With `&spa`,
paths matching no file fall back to the app's `index.html`; without it, they
404.

**Set the base path before building.** Apps are served from `/p/<slug>/`,
never the domain root, so a default config emits `/assets/…` URLs that 404 —
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

An app can ship a wasm component that answers requests. It is built for
`wasm32-wasip2` against [`wit/toolsite.wit`](wit/toolsite.wit) and uploaded
alongside the bundle:

```
curl -f -T handler.wasm '<upload-url>?handler'
```

Requests are then resolved in a fixed order:

1. `/p/<app>/api/...` → the handler, always. The prefix is reserved so a file
   can't shadow it.
2. an exact file on disk → served statically, no wasm involved.
3. no file, but a handler exists → the handler, so it can render its own
   routes server-side.
4. no file, no handler, `spa` set → the app's `index.html`.
5. otherwise 404.

The guest sees the path relative to its app (`/api/echo`, not
`/p/myapp/api/echo`), so a handler never needs to know where it is mounted.

**What a handler can and cannot do.** It gets two imports: `db.query`, bound
to its own app's database with parameters bound rather than interpolated, and
`identity.current-user`, which it cannot forge. It gets no filesystem, no
environment, no sockets, and no clock beyond what the world imports. wasi is
linked because a `wasm32-wasip2` guest imports it through std, so the sandbox
is the context — which grants nothing, and the test suite proves each denial.

Every request runs in a fresh instance with a fuel ceiling, a memory cap and a
wall-clock deadline. A handler that loops forever is killed and returns 500;
the server keeps serving. Because instances are never reused, state must live
in the database.

## Accounts

Visitors are separate from publishing: `BEARER_TOKEN` says who may deploy, an
account says who may look. There is no public signup — every account is
created by the owner, so there is nothing to abuse.

From a shell on the machine itself — no token, no network, which is how the
first account gets created:

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
toolsite gate reports granted
toolsite grant reports someone@example.com
```

A password is never typed by whoever does the inviting: the account is created
without one, and the link is the only way to set it. `--password` exists for
scripts, at the cost of putting it in shell history.

An admin account can do all of that from `/admin` instead: list accounts, add
one, disable or re-enable it, set any app's gate, and grant or revoke access.
Disabling ends the account's live sessions immediately rather than waiting for
them to expire, and destroys nothing — enabling restores the same password. It is a platform route
rather than a published app because an app cannot read the account database —
that isolation is what the rest of the security rests on, so an admin app
could only exist by breaking it.

An app's gate is one of:

| Gate | Who gets in |
|---|---|
| `public` (default) | anyone |
| `authenticated` | any signed-in account |
| `granted` | only accounts granted access to that app |

A gate covers the app's handler and its assets, not just its pages. Signing in
happens at `/auth/login`; a handler sees the visitor through
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

## The index

`GET /` lists published pages, newest first, each with an icon and title.
Multi-page apps and bundles appear once, as their root. Hidden and unlisted
pages don't appear. There's a client-side filter over slugs and titles.

- **Title** — from the page's own `<title>` (first 8 KB scanned). Pages
  without one are listed by slug.
- **Icon** — in priority order: an uploaded image (`?icon`), an emoji /
  inline SVG / `data:` URI from `set_icon`, or a generated badge of the slug's
  initials on a hash-derived colour, stable forever.

## Endpoints

| Route | Auth | Purpose |
|---|---|---|
| `POST /mcp` | token or OAuth | The MCP server. |
| `PUT /upload/<ticket>[/<page>]` | ticket | Write a page. `?icon` stores an icon, `?bundle` unpacks a tar, `&spa` marks it client-routed, `?handler` installs a wasm component. 64 MB. |
| `ANY /p/<slug>` | public | The page, a bundle asset, or the app's handler. An app root redirects to `/p/<slug>/` so relative links resolve. |
| `GET /icon/<slug>` | public | A page's icon, if set. |
| `GET /` | public | The index. |

## Auth

Two independent modes — use either, or both at once. At least one is required.

- **Bearer token** — set `BEARER_TOKEN`. Sent as
  `Authorization: Bearer <token>`; `x-api-key: <token>` is also accepted,
  since clients differ. Rejected requests are logged at `warn` with the
  headers that arrived (never the token itself), so a client stuck on 401 is
  diagnosable from the deploy log.
- **OAuth 2.1** — set `OAUTH_CLIENT_ID` + `OAUTH_CLIENT_SECRET`, for clients
  that require a full OAuth flow. A minimal single-user shim: `/authorize`
  auto-approves with no login screen, `/token` hands back
  `OAUTH_CLIENT_SECRET` as the access token, and redirects are restricted to
  `claude.ai` / `*.claude.ai`.

## Environment variables

| Variable | Required | Description |
|---|---|---|
| `TOOLSITE_TOKEN` | if not using OAuth | Static token for `/mcp`. |
| `TOOLSITE_OAUTH_CLIENT_ID` | if using OAuth | Paste into the client's "OAuth Client ID" field. |
| `TOOLSITE_OAUTH_CLIENT_SECRET` | if using OAuth | Paste into the client's "OAuth Client Secret" field. |
| `TOOLSITE_BASE_URL` | if using OAuth | Base URL of the deployment, e.g. `https://host.com`. A bare host gets `https://` prepended; stray quotes are stripped. Without it, published URLs come back relative. |
| `TOOLSITE_DATA_DIR` | no (default `/data`) | Where pages are stored. |
| `TOOLSITE_DATABASES` | no (default off) | `on` gives each app a SQLite database. |
| `PORT` | no (default `8080`) | Port to listen on. Unprefixed because platforms inject it. |
| `RUST_LOG` | no (default `info`) | Log filter. Unprefixed because the Rust ecosystem owns it. |

Every `TOOLSITE_`-prefixed name above also answers to its old unprefixed form
(`BEARER_TOKEN`, `OAUTH_CLIENT_ID`, `PUBLIC_BASE_URL`, `DATA_DIR`,
`DATABASES`), plus `MCP_TOKEN`, so an existing deployment needs no changes.

Boot logs the effective configuration, so a misconfigured deploy is visible
without a client to test against:

```
INFO toolsite: auth configuration bearer_auth=true oauth_auth=true base_url="https://host.com"
```

## On disk

Everything under `DATA_DIR` is plain files — no database:

```
budget-2026.html          a single page          -> /p/budget-2026
budget-2026.icon          its icon               -> /icon/budget-2026
budget-2026.meta          {"listed":true,...}
myapp/index.html          app root               -> /p/myapp/
myapp/about.html          a page of the app      -> /p/myapp/about
myapp/assets/main.js      a bundle asset         -> /p/myapp/assets/main.js
```

Slugs are restricted to letters, numbers, `-`, `_` and `/`, so a slug can
never escape `DATA_DIR`. Bundle paths additionally allow `.` inside a
filename but never at the start of a segment, which rules out `..` and
dotfiles in one stroke.
