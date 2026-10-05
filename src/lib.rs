pub mod accounts;
pub mod config;
pub mod content;
pub mod platform;
pub mod runtime;
pub mod ui;

pub use config::Config;

use crate::{
    accounts::{providers, users},
    content::serve::{index, serve_icon, serve_page},
    platform::{
        admin, blob_upload,
        bearer::require_bearer,
        deploy, export, github,
        client_oauth::{
            authorize_decide, authorize_form, oauth_authorization_server_metadata,
            oauth_protected_resource_metadata, register, token_endpoint,
        },
        mcp::PageHost,
        scaffold, secrets,
        upload::{self, upload_root, upload_sub, MAX_UPLOAD_BYTES},
    },
};
use crate::runtime::wasm::Runtime;
use axum::{
    extract::{DefaultBodyLimit, FromRef},
    http::Uri,
    middleware,
    routing::{any, get, post, put},
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::sync::Arc;

/// Shared by every handler. Split from `Config` so the data layer never has
/// to know the wasm runtime exists; `FromRef` lets handlers that only want
/// config keep asking for exactly that.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub runtime: Arc<Runtime>,
}

impl FromRef<AppState> for Arc<Config> {
    fn from_ref(state: &AppState) -> Self {
        state.config.clone()
    }
}

/// Assembles every route. Kept out of `main` so tests can drive the whole
/// surface in-process instead of over a socket.
pub fn build_router(config: Arc<Config>, runtime: Arc<Runtime>) -> Router {
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
    let mcp_runtime = runtime.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(PageHost::new(mcp_config.clone(), mcp_runtime.clone())),
        LocalSessionManager::default().into(),
        host_config,
    );

    let mcp_router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(config.clone(), require_bearer));

    let mut public_router = Router::new()
        .route("/", get(index))
        .route("/p/{*slug}", any(serve_page))
        .route("/icon/{*slug}", get(serve_icon))
        // What an agent needs to build a handler: the contract, and a crate
        // already wired to it.
        .route("/guide", get(scaffold::guide))
        .route("/wit/toolsite.wit", get(scaffold::wit))
        .route("/scaffold/handler.tar.gz", get(scaffold::handler_scaffold))
        .route("/scaffold/{app}", get(scaffold::handler_scaffold_named))
        .route("/auth/login", get(users::login_form).post(users::login_submit))
        // Signing in through a provider: out to it, and back.
        .route("/auth/login/{provider}", get(providers::begin))
        .route("/auth/callback/{provider}", get(providers::callback))
        .route("/auth/logout", post(users::logout).get(users::logout))
        .route("/auth/me", get(users::me))
        // The signed-in person's own page: how they sign in, and a new
        // password if they have one.
        .route("/account", get(platform::account::page))
        .route("/account/password", post(platform::account::change_password))
        .route("/settings/{token}", get(secrets::entry_form))
        .route("/settings", get(secrets::entry_form_query).post(secrets::entry_submit))
        .route(
            "/auth/setup",
            get(users::setup_form).post(users::setup_submit),
        )
        .route("/admin", get(admin::accounts_page))
        .route("/admin/apps", get(admin::apps_page))
        .route("/admin/apps/{app}", get(admin::app_overview))
        .route("/admin/apps/{app}/source", get(admin::download_source))
        .route("/admin/apps/{app}/{tab}", get(admin::app_tab_page))
        .route("/admin/apps/search", get(admin::search_apps))
        .route("/admin/accounts", get(admin::accounts_page))
        .route("/admin/accounts/new", get(admin::new_account_page))
        .route("/admin/accounts/search", get(admin::search_accounts))
        .route("/admin/accounts/{email}", get(admin::account_page))
        .route("/admin/reinvite", post(admin::reinvite))
        .route("/admin/users", post(admin::add_account))
        .route("/admin/access", post(admin::change_access))
        .route("/admin/active", post(admin::change_active))
        .route("/admin/gate", post(admin::change_gate))
        .route("/admin/rule", post(admin::change_rule))
        .route("/admin/visibility", post(admin::change_visibility))
        .route("/admin/notes", post(admin::change_notes))
        .route("/admin/settings-link", post(admin::settings_link))
        .route("/admin/job-run", post(admin::run_job))
        .route("/admin/scope", post(admin::change_scope))
        .route("/admin/folder", post(admin::new_folder))
        .route("/admin/move", post(admin::move_app))
        .route("/admin/exports", get(admin::exports_page).post(admin::change_export))
        // An app's repository: GitHub Actions builds, a per-app token deploys.
        .route("/admin/github", get(github::github_page))
        .route("/admin/github/repos/search", get(github::repos_search))
        .route("/admin/repo", post(github::repo_action))
        .route("/github/setup", get(github::setup))
        .route("/github/webhook", post(github::webhook))
        .route("/deploy/{app}", put(deploy::deploy_root).post(deploy::deploy_root))
        .route("/deploy/{app}/{*sub}", put(deploy::deploy_sub).post(deploy::deploy_sub))
        // One app's database, whole, for a token minted for that app alone.
        .route("/export/{file}", get(export::download))
        // Trades the site session for one scoped to a single app; the only
        // way an app ever sees a visitor.
        .route("/auth/handoff", get(users::handoff))
        .route(
            "/upload/{ticket}",
            put(upload_root).post(upload_root).get(upload::download),
        )
        .route("/upload/{ticket}/{*sub}", put(upload_sub).post(upload_sub))
        // A visitor's file, streamed to storage. The ticket caps it, not the
        // body limit below, which exists for things held in memory.
        .route(
            "/blob/{ticket}",
            put(blob_upload::receive)
                .post(blob_upload::receive)
                .layer(DefaultBodyLimit::disable()),
        )
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES));

    // The OAuth server MCP clients sign in through. Needs the site's own
    // address for its metadata, and nothing else.
    if config.oauth_enabled() {
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
            .route("/register", post(register))
            .route("/authorize", get(authorize_form).post(authorize_decide))
            .route("/token", post(token_endpoint));
    }

    let state = AppState {
        config: config.clone(),
        runtime,
    };
    // Nothing fires until something asks the clock, so the scheduler starts
    // with the router that serves the same apps.
    crate::platform::schedule::spawn(state.clone());
    let public_router = public_router.with_state(state);

    Router::new().merge(mcp_router).merge(public_router)
}
