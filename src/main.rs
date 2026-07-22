use axum::{
    body::Body,
    extract::{Form, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use rand::RngExt;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities,
        ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::fs;

struct AuthCode {
    redirect_uri: String,
    code_challenge: Option<String>,
    expires_at: Instant,
}

/// Present only when OAUTH_CLIENT_ID + OAUTH_CLIENT_SECRET are configured.
/// Mounts the OAuth discovery/authorize/token routes; absent means the server
/// only does plain bearer-token auth (for clients that support that directly).
struct OAuth {
    client_id: String,
    client_secret: String,
    auth_codes: Mutex<HashMap<String, AuthCode>>,
}

struct Config {
    data_dir: PathBuf,
    base_url: Option<String>,
    valid_tokens: Vec<String>,
    oauth: Option<OAuth>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PushPageRequest {
    #[schemars(description = "Full self-contained HTML document (inline any CSS/JS).")]
    html: String,
    #[schemars(
        description = "Optional URL slug. Random one is generated if omitted. Reusing a slug overwrites that page."
    )]
    slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PullPageRequest {
    #[schemars(description = "Slug of the page to fetch the current HTML for.")]
    slug: String,
}

#[derive(Clone)]
struct PageHost {
    config: Arc<Config>,
    #[allow(dead_code)]
    tool_router: ToolRouter<PageHost>,
}

#[tool_router]
impl PageHost {
    fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Publish a single self-contained HTML page and get back a public URL. Call again with the same slug to update it in place."
    )]
    async fn push_page(
        &self,
        Parameters(PushPageRequest { html, slug }): Parameters<PushPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = slug.unwrap_or_else(random_slug);
        if slug.is_empty()
            || !slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty and contain only letters, numbers, '-' or '_'",
            )]));
        }

        let path = self.config.data_dir.join(format!("{slug}.html"));
        fs::write(&path, html)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let url = match &self.config.base_url {
            Some(base) => format!("{base}/p/{slug}"),
            None => format!("/p/{slug}"),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(url)]))
    }

    #[tool(description = "Fetch the current HTML source of a previously pushed page by its slug, so it can be edited and pushed back.")]
    async fn pull_page(
        &self,
        Parameters(PullPageRequest { slug }): Parameters<PullPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let path = self.config.data_dir.join(format!("{slug}.html"));
        match fs::read_to_string(&path).await {
            Ok(html) => Ok(CallToolResult::success(vec![ContentBlock::text(html)])),
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no page found for slug '{slug}'"
            ))])),
        }
    }
}

#[tool_handler]
impl ServerHandler for PageHost {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_server_info(Implementation::new("page-host", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Push single-file HTML artifacts and get back a public URL to view them.",
            )
    }
}

fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

fn random_slug() -> String {
    random_token(8)
}

async fn require_bearer(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| config.valid_tokens.iter().any(|v| v == token))
        .unwrap_or(false);
    if !ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn serve_page(
    State(config): State<Arc<Config>>,
    Path(slug): Path<String>,
) -> impl IntoResponse {
    let path = config.data_dir.join(format!("{slug}.html"));
    match fs::read_to_string(&path).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

const INDEX_STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f7f7f8;
  --fg: #1a1a1a;
  --muted: #6b7280;
  --card-bg: #ffffff;
  --border: #e5e7eb;
  --accent: #4f46e5;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #111114;
    --fg: #e8e8ea;
    --muted: #9198a1;
    --card-bg: #1a1a1f;
    --border: #2a2a30;
    --accent: #818cf8;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0;
  padding: 3rem 1.5rem;
  background: var(--bg);
  color: var(--fg);
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
}
.container { max-width: 640px; margin: 0 auto; }
h1 { font-size: 1.5rem; margin: 0 0 .25rem; }
.count { color: var(--muted); font-size: .9rem; margin: 0 0 1.5rem; }
input[type="search"] {
  width: 100%;
  padding: .6rem .8rem;
  border-radius: .5rem;
  border: 1px solid var(--border);
  background: var(--card-bg);
  color: var(--fg);
  font-size: .95rem;
  margin-bottom: 1.25rem;
}
input[type="search"]:focus { outline: 2px solid var(--accent); outline-offset: 1px; }
ul.pages { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .5rem; }
ul.pages li a {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: .75rem 1rem;
  border-radius: .5rem;
  border: 1px solid var(--border);
  background: var(--card-bg);
  color: var(--fg);
  text-decoration: none;
  font-size: .95rem;
  transition: border-color .15s ease;
}
ul.pages li a:hover { border-color: var(--accent); }
ul.pages li a::after { content: "\2192"; color: var(--muted); margin-left: .5rem; }
.empty, .no-match { color: var(--muted); text-align: center; padding: 2rem 0; }
.no-match { display: none; }
"#;

