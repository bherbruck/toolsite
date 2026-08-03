use crate::{
    config::Config,
    content::{
        slug::{random_slug, random_token, valid_segment, valid_slug},
        store::{collect_slugs, page_path, page_title, page_url, read_meta, relative_time, write_meta},
    },
    platform::upload::{upload_url, UploadTicket, MAX_ICON_BYTES, UPLOAD_TTL},
    runtime::db,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities,
        ServerInfo,
    },
    tool, tool_handler, tool_router,
    ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Instant, SystemTime},
};
use tokio::fs;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct PushPageRequest {
    #[schemars(description = "Full self-contained HTML document (inline any CSS/JS).")]
    pub(crate) html: String,
    #[schemars(
        description = "Optional URL slug. Random one is generated if omitted. Reusing a slug overwrites that page. May contain '/' to namespace it under an app, e.g. 'myapp/about'."
    )]
    pub(crate) slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct PullPageRequest {
    #[schemars(description = "Slug of the page to fetch the current HTML for.")]
    pub(crate) slug: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct PushAppRequest {
    #[schemars(
        description = "App namespace all pages are published under, e.g. 'myapp'. Letters, numbers, '-' and '_' only."
    )]
    pub(crate) app: String,
    #[schemars(
        description = "Map of page name to full HTML document, e.g. {\"index\": \"<html>...\", \"about\": \"<html>...\"}. A page named 'index' is also served at the app's own root URL."
    )]
    pub(crate) pages: HashMap<String, String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateUploadRequest {
    #[schemars(
        description = "Slug the upload writes to. Random one is generated if omitted. For a multi-page app pass the app name, then upload one file per page."
    )]
    pub(crate) slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SetIconRequest {
    #[schemars(description = "Slug of the page to set the icon for.")]
    pub(crate) slug: String,
    #[schemars(
        description = "An emoji, a full inline <svg>...</svg>, or a data: URI. For a raster image file, use create_upload and PUT it to <upload-url>?icon instead."
    )]
    pub(crate) icon: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SetVisibilityRequest {
    #[schemars(description = "Slug of the page to change.")]
    pub(crate) slug: String,
    #[schemars(
        description = "true takes the page down: its URL 404s and it leaves the index. Nothing is deleted — set false to bring it straight back."
    )]
    pub(crate) hidden: Option<bool>,
    #[schemars(
        description = "false keeps the page working at its URL but removes it from the site index. Use for scratch or link-only pages."
    )]
    pub(crate) listed: Option<bool>,
    #[schemars(
        description = "Who may reach the app: 'public' (anyone), 'authenticated' (any signed-in account), or 'granted' (only accounts given access with set_access)."
    )]
    pub(crate) gate: Option<String>,
    #[schemars(
        description = "Apply the gate to paths starting with this prefix instead of the whole app, e.g. '/admin' or '/api/all'. Longest matching prefix wins, so a public app can have a private corner and a private app a public front page. Pass the prefix with no gate to drop the rule."
    )]
    pub(crate) path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ListPagesRequest {
    #[schemars(
        description = "Include pages that are hidden or unlisted. Defaults to false, which lists only what a visitor would see."
    )]
    pub(crate) include_all: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct PullAppRequest {
    #[schemars(description = "App namespace to fetch all pages for.")]
    pub(crate) app: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateUserRequest {
    #[schemars(description = "Email address, which is the account's identity across every app on this site.")]
    pub(crate) email: String,
    #[schemars(
        description = "Optional. Leave it out and the reply carries a one-time link the person opens to choose their own password, which is usually what you want — nobody else ever handles it."
    )]
    pub(crate) password: Option<String>,
    #[schemars(
        description = "Make this account an admin, able to see every account and change any app's access at /admin. Defaults to false."
    )]
    pub(crate) admin: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ScheduleRequest {
    #[schemars(description = "App whose jobs these are.")]
    pub(crate) app: String,
    #[schemars(description = "Job name. Omit to list what is scheduled.")]
    pub(crate) name: Option<String>,
    #[schemars(
        description = "Cron with seconds first, six fields: '0 */5 * * * *' is every five minutes, '0 0 3 * * *' is 03:00 daily. Omit with a name to remove that job."
    )]
    pub(crate) schedule: Option<String>,
    #[schemars(
        description = "Path handed to the app's handler when it fires, e.g. /api/refresh. The handler sees a x-toolsite-scheduled header and no signed-in user."
    )]
    pub(crate) path: Option<String>,
    #[schemars(description = "Run the named job now, whatever its schedule says.")]
    pub(crate) run_now: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SecretRequest {
    #[schemars(description = "App the setting belongs to.")]
    pub(crate) app: String,
    #[schemars(
        description = "Name the handler reads it by, e.g. API_KEY. Letters, numbers and '_'. Omit to list the names already set."
    )]
    pub(crate) name: Option<String>,
    #[schemars(
        description = "The value. Prefer leaving this out and passing link: true instead, so the value never enters the conversation. Omit while giving a name to remove that setting."
    )]
    pub(crate) value: Option<String>,
    #[schemars(
        description = "Return a link the site's owner opens to paste values in themselves, one NAME=value per line. Use this rather than asking someone to tell you a secret."
    )]
    pub(crate) link: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct NotesRequest {
    #[schemars(description = "App or page slug the notes belong to.")]
    pub(crate) slug: String,
    #[schemars(
        description = "Markdown for whoever works on this next: the schema, decisions and their reasons, what is unfinished. Omit to read what is already there instead of writing."
    )]
    pub(crate) notes: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SetActiveRequest {
    #[schemars(description = "Email of the account to turn off or back on.")]
    pub(crate) email: String,
    #[schemars(
        description = "false disables the account: it cannot sign in and its existing sessions stop working immediately. Nothing is deleted, so true restores it."
    )]
    pub(crate) active: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct AccessRequest {
    #[schemars(description = "App the grant applies to.")]
    pub(crate) app: String,
    #[schemars(description = "Email of an existing account.")]
    pub(crate) email: String,
    #[schemars(description = "Set false to take the grant away. Defaults to true.")]
    pub(crate) allow: Option<bool>,
    #[schemars(
        description = "What this account is on this app — 'viewer', 'editor', anything the app understands. The handler reads it through identity.current-role and decides what it means. Defaults to 'viewer'."
    )]
    pub(crate) role: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct RunSqlRequest {
    #[schemars(
        description = "App whose database to run against. Every app has its own file; there is no shared database."
    )]
    pub(crate) app: String,
    #[schemars(
        description = "SQL to run. Pass one statement when using params; a parameterless script may hold several statements, which is how a schema migration is applied."
    )]
    pub(crate) sql: String,
    #[schemars(
        description = "Values bound to '?' placeholders, in order. Always bind values rather than building SQL by concatenation."
    )]
    pub(crate) params: Option<Vec<serde_json::Value>>,
}

