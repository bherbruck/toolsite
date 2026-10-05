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

It gets five capabilities and nothing else:

- `db.query` — this app's own SQLite. Parameters are bound; there is no
  string-building entry point.
- `blobs` — this app's own files. See Files, below.
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

## Files

Uploads, images, exports, datasets: anything too large or too opaque for a
row is a blob. One namespace per app, keyed like a path (`photos/cat.jpg`),
with the same rules as a bundle path — no segment may start with `.`, so
`..` is refused before anything touches storage.

The handler decides; the platform moves the bytes. A request body into a
handler is capped at 8 MB and a blob may be gigabytes, so neither direction
goes through guest memory:

- **Taking a file from a browser.** Call `blobs::upload_url(key, max_bytes)`
  and hand the URL to the page. The browser `PUT`s the file there with its
  content type; the URL works once and expires in fifteen minutes. Decide who
  gets a URL the way you decide anything else — it is the credential.
- **Sending one.** Answer with the header `x-toolsite-blob: <key>` and an
  empty body. The platform streams the file in its place, with the stored
  content type unless you set one, and keeps your other headers — so
  `content-disposition` and `cache-control` are yours to add. Gate it however
  the route is gated: answering is the permission.
- **Small things from inside.** `put`, `get`, `stat`, `list(prefix)` and
  `delete`. `get` refuses anything over 16 MB rather than truncating it;
  serve those with the header.

Seeding from a shell: `curl -f -T file '<upload-url>?blob=<key>'`, up to 64 MB
per PUT, typed by the key's extension.

The ceiling per file is the deployment's, a few GB by default. Where the
bytes live — the volume, or a bucket — is not the app's concern.

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

An admin account passes every gate. An app that names no gate follows the site's default, which the owner set
for the whole deployment; on an internal site that is usually `granted`.
Say `gate = "public"` only when the app really should be open to anyone.

`[[route]]` applies a gate to a path prefix, longest match winning, so a
public page and a private one live in one app. Past the door it is the app's
call: read `identity::current-role()` and decide what "editor" means. The
platform never interprets a role.

There is no public signup. Accounts are created by the owner, and a person
sets their own password through a one-time link.

## A repository

An app can live in GitHub and deploy from there: `app_repo(app, "create")`
makes a repository out of the source you published with `?source`, with a
workflow that builds in GitHub Actions and deploys back here on every push;
`app_repo(app, "import", repo: "owner/name")` connects a repository that
already exists. The site has to be configured with a GitHub App for either;
`app_repo(app, "status")` says. Nothing is built on this server: keep
publishing the source, and the repository carries it from there.

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