const INDEX_SEARCH_SCRIPT: &str = r#"
<script>
  const input = document.getElementById('q');
  const items = Array.from(document.getElementById('list').children);
  const noMatch = document.getElementById('no-match');
  input.addEventListener('input', () => {
    const q = input.value.trim().toLowerCase();
    let visible = 0;
    items.forEach((li) => {
      const match = li.dataset.slug.includes(q);
      li.style.display = match ? '' : 'none';
      if (match) visible++;
    });
    noMatch.style.display = (items.length > 0 && visible === 0 && q !== '') ? 'block' : 'none';
  });
</script>
"#;

async fn index(State(config): State<Arc<Config>>) -> impl IntoResponse {
    let mut slugs = Vec::new();
    if let Ok(mut entries) = fs::read_dir(&config.data_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("html") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    slugs.push(stem.to_string());
                }
            }
        }
    }
    slugs.sort();

    let count_label = match slugs.len() {
        0 => "No pages yet".to_string(),
        1 => "1 page".to_string(),
        n => format!("{n} pages"),
    };

    let (body, script) = if slugs.is_empty() {
        (
            r#"<p class="empty">No pages yet. Push one to see it here.</p>"#.to_string(),
            "",
        )
    } else {
        let items: String = slugs
            .iter()
            .map(|s| {
                format!(
                    r#"<li data-slug="{lower}"><a href="/p/{s}">{s}</a></li>"#,
                    lower = s.to_lowercase()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let body = format!(
            r#"<input type="search" id="q" placeholder="Filter pages&hellip;" autocomplete="off">
<ul class="pages" id="list">
{items}
</ul>
<p class="no-match" id="no-match">No pages match that.</p>"#
        );
        (body, INDEX_SEARCH_SCRIPT)
    };

    let style = INDEX_STYLE;
    Html(format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pages</title>
<style>{style}</style>
</head>
<body>
<div class="container">
<h1>Pages</h1>
<p class="count">{count_label}</p>
{body}
</div>
{script}
</body>
</html>"#
    ))
}

// --- Minimal OAuth 2.1 shim (only mounted when OAuth is configured) -----
//
// claude.ai's custom connector flow (when the plain header-auth option isn't
// available) expects a real OAuth authorization server: it discovers
// endpoints via well-known metadata, redirects the user's browser through
// `/authorize`, then exchanges the resulting code at `/token`. There is only
// one user here, so `/authorize` auto-approves instead of showing a login
// screen. `/token` always hands back the configured client_secret as the
// access token, which is also what `require_bearer` accepts on `/mcp` — the
// client_secret is what actually gates the exchange.

async fn oauth_protected_resource_metadata(State(config): State<Arc<Config>>) -> impl IntoResponse {
    let base = config.base_url.as_deref().expect("base_url required in OAuth mode");
    Json(serde_json::json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
    }))
}

async fn oauth_authorization_server_metadata(
    State(config): State<Arc<Config>>,
) -> impl IntoResponse {
    let base = config.base_url.as_deref().expect("base_url required in OAuth mode");
    Json(serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
    }))
}

fn redirect_uri_allowed(uri: &str) -> bool {
    uri.parse::<Uri>()
        .ok()
        .and_then(|u| u.host().map(|h| h == "claude.ai" || h.ends_with(".claude.ai")))
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: Option<String>,
    #[allow(dead_code)]
    code_challenge_method: Option<String>,
}

async fn authorize(
    State(config): State<Arc<Config>>,
    Query(params): Query<AuthorizeParams>,
) -> impl IntoResponse {
    let oauth = config.oauth.as_ref().expect("/authorize only mounted when OAuth is configured");

    if !redirect_uri_allowed(&params.redirect_uri) {
        return (StatusCode::BAD_REQUEST, "redirect_uri not allowed").into_response();
    }
    if params.response_type != "code" || params.client_id != oauth.client_id {
        let mut url = format!("{}?error=invalid_request", params.redirect_uri);
        if let Some(state) = &params.state {
            url.push_str(&format!("&state={}", urlencoding::encode(state)));
        }
        return Redirect::to(&url).into_response();
    }

    let code = random_token(32);
    oauth.auth_codes.lock().unwrap().insert(
        code.clone(),
        AuthCode {
            redirect_uri: params.redirect_uri.clone(),
            code_challenge: params.code_challenge.clone(),
            expires_at: Instant::now() + Duration::from_secs(60),
        },
    );

    let mut url = format!("{}?code={}", params.redirect_uri, code);
    if let Some(state) = &params.state {
        url.push_str(&format!("&state={}", urlencoding::encode(state)));
    }
    Redirect::to(&url).into_response()
}

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    code_verifier: Option<String>,
}

