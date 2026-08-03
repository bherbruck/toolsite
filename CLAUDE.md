# toolsite

MCP server + web host in one container. An agent publishes HTML, a multi-page
app, or a compiled front-end bundle and gets a working URL back. See README.md
for the user-facing surface.

## Design principles

**Abstract, high cohesion, low coupling.** Each module owns one concern and
exposes the smallest surface that lets the others do their job. If a change
forces edits across three modules, the boundary is in the wrong place.

Two more that this codebase already leans on:

- **Nothing destroys data.** There is no delete tool by design; retraction is
  a visibility flag, so every action is reversible.
- **Capabilities are granted, never assumed.** A guest can only do what the
  linker hands it. wasi is linked (a wasm32-wasip2 guest imports it via std
  whether it uses it or not), so the sandbox is the *context*, which grants no
  directory, no environment, no sockets. Tests prove each denial rather than
  trusting the config.
- **Page content never passes through the model.** Tools hand back an upload
  URL; the agent writes a file and curls it. Inline-HTML tools exist only as a
  fallback for clients with no shell, and say so in their descriptions.

## Module layout

```
main.rs            startup: env -> Config, listener, --stdio
lib.rs             build_router: every route, assembled in one place
config.rs          Config shared by every layer

platform/          the site as its owner uses it
  admin.rs         /admin: accounts, gates and grants for whoever runs it
  mcp.rs           MCP tool definitions and the ServerHandler
  bearer.rs        bearer/x-api-key middleware for /mcp
  client_oauth.rs  OAuth shim for MCP clients — who may PUBLISH
  upload.rs        upload tickets and the PUT endpoints they authorise

content/           what gets published, and how it is served
  slug.rs          naming rules (what may become a path), tokens, escaping
  store.rs         page/icon/meta paths, titles, visibility, listing
  bundle.rs        tar unpacking, entry classification, traversal defence
  serve.rs         the public site: pages, assets, handler dispatch, index

runtime/           executing an app's own code and data
  wasm.rs          engine, guards, host imports
  db.rs            per-app SQLite and the authorizer keeping apps apart

accounts/          people who USE published apps
  users.rs         accounts, sessions, grants, sign-in routes

wit/               the contract guests compile against
cli/               the `toolsite` command (standalone crate)
```

Two auth systems live here and must never be conflated: `platform/` decides
who may publish, `accounts/` decides who may visit. They were adjacent files
called `auth.rs` and `users.rs` once, which invited exactly that mistake.

Dependency direction is one-way: `platform` and `content` depend on
`runtime` and `accounts`; everything depends on `config` and `content::slug`.
Nothing in `runtime` reaches back up into HTTP types.

## Conventions

- Storage is plain files under `DATA_DIR`; there is no database. A page is
  `<slug>.html`, its icon `<slug>.icon`, its state `<slug>.meta`. An app is a
  directory whose `index.html` serves at the app root.
- Slugs are validated before they touch the filesystem (`slug.rs`). Page slugs
  allow only `[A-Za-z0-9_-]` per segment; bundle asset paths additionally
  allow `.` inside a segment but never at the start, which rules out `..` and
  dotfiles at once. Never join user input to `DATA_DIR` without one of these.
- Anything that rejects a request should log why, at `warn`, including the
  headers that arrived — but never a token, and never file contents.

## Transports

The same `PageHost` serves two ways: HTTP (`POST /mcp`, token or OAuth) and
stdio (`--stdio`, for local clients). In stdio mode stdout is the protocol
channel, so logging must go to stderr — anything printed to stdout corrupts
the stream.

## Testing

`cargo test`. The crate is split so this is possible: `lib.rs` owns
`build_router`, `main.rs` only reads the environment and serves. Tests drive
the real router in-process with `tower`'s `oneshot`, so every layer runs
without binding a socket.

- **Unit tests** live beside the code they cover (`#[cfg(test)] mod tests`),
  since they need `pub(crate)` items: slug rules, bundle unpacking, database
  isolation.
- **Integration tests** live in `tests/http.rs` and go through the router.

Two conventions worth keeping:

- **Forge attacks, don't trust libraries to forge them for you.** The `tar`
  crate refuses to *write* `..` paths, so the traversal fixtures build tar
  headers by hand — an attacker is not constrained by our tar library either.
- **Name the property, not the mechanism.** `attach_is_refused_so_sql_cannot_
  reach_another_app` says why it exists; `test_authorizer` doesn't.

Requests to `/mcp` in tests must carry a `Host` header — rmcp's transport
enforces one as DNS-rebinding protection, and `oneshot` doesn't add it.

Anything touching upload, serving, or SQL needs a test for the security
property it rests on, not just the happy path.

**Wasm fixtures.** `tests/fixtures/handler.wasm` is a committed build of
`tests/fixtures/guest`, so the suite needs no wasm toolchain. Rebuild it with
`scripts/build-fixtures.sh` after any change to `wit/toolsite.wit` or the
guest — a stale fixture is how a breaking WIT change slips through. Low-level
limit tests use the `wat` crate inline instead, which needs no toolchain at
all.
