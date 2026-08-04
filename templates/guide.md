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

Any other flag is refused rather than guessed at. Order matters: migrations
and the manifest first, so an app is never briefly live without its tables or
its gate.

## What a visitor can reach

Only what the bundle contained. The project, the notes, the settings, the
schema and the metadata all live beside the app and are served by nothing.

Requests resolve in a fixed order:

1. `/p/<app>/api/...` — the app's handler, always. The prefix is reserved.
2. an exact file from the bundle — static, no wasm runs.
3. no file but a handler exists — the handler, so it can render its own routes.
4. no file, no handler, `spa` set — the app's `index.html`.
5. otherwise 404.

A handler sees the path relative to its app **with `/api` still attached**, so
strip that prefix yourself.

## Handlers

A wasm component built for `wasm32-wasip2` against `<server>/wit/toolsite.wit`.
Start from `<server>/scaffold/<app>`, which is a crate that builds unmodified.

It gets four capabilities and nothing else:

- `db.query` — this app's own SQLite. Parameters are bound; there is no
  string-building entry point.
- `identity.current-user` / `current-role` — established by the host from a
  verified session. A guest cannot forge either.
- `secrets.get` — settings the owner entered. Never in the bundle.
- `fetch.send` — only hosts the app declared in `allow_http`.

No filesystem. No environment. No sockets beyond that allowlist. **And no
clock**: `std::time` will not link. Take timestamps from SQLite instead:

    select cast(strftime('%s','now') as integer)

Every request runs in a fresh instance with a fuel ceiling, a memory cap and a
wall-clock deadline. State must live in the database — a global does not
survive the request that set it.

The host sets `x-toolsite-scheduled` on a job run. Client copies of any
`x-toolsite-*` header are stripped, so it means what it says.

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
gate = "public"            # or authenticated, granted
icon = "🧺"
allow_http = ["api.example.com"]

[[route]]                  # note the singular; unknown keys are refused
path = "/admin"
gate = "granted"

[[job]]                    # six cron fields, seconds first
name = "refresh"
schedule = "0 */5 * * * *"
path = "/api/refresh"
```

Routes and jobs are replaced wholesale, so deleting a line removes the thing.
Anything the file does not mention is left alone.

## Access

A gate decides whether a request arrives:

| Gate | Who |
|---|---|
| `public` | anyone |
| `authenticated` | any signed-in account |
| `granted` | accounts given access to that app |

`[[route]]` applies a gate to a path prefix, longest match winning, so a
public page and a private one live in one app. Past the door it is the app's
call: read `identity::current-role()` and decide what "editor" means. The
platform never interprets a role.

There is no public signup. Accounts are created by the owner, and a person
sets their own password through a one-time link.

## Settings

`app_settings(app, name, value)` writes one; `link: true` returns a URL the
owner opens to paste values in themselves. Prefer the link — a secret that
never enters a conversation cannot leak from one. Values are encrypted at
rest and never come back out: listings give names only.

## Before saying it works

Fetch the thing. A page that returns 200 with its assets 404ing renders blank
and looks like a success — the usual cause is a build whose base path is not
`/p/<slug>/`.

    curl -I <page-url>/assets/<a-built-file>
    curl <page-url>/api/<a-route>
