//! Tools an app declares in its `toolsite.toml`, offered over MCP.
//!
//! The app writes a handler route and describes it; the platform does the
//! rest. It signs the person in (the same OAuth server and account as every
//! other connector), decides whether they may see the app at all (its
//! general access and their permissions), and calls the route as that
//! person. So the handler's `current-user`, `current-role`, `current_user()`
//! in SQL and the app's row-level policies apply to a tool call exactly as
//! they apply to a page. The app writes no auth code.
//!
//! One code path, three ways in: typed tools named `<app>__<tool>` on the
//! main connectors for apps a person pinned, the `app_tools` and
//! `call_app_tool` pair on the same connectors for every other app, and
//! `/p/<app>/mcp`, a connector with that app's tools alone.
//!
//! A tool's description and its results come from app code. Inside one
//! organisation that is the point; it is still text an Editor wrote, going
//! straight to someone's model.

use crate::{
    accounts::users::User,
    config::Config,
    runtime::wasm::Runtime,
};
use rmcp::model::{CallToolResult, ContentBlock, Icon, MetaObject, Tool, ToolAnnotations};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// MCP limits a tool name to 64 characters of letters, digits, `_` and `-`.
pub const MAX_TOOL_NAME: usize = 64;
/// The most of an error body that reaches the model.
const MAX_ERROR_TEXT: usize = 2048;
/// A handler's request body ceiling, as for any request.
const MAX_ARGUMENTS: usize = 8 * 1024 * 1024;
/// The header that tells a handler which tool was called. Set by the host;
/// a client's copy is stripped before any request reaches a handler.
pub const TOOL_HEADER: &str = "x-toolsite-tool";

/// One declared tool, with its schemas resolved and inlined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub description: String,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub idempotent: bool,
    #[serde(default)]
    pub open_world: bool,
    pub input: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}

impl AppTool {
    /// The title a person reads: the declared one, or the name with its
    /// underscores as spaces and a capital first letter.
    pub fn title(&self) -> String {
        if let Some(title) = self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            return title.trim().to_string();
        }
        let words = self.name.replace('_', " ");
        let mut chars = words.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }
}

/// The tools an app declares. Nothing when it declares none, or when they
/// cannot be read (logged): an app's tools are offered, never required.
pub async fn read(config: &Config, app: &str) -> Vec<AppTool> {
    if !crate::platform::export::valid_app(app) {
        return Vec::new();
    }
    match crate::platform::records::of(config).tools(app).await {
        Ok(Some(text)) => serde_json::from_str(&text).unwrap_or_default(),
        Ok(None) => Vec::new(),
        Err(why) => {
            tracing::warn!(app, %why, "an app's tools could not be read");
            Vec::new()
        }
    }
}

/// Replaces an app's tools wholesale. An empty list removes them.
pub async fn write(config: &Config, app: &str, tools: &[AppTool]) -> Result<(), String> {
    if !crate::platform::export::valid_app(app) {
        return Err("invalid app name".into());
    }
    let text = if tools.is_empty() { None } else { Some(crate::platform::records::pretty(tools)?) };
    crate::platform::records::of(config).set_tools(app, text.as_deref()).await
}

/// `<app>__<tool>`: the name a main connector lists an app's tool under.
/// Platform tools never contain a double underscore, so an app tool can
/// never stand in for one.
pub fn full_name(app: &str, tool: &str) -> String {
    format!("{app}__{tool}")
}

/// The app whose connector a request path is, when it is `/p/<app>/mcp`.
pub fn connector_app(path: &str) -> Option<&str> {
    path.strip_prefix("/p/")
        .and_then(|rest| rest.strip_suffix("/mcp"))
        .filter(|app| !app.contains('/') && crate::platform::export::valid_app(app))
}

/// Splits a main connector's tool name back into app and tool.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let (app, tool) = name.split_once("__")?;
    (!app.is_empty() && !tool.is_empty()).then_some((app, tool))
}