#[derive(Clone)]
pub struct PageHost {
    pub(crate) config: Arc<Config>,
    /// Needed to run a job on demand, which is the same call a request makes.
    pub(crate) runtime: Arc<crate::runtime::wasm::Runtime>,
    #[allow(dead_code)]
    pub(crate) tool_router: ToolRouter<PageHost>,
}

#[tool_router]
impl PageHost {
    pub fn new(config: Arc<Config>, runtime: Arc<crate::runtime::wasm::Runtime>) -> Self {
        Self {
            config,
            runtime,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Fallback publisher for when you cannot reach this host from a shell: pastes the page's HTML through this call. If you can run shell commands with network access, use create_upload instead. Call again with the same slug to update in place. Use a slug like 'myapp/about' to group pages under one app."
    )]
    async fn push_page(
        &self,
        Parameters(PushPageRequest { html, slug }): Parameters<PushPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = slug.unwrap_or_else(random_slug);
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }

        let path = self.config.data_dir.join(format!("{slug}.html"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        fs::write(&path, html)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let url = page_url(&self.config, &slug);
        Ok(CallToolResult::success(vec![ContentBlock::text(url)]))
    }

    #[tool(description = "Fetch the current HTML source of a previously pushed page by its slug, so it can be edited and pushed back.")]
    async fn pull_page(
        &self,
        Parameters(PullPageRequest { slug }): Parameters<PullPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let path = self.config.data_dir.join(format!("{slug}.html"));
        match fs::read_to_string(&path).await {
            Ok(html) => Ok(CallToolResult::success(vec![ContentBlock::text(html)])),
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no page found for slug '{slug}'"
            ))])),
        }
    }

    #[tool(
        description = "Fallback publisher for a whole multi-page app when you cannot reach this host from a shell: pastes every page's HTML through this call. If you can run shell commands with network access, use create_upload instead. A page named 'index' is also served at the app's own root URL. Returns each page's URL."
    )]
    async fn push_app(
        &self,
        Parameters(PushAppRequest { app, pages }): Parameters<PushAppRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_segment(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty and contain only letters, numbers, '-' or '_'",
            )]));
        }
        if pages.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "pages must not be empty",
            )]));
        }
        for name in pages.keys() {
            if !valid_segment(name) {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "page name '{name}' must be non-empty and contain only letters, numbers, '-' or '_'"
                ))]));
            }
        }

        let app_dir = self.config.data_dir.join(&app);
        fs::create_dir_all(&app_dir)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut urls = Vec::new();
        for (name, html) in &pages {
            let path = app_dir.join(format!("{name}.html"));
            fs::write(&path, html)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            // 'index' is what the app root serves, so report that URL for it.
            let slug = if name == "index" {
                app.clone()
            } else {
                format!("{app}/{name}")
            };
            urls.push(format!("{name}: {}", page_url(&self.config, &slug)));
        }
        urls.sort();
        Ok(CallToolResult::success(vec![ContentBlock::text(
            urls.join("\n"),
        )]))
    }

    #[tool(
        description = "The default way to publish. Returns a short-lived upload URL; write the HTML to a local file, then PUT the file to that URL with curl. Do not paste HTML into this call — the point is that the page never passes through the conversation. Works for a single page, a multi-page app, or a whole built front-end (TypeScript/React/Vite — tar the dist folder to ?bundle). The response includes the base path the build must be configured with. If the upload URL turns out to be unreachable from your sandbox, fall back to push_page/push_app."
    )]
    async fn create_upload(
        &self,
        Parameters(CreateUploadRequest { slug }): Parameters<CreateUploadRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = slug.unwrap_or_else(random_slug);
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }

        let ticket = random_token(32);
        {
            let now = Instant::now();
            let mut tickets = self.config.uploads.lock().unwrap();
            tickets.retain(|_, t| t.expires_at > now);
            tickets.insert(
                ticket.clone(),
                UploadTicket {
                    slug: slug.clone(),
                    expires_at: now + UPLOAD_TTL,
                },
            );
        }

        let upload = upload_url(&self.config, &ticket);
        let minutes = UPLOAD_TTL.as_secs() / 60;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Upload with (expires in {minutes} min, reusable until then):\n\
             \n  curl -fT <file.html> {upload}\n\
             \nMulti-page app — append the page name, one PUT per page:\n\
             \n  curl -fT index.html {upload}/index\n  curl -fT about.html {upload}/about\n\
             \nWhole built site (TypeScript/React/Vite/Svelte/etc), gzipped tar of dist:\n\
             \n  tar -czf - -C dist . | curl -f -T - '{upload}?bundle'\n\
             \nAdd &spa for a client-side router, so unknown paths serve index.html:\n\
             \n  tar -czf - -C dist . | curl -f -T - '{upload}?bundle&spa'\n\
             \nIMPORTANT — this app is served from {base}, not from the domain root, so a \
             default build config will emit /assets/... URLs that 404 and render a blank \
             page. Before building, set the base path:\n\
             \n  vite.config: base: '{base}'\n  next.config: basePath: '{trimmed}', assetPrefix: '{base}'\n  \
             create-react-app package.json: \"homepage\": \"{base}\"\n\
             \nWith a router, set its basename to '{trimmed}' too (e.g. \
             createBrowserRouter(routes, {{ basename: '{trimmed}' }})).\n\
             \nAfter uploading, verify with: curl -I {page}/assets/<one-built-file>\n\
             \nServer-side code — a wasm component that gets this app's own SQLite database \
             and nothing else. Start from the scaffold; it vendors the contract and builds \
             as-is:\n\
             \n  curl -s {site}/scaffold/{slug} | tar xz && cd {slug}-handler\n\
             \n  rustup target add wasm32-wasip2\n\
             \n  cargo build --release --target wasm32-wasip2\n\
             \n  curl -f -T target/wasm32-wasip2/release/*.wasm '{upload}?handler'\n\
             \nThe contract alone is at {site}/wit/toolsite.wit.\n\
             \nKeep the project with the app, since a bundle cannot be turned back into the \
             sources that built it. Visitors only ever see what the bundle contained:\n\
             \n  tar -czf - --exclude node_modules --exclude target . | curl -f -T - '{upload}?source'\n\
             \nAnd to pick up where a previous session left off:\n\
             \n  curl -s '{upload}?source' | tar xz\n\
             \nIt then answers every request under {page}/api/, and any route with no file              behind it. It is rejected at upload if it is not a valid component.\n\
             \nEach upload replies with the page's public URL. Single-file page lands at {page}",
            page = page_url(&self.config, &slug),
            site = self
                .config
                .base_url
                .as_deref()
                .unwrap_or(&self.config.local_base),
            base = format!("/p/{slug}/"),
            trimmed = format!("/p/{slug}"),
        ))]))
    }

    #[tool(
        description = "Run SQL against one app's own SQLite database — create tables, seed or inspect data. Only reachable over MCP, never from a published page, so it is safe for schema work but is not how an app reads its own data at runtime."
    )]
    pub(crate) async fn run_sql(
        &self,
        Parameters(RunSqlRequest { app, sql, params }): Parameters<RunSqlRequest>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.config.clone();
        let params = params.unwrap_or_default();
        let outcome =
            tokio::task::spawn_blocking(move || db::run(&config, &app, &sql, &params))
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
                    serde_json::to_string(&body)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Create an account someone can sign in with. Accounts are global to the site; use set_access to say which apps they may reach. There is no public signup, so this is the only way an account comes into being."
    )]
    pub(crate) async fn create_user(
        &self,
        Parameters(CreateUserRequest {
            email,
            password,
            admin,
        }): Parameters<CreateUserRequest>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.config.clone();
        let outcome =
            tokio::task::spawn_blocking(move || match password {
                Some(password) => crate::accounts::users::sign_up_as(
                    &config,
                    &email,
                    &password,
                    admin.unwrap_or(false),
                )
                .map(|user| (user, None)),
                None => crate::accounts::users::invite(&config, &email, admin.unwrap_or(false))
                    .map(|(user, token)| {
                        let url = crate::accounts::users::invite_url(&config, &token);
                        (user, Some(url))
                    }),
            })
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match outcome {
            Ok((user, invite)) => {
                let admin = if user.is_admin { " as an admin" } else { "" };
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    match invite {
                        Some(url) => format!(
                            "created {}{}. Send them this link to choose a password \
                             (good for 48 hours, works once):\n{url}",
                            user.email, admin
                        ),
                        None => format!("created {}{} ({})", user.email, admin, user.id),
                    },
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Schedule an app's handler to run on its own — refreshing a cache, pulling from an API, tidying a table. A job is a cron expression and a path, and firing it calls the same handler a request would, with the same sandbox and limits. Give run_now to trigger one immediately, or no name to list them with when each last ran."
    )]
    pub(crate) async fn app_jobs(
        &self,
        Parameters(ScheduleRequest {
            app,
            name,
            schedule,
            path,
            run_now,
        }): Parameters<ScheduleRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let state = crate::AppState {
            config: self.config.clone(),
            runtime: self.runtime.clone(),
        };

        let outcome: Result<String, String> = match (name, schedule, path, run_now) {
            (Some(name), _, _, Some(true)) => {
                crate::platform::schedule::run_job(&state, &app, &name)
                    .await
                    .map(|status| format!("{name} ran: {status}"))
            }
            (Some(name), Some(schedule), Some(path), _) => {
                crate::platform::schedule::set_job(&self.config, &app, &name, &schedule, &path)
                    .map(|next| format!("{name} scheduled; {next}"))
            }
            (Some(name), None, None, _) => {
                crate::platform::schedule::remove_job(&self.config, &app, &name)
                    .map(|()| format!("{name} is no longer scheduled"))
            }
            (Some(_), _, _, _) => Err("give both schedule and path, or neither to remove".into()),
            (None, _, _, _) => {
                let jobs = crate::platform::schedule::read_jobs(&self.config, &app);
                Ok(if jobs.is_empty() {
                    format!("{app} has no scheduled jobs")
                } else {
                    jobs.iter()
                        .map(|(name, job)| {
                            format!(
                                "{name}: {} -> {} (last: {})",
                                job.schedule,
                                job.path,
                                job.last_status.as_deref().unwrap_or("never run")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            }
        };

        match outcome {
            Ok(message) => Ok(CallToolResult::success(vec![ContentBlock::text(message)])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "An app's settings — API keys and the like, which its handler reads through the secrets import. Pass link: true to get a URL the owner opens to paste values in, which is the right way: a secret you never see cannot leak through you. Values never come back out: this lists names only, they are absent from the source archive, and no URL serves them. Give a name and value to set, a name alone to remove, neither to list."
    )]
    pub(crate) async fn app_settings(
        &self,
        Parameters(SecretRequest {
            app,
            name,
            value,
            link,
        }): Parameters<SecretRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }

        if link.unwrap_or(false) {
            let url = crate::platform::secrets::create_entry(&self.config, &app)
                .map_err(|e| McpError::internal_error(e, None))?;
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Send this to whoever holds the credentials. It lasts an hour, takes one \
                 NAME=value per line, and the values never come back through here:\n{url}"
            ))]));
        }

        let config = self.config.clone();
        let outcome = tokio::task::spawn_blocking(move || match name {
            Some(name) => crate::platform::secrets::set(&config, &app, &name, value.as_deref())
                .map(|()| match value {
                    Some(_) => format!("{name} is set for {app}"),
                    None => format!("{name} is gone from {app}"),
                }),
            None => {
                let names = crate::platform::secrets::names(&config, &app);
                Ok(if names.is_empty() {
                    format!("{app} has no settings")
                } else {
                    format!("{app}: {}", names.join(", "))
                })
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        match outcome {
            Ok(message) => Ok(CallToolResult::success(vec![ContentBlock::text(message)])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Read or write notes kept with an app for the next session to find. Call it with no notes to read. Write down the database schema, why things are the way they are, and what is half-finished — a later session sees only the rendered page otherwise, and a bundle's source is not recoverable from it."
    )]
    pub(crate) async fn app_notes(
        &self,
        Parameters(NotesRequest { slug, notes }): Parameters<NotesRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }

        match notes {
            Some(notes) => {
                crate::content::store::write_notes(&self.config, &slug, &notes)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "saved {} characters of notes for {slug}",
                    notes.len()
                ))]))
            }
            None => match crate::content::store::read_notes(&self.config, &slug).await {
                Some(notes) => Ok(CallToolResult::success(vec![ContentBlock::text(notes)])),
                None => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "no notes for {slug} yet"
                ))])),
            },
        }
    }

    #[tool(
        description = "Turn an account off or back on. A disabled account cannot sign in and its live sessions stop working at once, but it is not deleted."
    )]
    pub(crate) async fn set_user_active(
        &self,
        Parameters(SetActiveRequest { email, active }): Parameters<SetActiveRequest>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.config.clone();
        let owned = email.clone();
        let outcome =
            tokio::task::spawn_blocking(move || crate::accounts::users::set_active(&config, &owned, active))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match outcome {
            Ok(()) if active => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{email} is active again"
            ))])),
            Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{email} is disabled; its sessions are gone"
            ))])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Give or take away one account's access to one app. Only matters for apps whose gate is 'granted'."
    )]
    pub(crate) async fn set_access(
        &self,
        Parameters(AccessRequest {
            app,
            email,
            allow,
            role,
        }): Parameters<AccessRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let allow = allow.unwrap_or(true);
        let config = self.config.clone();
        let (owned_app, owned_email) = (app.clone(), email.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            if allow {
                crate::accounts::users::grant(
                    &config,
                    &owned_email,
                    &owned_app,
                    role.as_deref().unwrap_or("viewer"),
                )
            } else {
                crate::accounts::users::revoke(&config, &owned_email, &owned_app)
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        match outcome {
            Ok(()) if allow => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{email} can now reach {app}"
            ))])),
            Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "{email} can no longer reach {app}"
            ))])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "List published pages: slug, title, URL, when each was last changed, and its visibility. Call this to find out what already exists before editing or reusing a slug."
    )]
    async fn list_pages(
        &self,
        Parameters(ListPagesRequest { include_all }): Parameters<ListPagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let include_all = include_all.unwrap_or(false);
        let mut slugs = Vec::new();
        collect_slugs(&self.config.data_dir, String::new(), &mut slugs).await;

        let mut rows = Vec::new();
        for slug in slugs {
            let meta = read_meta(&self.config, &slug).await;
            if !include_all && (meta.hidden || !meta.listed) {
                continue;
            }
            let path = page_path(&self.config, &slug).await;
            let modified = match &path {
                Some(p) => fs::metadata(p).await.ok().and_then(|m| m.modified().ok()),
                None => None,
            };
            let title = match &path {
                Some(p) => page_title(p).await,
                None => None,
            };
            rows.push(serde_json::json!({
                "slug": slug,
                "title": title,
                "url": page_url(&self.config, &slug),
                "modified": modified.map(relative_time),
                "modified_epoch": modified
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                "listed": meta.listed,
                "hidden": meta.hidden,
            }));
        }
        // Most recently touched first: that's what a follow-up edit is after.
        rows.sort_by_key(|r| std::cmp::Reverse(r["modified_epoch"].as_u64().unwrap_or(0)));

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "no pages published yet",
            )]));
        }
        let json = serde_json::to_string(&rows)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Take a page down or restore it, and control whether it appears on the site index. Nothing is ever deleted, so this is the safe way to retract a page published by mistake."
    )]
    async fn set_visibility(
        &self,
        Parameters(SetVisibilityRequest {
            slug,
            hidden,
            listed,
            gate,
            path,
        }): Parameters<SetVisibilityRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        if hidden.is_none() && listed.is_none() && gate.is_none() && path.is_none() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "pass hidden, listed or gate",
            )]));
        }
        if let Some(gate) = &gate {
            if !matches!(gate.as_str(), "public" | "authenticated" | "granted") {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "gate must be 'public', 'authenticated' or 'granted'",
                )]));
            }
        }
        if page_path(&self.config, &slug).await.is_none() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no page found for slug '{slug}'"
            ))]));
        }

        let mut meta = read_meta(&self.config, &slug).await;
        if let Some(hidden) = hidden {
            meta.hidden = hidden;
        }
        if let Some(listed) = listed {
            meta.listed = listed;
        }
        match (path, gate) {
            // A rule for one corner of the app.
            (Some(prefix), Some(gate)) => {
                meta.rules.retain(|rule| rule.prefix != prefix);
                meta.rules.push(crate::content::store::PathRule { prefix, gate });
            }
            (Some(prefix), None) => {
                let before = meta.rules.len();
                meta.rules.retain(|rule| rule.prefix != prefix);
                if meta.rules.len() == before {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "{slug} has no rule for {prefix}"
                    ))]));
                }
            }
            (None, Some(gate)) => meta.gate = gate,
            (None, None) => {}
        }
        write_meta(&self.config, &slug, &meta)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let rules = if meta.rules.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                meta.rules
                    .iter()
                    .map(|rule| format!("{} is {}", rule.prefix, rule.gate))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let state = match (meta.hidden, meta.listed) {
            (true, _) => "hidden (URL returns 404; set hidden=false to restore)".to_string(),
            (false, false) => format!(
                "live but unlisted at {}{rules}",
                page_url(&self.config, &slug)
            ),
            (false, true) => format!(
                "live and listed at {}{rules}",
                page_url(&self.config, &slug)
            ),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "{slug}: {state}"
        ))]))
    }

    #[tool(
        description = "Set the icon shown next to a page on the site index: an emoji, inline SVG, or data: URI. Pages without one get a generated icon, so this is optional."
    )]
    async fn set_icon(
        &self,
        Parameters(SetIconRequest { slug, icon }): Parameters<SetIconRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let icon = icon.trim();
        if icon.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "icon must not be empty",
            )]));
        }
        if icon.len() > MAX_ICON_BYTES {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "icon must be under {} KB",
                MAX_ICON_BYTES / 1024
            ))]));
        }
        if page_path(&self.config, &slug).await.is_none() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no page found for slug '{slug}'; publish the page first"
            ))]));
        }

        // Sits beside the page file, matching however that page was resolved.
        let path = match self.config.data_dir.join(format!("{slug}.html")) {
            p if p.exists() => self.config.data_dir.join(format!("{slug}.icon")),
            _ => self.config.data_dir.join(format!("{slug}/index.icon")),
        };
        fs::write(&path, icon)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "icon set for {}",
            page_url(&self.config, &slug)
        ))]))
    }

    #[tool(
        description = "Fetch the current HTML for every page in an app namespace, keyed by page name, so the app can be edited and pushed back with push_app."
    )]
    async fn pull_app(
        &self,
        Parameters(PullAppRequest { app }): Parameters<PullAppRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_segment(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty and contain only letters, numbers, '-' or '_'",
            )]));
        }
        let app_dir = self.config.data_dir.join(&app);
        let mut pages = HashMap::new();
        if let Ok(mut entries) = fs::read_dir(&app_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("html") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(html) = fs::read_to_string(&path).await {
                            pages.insert(stem.to_string(), html);
                        }
                    }
                }
            }
        }
        if pages.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no pages found for app '{app}'"
            ))]));
        }
        let json = serde_json::to_string(&pages)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for PageHost {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_server_info(Implementation::new("toolsite", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Publishes self-contained HTML pages at public URLs.\n\
                 \n\
                 How to publish, in order of preference:\n\
                 1. If you can run shell commands: write the HTML to a file, call \
                 create_upload, then `curl -fT <file> <upload-url>`. Never read the file back \
                 into the conversation — that is the whole point. Emitting a page's HTML as \
                 tool-call arguments when you could have written a file is wasteful, so treat \
                 this as the default path.\n\
                 2. If curl fails because the sandbox has no network access to this host, fall \
                 back to push_page / push_app, which take the HTML inline.\n\
                 3. If there is no shell at all, use push_page / push_app directly.\n\
                 \n\
                 You are not limited to hand-written HTML. If you can run a shell with network \
                 access, scaffolding a real TypeScript/React/Vite project, building it, and \
                 uploading the dist folder is fully supported and often the better answer for \
                 anything interactive: `tar -czf - -C dist . | curl -f -T - \
                 '<upload-url>?bundle'`, adding &spa if it uses a client-side router. Every \
                 app is served from a subpath (/p/<slug>/), never the domain root, so set the \
                 build's base path accordingly — create_upload prints the exact value. \
                 Skipping that step produces a page that loads but renders blank, because its \
                 assets 404.\n\
                 \n\
                 Call list_pages to see what already exists before picking a slug or editing \
                 something, and app_notes to read what a previous session left about an app \
                 before changing it — a bundle's source cannot be recovered from the page it \
                 serves, so those notes may be the only record. Leave your own when you \
                 finish. To edit, fetch the page with `curl <page-url>` into a file (or \
                 pull_page / pull_app when you have no shell), edit it, then re-upload to the \
                 same slug. set_visibility retracts a page without deleting it — nothing here \
                 destroys data. Icons are optional: set_icon takes an emoji or SVG, an image \
                 file goes to `<upload-url>?icon`, and anything without one gets a generated \
                 badge.",
            )
    }
}
