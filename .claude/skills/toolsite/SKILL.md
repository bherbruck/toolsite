---
name: toolsite
description: Use when publishing a page, site, or app to toolsite, the self-hosted MCP server and web host, covering single HTML files, multi-page apps, built front-end bundles, wasm request handlers, per-app databases and files, and apps that live in a GitHub repository.
---

# Publishing to toolsite

toolsite is one program that is both an MCP server and a web host. You publish
a page and get back a URL. Pages live at `/p/<slug>`, apps at `/p/<slug>/`.
Each app gets its own SQLite database, its own file storage, settings its
handler reads, scheduled jobs, and a place in GitHub if you want one.

## Pick the path

Decide by what the thing does, not by what is quickest to start:

| What you're publishing | How |
|---|---|
| **Anything with state, forms, or more than one screen** | **`toolsite init <name> --react`**: Vite + React + Tailwind, base path already right |
| One static page: a document, a chart, a readout | Write the HTML, then `curl -fT page.html <upload-url>` |
| A front-end project you already have | `toolsite deploy --slug <slug> --spa`; it installs and builds |
| Needs server-side data or logic | Add `--handler`, see [Server-side handlers](#server-side-handlers) |
| Something that should keep deploying from a repository | Publish once, then `app_repo(app, "create")`, see [A repository](#a-repository) |
| No shell available (claude.ai web) | Fall back to the `push_page` / `push_app` MCP tools |
| A shell that cannot reach the host (ChatGPT's sandbox) | Build there, then send the file with `upload_begin` / `upload_chunk` / `upload_finish` in base64 chunks |

**Reach for `--react` by default for an app.** Hand-writing one big
`index.html` looks cheaper because it starts with no setup, and that is the
only point at which it is cheaper. By the third feature you are editing
markup with string replacement, re-sending the whole file on every change,
and hand-rolling what `useState` does for free. The scaffold below installs
and builds unmodified, so the setup cost you are avoiding is one command:

```bash
toolsite init pantry --react --handler
cd pantry && toolsite deploy
```

`deploy` runs the install and the build itself, so that is the whole flow:
two commands, fewer than hand-writing a page and then fixing it twice.

Vanilla HTML is the right answer for a genuinely static page. It is not the
right answer for an app, and "there is no build step" is not a reason. There
is a build step available and it costs 30 seconds.

## The cardinal rule

**Never paste page HTML into a tool call when you have a shell.** Always:

```
write file  ->  create_upload  ->  curl -fT
```

The bytes go from disk to the server without passing through the model. A 40 KB
page pasted into a tool call costs ~10k tokens and buys nothing; `create_upload`
exists precisely so that never has to happen. `push_page` / `push_app` are the
fallback for clients with no shell, not a shortcut. Don't read a published page
back into the conversation either; `curl` it to a file and edit that.

## Before you start

- Check the environment: `npm --version; cargo --version; curl -sI <server>/guide`.
  A React or wasm build needs npm or cargo; uploading needs a route to the
  host or the inline upload tools. Without a build tool, do what still works:
  plain HTML with `push_page`, SQL, accounts, access, exports, repositories,
  notes. Say what you have before promising an app.
- `list_pages` first, so you do not reuse a slug by accident.
- `app_notes(slug)` for the app you are about to change. A bundle cannot be
  turned back into its source, so the notes may be the only record of why it
  is built the way it is.
- `GET <server>/guide` is how the platform works, kept current. Fetch it
  before building a handler, a schema or a gate rather than inferring
  conventions from a neighbouring app.

## The CLI (preferred when installed)

The repo ships a CLI at `./cli`. Check for it first: `command -v toolsite`.
Install with `cargo install --path cli` (binary name `toolsite`).

| Command | Does |
|---|---|
| `toolsite init <name> [--react] [--spa] [--handler]` | Scaffold an app: `toolsite.toml`, a web root, and with `--handler` a ready-to-build `handler/` crate with the WIT already copied in |
| `toolsite deploy [dir] [--slug <slug>] [--spa] [--no-build] [--without-source]` | Build, apply migrations and `toolsite.toml`, upload the bundle, the handler, the notes and the source, then verify the live URL |
| `toolsite fetch [--slug <slug>] [dir]` | Unpack the project a previous deploy kept with the app |
| `toolsite sql <app> "<sql>" [--param v]` | Run SQL against that app's database. Repeat `--param` per placeholder; digits and `true`/`false`/`null` bind as those types, everything else as text |
| `toolsite list [--all]` | What is already published |
| `toolsite gate <app> <public\|authenticated\|restricted\|default> [--path /prefix]` | Who may reach an app, or one path within it |
| `toolsite grant <app> <email> [--role r]` / `toolsite revoke <app> <email>` | Access to an app whose access is `restricted` |
| `toolsite job <app> [name] [--schedule c] [--path p] [--now] [--remove]` | Scheduled work; omit the name to list |
| `toolsite secret <app> [name] [--value v] [--link] [--remove]` | Settings the handler reads; `--link` prints a URL for the owner to paste values into |
| `toolsite notes <slug> [--file notes.md]` | Read or write the notes kept with an app |
| `toolsite user add <email> [--admin] [--password p]` | Create an account; prints a one-time setup link unless a password is given |
| `toolsite hide <slug>` / `toolsite unhide <slug>` | Retract and restore, reversibly |
| `toolsite remove <slug> [--page-only]` | Take a slug down for good; files move to `.trash/` on the server |

Config comes from `TOOLSITE_URL` and `TOOLSITE_TOKEN`, or `--url` / `--token`.
The CLI needs a static token (`TOOLSITE_MCP_TOKEN` on the server); it does
not sign in. An MCP client such as Claude signs in with an admin account and
needs no token at all.

```bash
export TOOLSITE_URL=https://yourdomain.com
export TOOLSITE_TOKEN=<TOOLSITE_MCP_TOKEN>

# From scratch: writes the project and toolsite.toml, deployable as-is.
toolsite init dashboard --react --handler
cd dashboard && toolsite deploy

# An existing front-end project: build it yourself first.
npm run build && toolsite deploy --slug dashboard --spa --no-build
```

What `deploy` decides for you:

- **Slug**: `--slug`, else `slug` in `toolsite.toml`, else the directory name.
- **Web root**: the first of `dist/`, `build/`, `public/` that contains an
  `index.html`, else the directory itself. A project with a `package.json` is
  built first; `--no-build` skips that.
- **Handler**: a prebuilt `handler.wasm` in the directory, else it runs
  `cargo build --release --target wasm32-wasip2` on `handler/Cargo.toml` if that
  exists. Neither present means no handler; that is not an error.
- **spa**: `--spa` or `spa = true` in `toolsite.toml`.
- **Source**: the project is kept with the app unless `--without-source`.

After uploading it GETs the page, fails if that isn't a success, and warns if
`index.html` still references `/assets/…` from the domain root, the blank-page
failure below. Without the CLI, use the MCP tools plus `curl`; both paths hit
the same endpoints.

## The manual path

1. `list_pages` first.
2. `create_upload(slug)` returns an upload URL carrying a one-off ticket,
   valid 15 minutes, reusable within that window, and able to write only to its
   own slug. The response also prints the base path with the real slug filled in.
3. `curl` the files up.

```bash
curl -fT page.html  <upload-url>                              # single page
curl -fT index.html <upload-url>/index                        # a page of an app
curl -fT about.html <upload-url>/about
curl -fT logo.png   '<upload-url>?icon'                       # index icon
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle'     # static build
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle&spa' # client-side router
curl -f -T handler.wasm '<upload-url>?handler'                # server-side code
tar -czf - -C migrations . | curl -f -T - '<upload-url>?migrations'   # schema
curl -f -T toolsite.toml '<upload-url>?manifest'              # settings
curl -f -T photo.jpg '<upload-url>?blob=photos/cover.jpg'     # a file the app keeps
tar -czf - --exclude node_modules --exclude target . | curl -f -T - '<upload-url>?source'
```

Those are every flag the URL takes: `?bundle`, `&spa`, `?handler`,
`?migrations`, `?manifest`, `?icon`, `?source`, `?blob=<key>`. No flag means
"publish the body as a page". An unknown flag is refused, not guessed at.

`?bundle` unpacks a gzipped tar; a single shared top-level directory is
stripped, so `tar -czf - dist` works too. `&spa` makes paths matching no file
fall back to the app's `index.html`; without it they 404. Limits: 64 MB per
PUT, 128 MB unpacked, 2000 files. Paths containing `..` or a leading `/`
abort the whole upload; symlinks and dotfiles are skipped and the response says
so.

To edit an existing page: `curl <page-url> -o page.html`, edit, re-upload to the
same slug. Without a shell, `pull_page` / `pull_app` do the same read.

## The base path trap

This is the most common failure, and it looks like success. **Apps are served
from `/p/<slug>/`, never the domain root.** A default Vite/Next/CRA config emits
absolute `/assets/...` URLs. Those 404, so the HTML loads with a 200 and the
page renders **blank**.

Set this *before* building:

| Build tool | Setting |
|---|---|
| Vite | `base: '/p/<slug>/'` in `vite.config` |
| Next | `basePath: '/p/<slug>'`, `assetPrefix: '/p/<slug>/'` in `next.config` |
| CRA | `"homepage": "/p/<slug>/"` in `package.json` |
| Client router | `basename: '/p/<slug>'` (e.g. `createBrowserRouter(routes, { basename: '/p/<slug>' })`) |

Relative (`base: './'`) also works for a static multi-page bundle, but breaks on
deep client-side routes; prefer the absolute form for anything with a router.

Then verify the assets actually resolve:

```bash
curl -I https://yourdomain.com/p/<slug>/assets/<a-built-file>   # expect 200
```

## MCP tools

For when the CLI isn't installed, or there is no shell at all.

| Tool | Use |
|---|---|
| `create_upload(slug?)` | The default. Returns the upload URL and the base path to build for. |
| `list_pages(include_all?)` | Slug, title, URL, last modified, visibility. Newest first. |
| `set_visibility(slug, hidden?, listed?, gate?, path?)` | `hidden: true` 404s the URL. `listed: false` keeps it live but off the index. `gate` is `public`, `authenticated`, `restricted` (once `granted`), or `default` (follow the site); with `path` it guards one part of an app. |
| `set_icon(slug, icon)` | Emoji, inline `<svg>`, or `data:` URI. Optional; pages without one get a generated badge. |
| `run_sql(app, sql, params?)` | Schema and seed work against one app's database. MCP only; never reachable from a published page. |
| `app_migrations(app, files?)` | The app's schema as numbered `.sql` files, applied once each in order. Omit `files` to see what ran. |
| `app_jobs(app, name?, schedule?, path?, run_now?)` | Scheduled work. Cron with seconds first; fires the app's own handler at a path. |
| `app_settings(app, name?, value?, link?)` | API keys the handler reads. Pass `link: true` for a URL the owner pastes into; never ask for a secret directly. |
| `app_notes(slug, notes?)` | Markdown kept with an app for the next session. Reads when `notes` is omitted. |
| `app_repo(app, action, repo?, branch?, directory?, installation?, public?)` | `status`, `installations`, `discover`, `create`, `import`, `sync`, `disconnect`. See [A repository](#a-repository). |
| `app_deploy_tokens(app, action, label?, id?)` | A token that publishes one app only, for a pipeline that is not GitHub: `create`, `list`, `revoke`. |
| `app_exports(app, action, label?, id?)` | A read-only token for `GET <site>/export/<app>.sqlite`, a snapshot of the whole database for a reporting tool. |
| `projects(action, path?, name?, app?, email?, scope?)` | Projects and who may act in them: `list`, `create` (admin at the parent), `move` an app (admin at both ends, the target must exist), `permissions`, `grant`, `revoke` (admin there, never more than you hold). The same rules as the app browser. |
| `create_user(email, password?, admin?)` | An account. Leave the password out and the reply carries a one-time setup link for them. |
| `set_access(app, email, allow?, role?)` / `set_user_active(email, active)` | Access on a `restricted` app, and disabling an account. |
| `push_page(html, slug?)` / `push_app(app, pages)` | No-shell fallbacks, HTML inline. A page named `index` also serves at the app root. |
| `upload_begin(slug, kind, …)` / `upload_chunk(id, index, data)` / `upload_finish(id, chunks)` | The upload URL's kinds (`bundle`, `handler`, `migrations`, `manifest`, `source`, `icon`, `blob`, `page`) sent inline as base64 chunks of at most 768 KB decoded, for a sandbox that cannot reach the host. Same rules, same reply. |
| `pull_page(slug)` / `pull_app(app)` | Read a page back for editing. With a shell, `curl` the public URL instead. |
| `remove_page(slug, confirm, page_only?)` | Takes a slug down for good. Files move to `.trash/` on the server, never deleted. Prefer `set_visibility`. |

Retract with `set_visibility(slug, hidden: true)`, which is instantly
reversible with `hidden: false`. `remove_page` is for junk: a probe published
as a page, an app nobody wants.

## Server-side handlers

An app can ship `handler.wasm`: a component built for `wasm32-wasip2` against
[`wit/toolsite.wit`](../../../wit/toolsite.wit), uploaded with
`curl -f -T handler.wasm '<upload-url>?handler'`. Invalid components are
rejected at upload. `GET <server>/scaffold/<app>` is a crate that builds as-is.

**Routing order**, fixed:

| # | Condition | Result |
|---|---|---|
| 1 | `/p/<app>/api/...` | The handler, always. The prefix is reserved, so no file can shadow it. |
| 2 | An exact file on disk | Served statically, no wasm involved. |
| 3 | No file, handler exists | The handler, so it can render routes server-side. |
| 4 | No file, no handler, `spa` set | The app's `index.html`. |
| 5 | otherwise | 404. |

The guest sees the path relative to its app **with `/api` still attached**:
`/api/echo`, not `/p/myapp/api/echo`. Strip that prefix yourself.

**Capabilities.** A handler gets five imports and nothing else:

- `db.query`: this app's own SQLite, parameters bound rather than interpolated.
- `blobs`: this app's own files. See [Files](#files).
- `identity.current-user` / `current-role`: who is here, established by the
  host from a verified session. A guest cannot forge either.
- `secrets.get` / `names`: settings the owner entered. Never in the bundle.
- `fetch.send`: only hosts the app declared in `allow_http`.

No filesystem, no environment, no sockets beyond that allowlist. wasi is
linked because a `wasm32-wasip2` guest imports it through std, but the context
grants nothing.

**Scopes.** Apps sit in a project tree and the account behind your connection
may hold `editor` on one folder only. A refused tool names the folder and the
scope it needs ("holds editor at ops/yard; this needs admin"). Publish under a
folder you hold, or ask for the scope; `list_pages` shows what you may open.

**Access.** A gate decides whether a request arrives; what it may then do is
yours to decide. An app that names no gate follows the site's default
(`TOOLSITE_DEFAULT_ACCESS`), which on an internal site is usually `restricted`,
so say `gate = "public"` only when the app really should be open to anyone.
An admin account passes every gate. Guard part of an app with a `[[route]]`
or `set_visibility(slug, gate, path)`, longest matching prefix wins, and
inside the handler read `identity::current-role()`, which returns whatever the
owner granted (`viewer`, `editor`, anything). The platform never interprets a
role. Declare the roles your handler checks in `toolsite.toml`
(`roles = ["viewer", "editor"]`) so whoever grants access can pick the right
word; it is a hint, and any role can still be granted.

**Row-level access.** Declare it in `toolsite.toml`, do not write it:

```toml
[[access.table]]
table = "orders"
where = "owner_id = current_user()"   # any table in the app's db may appear here
owner = "owner_id"
write = true
```

The platform generates the view `my_orders` and the triggers that keep writes
inside it. `current_user()`, `current_email()` and `current_role()` are bound
on every connection. A regular account then queries the view as themselves
through `<site>/me/mcp` (`my_apps`, `query`); a handler can offer the same
with `db::query-scoped`. For site or team scoping, read the attribute from a
membership table in the `where`. Prove it before claiming it:
`run_sql(app, sql, as_user: "someone@x")` runs as that account, so run the
same query as two accounts.

**Reaching other services.** `fetch::send` works only for hosts the app
declared in `toolsite.toml` as `allow_http = ["api.example.com"]`. Off by
default. Addresses inside the server's own network are refused whatever the
allowlist says, so a URL taken from user input cannot be pointed at cloud
metadata. Keep the API key in settings, not in the bundle.

**No clock.** `std::time` will not link; the world imports no clock. Take
timestamps from SQLite: `select cast(strftime('%s','now') as integer)`.

**Every request runs in a fresh instance** with a fuel ceiling, a memory cap and
a wall-clock deadline. One that loops forever is killed and returns 500. Because
instances are never reused, **state must live in the database or in files,
never in globals or statics.** A request body into a handler is capped at
8 MB; a file that big or bigger goes through [Files](#files).

### Minimal Rust handler

`toolsite init <name> --handler` scaffolds this crate. By hand: a standalone
`cdylib` crate depending on `wit-bindgen`, with `wit/toolsite.wit` copied in
beside `src/`. Full `Cargo.toml` in [reference.md](reference.md). `src/lib.rs`:

```rust
wit_bindgen::generate!({
    path: "wit",
    world: "app",
});

use toolsite::app::db;

struct Handler;

fn json(status: u16, body: String) -> Response {
    Response {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: body.into_bytes(),
    }
}

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        // The host passes /api through, so strip it the way any router would.
        let route = req.path.strip_prefix("/api").unwrap_or(&req.path);
        match (req.method.as_str(), route) {
            // Values are bound to '?', never concatenated into the SQL.
            ("POST", "/items") => {
                let name = String::from_utf8_lossy(&req.body).to_string();
                match db::query("insert into items (name) values (?)", &[db::Value::Text(name)]) {
                    Ok(_) => json(200, r#"{"ok":true}"#.to_string()),
                    Err(e) => json(500, format!("{{\"error\":{:?}}}", format!("{e:?}"))),
                }
            }
            ("GET", "/count") => match db::query("select count(*) from items", &[]) {
                Ok(rows) => match rows.values.first().and_then(|row| row.first()) {
                    Some(db::Value::Integer(n)) => json(200, format!("{{\"count\":{n}}}")),
                    other => json(500, format!("{{\"error\":{:?}}}", format!("{other:?}"))),
                },
                Err(e) => json(500, format!("{{\"error\":{:?}}}", format!("{e:?}"))),
            },
            _ => json(404, r#"{"error":"not found"}"#.to_string()),
        }
    }
}

export!(Handler);
```

Build and upload:

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
# The artifact is named after the crate, with dashes turned into underscores.
curl -f -T target/wasm32-wasip2/release/<crate_name>.wasm '<upload-url>?handler'
```

The `items` table comes from `migrations/`, below, not from the handler.

## Files

Uploads, images, exports, datasets: anything too large or too opaque for a
row is a blob. One namespace per app, keyed like a path (`photos/cat.jpg`),
with the same rules as a bundle path, so `..` and dotfiles are refused before
anything touches storage. Where the bytes live, the volume or a bucket, is the
deployment's choice and not the app's concern. The ceiling per file is a few
GB by default.

The handler decides; the platform moves the bytes. Neither direction goes
through guest memory:

- **Taking a file from a browser.** Call `blobs::upload_url(key, max_bytes)`
  and hand the URL to the page. The browser `PUT`s the file there with its
  content type; the URL works once and expires in fifteen minutes. Decide who
  gets a URL the way you decide anything else: it is the credential.
- **Sending one.** Answer with the header `x-toolsite-blob: <key>` and an
  empty body. The platform streams the file in its place, with the stored
  content type unless you set one, and keeps your other headers, so
  `content-disposition` and `cache-control` are yours to add. Answering is the
  permission.
- **Small things from inside.** `put`, `get`, `stat`, `list(prefix)` and
  `delete`. `get` refuses anything over 16 MB rather than truncating it; serve
  those with the header.

Seeding from a shell: `curl -f -T file '<upload-url>?blob=<key>'`, up to
64 MB per PUT, typed by the key's extension.

## Schema goes in migrations/, not in the handler

Never write `create table if not exists` in a handler. It cannot evolve
anything: once the table exists, adding a column does nothing and the failure
surfaces later as "no such column" against real data.

Put numbered files in `migrations/` beside the source; `toolsite deploy`
applies them before the app is reachable, each once, in order, in a
transaction.

```
migrations/001_initial.sql      create table todos (...)
migrations/002_add_done.sql     alter table todos add column done integer
```

Add a file for the next change rather than editing an old one; a database
that already ran it will never run it again. Without the CLI:
`tar -czf - -C migrations . | curl -f -T - '<upload-url>?migrations'`, or
`app_migrations(app, files)` over MCP, sending the whole set each time.

## Configure in the file, not in commands

Put an app's gate, route rules, roles, jobs, icon and outbound hosts in
`toolsite.toml` beside its source rather than issuing commands. It travels
with the project, so the next session sees what was intended, and a redeploy
reproduces it.

```toml
slug = "board"
spa  = true
gate = "default"               # follow the site; or public, authenticated, restricted
icon = "📋"
allow_http = ["api.example.com"]
roles = ["viewer", "editor"]   # what the handler checks; a hint for whoever grants

[[route]]                      # note the singular; unknown keys are refused
path = "/triage"
gate = "authenticated"

[[job]]                        # six cron fields, seconds first
name = "rollup"
schedule = "0 0 3 * * *"
path = "/api/rollup"
```

`toolsite deploy` applies it; otherwise `curl -f -T toolsite.toml
'<upload-url>?manifest'`. Routes and jobs are replaced wholesale, so removing
a line removes the thing. Anything the file does not mention is left alone.

## Keep the project, and start from it

A bundle cannot be turned back into the sources that built it, so publish the
project alongside it. Visitors only ever see what the bundle contained.

```bash
tar -czf - --exclude node_modules --exclude target . | curl -f -T - '<upload-url>?source'
curl -s '<upload-url>?source' | tar xz          # a later session picks it up
```

With the CLI this is automatic: `toolsite deploy` keeps the project, and
`toolsite fetch` brings it back. The admin can download it from the app's
Overview page too.

## A repository

An app's source can live in a GitHub repository, with its history. The
repository is a mirror: toolsite pushes when you publish the source and pulls
when someone pushes. Nothing is built or run in GitHub; you build and publish
from where you run, as always.

- `app_repo(app, "create")` makes a repository out of the source you
  published with `?source`, named `toolsite-<app>` unless `repo` says
  otherwise, private unless `public: true`, tagged with the `toolsite` topic.
- `app_repo(app, "import", repo: "owner/name", branch?, directory?)` links a
  repository that already exists and pulls its branch into the app's source
  archive. Fetch it with `curl -s '<upload-url>?source' | tar xz`, build, and
  publish with `toolsite deploy`.
- Publishing the source of a linked app pushes one commit. Say why with
  `'<upload-url>?source&message=<url-encoded text>'` or
  `toolsite deploy --message "..."`. Name the commit the build came from with
  `&commit=<sha>` (`toolsite deploy` sends `git rev-parse HEAD` itself) so the
  Repo tab can say whether the live app is the repository's head.
- `app_repo(app, "status")` says what is linked, the last push, the drift,
  and the newest commits; `"pull"` pulls the branch again; `"disconnect"`
  forgets the link, leaving the repository alone.
- `app_repo(app, "discover")` lists repositories the installation can reach
  that carry the `toolsite` topic and are not linked yet. It proposes;
  nothing is imported until you say so.

The site has to be configured with a GitHub App for any of this;
`app_repo(app, "installations")` says whether it is and on which accounts. If
it is not, say so and carry on with `toolsite deploy`; do not try to set the
App up from an agent.

## Leave notes, and read them first

A published app is a rendered page; its source does not come back out of it,
and a bundle's `dist/` is gone once uploaded. Before changing an app, read
`app_notes(slug)`. Before finishing, write what the next session needs: the
database schema, decisions and their reasons, what is half-finished.

Notes are about one app: what it is, where things are, its schema, what is
unfinished. Do not write platform behaviour into them; it goes stale there and
misleads whoever reads it next. Notes live beside the app, not inside the
bundle, so they are never served to a visitor.

## Verify before you report success

Never claim a page is live without checking it.

```bash
curl -sS -o /dev/null -w '%{http_code}\n' <page-url>          # expect 200, or 303 to sign-in on a gated app
curl -sS <page-url> | head -c 400                             # expect real content
curl -I <page-url>/assets/<a-built-file>                      # bundles: expect 200
curl -sS <page-url>/api/<route>                               # handlers: expect the handler's answer
```

Then look at it: `screenshot(slug)` over MCP, or `toolsite shot <slug> -o page.png`,
renders the page in a browser on the server. Pass `as_user` to see a gated
page as that account. Describe what the picture shows before you report
success; a blank page or a sign-in form is a failure to fix.

A 200 on the HTML with a 404 on the assets is the blank-page failure above; go
back and fix the base path, rebuild, re-upload. A 303 to `/auth/login` on an
app you expected open means it follows a site default of `restricted` or
`authenticated`; set `gate = "public"` if it should be open.

See [reference.md](reference.md) for the WIT type surface, storage layout, index
and icon behaviour, and server environment variables.
