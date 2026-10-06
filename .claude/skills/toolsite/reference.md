# toolsite reference

Detail behind [SKILL.md](SKILL.md). Read this when writing a handler, debugging
a 404, or standing up a deployment.

## WIT surface

The full contract is [`wit/toolsite.wit`](../../../wit/toolsite.wit), also
served at `GET <server>/wit/toolsite.wit`. A handler implements the `app`
world: it imports `db`, `blobs`, `identity`, `secrets` and `fetch`, and exports
`handle: func(req: request) -> response`.

### `db`

```wit
variant value { null, integer(s64), real(f64), text(string) }

record rows {
    columns: list<string>,
    values: list<list<value>>,
    truncated: bool,        // true when the host's row cap was hit
    rows-affected: u64,
}

variant error {
    failed(string),         // statement rejected or failed
    denied(string),         // host refuses outright, e.g. ATTACH
}

query: func(sql: string, params: list<value>) -> result<rows, error>;
```

One statement per call when parameters are given. There is no string-building
entry point on purpose: values are bound to `?` placeholders, never
interpolated. Binary values are absent from `value`; a file goes in `blobs`.

`truncated` lets a guest tell a short result from a capped one. `ATTACH` comes
back as `denied`, which is enforced by the host's SQLite authorizer, not by the
guest. The database has a size ceiling set by the deployment (4 GB unless
changed); a write past it fails its own statement with "database or disk is
full".

### `blobs`

```wit
record entry { key: string, size: u64, content-type: string }
record blob  { content-type: string, body: list<u8> }

variant error {
    not-found,
    invalid-key(string),    // segments of letters, digits, '-', '_', '.'; none starting with '.'
    too-large(u64),         // over get's 16 MB, or the platform's per-file ceiling
    failed(string),
}

put:        func(key: string, content-type: string, body: list<u8>) -> result<_, error>;
get:        func(key: string) -> result<blob, error>;
stat:       func(key: string) -> result<option<entry>, error>;
delete:     func(key: string) -> result<_, error>;
list:       func(prefix: string) -> result<list<entry>, error>;   // sorted, capped by the host
upload-url: func(key: string, max-bytes: u64) -> result<string, error>;
```

`list` returns `application/octet-stream` as every content type; `stat` has the
real one. `upload-url` is a URL a browser may `PUT` one file to, once, within
fifteen minutes; `max-bytes` of zero means the platform's ceiling. To send a
file, answer with the header `x-toolsite-blob: <key>` and an empty body.

### `identity`

```wit
record user { id: string, email: string }
current-user: func() -> option<user>;
current-role: func() -> option<string>;
```

Derived from a session cookie the host verified, so a guest cannot forge it.
`none` means anonymous. `current-role` is whatever the owner granted this
account on this app, or `none` for a visitor with no grant, including an admin
who was never granted one. The platform never interprets a role.

### `secrets`

```wit
get:   func(name: string) -> option<string>;
names: func() -> list<string>;
```

Settings the owner entered for this app. Only this app's, only from inside a
handler; nothing the platform serves ever returns a value.

### `fetch`

```wit
record request  { method: string, url: string, headers: list<tuple<string,string>>, body: list<u8> }
record response { status: u16, headers: list<tuple<string,string>>, body: list<u8> }
send: func(req: request) -> result<response, string>;
```

Only for hosts named in `allow_http` in `toolsite.toml`. The error string says
whether the host was not declared or was down. Addresses inside the server's
own network are refused whatever the allowlist says.

### `http`

```wit
record request  { method, path, query, headers: list<tuple<string,string>>, body: list<u8> }
record response { status: u16, headers: list<tuple<string,string>>, body: list<u8> }
```

`path` is relative to the app: the `/p/<app>` prefix is already stripped, but
`/api` is not. The body is capped at 8 MB. A client's `x-toolsite-*` headers
are stripped; `x-toolsite-scheduled` is set by the host on a job run.

### Rust binding notes

Handler `Cargo.toml`:

```toml
# Standalone on purpose: this targets wasm32-wasip2, not the host, so it must
# not join a host workspace.
[workspace]

[package]
name = "handler"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.51"

[profile.release]
opt-level = "s"
strip = true
```