/// A tool name as an app declares it: starts with a letter, no double
/// underscore and no trailing one, so `<app>__<tool>` splits one way only.
pub fn valid_tool_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && !name.ends_with('_')
        && !name.contains("__")
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// An app may offer tools only if its slug cannot blur the split either:
/// no double underscore in it and no underscore at its end.
pub fn app_may_offer_tools(app: &str) -> bool {
    !app.contains("__") && !app.ends_with('_')
}

/// The most tools one app may declare.
pub const MAX_TOOLS: usize = 64;
/// The longest description a model is handed for one tool.
pub const MAX_DESCRIPTION: usize = 2000;
/// The longest title.
pub const MAX_TITLE: usize = 120;
/// The largest a schema may be, serialised.
pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;
/// The deepest a schema may nest.
pub const MAX_SCHEMA_DEPTH: usize = 32;

/// How deeply a JSON value nests.
pub fn depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
        serde_json::Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// A handler route a tool may name: `/api/` and then path segments of
/// letters, digits, `-`, `_` and `.`, none empty and none starting with a
/// dot, so no `..`, no `//`, no percent escapes and no query.
pub fn valid_tool_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/") else {
        return false;
    };
    !rest.is_empty()
        && rest.split('/').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('.')
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

/// Text a person or a model reads: no control characters but line breaks
/// and tabs in a description.
pub fn clean_text(text: &str, allow_lines: bool) -> bool {
    text.chars().all(|c| !c.is_control() || (allow_lines && (c == '\n' || c == '\t')))
}

/// Where a person adds this app as a connector of its own: on the app's
/// host in subdomain mode.
pub fn connector_url(config: &Config, app: &str) -> String {
    format!("{}/p/{app}/mcp", crate::content::origins::app_base(config, app))
}

/// Checks a schema is an object schema, which is what MCP asks of a tool's
/// input and output.
pub fn check_schema(schema: &serde_json::Value, what: &str) -> Result<(), String> {
    if depth(schema) > MAX_SCHEMA_DEPTH {
        return Err(format!("{what} nests deeper than {MAX_SCHEMA_DEPTH} levels"));
    }
    if serde_json::to_vec(schema).map(|v| v.len()).unwrap_or(usize::MAX) > MAX_SCHEMA_BYTES {
        return Err(format!("{what} is over {} KB", MAX_SCHEMA_BYTES / 1024));
    }
    let Some(object) = schema.as_object() else {
        return Err(format!("{what} must be a JSON object schema"));
    };
    match object.get("type").and_then(|t| t.as_str()) {
        Some("object") => Ok(()),
        _ => Err(format!("{what} must have \"type\": \"object\"")),
    }
}

/// The app's name as people read it: its page title, else its slug.
pub async fn app_title(config: &Config, app: &str) -> String {
    match crate::content::store::page_path(config, app).await {
        Some(path) => crate::content::store::page_title(&path).await.unwrap_or_else(|| app.to_string()),
        None => app.to_string(),
    }
}