fn oauth_error(status: StatusCode, error: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

fn client_credentials(headers: &HeaderMap, body: &TokenRequest) -> (Option<String>, Option<String>) {
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(b64) = auth.strip_prefix("Basic ") {
            if let Ok(decoded) = BASE64_STANDARD.decode(b64) {
                if let Ok(s) = String::from_utf8(decoded) {
                    if let Some((id, secret)) = s.split_once(':') {
                        return (Some(id.to_string()), Some(secret.to_string()));
                    }
                }
            }
        }
    }
    (body.client_id.clone(), body.client_secret.clone())
}

async fn token_endpoint(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(body): Form<TokenRequest>,
) -> impl IntoResponse {
    let oauth = config.oauth.as_ref().expect("/token only mounted when OAuth is configured");

    let (client_id, client_secret) = client_credentials(&headers, &body);
    if client_id.as_deref() != Some(oauth.client_id.as_str())
        || client_secret.as_deref() != Some(oauth.client_secret.as_str())
    {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }

    match body.grant_type.as_str() {
        "authorization_code" => {
            let Some(code) = body.code.clone() else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            let entry = oauth.auth_codes.lock().unwrap().remove(&code);
            let Some(entry) = entry else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            };
            if entry.expires_at < Instant::now() {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            if body.redirect_uri.as_deref() != Some(entry.redirect_uri.as_str()) {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            if let Some(challenge) = &entry.code_challenge {
                let verifier = body.code_verifier.clone().unwrap_or_default();
                let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                if &computed != challenge {
                    return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
                }
            }
            success_token(oauth)
        }
        "refresh_token" => success_token(oauth),
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
}

fn success_token(oauth: &OAuth) -> axum::response::Response {
    Json(serde_json::json!({
        "access_token": oauth.client_secret,
        "token_type": "Bearer",
        "expires_in": 31_536_000,
        "refresh_token": oauth.client_secret,
    }))
    .into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    fs::create_dir_all(&data_dir).await?;

    let bearer_token = std::env::var("BEARER_TOKEN").ok().filter(|s| !s.is_empty());
    let oauth_client_id = std::env::var("OAUTH_CLIENT_ID").ok().filter(|s| !s.is_empty());
    let oauth_client_secret = std::env::var("OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|s| !s.is_empty());

    let oauth = match (oauth_client_id, oauth_client_secret) {
        (Some(client_id), Some(client_secret)) => Some(OAuth {
            client_id,
            client_secret,
            auth_codes: Mutex::new(HashMap::new()),
        }),
        (None, None) => None,
        _ => panic!("set both OAUTH_CLIENT_ID and OAUTH_CLIENT_SECRET together, or neither"),
    };

    if bearer_token.is_none() && oauth.is_none() {
        panic!("set BEARER_TOKEN, or OAUTH_CLIENT_ID + OAUTH_CLIENT_SECRET (or both)");
    }

    let base_url = std::env::var("PUBLIC_BASE_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string());
    if oauth.is_some() && base_url.is_none() {
        panic!(
            "PUBLIC_BASE_URL is required when OAUTH_CLIENT_ID/OAUTH_CLIENT_SECRET are set \
             (OAuth discovery metadata needs absolute URLs)"
        );
    }

    let mut valid_tokens = Vec::new();
    if let Some(t) = &bearer_token {
        valid_tokens.push(t.clone());
    }
    if let Some(o) = &oauth {
        valid_tokens.push(o.client_secret.clone());
    }

    let oauth_enabled = oauth.is_some();
    let config = Arc::new(Config {
        data_dir,
        base_url,
        valid_tokens,
        oauth,
    });

    // rmcp's Streamable HTTP transport validates the inbound `Host` header
    // (DNS-rebinding protection) against an allowlist that defaults to
    // localhost only. Deployed behind a real domain, that must include the
    // public host or every request 403s before auth even runs.
    let host_config = match config
        .base_url
        .as_deref()
        .and_then(|b| b.parse::<Uri>().ok())
        .and_then(|u| u.authority().map(|a| a.as_str().to_string()))
    {
        Some(authority) => StreamableHttpServerConfig::default().with_allowed_hosts([
            authority,
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ]),
        None => StreamableHttpServerConfig::default().disable_allowed_hosts(),
    };

    let mcp_config = config.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(PageHost::new(mcp_config.clone())),
        LocalSessionManager::default().into(),
        host_config,
    );

    let mcp_router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(config.clone(), require_bearer));

    let mut public_router = Router::new()
        .route("/", get(index))
        .route("/p/{slug}", get(serve_page));

    if oauth_enabled {
        public_router = public_router
            .route(
                "/.well-known/oauth-protected-resource",
                get(oauth_protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(oauth_protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(oauth_authorization_server_metadata),
            )
            .route("/authorize", get(authorize))
            .route("/token", post(token_endpoint));
    }

    let public_router = public_router.with_state(config.clone());

    let app = Router::new().merge(mcp_router).merge(public_router);

    let addr = "0.0.0.0:8080";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
