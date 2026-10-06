pub mod accounts;
pub mod config;
pub mod content;
pub mod platform;
pub mod runtime;
pub mod ui;

pub use config::Config;

use crate::{
    accounts::{providers, users},
    content::{browse::{browse, index}, serve::{serve_icon, serve_page}},
    platform::{
        admin, blob_upload,
        bearer::{require_bearer, require_person},
        deploy, export, github,
        client_oauth::{
            authorize_decide, authorize_form, oauth_authorization_server_metadata,
            me_protected_resource_metadata, oauth_protected_resource_metadata, register,
            token_endpoint,
        },
        mcp::PageHost,
        mcp_me::MeHost,
        scaffold, secrets,
        upload::{self, upload_root, upload_sub, MAX_UPLOAD_BYTES},
        mcp_log,
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
    //
    // No sessions. MCP 2026-07-28 has none: a client sends `server/discover`
    // and then whatever it needs, each request naming its protocol version,
    // which is what ChatGPT does. Older clients that still `initialize` are
    // answered too, and are not made to keep a session id either, since a
    // fresh PageHost per request is cheap and nothing here lives between
    // calls. The legacy session mode would hand a 2025-era client that
    // skipped `initialize` a 422, which is how ChatGPT first failed.
    let transport = StreamableHttpServerConfig::default().with_legacy_session_mode(false);
    let host_config = match config
        .base_url
        .as_deref()
        .and_then(|b| b.parse::<Uri>().ok())
        .and_then(|u| u.authority().map(|a| a.as_str().to_string()))
    {
        Some(authority) => transport.with_allowed_hosts([
            authority,
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ]),
        None => transport.disable_allowed_hosts(),
    };

    let me_host_config = host_config.clone();
    let app_tools_host_config = host_config.clone();
    let mcp_config = config.clone();
    let mcp_runtime = runtime.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(PageHost::new(mcp_config.clone(), mcp_runtime.clone())),
        LocalSessionManager::default().into(),
        host_config,
    );

    // The method log sits inside the auth layer, so it sees who the caller
    // is and runs only for accepted requests; a refusal is already logged
    // by the auth layer itself.
    let mcp_router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn(mcp_log::log_mcp))
        .layer(middleware::from_fn_with_state(config.clone(), require_bearer));

    // The same transport for a regular account, with two tools and the
    // account itself carried on the request by the middleware.
    let me_config = config.clone();
    let me_runtime = runtime.clone();
    let me_service = StreamableHttpService::new(
        move || Ok(MeHost::new(me_config.clone(), me_runtime.clone())),
        LocalSessionManager::default().into(),
        me_host_config,
    );
    let me_router = Router::new()
        .nest_service("/me/mcp", me_service)
        .layer(middleware::from_fn(mcp_log::log_mcp))
        .layer(middleware::from_fn_with_state(config.clone(), require_person));

    // One app's tools as a connector of their own. Its own router, since
    // `/p/{app}/mcp` cannot sit beside the `/p/{*slug}` wildcard; a layer on
    // the whole site sends exactly that path here before the site's routes
    // see it, so an app cannot serve its own page or handler at `mcp`.
    let app_tools_config = config.clone();
    let app_tools_runtime = runtime.clone();
    let app_tools_service = StreamableHttpService::new(
        move || Ok(crate::platform::app_tools::AppHost::new(app_tools_config.clone(), app_tools_runtime.clone())),
        LocalSessionManager::default().into(),
        app_tools_host_config,
    );
    let app_tools_router = Router::new()
        .route_service("/p/{app}/mcp", app_tools_service)
        .layer(middleware::from_fn(mcp_log::log_mcp))
        .layer(middleware::from_fn_with_state(config.clone(), crate::platform::bearer::require_app_caller));

    // A browser's live connection to an app: a WebSocket upgrade at one of
    // the paths the app declared. Taken before the site's routes, so the
    // upgrade never reaches a file or the handler as a request; a plain
    // request to the same path is served as always.
    let sockets_router = Router::new()
        .route("/p/{*rest}", get(crate::platform::websocket::upgrade))
        .with_state(AppState { config: config.clone(), runtime: runtime.clone() });

    let mut public_router = Router::new()
        .route("/", get(index))
        .route("/favicon.svg", get(crate::content::serve::site_favicon_svg))
        .route("/favicon.ico", get(crate::content::serve::site_favicon_ico))
        .route("/browse/{*path}", get(browse))
        .route("/p/{*slug}", any(serve_page))
        .route("/icon/{*slug}", get(serve_icon))
        // What an agent needs to build a handler: the contract, and a crate
        // already wired to it.
        .route("/guide", get(scaffold::guide))
        .route("/wit/toolsite.wit", get(scaffold::wit))
        .route("/scaffold/handler.tar.gz", get(scaffold::handler_scaffold))
        .route("/scaffold/{app}", get(scaffold::handler_scaffold_named))
        // Working apps to start from, packed from examples/ at build time.
        .route("/examples", get(crate::platform::examples::list))
        .route("/examples/{file}", get(crate::platform::examples::download))
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
        // The permissions grid: one cell, several people, the lock.
        .route("/admin/permissions/cell", post(crate::platform::permissions::change_cell))
        .route("/admin/permissions/add", post(crate::platform::permissions::add))
        .route("/admin/permissions/lock", post(crate::platform::permissions::change_lock))
        .route("/admin/permissions/candidates", get(crate::platform::permissions::candidates))
        .route("/admin/folder", post(admin::new_folder))
        .route("/admin/project", post(admin::change_project))
        .route("/admin/projects/search", get(admin::search_projects))
        .route("/admin/move", post(admin::move_app))
        .route("/admin/pin", post(admin::pin_tools))
        .route("/admin/exports", get(admin::exports_page).post(admin::change_export))
        // Tokens a device presents over TCP or UDP, checked by the app.
        .route("/admin/devices", post(admin::change_devices))
        // An app's repository: a source mirror, pushed on publish and pulled on push.
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
        // A headless browser's one-time sign-in for a screenshot.
        .route("/preview/{token}", get(crate::platform::preview::open))
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
                "/.well-known/oauth-protected-resource/me/mcp",
                get(me_protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/p/{app}/mcp",
                get(crate::platform::client_oauth::app_protected_resource_metadata),
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

    Router::new()
        .merge(mcp_router)
        .merge(me_router)
        .merge(public_router)
        .layer(middleware::from_fn(move |request: axum::extract::Request, next: middleware::Next| {
            let app_tools = app_tools_router.clone();
            let sockets = sockets_router.clone();
            async move {
                if crate::platform::app_tools::connector_app(request.uri().path()).is_none()
                    && crate::platform::websocket::is_upgrade(&request)
                {
                    use tower::ServiceExt;
                    return match sockets.oneshot(request).await {
                        Ok(response) => response,
                        Err(never) => match never {},
                    };
                }
                if crate::platform::app_tools::connector_app(request.uri().path()).is_some() {
                    use tower::ServiceExt;
                    return match app_tools.oneshot(request).await {
                        Ok(response) => response,
                        Err(never) => match never {},
                    };
                }
                next.run(request).await
            }
        }))
        // Outermost, so it sees app responses from every router above and
        // toolsite's own pages before anything else answers them.
        .layer(middleware::from_fn_with_state(config.clone(), crate::platform::shield::shield))
}
