# page-host

A single-container MCP server + web host. Claude pushes a self-contained HTML
page via an MCP tool call; the server stores it and serves it back at a
public URL.

## Tools

- `create_upload(slug?)` — **preferred when the agent has a shell.** Returns a
  short-lived (15 min) upload URL. The agent `curl`s the file to it, so the
  page HTML never passes through the model:

  ```
  curl -fT page.html https://host/upload/<ticket>       # single page
  curl -fT index.html https://host/upload/<ticket>/index   # multi-page app
  curl -fT about.html https://host/upload/<ticket>/about
  ```

  Each upload replies with the page's public URL. The ticket is the only
  credential needed, so the server's real token is never handed to the agent.

  The server's MCP `instructions` tell clients to take this path by default
  and to fall back to `push_page` only if the upload URL is unreachable —
  hosted sandboxes often have a filesystem but no outbound network. That's a
  hint, not an enforcement; a client that ignores `instructions` will still
  see "use create_upload instead" in the `push_page` description.
- `push_page(html, slug?)` — fallback: publish an HTML page by passing its
  source inline. For clients that can't reach this host from a shell. Omit
  `slug` for a random one. Reusing a slug overwrites that page in place.
  Returns the page URL. `slug` may contain `/` to namespace it under an app,
  e.g. `myapp/about`.
- `pull_page(slug)` — fetch the current HTML for a previously pushed page
  (so it can be edited and pushed back).
- `push_app(app, pages)` — publish multiple pages under one app namespace in
  a single call (`pages` maps page name to HTML). A page named `index` is
  also served at the app's own root URL. Returns each page's URL.
- `pull_app(app)` — fetch every page in an app namespace, keyed by page name,
  so the app can be edited and pushed back with `push_app`.
- `set_icon(slug, icon)` — set the icon shown beside a page on the index.
  Takes an emoji, inline `<svg>…</svg>`, or a `data:` URI. Optional.
- `list_pages(include_all?)` — what's published: slug, title, URL, when it
  last changed, visibility. Newest first. Pass `include_all` to see hidden and
  unlisted pages too.
- `set_visibility(slug, hidden?, listed?)` — `hidden` takes a page down (its
  URL 404s) and `listed` controls whether it appears on the index. Nothing is
  deleted, so both are reversible. There is deliberately no delete tool.

## Bundles

A built front-end goes up in one shot as a gzipped tar:

```
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle'        # static site
tar -czf - -C dist . | curl -f -T - '<upload-url>?bundle&spa'    # client-side router
```

### Set the base path before building

Apps are served from `/p/<slug>/`, never the domain root. A default Vite /
Next / CRA config emits absolute `/assets/…` URLs, which 404 here — the page
loads and renders blank. `create_upload` prints the exact values for the slug
you asked for:

```
vite.config:  base: '/p/<slug>/'
next.config:  basePath: '/p/<slug>', assetPrefix: '/p/<slug>/'
CRA:          "homepage": "/p/<slug>/"
router:       basename: '/p/<slug>'
```

Relative (`base: './'`) also works for a static multi-page bundle, but breaks
on deep client-side routes — prefer the absolute form for SPAs.

Both `tar -czf - -C dist .` and `tar -czf - dist` work — a single shared top
level directory is stripped. Files are served from `/p/<app>/…` with a content
type derived from the extension (JS, CSS, JSON, wasm, fonts, images). With
`&spa`, a path that matches no file falls back to the app's `index.html` so
client-side routes resolve; without it, unknown paths 404.

Limits and rules: 64 MB compressed, 128 MB unpacked, 2000 files. Paths
containing `..` or a leading `/` are rejected outright. Symlinks and dotfiles
are skipped, and the response says so rather than silently shipping less.

## Titles and icons on the index

Each listed page shows an icon and a title, neither of which needs to be
supplied:

- **Title** — read from the page's own `<title>` tag (first 8 KB scanned).
  Pages without one are listed by slug.
- **Icon** — in priority order: an uploaded image (`PUT <upload-url>?icon`),
  an emoji / inline SVG / `data:` URI set via `set_icon`, or a generated
  badge showing the slug's initials on a colour derived from the slug (stable
  forever, since it's a hash).

