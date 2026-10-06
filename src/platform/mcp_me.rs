//! The MCP server for a regular account: `/me/mcp`.
//!
//! An admin's client publishes through `/mcp`. Everyone else's client comes
//! here, where there are a few tools and no way to change anything an app did
//! not open up: `my_apps` lists the apps the account may open and what each
//! shares, and `query` runs one statement inside that, as the account. The
//! boundary is the scoped authorizer in `runtime::db`, decided per parsed
//! action; the identity comes from the bearer middleware, which put the
//! verified account on the request, and never from the SQL.

use crate::{
    accounts::users::{self, User},
    config::Config,
    content::store::{collect_slugs, read_meta, read_meta_blocking},
    platform::knowledge::{self, FetchOutput, SearchOutput},
    runtime::db,
};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SearchRequest {
    #[schemars(description = "Words to look for in app titles, slugs and notes. Empty lists everything this account may open.")]
    pub(crate) query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct FetchRequest {
    #[schemars(description = "The id a search result carried: a page's slug, or 'guide'.")]
    pub(crate) id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct QueryRequest {
    #[schemars(description = "The app, as my_apps lists it.")]
    pub(crate) app: String,
    #[schemars(
        description = "One SQL statement against the views my_apps lists for that app. SELECT on any of them; INSERT, UPDATE or DELETE only on a view marked writable, where the app's policy decides which rows are yours. Anything else is refused by the server, not by this description."
    )]
    pub(crate) sql: String,
    #[schemars(description = "Values bound to '?' placeholders, in order.")]
    pub(crate) params: Option<Vec<serde_json::Value>>,
}

/// call_app_tool here has no as_user: everyone on this endpoint is a person
/// calling as themselves.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct MeCallAppToolRequest {
    #[schemars(description = "The app slug.")]
    pub(crate) app: String,
    #[schemars(description = "The tool's name as app_tools lists it, without the app prefix.")]
    pub(crate) tool: String,
    #[schemars(description = "The tool's input, matching the input schema app_tools shows.")]
    pub(crate) arguments: Option<serde_json::Value>,
}

/// The tools come from `Self::tool_router()`, which the handler macro calls;
/// nothing else is kept per session.
#[derive(Clone)]
pub struct MeHost {
    config: Arc<Config>,
    /// Runs an app's tool, which is the same call a request makes.
    runtime: Arc<crate::runtime::wasm::Runtime>,
}

/// The account the middleware verified for this request.
fn caller(ctx: &RequestContext<RoleServer>) -> Result<User, McpError> {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<User>().cloned())
        .ok_or_else(|| McpError::invalid_request("no signed-in account on this request", None))
}

#[tool_router]
impl MeHost {
    pub fn new(config: Arc<Config>, runtime: Arc<crate::runtime::wasm::Runtime>) -> Self {
        Self { config, runtime }
    }

    /// Apps this account may open, with what each shares.
    async fn apps_for(&self, user: &User) -> Vec<(String, db::Scope, Vec<(String, Vec<String>)>, Vec<String>)> {
        let mut slugs = Vec::new();
        collect_slugs(&self.config.data_dir, String::new(), &mut slugs).await;
        let mut apps: Vec<String> = slugs
            .into_iter()
            .map(|slug| slug.split('/').next().unwrap_or(&slug).to_string())
            .collect();
        apps.sort();
        apps.dedup();

        let mut out = Vec::new();
        for app in apps {
            if !crate::content::serve::may_open(&self.config, &app, user).await {
                continue;
            }
            let (config, name) = (self.config.clone(), app.clone());
            let (scope, views, writable) = tokio::task::spawn_blocking(move || {
                let meta = read_meta_blocking(&config, &name);
                let scope = db::Scope::of(&meta);
                let mut names: Vec<String> = scope.readable.iter().chain(scope.writable.iter()).cloned().collect();
                names.sort();
                names.dedup();
                // Real names, as declared, so a person types what they see.
                let declared: Vec<String> = meta
                    .queryable
                    .iter()
                    .cloned()
                    .chain(meta.policies.iter().map(|p| p.view.clone()))
                    .filter(|v| names.contains(&v.to_lowercase()))
                    .collect();
                let writable: Vec<String> = meta
                    .policies
                    .iter()
                    .filter(|p| p.write && scope.writable.contains(&p.view.to_lowercase()))
                    .map(|p| p.view.clone())
                    .collect();
                (scope, db::describe_views(&config, &name, &declared), writable)
            })
            .await
            .unwrap_or_else(|_| {
                (
                    db::Scope {
                        readable: Default::default(),
                        writable: Default::default(),
                        triggers: Default::default(),
                        inner: Default::default(),
                    },
                    Vec::new(),
                    Vec::new(),
                )
            });
            if views.is_empty() {
                continue;
            }
            out.push((app, scope, views, writable));
        }
        out
    }