/// An app's tool as MCP describes it. `prefixed` is for the main
/// connectors, where tools of many apps sit side by side.
pub fn to_mcp(config: &Config, app: &str, app_title: &str, project: &str, tool: &AppTool, prefixed: bool) -> Tool {
    let name = if prefixed { full_name(app, &tool.name) } else { tool.name.clone() };
    let title = if prefixed { format!("{app_title}: {}", tool.title()) } else { tool.title() };
    let input = tool.input.as_object().cloned().unwrap_or_default();
    // Beside toolsite's own tools, the app's words are marked as the app's,
    // so a description that reads like an instruction from the platform is
    // plainly not one. On the app's own connector everything is the app's.
    let description = if prefixed {
        format!("[A tool of the app {app}; the text after this is the app's own.] {}", tool.description)
    } else {
        tool.description.clone()
    };
    let mut mcp = Tool::new(name, description, Arc::new(input))
        .with_title(title.clone())
        .with_annotations(
            ToolAnnotations::from_raw(
                Some(title),
                Some(tool.read_only),
                Some(tool.destructive),
                Some(tool.idempotent),
                Some(tool.open_world),
            ),
        )
        .with_icons(vec![Icon::new(format!(
            "{}/p/{app}/favicon.svg",
            crate::content::origins::app_base(config, app)
        ))
        .with_mime_type("image/svg+xml")]);
    if let Some(output) = tool.output.as_ref().and_then(|o| o.as_object()) {
        mcp = mcp.with_raw_output_schema(Arc::new(output.clone()));
    }
    let mut meta = serde_json::Map::new();
    meta.insert("io.toolsite/app".into(), serde_json::Value::String(app.to_string()));
    meta.insert("io.toolsite/project".into(), serde_json::Value::String(project.to_string()));
    meta.insert("io.toolsite/tool".into(), serde_json::Value::String(tool.name.clone()));
    mcp.with_meta(MetaObject(meta))
}

/// Whether this caller may see and call the app's tools. `None` is a static
/// token, which has every power. A person needs the app to exist, not be
/// hidden, and to let them open it, by the same rule serving uses.
pub async fn may_use(config: &Arc<Config>, app: &str, user: Option<&User>) -> bool {
    if !crate::platform::export::valid_app(app) || !crate::content::store::app_exists(config, app).await {
        return false;
    }
    match user {
        None => true,
        Some(user) => crate::content::serve::may_open(config, app, user).await,
    }
}

/// Whether this caller may reach one tool's route. An app can close a
/// path inside itself with a route rule, and a tool on that path is closed
/// to whoever the rule keeps out, exactly as the route is.
pub async fn may_reach(config: &Arc<Config>, app: &str, tool: &AppTool, user: Option<&User>) -> bool {
    match user {
        None => true,
        Some(user) => {
            let gate = crate::content::store::effective_gate(config, app, &tool.path).await.gate;
            crate::content::serve::admits(config, &gate, app, Some(user)).await
        }
    }
}

/// The app's tools this caller may reach, or none when they may not open
/// the app at all.
pub async fn reachable(config: &Arc<Config>, app: &str, user: Option<&User>) -> Vec<AppTool> {
    if !may_use(config, app, user).await {
        return Vec::new();
    }
    let mut out = Vec::new();
    for tool in read(config, app).await {
        if may_reach(config, app, &tool, user).await {
            out.push(tool);
        }
    }
    out
}

/// Every app that declares tools.
pub async fn apps_with_tools(config: &Config) -> Vec<String> {
    crate::platform::records::of(config).apps_with_tools().await.unwrap_or_else(|why| {
        tracing::warn!(%why, "the apps with tools could not be listed");
        Vec::new()
    })
}

/// The typed tools a main connector lists for this caller: the tools of
/// every app they pinned and may still open. A static token pins nothing.
pub async fn pinned_tools(config: &Arc<Config>, user: Option<&User>) -> Vec<Tool> {
    let Some(user) = user else {
        return Vec::new();
    };
    let pins = {
        let (config, id) = (config.clone(), user.id.clone());
        tokio::task::spawn_blocking(move || crate::accounts::users::pins_for(&config, &id))
            .await
            .unwrap_or_default()
    };
    let mut listed = Vec::new();
    for app in pins {
        let tools = reachable(config, &app, Some(user)).await;
        if tools.is_empty() {
            continue;
        }
        let title = app_title(config, &app).await;
        let project = crate::content::catalog::meta(config, &app).await.project.unwrap_or_default();
        for tool in &tools {
            listed.push(to_mcp(config, &app, &title, &project, tool, true));
        }
    }
    listed
}

/// Whether this account pinned the app.
pub async fn is_pinned(config: &Arc<Config>, user: &User, app: &str) -> bool {
    let (config, id) = (config.clone(), user.id.clone());
    let app = app.to_string();
    tokio::task::spawn_blocking(move || crate::accounts::users::pins_for(&config, &id).contains(&app))
        .await
        .unwrap_or(false)
}