- `wit_bindgen::generate!({ path: "wit", world: "app" })` puts `Guest`,
  `Request` and `Response` at the crate root; imports land under
  `toolsite::app::{db, blobs, identity, secrets, fetch}`.
- Implement `impl Guest for YourType`, then `export!(YourType)` at the end of
  the file. Missing the `export!` produces a component with no `handle` export,
  which the server rejects at upload.
- The guest crate must be standalone (`[workspace]` in its own `Cargo.toml`);
  it targets `wasm32-wasip2`, not the host, so it must not join a host workspace.
- `tests/fixtures/guest/src/lib.rs` in this repo is a working handler that
  exercises every granted capability and every denied one.

## Sandbox guarantees

Enforced by the host and covered by tests, not by convention:

| Attempt | Result |
|---|---|
| `std::fs::read_to_string("/etc/passwd")` | Denied |
| `std::fs::read_dir("/")` | Denied |
| `std::env::vars()` | Empty |
| `TcpStream::connect(...)` | Denied |
| `fetch::send` to a host not in `allow_http`, or to a private address | Refused with the reason |
| `attach database '../other/data.db'` | `db::Error::Denied` |
| `blobs::put("../other/x", ...)` | `blobs::Error::InvalidKey` |
| Infinite loop | Killed at the fuel/deadline ceiling; request returns 500 |

wasi is linked because a `wasm32-wasip2` guest imports it through std whether it
uses it or not. The sandbox is the *context*, which grants no directory, no
environment and no sockets.

## Storage layout

Everything under `TOOLSITE_DATA_DIR` is plain files.

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
myapp.notes, .secrets, .jobs, .migrations, .source, .exports, .deploys, .repo
                          sidecars               (never served)
