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
- `GET /` — index of published pages, each with an icon and title. Multi-page
  apps appear once, as their root; their inner pages are the app's own
  business.

## Auth

Two independent auth modes, use either or both at once:

- **Bearer token** — set `BEARER_TOKEN`. Any MCP client that supports a
  plain `Authorization: Bearer <token>` header can connect directly.
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
