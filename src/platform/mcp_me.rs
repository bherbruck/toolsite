//! The MCP server for a regular account: `/me/mcp`.
//!
//! An admin's client publishes through `/mcp`. Everyone else's client comes
//! here, where there are two tools and no way to change anything an app did
//! not open up: `my_apps` lists the apps the account may open and what each
//! shares, and `query` runs one statement inside that, as the account. The
//! boundary is the scoped authorizer in `runtime::db`, decided per parsed
//! action; the identity comes from the bearer middleware, which put the
//! verified account on the request, and never from the SQL.

use crate::{
    accounts::users::{self, User},
    config::Config,
    content::store::{collect_slugs, read_meta_blocking},
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

/// The tools come from `Self::tool_router()`, which the handler macro calls;
/// nothing else is kept per session.
#[derive(Clone)]
pub struct MeHost {
    config: Arc<Config>,
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
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
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

    #[tool(
        description = "The apps this account may open that share data, and for each the views you may query with their columns. A view marked writable takes INSERT, UPDATE and DELETE as well; the app's policy decides which rows are yours. Call this first; query takes the app and view names exactly as listed."
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
        description = "Run one SQL statement as this account against an app's shared views. Reads any view my_apps lists; writes only a view it marks writable, and only the rows the app's policy says are yours: an insert for someone else is aborted, an update or delete of someone else's row changes nothing. Returns columns, rows and rows_affected. Bind values with '?'."
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
}

#[tool_handler]
impl ServerHandler for MeHost {
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
