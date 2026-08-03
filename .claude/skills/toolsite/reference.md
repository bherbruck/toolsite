# toolsite reference

Detail behind [SKILL.md](SKILL.md). Read this when writing a handler, debugging
a 404, or standing up a deployment.

## WIT surface

The full contract is [`wit/toolsite.wit`](../../../wit/toolsite.wit). A handler
implements the `app` world: it imports `db` and `identity`, and exports
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

One statement per call. There is no string-building entry point on purpose —
values are bound to `?` placeholders, never interpolated. Blobs are absent for
now; adding a `value` case later is backwards compatible.

`truncated` lets a guest tell a short result from a capped one. `ATTACH` comes
back as `denied`, which is enforced by the host's SQLite authorizer, not by the
guest.

### `identity`

```wit
record user { id: string, email: string }
current-user: func() -> option<user>;
```

Derived from a session cookie the host verified, so a guest cannot forge it.
`none` means anonymous.

### `http`

```wit
record request  { method, path, query, headers: list<tuple<string,string>>, body: list<u8> }
record response { status: u16, headers: list<tuple<string,string>>, body: list<u8> }
```

`path` is relative to the app — the `/p/<app>` prefix is already stripped, but
`/api` is not.

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
  `toolsite::app::{db, identity}`.
- Implement `impl Guest for YourType`, then `export!(YourType)` at the end of
  the file. Missing the `export!` produces a component with no `handle` export,
  which the server rejects at upload.
- The guest crate must be standalone (`[workspace]` in its own `Cargo.toml`) —
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
| `attach database '../other/data.db'` | `db::Error::Denied` |
| Infinite loop | Killed at the fuel/deadline ceiling; request returns 500 |

wasi is linked because a `wasm32-wasip2` guest imports it through std whether it
uses it or not. The sandbox is the *context*, which grants no directory, no
environment and no sockets.

## Storage layout

Everything under `DATA_DIR` is plain files — there is no database for pages.

```
budget-2026.html          a single page          -> /p/budget-2026
budget-2026.icon          its icon               -> /icon/budget-2026
budget-2026.meta          {"listed":true,...}
myapp/index.html          app root               -> /p/myapp/
myapp/about.html          a page of the app      -> /p/myapp/about
myapp/assets/main.js      a bundle asset         -> /p/myapp/assets/main.js
```

Page slugs allow only `[A-Za-z0-9_-]` per segment, joined by `/`, so a slug can
never escape `DATA_DIR`. Bundle asset paths additionally allow `.` inside a
segment but never at the start of one, which rules out `..` and dotfiles at
once.

An app root redirects `/p/<slug>` to `/p/<slug>/` so relative links resolve.

## The index

`GET /` lists published pages, newest first. Multi-page apps and bundles appear
once, as their root. Hidden and unlisted pages don't appear. There's a
client-side filter over slugs and titles.

- **Title** — the page's own `<title>` (first 8 KB scanned). Pages without one
  are listed by slug.
- **Icon** — in priority order: an uploaded image (`?icon`), then an emoji /
  inline SVG / `data:` URI from `set_icon`, then a generated badge of the slug's
  initials on a hash-derived colour, stable forever.

## Endpoints

| Route | Auth | Purpose |
|---|---|---|
| `POST /mcp` | token or OAuth | The MCP server. Streamable HTTP — no `/sse` suffix. |
| `PUT /upload/<ticket>[/<page>]` | ticket | Write a page. `?icon`, `?bundle`, `&spa`, `?handler`. 64 MB. |
| `ANY /p/<slug>` | public | The page, a bundle asset, or the app's handler. |
| `GET /icon/<slug>` | public | A page's icon, if set. |
| `GET /` | public | The index. |

`GET /mcp` on its own returns `400 Session ID is required`. That is normal for
Streamable HTTP — the session is issued by `initialize`.

## Server environment

| Variable | Required | Description |
|---|---|---|
| `BEARER_TOKEN` | if not using OAuth | Static token for `/mcp`. Sent as `Authorization: Bearer <token>`; `x-api-key` also accepted. |
| `OAUTH_CLIENT_ID` / `OAUTH_CLIENT_SECRET` | if using OAuth | For clients that require a full OAuth flow. |
| `PUBLIC_BASE_URL` | if using OAuth | e.g. `https://host.com`. A bare host gets `https://` prepended. Without it, published URLs come back relative. |
| `DATABASES` | no (default off) | `on`/`1`/`true`/`yes` enables per-app SQLite. `run_sql` and `db.query` need it. |
| `DATA_DIR` | no (default `/data`) | Where pages are stored. |
| `PORT` | no (default `8080`) | Port to listen on. |
| `RUST_LOG` | no (default `info`) | Log filter. |

Boot logs the effective configuration, so a misconfigured deploy is visible from
the logs alone:

```
INFO toolsite: auth configuration bearer_auth=true oauth_auth=true base_url="https://host.com"
```

Rejected requests are logged at `warn` with the headers that arrived — never the
token, never file contents — so a client stuck on 401 is diagnosable from the
deploy log.

## Troubleshooting

| Symptom | Cause |
|---|---|
| Page 200s but renders blank | Base path not set at build time; assets 404. Check `curl -I <page-url>/assets/<file>`. |
| Deep route 404s, app root works | Bundle uploaded without `&spa`, or the router's `basename` is unset. |
| `curl` to the upload URL hangs or fails to connect | The sandbox can't reach the host — fall back to `push_page` / `push_app`. |
| Upload rejected wholesale | A tar entry had `..` or a leading `/`. Rebuild the archive from inside `dist`. |
| Fewer files landed than expected | Symlinks and dotfiles are skipped; the upload response says which. |
| `run_sql` says databases are off | Server started without `DATABASES=on`. |
| Handler upload rejected | Not a valid `wasm32-wasip2` component, or `export!` is missing. |
| Handler returns 500 on every request | Usually the fuel/deadline ceiling — an unbounded loop or query. |
| State resets between requests | Expected. Instances are never reused; keep state in the database. |
