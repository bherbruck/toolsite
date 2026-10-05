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

The host sets `x-toolsite-scheduled` on a job run. Client copies of any
`x-toolsite-*` header are stripped, so it means what it says.

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
  the route is gated: answering is the permission.
- **Small things from inside.** `put`, `get`, `stat`, `list(prefix)` and
  `delete`. `get` refuses anything over 16 MB rather than truncating it;
  serve those with the header.

Seeding from a shell: `curl -f -T file '<upload-url>?blob=<key>'`, up to 64 MB
per PUT, typed by the key's extension.

The ceiling per file is the deployment's, a few GB by default. Where the
bytes live, the volume or a bucket, is not the app's concern.

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
roles = ["viewer", "editor"]  # what the handler checks; a hint for whoever grants

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

Publishing may be limited to a folder. Apps sit in a project tree, and the
account behind your connection may hold editor on `ops/yard` and nothing
elsewhere. A tool that is asked to touch an app outside that refuses with the
folder and the scope it would take, for example "holds editor at ops/yard;
this needs admin". Ask the person for the scope, or publish under a folder you
hold; `list_pages` shows what you may open.

A gate decides whether a request arrives:

| Gate | Who |
|---|---|
| `public` | anyone |
| `authenticated` | any signed-in account |
| `granted` | accounts with a grant on the app, or a scope on it or a folder above it |

An admin account passes every gate. An app that names no gate follows the site's default, which the owner set
for the whole deployment; on an internal site that is usually `granted`.
Say `gate = "public"` only when the app really should be open to anyone.

`[[route]]` applies a gate to a path prefix, longest match winning, so a
public page and a private one live in one app. Past the door it is the app's
call: read `identity::current-role()` and decide what "editor" means. The
platform never interprets a role. Declare the roles your handler checks
(`roles = ["viewer", "editor"]` in toolsite.toml) so whoever grants access
can pick the right word; it is a hint, and any role can still be granted.

There is no public signup. Accounts are created by the owner, and a person
sets their own password through a one-time link.

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

Three facts to write policies by:

- Against a NULL identity a `where` matches nothing. `current_user()` is
  NULL for a job and for an anonymous visitor, and `owner_id = NULL` is not
  true. Never write `or current_user() is null` to "let the job see
  everything": it lets everyone with no identity see everything.
- A row with a NULL owner is nobody's.
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