Icons are stored beside the page as `<slug>.icon` and served from
`/icon/<slug>`; content type is sniffed (PNG, JPEG, GIF, WebP, ICO, SVG).
1 MB cap.

```
curl -fT logo.png "<upload-url>?icon"          # icon for the ticket's page
curl -fT logo.svg "<upload-url>/about?icon"    # icon for one page of an app
```

## Endpoints

- `POST /mcp` — the MCP server (Streamable HTTP transport). Requires auth.
- `PUT /upload/<ticket>[/<page>]` — write a page from a raw request body.
  Authenticated by the ticket from `create_upload`, not the server token.
  16 MB max.
- `GET /p/<slug>` — the published page. Public, no auth. An app root
  (`/p/myapp`) redirects to `/p/myapp/` and serves that app's `index` page, so
  relative links inside the app resolve correctly.
- `GET /icon/<slug>` — a page's icon, if one was set. Public, no auth.
- `GET /` — index of published pages, newest first, each with an icon, title
  and when it last changed. Multi-page apps and bundles appear once, as their
  root; their inner pages are the app's own business. Hidden and unlisted
  pages don't appear.

## Auth

Two independent auth modes, use either or both at once:

- **Bearer token** — set `BEARER_TOKEN`. Any MCP client that supports a
  plain `Authorization: Bearer <token>` header can connect directly.
  `x-api-key: <token>` is accepted too, since clients differ on which one
  they send.
- **OAuth 2.1** — set `OAUTH_CLIENT_ID` + `OAUTH_CLIENT_SECRET`. Needed for
  clients (like claude.ai custom connectors) that require a full OAuth
  flow. This is a minimal single-user shim: `/authorize` auto-approves (no
  login screen), `/token` hands back `OAUTH_CLIENT_SECRET` as the access
  token. `/authorize` only allows redirecting back to `claude.ai` /
  `*.claude.ai`.

At least one of the two must be configured.

## Environment variables

| Variable              | Required                   | Description                                                        |
|-----------------------|-----------------------------|----------------------------------------------------------------------|
| `BEARER_TOKEN`        | if not using OAuth          | Static token for direct bearer auth on `/mcp`.                       |
| `OAUTH_CLIENT_ID`     | if using OAuth              | Paste into claude.ai's custom connector "OAuth Client ID" field.      |
| `OAUTH_CLIENT_SECRET` | if using OAuth              | Paste into claude.ai's "OAuth Client Secret" field.                   |
| `PUBLIC_BASE_URL`     | if using OAuth              | Absolute base URL of the deployed server (e.g. `https://host.com`). Needed for OAuth discovery metadata; if omitted (bearer-only mode), `push_page` returns a relative `/p/<slug>` URL instead. |
| `DATA_DIR`            | no (default `/data`)       | Where pushed pages are stored.                                        |
| `PORT`                | no (default `8080`)        | Port to listen on.                                                    |
| `RUST_LOG`            | no (default `info`)        | Log filter. Rejected requests are logged at `warn` with the headers that arrived (never the token). |

## Run

Locally, `cp .env.example .env`, fill in a `BEARER_TOKEN`, then
`cargo run --release` — `.env` is loaded at startup and is gitignored.

```
docker build -t page-host .

docker run -d -p 8080:8080 -v ./data:/data \
  -e BEARER_TOKEN=<random-secret> \
  -e OAUTH_CLIENT_ID=<random-string> \
  -e OAUTH_CLIENT_SECRET=<random-secret> \
  -e PUBLIC_BASE_URL=https://yourdomain.com \
  page-host
```

## Adding to claude.ai

1. Deploy this behind a real HTTPS URL (Fly.io, a VPS + reverse proxy,
   Cloudflare Tunnel, etc).
2. Settings → Connectors → Add custom connector.
3. URL: `https://yourdomain.com/mcp`.
4. If a "Request headers" field is offered, use `Authorization: Bearer
   <BEARER_TOKEN>` and skip OAuth entirely.
5. Otherwise, fill in OAuth Client ID / Secret with the values you set
   above.