.site/auth.db             accounts and sessions
.site/oauth.db            MCP clients' tokens
.trash/                   what remove_page moved aside
```

With a bucket configured, `.blobs/` is replaced by objects under `<app>/` in
the bucket; the app sees no difference.

Page slugs allow only `[A-Za-z0-9_-]` per segment, joined by `/`, so a slug can
never escape the data directory. Bundle asset paths and blob keys additionally
allow `.` inside a segment but never at the start of one, which rules out `..`
and dotfiles at once; that is also why nothing under `.site/`, `.trash/` or
`.blobs/` can ever be named by a URL.

An app root redirects `/p/<slug>` to `/p/<slug>/` so relative links resolve.

## Access

A gate decides whether a request arrives:

| Gate | Who |
|---|---|
| `public` | anyone |
| `authenticated` | any signed-in account |
| `granted` | accounts given access to that app |
| `default` | whatever the site set in `TOOLSITE_DEFAULT_ACCESS` (`public` unless changed) |

An app that names no gate is on `default`. An admin account passes every gate.
A `[[route]]` applies a gate to a path prefix, longest match winning. A gate
covers the handler and the assets, not just the pages, and keeps an app off
the index of anyone who cannot open it. A gated app sends a visitor to
`/auth/login`; from there a handoff gives the browser a cookie scoped to that
one app, so an app never holds a visitor's standing with a neighbour.

Grants carry a role (`viewer` by default). `roles = [...]` in `toolsite.toml`
is a hint shown to whoever grants; any role can still be granted and the
platform never interprets it.

## The index

`GET /` is titled Apps and lists what the viewer may open, newest first, as
cards or a list. Multi-page apps and bundles appear once, as their root. Hidden
and unlisted pages don't appear, nor do apps the viewer could not open. There's
a client-side filter over slugs and titles.

- **Title**: the page's own `<title>` (first 8 KB scanned). Pages without one
  are listed by slug.
- **Icon**: in priority order, an uploaded image (`?icon`), then an emoji /
  inline SVG / `data:` URI from `set_icon`, then a generated badge of the slug's
  initials on a hash-derived colour, stable forever.

## The admin pages

An admin account sees `/admin`. Everything an agent does over MCP, an admin
can do here, and some things only happen here.

| Page | What is there |
|---|---|
| `/admin/apps` | Every app with its access and whether it ships a handler. Search and paging. Each row opens the app. |
| `/admin/apps/<app>` | Overview (title, database size, outbound hosts, visibility, source download), then tabs: Access (gate, route rules, granted accounts), Exports, Settings, Jobs, Notes, Repo. |
| `/admin/accounts` and `/admin/accounts/<email>` | Accounts with role and status; one account's grants, a fresh setup link, disable or enable. New account: a setup link by default, or a generated or typed password for a shared login. |
| `/admin/exports` | Every export token, by app and label. |
| `/admin/github` | The GitHub App: a guided setup when none is configured, the install step, then installations and linked apps. |
| `/account` | Anyone signed in: how they sign in, and a change-password form for a password account. |

## Endpoints

| Route | Auth | Purpose |
|---|---|---|
| `POST /mcp` | sign-in or token | The MCP server. Streamable HTTP, no `/sse` suffix. |
| `GET /.well-known/oauth-authorization-server`, `POST /register`, `/authorize`, `POST /token` | public | The OAuth server MCP clients sign in through. Present when `TOOLSITE_BASE_URL` is set. |
| `PUT /upload/<ticket>[/<page>]` | ticket | Write a page. `?icon`, `?bundle`, `&spa`, `?handler`, `?migrations`, `?manifest`, `?source`, `?blob=<key>`. 64 MB. |
| `PUT /deploy/<app>[/<page>]` | deploy token | The same flags, for one app, from a CI system of your own. `&commit=<sha>` names the commit. |
| `PUT /blob/<ticket>` | ticket | A visitor's file, streamed to the app's storage. Minted by the handler's `upload-url`. |
| `GET /export/<app>.sqlite` | export token | A snapshot of that app's database, for a reporting tool. |
| `ANY /p/<slug>` | gate | The page, a bundle asset, or the app's handler. |
| `GET /icon/<slug>` | public | A page's icon, if set. |
| `GET /auth/login`, `/auth/login/<slug>`, `/auth/callback/<slug>`, `/auth/logout`, `/auth/setup`, `/auth/handoff`, `/auth/me` | public | Visitor sign-in: password, provider, one-time setup link, the app handoff, who am I. |
| `GET /settings/<token>` | link | Where someone pastes an app's settings in. |
| `GET /github/setup`, `POST /github/webhook` | admin, signature | The GitHub App's install callback and webhook. |
| `GET /admin/...`, `GET /account` | admin, signed in | The admin pages and one's own account. |
| `GET /guide` | public | How the platform works, for an agent about to build on it. |
| `GET /wit/toolsite.wit`, `GET /scaffold/<app>` | public | The contract, and a handler crate ready to build. |
| `GET /` | public | The index. |

The transport holds no session. A client on MCP 2026-07-28 asks
`server/discover` and then calls what it needs, each request naming its
protocol version; an older client may still `initialize` first. Neither is
given a session id. ChatGPT speaks the newer lifecycle.

## Connecting a client

- **claude.ai**: Settings, Connectors, Add custom connector, URL
  `https://<host>/mcp` and nothing else. Claude registers itself, sends the
  person to sign in with an admin account, and asks to be allowed. No token.
- **Claude Code**: `claude mcp add --transport http toolsite https://<host>/mcp`,
  then `/mcp` to sign in. Or add it with an `Authorization: Bearer` header
  carrying `TOOLSITE_MCP_TOKEN`.
- **The CLI**: a static token only, `TOOLSITE_TOKEN=<TOOLSITE_MCP_TOKEN>`.

Only an admin account may connect a publishing client; a visitor account is
told no.

## Server environment