fn refusal(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text.into())])
}

/// The answer to "may this caller call this app's tool", shared by every
/// way in: nothing distinguishes an app the caller may not open from one
/// that does not exist, so a tool cannot be used to learn which apps exist.
pub async fn find(config: &Arc<Config>, app: &str, tool: &str, user: Option<&User>) -> Result<AppTool, CallToolResult> {
    reachable(config, app, user)
        .await
        .into_iter()
        .find(|t| t.name == tool)
        .ok_or_else(|| refusal(format!("no such tool: {app}/{tool}")))
}

/// Calls an app's tool as `user` (`None` calls as nobody, which a handler
/// sees as an anonymous visitor). The caller has already decided this
/// person may: use `find` first.
pub async fn call(
    config: &Arc<Config>,
    runtime: &Arc<Runtime>,
    app: &str,
    tool: &AppTool,
    arguments: serde_json::Value,
    user: Option<User>,
) -> CallToolResult {
    let body = serde_json::json!({ "tool": tool.name, "arguments": arguments }).to_string();
    if body.len() > MAX_ARGUMENTS {
        return refusal("the arguments are over the 8 MB request limit");
    }
    let Ok(wasm) = tokio::fs::read(config.data_dir.join(app).join("handler.wasm")).await else {
        return refusal(format!("{app} has no handler to run this tool"));
    };
    let request = crate::runtime::wasm::Request {
        method: "POST".to_string(),
        path: tool.path.clone(),
        query: String::new(),
        headers: vec![
            ("content-type".to_string(), "application/json".to_string()),
            (TOOL_HEADER.to_string(), tool.name.clone()),
        ],
        body: body.into_bytes(),
    };
    let visitor = user.map(|user| crate::runtime::wasm::User { id: user.id, email: user.email });
    let guards = crate::runtime::limits::of(config, app).await.request;
    let (runtime, config, owned_app) = (runtime.clone(), config.clone(), app.to_string());
    let outcome = tokio::task::spawn_blocking(move || {
        runtime.handle(config, &owned_app, &wasm, visitor, request, guards)
    })
    .await;
    let response = match outcome {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!(app = %app, tool = %tool.name, error = %error, "app tool failed");
            return refusal("the app's handler failed while running this tool");
        }
        Err(_) => return refusal("the app's handler stopped while running this tool"),
    };
    let text = String::from_utf8_lossy(&response.body).to_string();
    if !(200..300).contains(&response.status) {
        let short: String = text.chars().take(MAX_ERROR_TEXT).collect();
        return refusal(format!("{} answered {}: {short}", tool.name, response.status));
    }
    let is_json = response
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("content-type") && value.contains("json"));
    if is_json
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
    {
        return CallToolResult::structured(value);
    }
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// What `app_tools` says: with an app, its tools and their inputs; without
/// one, the apps this caller may open that offer tools.
pub async fn describe(config: &Arc<Config>, app: Option<&str>, user: Option<&User>) -> Result<serde_json::Value, String> {
    match app {
        Some(app) => {
            let tools = reachable(config, app, user).await;
            if tools.is_empty() {
                return Err(format!("no such app with tools: {app}"));
            }
            let pinned = match user {
                Some(user) => is_pinned(config, user, app).await,
                None => false,
            };
            Ok(serde_json::json!({
                "app": app,
                "title": app_title(config, app).await,
                "connector": connector_url(config, app),
                "pinned": pinned,
                "tools": tools.iter().map(|t| serde_json::json!({
                    "name": t.name,
                    "typed_name": full_name(app, &t.name),
                    "title": t.title(),
                    "description": t.description,
                    "read_only": t.read_only,
                    "destructive": t.destructive,
                    "input": t.input,
                    "output": t.output,
                })).collect::<Vec<_>>(),
            }))
        }
        None => {
            let mut apps = Vec::new();
            for app in apps_with_tools(config).await {
                let tools = reachable(config, &app, user).await;
                if tools.is_empty() {
                    continue;
                }
                let pinned = match user {
                    Some(user) => is_pinned(config, user, &app).await,
                    None => false,
                };
                apps.push(serde_json::json!({
                    "app": app,
                    "title": app_title(config, &app).await,
                    "tools": tools.len(),
                    "pinned": pinned,
                    "connector": connector_url(config, &app),
                }));
            }
            Ok(serde_json::json!({ "apps": apps }))
        }
    }
}