    /// Whether this account may open the page at `slug`: the app's gate and
    /// hidden flag, decided as serving decides them.
    async fn may_see(&self, user: &User, slug: &str) -> bool {
        if read_meta(&self.config, slug).await.hidden {
            return false;
        }
        let app = slug.split('/').next().unwrap_or(slug).to_string();
        crate::content::serve::may_open(&self.config, &app, user).await
    }

    async fn visible_slugs(&self, user: &User) -> Vec<String> {
        let mut slugs = Vec::new();
        collect_slugs(&self.config.data_dir, String::new(), &mut slugs).await;
        let mut out = Vec::new();
        for slug in slugs {
            if self.may_see(user, &slug).await {
                out.push(slug);
            }
        }
        out
    }

    #[tool(
        description = "Find apps and pages this account may open, by words in their title, slug or notes, plus the platform guide when the question is about toolsite itself. Returns ids for fetch.",
        annotations(title = "Search", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_output::<SearchOutput>()
    )]
    pub(crate) async fn search(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(SearchRequest { query }): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let visible = self.visible_slugs(&user).await;
        let output = knowledge::search(&self.config, &query, &visible).await;
        let value = serde_json::to_value(output).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        description = "Read one app or page this account may open as text, by the id search returned: the page's visible words, then the notes kept with the app, with metadata. 'guide' returns the platform guide.",
        annotations(title = "Fetch", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_output::<FetchOutput>()
    )]
    pub(crate) async fn fetch(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(FetchRequest { id }): Parameters<FetchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let refused = || CallToolResult::error(vec![ContentBlock::text(format!("nothing to fetch at {id}"))]);
        if id == knowledge::GUIDE_ID {
            let value = serde_json::to_value(knowledge::fetch_guide(&self.config))
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::structured(value));
        }
        if !crate::content::slug::valid_slug(&id) || !self.may_see(&user, &id).await {
            return Ok(refused());
        }
        match knowledge::fetch_page(&self.config, &id).await {
            Some(output) => {
                let value = serde_json::to_value(output).map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::structured(value))
            }
            None => Ok(refused()),
        }
    }

    #[tool(
        description = "The apps this account may open that share data, and for each the views you may query with their columns. A view marked writable takes INSERT, UPDATE and DELETE as well; the app's policy decides which rows are yours. Call this first; query takes the app and view names exactly as listed.",
        annotations(title = "My apps", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn my_apps(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let apps = self.apps_for(&user).await;
        if apps.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "No app shares data with {}. An app shares views through its toolsite.toml; ask its owner.",
                user.email
            ))]));
        }
        let mut text = format!("Signed in as {}.\n", user.email);
        for (app, _, views, writable) in &apps {
            text.push_str(&format!("\n{app}\n"));
            for (view, columns) in views {
                let mode = if writable.iter().any(|w| w == view) { "read, write" } else { "read" };
                text.push_str(&format!("  {view} ({mode}): {}\n", columns.join(", ")));
            }
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Run one SQL statement as this account against an app's shared views. Reads any view my_apps lists; writes only a view it marks writable, and only the rows the app's policy says are yours: an insert for someone else is aborted, an update or delete of someone else's row changes nothing. Returns columns, rows and rows_affected. Bind values with '?'.",
        annotations(title = "Query shared data", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    pub(crate) async fn query(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(QueryRequest { app, sql, params }): Parameters<QueryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        if !crate::content::slug::valid_slug(&app) || app.contains('/') {
            return Ok(CallToolResult::error(vec![ContentBlock::text("that is not an app name")]));
        }
        if !crate::content::serve::may_open(&self.config, &app, &user).await {
            tracing::warn!(email = %user.email, app = %app, "me/mcp query refused: no access to the app");
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "{} may not open {app}",
                user.email
            ))]));
        }
        let config = self.config.clone();
        let params = params.unwrap_or_default();
        let outcome = tokio::task::spawn_blocking(move || {
            let identity = db::Identity {
                role: users::role_for(&config, &user.id, &app),
                user_id: user.id.clone(),
                email: user.email.clone(),
            };
            let meta = read_meta_blocking(&config, &app);
            let scope = db::Scope::of(&meta);
            tracing::info!(email = %user.email, app = %app, "me/mcp query");
            db::run_scoped(&config, &app, Some(&identity), &scope, &sql, &params)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        match outcome {
            Ok(outcome) => {
                let body = serde_json::json!({
                    "columns": outcome.columns,
                    "rows": outcome.rows,
                    "rows_affected": outcome.rows_affected,
                    "truncated": outcome.truncated,
                });
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string(&body).map_err(|e| McpError::internal_error(e.to_string(), None))?,
                )]))
            }
            Err(message) if message.contains("not authorized") => Ok(CallToolResult::error(vec![ContentBlock::text(
                format!("refused: {message}. Only the views my_apps lists are reachable, and only writable ones take writes."),
            )])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Tools that apps on this site offer. Without app: the apps you may open that declare tools, with how many and whether you pinned them. With app: its tools, each with its input schema, the typed name it has when pinned, and the app's own connector URL. Call one with call_app_tool.",
        annotations(title = "App tools", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn app_tools(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(crate::platform::mcp::AppToolsRequest { app }): Parameters<crate::platform::mcp::AppToolsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(match crate::platform::app_tools::describe(&self.config, app.as_deref(), Some(&user)).await {
            Ok(value) => CallToolResult::structured(value),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    #[tool(
        description = "Call a tool an app offers. The app's own handler runs it as you, with your access and the app's own rules, so it can only do what the app allows you to do. app_tools lists what each app offers and its input.",
        annotations(title = "Call app tool", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn call_app_tool(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(request): Parameters<MeCallAppToolRequest>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let found = match crate::platform::app_tools::find(&self.config, &request.app, &request.tool, Some(&user)).await {
            Ok(found) => found,
            Err(refused) => return Ok(refused),
        };
        let args = request.arguments.unwrap_or_else(|| serde_json::json!({}));
        Ok(crate::platform::app_tools::call(&self.config, &self.runtime, &request.app, &found, args, Some(user)).await)
    }

    #[tool(
        description = "Pin or unpin an app's tools for your account. A pinned app's tools are listed on this connector as typed tools named <app>__<tool>; every other app's tools stay reachable through call_app_tool.",
        annotations(title = "Pin app tools", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn pin_app(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(crate::platform::mcp::PinAppRequest { app, pinned }): Parameters<crate::platform::mcp::PinAppRequest>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(crate::platform::mcp::pin_for(&self.config, Some(&user), &app, pinned).await)
    }
}

#[tool_handler]
impl ServerHandler for MeHost {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        let user = caller(&context)?;
        Ok(crate::platform::mcp::list_with_pins(&self.config, Self::tool_router().list_all(), Some(&user), &context).await)
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        if crate::platform::app_tools::split_name(&request.name).is_some() {
            let user = caller(&context)?;
            if let Some(result) =
                crate::platform::mcp::call_pinned(&self.config, &self.runtime, &request.name, request.arguments.clone(), Some(&user)).await
            {
                return Ok(result.into());
            }
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        Self::tool_router().call(tcc).await
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
            .with_server_info(Implementation::new("toolsite (me)", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Reads the data that apps on this site share with the signed-in account. \
                 Call my_apps first to see which apps and views are available, then query \
                 with one SQL statement at a time. The server enforces what the account may \
                 see and change; a refused statement is a boundary, not a bug to work around.",
            )
    }
}