| Variable | Required | Description |
|---|---|---|
| `TOOLSITE_BASE_URL` | if clients sign in | Where the outside world reaches the server, e.g. `https://host.com`. Turns the OAuth server on and is what upload URLs are built from. Without it, URLs come back relative. |
| `TOOLSITE_MCP_TOKEN` | if clients don't sign in | Static token for `/mcp` and the CLI. `Authorization: Bearer <token>`; `x-api-key` also accepted. |
| `TOOLSITE_DATA_DIR` | no (default `/data`) | Where everything is stored. |
| `TOOLSITE_DEFAULT_ACCESS` | no (default `public`) | The gate an app has until it sets its own: `public`, `authenticated` or `granted`. |
| `TOOLSITE_LOGIN_<SLUG>_CLIENT_ID` / `_CLIENT_SECRET` | no | A sign-in provider. Presets `GOOGLE`, `GITHUB`, `MICROSOFT`, `ENTRA` (needs `_TENANT`); any other slug needs `_ISSUER`. Optional `_NAME`, `_ALLOW_DOMAIN`. Redirect URI to register: `https://<host>/auth/callback/<slug>`. |
| `TOOLSITE_MAX_DB_MB` / `TOOLSITE_MAX_BLOB_MB` | no (default `4096`) | Ceilings on one SQLite file and one stored file, in MB. `0` means none. |
| `TOOLSITE_BLOB_S3_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY`, `_REGION` | no | Store apps' files in an S3-compatible bucket instead of on the volume. A Railway bucket's unprefixed `ENDPOINT`, `BUCKET`, `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY`, `REGION` are accepted too. `TOOLSITE_BLOB_S3_PATH_STYLE=1` for path-style buckets. |
| `TOOLSITE_GITHUB_APP_ID`, `TOOLSITE_GITHUB_APP_PRIVATE_KEY`, `TOOLSITE_GITHUB_APP_SLUG`, `TOOLSITE_GITHUB_WEBHOOK_SECRET` | no | A GitHub App, so apps can live in repositories. `/admin/github` walks through registering one. `TOOLSITE_GITHUB_API` for GitHub Enterprise. |
| `TOOLSITE_SECRET_KEY` | no | Base64, 32 bytes. Encrypts app settings. Generated beside the data when unset, which is weaker. |
| `PORT` | no (default `8080`) | Port to listen on. |
| `RUST_LOG` | no (default `info`) | Log filter. |

Older names still answer (`BEARER_TOKEN`, `TOOLSITE_TOKEN`, `PUBLIC_BASE_URL`,
`DATA_DIR`). `TOOLSITE_MCP_OAUTH_CLIENT_ID` / `_SECRET` configured an earlier
single-user OAuth shim that signing in replaced; the secret is still accepted
as a bearer token. There is no `DATABASES` switch: every app has a database.

Boot logs the effective configuration, so a misconfigured deploy is visible from
the logs alone:

```
INFO toolsite: auth configuration bearer_auth=false oauth_auth=true base_url="https://host.com" default_access=granted
INFO toolsite: storage configuration (0 MB means no ceiling) blobs="s3" max_db_mb=4096 max_blob_mb=4096
```

Rejected requests are logged at `warn` with the headers that arrived, never the
token, never file contents, so a client stuck on 401 is diagnosable from the
deploy log.

## Troubleshooting

| Symptom | Cause |
|---|---|
| Page 200s but renders blank | Base path not set at build time; assets 404. Check `curl -I <page-url>/assets/<file>`. |
| Deep route 404s, app root works | Bundle uploaded without `&spa`, or the router's `basename` is unset. |
| An app you expected open answers 303 to `/auth/login` | It follows a site default of `granted` or `authenticated`. Set `gate = "public"` if it should be open. |
| An app is missing from the index | Hidden, unlisted, or gated past the viewer. The admin's `/admin/apps` lists everything. |
| `curl` to the upload URL hangs or fails to connect | The sandbox can't reach the host; fall back to `push_page` / `push_app`. |
| Upload rejected wholesale | A tar entry had `..` or a leading `/`. Rebuild the archive from inside `dist`. |
| Fewer files landed than expected | Symlinks and dotfiles are skipped; the upload response says which. |
| `unknown upload flag` | A flag the URL does not take. The response lists the ones it does. |
| `database or disk is full` | The app's database hit `TOOLSITE_MAX_DB_MB`. |
| `blobs::get` returns `too-large` | Over 16 MB. Serve it with `x-toolsite-blob` instead of reading it into the guest. |
| `fetch::send` refused | Host not in `allow_http`, or it resolved to a private address. |
| Handler upload rejected | Not a valid `wasm32-wasip2` component, or `export!` is missing. |
| Handler returns 500 on every request | Usually the fuel/deadline ceiling: an unbounded loop or query. The Jobs tab shows a job's last status. |
| State resets between requests | Expected. Instances are never reused; keep state in the database or in files. |
| `app_repo` says no GitHub App is configured | The deployment has no `TOOLSITE_GITHUB_*`. Deploy with the CLI and tell the owner `/admin/github` sets it up. |