/// The MCP server at `/p/<app>/mcp`: that app's tools, unprefixed, and
/// nothing else. The app and the caller arrive on each request from the
/// middleware, so one server answers for every app.
#[derive(Clone)]
pub struct AppHost {
    config: Arc<Config>,
    runtime: Arc<Runtime>,
}

impl AppHost {
    pub fn new(config: Arc<Config>, runtime: Arc<Runtime>) -> Self {
        Self { config, runtime }
    }

    fn who(context: &rmcp::service::RequestContext<rmcp::RoleServer>) -> Result<(String, Option<User>), rmcp::ErrorData> {
        let parts = context
            .extensions
            .get::<axum::http::request::Parts>()
            .ok_or_else(|| rmcp::ErrorData::invalid_request("no HTTP request", None))?;
        let app = parts
            .extensions
            .get::<crate::platform::bearer::ToolApp>()
            .map(|a| a.0.clone())
            .ok_or_else(|| rmcp::ErrorData::invalid_request("no app on this request", None))?;
        let user = parts
            .extensions
            .get::<crate::platform::bearer::Caller>()
            .and_then(|c| c.user.clone());
        Ok((app, user))
    }
}

impl rmcp::ServerHandler for AppHost {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig};
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
            .with_server_info(Implementation::new("toolsite app", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "The tools of one app on this site. Each runs as the signed-in person, with \
                 their access and the app's own rules; a refusal is the app's decision.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let (app, user) = Self::who(&context)?;
        let mut tools = Vec::new();
        {
            let declared = reachable(&self.config, &app, user.as_ref()).await;
            if !declared.is_empty() {
                let title = app_title(&self.config, &app).await;
                let project = crate::content::catalog::meta(&self.config, &app).await.project.unwrap_or_default();
                tools = declared
                    .iter()
                    .map(|tool| to_mcp(&self.config, &app, &title, &project, tool, false))
                    .collect();
            }
        }
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Private),
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let (app, user) = Self::who(&context)?;
        let found = match find(&self.config, &app, &request.name, user.as_ref()).await {
            Ok(found) => found,
            Err(refused) => return Ok(refused.into()),
        };
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        Ok(call(&self.config, &self.runtime, &app, &found, args, user).await.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_name_cannot_pose_as_a_platform_tool_or_break_the_prefix() {
        assert!(valid_tool_name("log_production"));
        assert!(!valid_tool_name("log__production"), "a double underscore would split wrongly");
        assert!(!valid_tool_name("Log"), "upper case is refused");
        assert!(!valid_tool_name("log-production"));
        assert!(!valid_tool_name(""));
        assert_eq!(split_name("production__log_production"), Some(("production", "log_production")));
        assert_eq!(split_name("run_sql"), None);
    }

    #[test]
    fn a_title_falls_back_to_the_name() {
        let tool = AppTool {
            name: "log_production".into(),
            title: None,
            description: String::new(),
            path: "/api/x".into(),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
            input: serde_json::json!({"type":"object"}),
            output: None,
        };
        assert_eq!(tool.title(), "Log production");
    }

    #[test]
    fn a_schema_must_be_an_object_schema() {
        assert!(check_schema(&serde_json::json!({"type":"object"}), "input").is_ok());
        assert!(check_schema(&serde_json::json!({"type":"string"}), "input").is_err());
        assert!(check_schema(&serde_json::json!([1]), "input").is_err());
    }
}
