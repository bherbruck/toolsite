---
name: toolsite
description: Use when publishing a page, site, or app to toolsite — the self-hosted MCP server and web host — covering single HTML files, multi-page apps, built front-end bundles, and wasm request handlers.
---

# Publishing to toolsite

toolsite is one container that is both an MCP server and a web host. You publish
a page and get back a URL. Pages live at `/p/<slug>`, apps at `/p/<slug>/`.

## Pick the path

| What you're publishing | How |
|---|---|
| One self-contained HTML page | Write the file, then `curl -fT page.html <upload-url>` |
| Multi-page static app | One PUT per page: `curl -fT about.html <upload-url>/about`, or ship a tar bundle |
| Compiled front-end (React/Vite/TS) | `tar -czf - -C dist . \| curl -f -T - '<upload-url>?bundle&spa'` |
| Needs server-side data or logic | Also ship a wasm handler — see [Server-side handlers](#server-side-handlers) |
| No shell available (claude.ai web) | Fall back to the `push_page` / `push_app` MCP tools |

## The cardinal rule

**Never paste page HTML into a tool call when you have a shell.** Always:

```
write file  ->  create_upload  ->  curl -fT
```

The bytes go from disk to the server without passing through the model. A 40 KB
page pasted into a tool call costs ~10k tokens and buys nothing — `create_upload`
exists precisely so that never has to happen. `push_page` / `push_app` are the
fallback for clients with no shell, not a shortcut. Don't read a published page
back into the conversation either; `curl` it to a file and edit that.

## The CLI (preferred when installed)

The repo ships a CLI at `./cli`. Check for it first: `command -v toolsite`.
Install with `cargo install --path cli` (binary name `toolsite`).

| Command | Does |
|---|---|
| `toolsite init <name> [--spa] [--handler]` | Scaffold an app: `toolsite.toml`, `dist/index.html`, and with `--handler` a ready-to-build `handler/` crate with the WIT already copied in |
| `toolsite deploy [dir] [--slug <slug>] [--spa]` | Tar the web root, upload as a bundle, upload the handler, then verify the live URL |
| `toolsite sql <app> "<sql>" [--param v]` | Run SQL against that app's database. Repeat `--param` per placeholder; digits and `true`/`false`/`null` bind as those types, everything else as text |
| `toolsite list [--all]` | What is already published |
| `toolsite hide <slug>` / `toolsite unhide <slug>` | Retract and restore, reversibly |

Config comes from `TOOLSITE_URL` and `TOOLSITE_TOKEN`, or `--url` / `--token`.

```bash
export TOOLSITE_URL=https://yourdomain.com
export TOOLSITE_TOKEN=<bearer-token>

# From scratch: writes dist/index.html and toolsite.toml, deployable as-is.
toolsite init dashboard --handler
cd dashboard && toolsite deploy

# An existing front-end project: build it yourself first.
npm run build && toolsite deploy --slug dashboard --spa
```

What `deploy` decides for you:

- **Slug** — `--slug`, else `slug` in `toolsite.toml`, else the directory name.
- **Web root** — the first of `dist/`, `build/`, `public/` that contains an
  `index.html`, else the directory itself. Build before deploying, or it uploads
  your sources.
- **Handler** — a prebuilt `handler.wasm` in the directory, else it runs
  `cargo build --release --target wasm32-wasip2` on `handler/Cargo.toml` if that
  exists. Neither present means no handler; that is not an error.
- **spa** — `--spa` or `spa = true` in `toolsite.toml`.

After uploading it GETs the page, fails if that isn't a success, and warns if
`index.html` still references `/assets/…` from the domain root — the blank-page
failure below. Without the CLI, use the MCP tools plus `curl`; both paths hit
the same endpoints.

## The manual path

1. `list_pages` first — see what exists before picking or reusing a slug.
2. `create_upload(slug)` — returns an upload URL carrying a one-off ticket,
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
```

`?bundle` unpacks a gzipped tar; a single shared top-level directory is
stripped, so `tar -czf - dist` works too. `&spa` makes paths matching no file
fall back to the app's `index.html`; without it they 404. Limits: 64 MB
compressed, 128 MB unpacked, 2000 files. Paths containing `..` or a leading `/`
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
deep client-side routes — prefer the absolute form for anything with a router.

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
| `set_visibility(slug, hidden?, listed?, gate?, path?)` | `hidden: true` 404s the URL. `listed: false` keeps it live but off the index. `gate` with `path` guards one part of an app. |
| `set_icon(slug, icon)` | Emoji, inline `<svg>`, or `data:` URI. Optional — pages without one get a generated badge. |
| `run_sql(app, sql, params?)` | Schema and seed work against one app's database. MCP only; never reachable from a published page. |
| `push_page(html, slug?)` | No-shell fallback, HTML inline. |
| `push_app(app, pages)` | No-shell fallback, multi-page. A page named `index` also serves at the app root. |
| `pull_page(slug)` / `pull_app(app)` | Read a page back for editing. With a shell, `curl` the public URL instead. |
| `app_jobs(app, name?, schedule?, path?, run_now?)` | Scheduled work. Cron with seconds first; fires the app's own handler at a path. |
| `app_settings(app, name?, value?, link?)` | API keys the handler reads. Pass `link: true` for a URL the owner pastes into — never ask for a secret directly. |
| `app_notes(slug, notes?)` | Markdown kept with an app for the next session. Reads when `notes` is omitted. |

**There is deliberately no delete tool.** Nothing here destroys data. Retract
with `set_visibility(slug, hidden: true)`, which is instantly reversible with
`hidden: false`.

## Server-side handlers

An app can ship `handler.wasm`: a component built for `wasm32-wasip2` against
[`wit/toolsite.wit`](../../../wit/toolsite.wit), uploaded with
`curl -f -T handler.wasm '<upload-url>?handler'`. Invalid components are
rejected at upload.

**Routing order**, fixed:

| # | Condition | Result |
|---|---|---|
| 1 | `/p/<app>/api/...` | The handler, always. The prefix is reserved, so no file can shadow it. |
| 2 | An exact file on disk | Served statically, no wasm involved. |
| 3 | No file, handler exists | The handler, so it can render routes server-side. |
| 4 | No file, no handler, `spa` set | The app's `index.html`. |
| 5 | otherwise | 404. |

The guest sees the path relative to its app **with `/api` still attached** —
`/api/echo`, not `/p/myapp/api/echo`. Strip that prefix yourself.

**Access.** A gate decides whether a request arrives; what it may then do is
yours to decide. Guard part of an app with `set_visibility(slug, gate, path)`
— longest matching prefix wins — and inside the handler read
`identity::current-role()`, which returns whatever the owner granted
(`viewer`, `editor`, anything). The platform never interprets a role.

**Capabilities.** A handler gets three imports: `db.query`, bound to its
own app's SQLite with parameters bound rather than interpolated, and
`identity.current-user`, which it cannot forge. No filesystem, no environment,
no sockets. wasi is linked because a `wasm32-wasip2` guest imports it through
std, but the context grants nothing.

**Every request runs in a fresh instance** with a fuel ceiling, a memory cap and
a wall-clock deadline. One that loops forever is killed and returns 500. Because
instances are never reused, **state must live in the database, never in globals
or statics.**

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

Create the schema with `run_sql` (or `toolsite sql <app> "..."`) before first
use, or have the handler run `create table if not exists ...` itself.

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

Add a file for the next change rather than editing an old one — a database
that already ran it will never run it again. Without the CLI:
`app_migrations(app, files)` over MCP, sending the whole set each time.

## Configure in the file, not in commands

Put an app's gate, route rules, jobs and icon in `toolsite.toml` beside its
source rather than issuing commands. It travels with the project, so the next
session sees what was intended, and a redeploy reproduces it.

```toml
slug = "board"
gate = "public"
icon = "📋"

[[route]]
path = "/triage"
gate = "authenticated"

[[job]]
name = "rollup"
schedule = "0 0 3 * * *"
path = "/api/rollup"
```

`toolsite deploy` applies it; otherwise `curl -f -T toolsite.toml
'<upload-url>?manifest'`. Routes and jobs are replaced wholesale, so removing
a line removes the thing.

## Keep the project, and start from it

A bundle cannot be turned back into the sources that built it, so publish the
project alongside it. Visitors only ever see what the bundle contained.

```bash
tar -czf - --exclude node_modules --exclude target . | curl -f -T - '<upload-url>?source'
curl -s '<upload-url>?source' | tar xz          # a later session picks it up
```

With the CLI this is automatic: `toolsite deploy` keeps the project, and
`toolsite fetch` brings it back.

## Leave notes, and read them first

A published app is a rendered page — its source does not come back out of it,
and a bundle's `dist/` is gone once uploaded. Before changing an app, read
`app_notes(slug)`; those notes may be the only record of why it is built the
way it is. Before finishing, write what the next session needs: the database
schema, decisions and their reasons, what is half-finished.

Notes live beside the app, not inside the bundle, so they are never served to
a visitor and need no place in the build.

## Verify before you report success

Never claim a page is live without checking it.

```bash
curl -sS -o /dev/null -w '%{http_code}\n' <page-url>          # expect 200
curl -sS <page-url> | head -c 400                             # expect real content
curl -I <page-url>/assets/<a-built-file>                      # bundles: expect 200
curl -sS <page-url>/api/<route>                               # handlers: expect the handler's answer
```

A 200 on the HTML with a 404 on the assets is the blank-page failure above — go
back and fix the base path, rebuild, re-upload.

See [reference.md](reference.md) for the WIT type surface, storage layout, index
and icon behaviour, and server environment variables.
