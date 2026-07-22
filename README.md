# page-host

A single-container MCP server + web host. Claude pushes a self-contained HTML
page via an MCP tool call; the server stores it and serves it back at a
public URL.

## Tools

- `push_page(html, slug?)` — publish an HTML page. Omit `slug` for a random
  one. Reusing a slug overwrites that page in place. Returns the page URL.
- `pull_page(slug)` — fetch the current HTML for a previously pushed page
  (so it can be edited and pushed back).

## Endpoints

- `POST /mcp` — the MCP server (Streamable HTTP transport). Requires auth.
- `GET /p/<slug>` — the published page. Public, no auth.
- `GET /` — index of all published pages.

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

## Run

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
