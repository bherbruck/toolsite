use crate::{
    accounts::users::{self, Scope},
    config::Config,
    content::{
        slug::{random_slug, random_token, valid_segment, valid_slug},
        store::{collect_slugs, page_path, page_title, page_url, read_meta, relative_time, write_meta},
    },
    platform::{
        bearer::Caller,
        knowledge::{self, FetchOutput, SearchOutput},
        inline_upload,
        upload::{self, upload_url, SourceMeta, UploadKind, UploadTicket, MAX_ICON_BYTES, UPLOAD_TTL},
    },
    runtime::db,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    service::RequestContext,
    RoleServer,
    model::{
        CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities,
        ServerConfig,
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
pub(crate) struct SearchRequest {
    #[schemars(description = "Words to look for in app titles, slugs and notes. Empty lists everything the caller may open.")]
    pub(crate) query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct FetchRequest {
    #[schemars(description = "The id a search result carried: a page's slug, or 'guide'.")]
    pub(crate) id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct PushPageRequest {
    #[schemars(
        description = "For a new app: the project folder it goes in, e.g. 'ops/yard'. Needed when the account behind this connection holds editor in more than one folder; left out, the one folder it holds is used. Ignored for an app that exists: move one with projects(action: 'move')."
    )]
    pub(crate) project: Option<String>,

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
        description = "For a new app: the project folder it goes in, e.g. 'ops/yard'. Needed when the account behind this connection holds editor in more than one folder; left out, the one folder it holds is used. Ignored for an app that exists: move one with projects(action: 'move')."
    )]
    pub(crate) project: Option<String>,

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
        description = "For a new app: the project folder it goes in, e.g. 'ops/yard'. Needed when the account behind this connection holds editor in more than one folder; left out, the one folder it holds is used. Ignored for an app that exists: move one with projects(action: 'move')."
    )]
    pub(crate) project: Option<String>,

    #[schemars(
        description = "Slug the upload writes to. Random one is generated if omitted. For a multi-page app pass the app name, then upload one file per page."
    )]
    pub(crate) slug: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct UploadBeginRequest {
    #[schemars(description = "Slug the upload writes to: the app, or 'myapp/about' for one page of it.")]
    pub(crate) slug: String,
    #[schemars(
        description = "What is coming: 'page' (one HTML page), 'bundle' (gzipped tar of a built site), 'handler' (a wasm32-wasip2 component), 'migrations' (gzipped tar of numbered .sql files), 'manifest' (toolsite.toml), 'source' (gzipped tar of the project), 'icon', or 'blob' (one file for the app, needs key). The same kinds the upload URL takes, with the same rules."
    )]
    pub(crate) kind: String,
    #[schemars(description = "For a new app: the project folder it goes in, as for create_upload.")]
    pub(crate) project: Option<String>,
    #[schemars(description = "bundle only: true when the app has a client-side router, so unknown paths serve its index.html.")]
    pub(crate) spa: Option<bool>,
    #[schemars(description = "blob only: the key to store the file under, like 'photos/cover.jpg'.")]
    pub(crate) key: Option<String>,
    #[schemars(description = "page or icon only: the page's name under the app, like 'about'. Left out, the slug itself is the page.")]
    pub(crate) page: Option<String>,
    #[schemars(description = "source only: the commit message when the app is linked to a repository.")]
    pub(crate) message: Option<String>,
    #[schemars(description = "source only: the commit the project was built from, 7 to 40 hex characters.")]
    pub(crate) commit: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct UploadChunkRequest {
    #[schemars(description = "The id upload_begin returned.")]
    pub(crate) id: String,
    #[schemars(description = "Position of this chunk, from 0. Chunks may arrive in any order; a repeat of an index replaces it.")]
    pub(crate) index: u32,
    #[schemars(description = "The chunk, standard base64, at most 786432 bytes decoded.")]
    pub(crate) data: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct UploadFinishRequest {
    #[schemars(description = "The id upload_begin returned.")]
    pub(crate) id: String,
    #[schemars(description = "How many chunks there are in total. Every index from 0 to chunks-1 must have arrived.")]
    pub(crate) chunks: u32,
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
pub(crate) struct ProjectsRequest {
    #[schemars(
        description = "'list' the projects you can see with what you hold at each; 'create' a project (path = parent, name); 'move' an app (app, path = target, empty for the top level); 'rename' a project (path, name); 'move_project' (path, parent, empty for the top level); 'remove' an empty project (path); 'permissions' of a project (path); 'grant' a scope (path, email, scope); 'revoke' what an account holds at a project (path, email)."
    )]
    pub(crate) action: String,
    #[schemars(description = "A project path like 'ops/yard'. Empty or '/' means the top level. For create it is the parent; for move it is the target.")]
    pub(crate) path: Option<String>,
    #[schemars(description = "create and rename: the project's name, letters, numbers, '-' or '_'.")]
    pub(crate) name: Option<String>,
    #[schemars(description = "move_project only: the project to move it into. Empty or '/' means the top level.")]
    pub(crate) parent: Option<String>,
    #[schemars(description = "move only: the app to move.")]
    pub(crate) app: Option<String>,
    #[schemars(description = "grant and revoke: the account's email.")]
    pub(crate) email: Option<String>,
    #[schemars(description = "grant only: 'viewer' opens apps, 'editor' also publishes and changes them, 'admin' also sets access. Never more than you hold there.")]
    pub(crate) scope: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct RepoRequest {
    #[schemars(description = "The app. For import, the slug the repository will be served at; it need not exist yet.")]
    pub(crate) app: String,
    #[schemars(
        description = "'status' says where the app's source is mirrored, the last push, which commit the live app came from, whether the repository is ahead of it, and the newest commits. 'discover' lists repositories tagged toolsite that no app is linked to yet, each with the import call to make (the app argument is ignored; pass any name). 'create' makes a new repository from the app's stored source (publish it with ?source first). 'import' links a repository you already have and pulls its branch into the app's source archive. 'pull' pulls the branch again. 'disconnect' forgets the link; the repository stays. 'installations' lists the accounts the GitHub App is installed on."
    )]
    pub(crate) action: String,
    #[schemars(description = "For create: the repository name, default the app's slug. For import: owner/name of the existing repository.")]
    pub(crate) repo: Option<String>,
    #[schemars(description = "For import: the branch to deploy from, default the repository's default branch.")]
    pub(crate) branch: Option<String>,
    #[schemars(description = "For import: the folder inside the repository that holds the project, if not the root.")]
    pub(crate) directory: Option<String>,
    #[schemars(description = "Installation id from 'installations'. Optional when the App is installed on exactly one account.")]
    pub(crate) installation: Option<u64>,
    #[schemars(description = "For create: make the repository public. Private unless said otherwise.")]
    pub(crate) public: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DeployTokenRequest {
    #[schemars(description = "App the token may publish.")]
    pub(crate) app: String,
    #[schemars(description = "'create' mints a token and returns it once; 'list' shows the tokens that exist; 'revoke' ends the one named by id.")]
    pub(crate) action: String,
    #[schemars(description = "For create: what will hold the token, e.g. 'ci'. Required.")]
    pub(crate) label: Option<String>,
    #[schemars(description = "For revoke: the token's id, as list shows it.")]
    pub(crate) id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ExportRequest {
    #[schemars(description = "App whose database the token reads.")]
    pub(crate) app: String,
    #[schemars(
        description = "'create' mints a token and returns it once; 'list' shows the tokens that exist (never their values); 'revoke' ends the one named by id."
    )]
    pub(crate) action: String,
    #[schemars(description = "For create: what will hold the token, e.g. 'reporting'. Required.")]
    pub(crate) label: Option<String>,
    #[schemars(description = "For revoke: the token's id, as list shows it.")]
    pub(crate) id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ScreenshotRequest {
    #[schemars(description = "App (or page) to render.")]
    pub(crate) slug: String,
    #[schemars(description = "Path within the app, e.g. '/reports/2026'. Defaults to '/'.")]
    pub(crate) path: Option<String>,
    #[schemars(
        description = "Render signed in as this account's email, so a gated page shows its data. Site admins only. Omit to render as nobody."
    )]
    pub(crate) as_user: Option<String>,
    #[schemars(description = "Viewport width in pixels, 320 to 1600. Defaults to 1280. The image comes back at most 1280 wide.")]
    pub(crate) width: Option<u32>,
    #[schemars(description = "true renders a tall viewport (up to 4000 px) so a long page is captured whole.")]
    pub(crate) full_page: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct RemoveRequest {
    #[schemars(description = "Slug to take down.")]
    pub(crate) slug: String,
    #[schemars(
        description = "Repeat the slug here to confirm. Removal takes an app's pages, database, settings and schedule with it, so it is not something to do by accident."
    )]
    pub(crate) confirm: String,
    #[schemars(
        description = "Remove only a single page of that name, leaving an app with the same slug alone. Use this to clear a page that is shadowing an app."
    )]
    pub(crate) page_only: Option<bool>,
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
        description = "Who may reach the app: 'public' (anyone), 'authenticated' (any signed-in account), 'restricted' (only people given access, with set_access or projects; 'granted' is the old name and still works), or 'default' to follow the site's TOOLSITE_DEFAULT_ACCESS, which is what an app does until told otherwise."
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
pub(crate) struct MigrationsRequest {
    #[schemars(description = "App whose schema this is.")]
    pub(crate) app: String,
    #[schemars(
        description = "Every migration the app has, as a map of file name to SQL, e.g. {\"001_initial.sql\": \"create table ...\"}. Send the whole set each time — names order them, and each runs once. Omit to see what is stored and which version the database is at."
    )]
    pub(crate) files: Option<std::collections::BTreeMap<String, String>>,
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
    #[schemars(
        description = "An account email. When set, the statement runs as that person inside the app's declared access, exactly as /me/mcp would run it: only the declared views, current_user() and current_role() bound from the account and its grant, writes only through a policy with write = true. Use it to prove a policy holds before saying so: run the same query as two accounts."
    )]
    pub(crate) as_user: Option<String>,
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

    /// Who is calling: the account behind an OAuth token, or nobody in
    /// particular for a static token or the stdio transport, which have
    /// every power. The bearer middleware put it on the HTTP request.
    fn caller(ctx: &RequestContext<RoleServer>) -> Caller {
        ctx.extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Caller>().cloned())
            .unwrap_or(Caller { user: None })
    }

    /// Refuses the call unless the caller holds `needed` at the app that
    /// `slug` belongs to. The refusal names the place and the scope it would
    /// take, so an agent can ask for the right thing.
    async fn allowed(
        &self,
        ctx: &RequestContext<RoleServer>,
        slug: &str,
        needed: Scope,
    ) -> Result<Caller, CallToolResult> {
        let caller = Self::caller(ctx);
        let Some(user) = caller.user.clone() else {
            return Ok(caller);
        };
        let app = slug.split('/').next().unwrap_or(slug).to_string();
        let path = crate::content::store::logical_path(&self.config, &app).await;
        let held = self.held_on(&user, &app).await;
        if held.is_some_and(|held| held >= needed) {
            Ok(caller)
        } else {
            tracing::warn!(email = %user.email, path = %path, needed = %needed, "tool refused: scope");
            Err(CallToolResult::error(vec![ContentBlock::text(match held {
                Some(held) => format!(
                    "{} holds {held} at {path}; this needs {needed}. Ask an admin of that project for it.",
                    user.email
                ),
                None => format!(
                    "{} holds no access at {path}; this needs {needed}. Ask an admin of that project for it.",
                    user.email
                ),
            })]))
        }
    }

    /// What `user` may do to `app`, wherever it sits.
    async fn held_on(&self, user: &users::User, app: &str) -> Option<Scope> {
        let folder = crate::content::store::app_folder(&self.config, app).await;
        let (config, user, app) = (self.config.clone(), user.clone(), app.to_string());
        tokio::task::spawn_blocking(move || {
            let locks = crate::content::store::locked_prefixes_blocking(&config);
            users::app_scope(&config, &user, &folder, &app, &locks)
        })
            .await
            .ok()
            .flatten()
    }

    /// For publishing: an app that exists is judged where it sits; a new one
    /// by the folder it is about to land in, named by `project` or implied
    /// by the one folder the account is editor of. Returns the caller and
    /// the folder to record on a first publish.
    async fn allowed_to_publish(
        &self,
        ctx: &RequestContext<RoleServer>,
        slug: &str,
        project: Option<&str>,
    ) -> Result<(Caller, Option<String>), CallToolResult> {
        let caller = Self::caller(ctx);
        let app = slug.split('/').next().unwrap_or(slug).to_string();
        let project = project.map(|p| p.trim_matches('/').to_string()).filter(|p| !p.is_empty());
        if crate::content::store::app_exists(&self.config, &app).await {
            return self.allowed(ctx, slug, Scope::Editor).await.map(|caller| (caller, None));
        }
        let Some(user) = caller.user.clone() else {
            if let Some(folder) = &project
                && !crate::content::store::folder_exists(&self.config, folder).await
            {
                return Err(CallToolResult::error(vec![ContentBlock::text(format!(
                    "there is no folder '{folder}'; an admin creates folders on /admin/apps"
                ))]));
            }
            return Ok((caller, project));
        };
        let folder = match project {
            Some(folder) => folder,
            None => {
                let (config, who) = (self.config.clone(), user.clone());
                match tokio::task::spawn_blocking(move || users::default_folder_for(&config, &who)).await.ok().flatten() {
                    Some(folder) => folder,
                    None => {
                        return Err(CallToolResult::error(vec![ContentBlock::text(format!(
                            "{} holds editor in more than one folder; pass project: \"<folder>\" to say where {app} goes.",
                            user.email
                        ))]));
                    }
                }
            }
        };
        if !crate::content::store::folder_exists(&self.config, &folder).await {
            return Err(CallToolResult::error(vec![ContentBlock::text(format!(
                "there is no folder '{folder}'; an admin creates folders on /admin/apps"
            ))]));
        }
        let (config, who, at) = (self.config.clone(), user.clone(), folder.clone());
        let held = tokio::task::spawn_blocking(move || {
            let locks = crate::content::store::locked_prefixes_blocking(&config);
            users::effective_scope(&config, &who, &at, &locks)
        })
            .await
            .ok()
            .flatten();
        if held.is_some_and(|held| held >= Scope::Editor) {
            Ok((caller, Some(folder)))
        } else {
            let place = if folder.is_empty() { "the root".to_string() } else { folder.clone() };
            tracing::warn!(email = %user.email, folder = %place, "publish refused: scope");
            Err(CallToolResult::error(vec![ContentBlock::text(match held {
                Some(held) => format!("{} holds {held} at {place}; publishing a new app there needs editor.", user.email),
                None => format!("{} holds no access at {place}; publishing a new app there needs editor.", user.email),
            })]))
        }
    }

    /// For the few tools that are about the site rather than an app.
    fn root_only(ctx: &RequestContext<RoleServer>) -> Result<Caller, CallToolResult> {
        let caller = Self::caller(ctx);
        match &caller.user {
            Some(user) if !user.is_admin => Err(CallToolResult::error(vec![ContentBlock::text(
                format!("{} is not a site admin; only a site admin may do that.", user.email),
            )])),
            _ => Ok(caller),
        }
    }

    /// Remembers who first published an app and where it landed.
    async fn stamp_new_app(&self, app: &str, caller: &Caller, project: Option<&str>) {
        let user_id = caller.user.as_ref().map(|user| user.id.as_str());
        crate::platform::upload::stamp_new_app(&self.config, app, user_id, project).await;
    }

    /// Whether the caller may open `slug`: a hidden page is nobody's; a
    /// static token sees the rest; an account sees public and signed-in
    /// apps and the ones it holds a scope on. The same rule as list_pages.
    async fn may_see(&self, caller: &Caller, slug: &str) -> bool {
        let meta = read_meta(&self.config, slug).await;
        if meta.hidden {
            return false;
        }
        let Some(user) = &caller.user else {
            return true;
        };
        let app = slug.split('/').next().unwrap_or(slug).to_string();
        let gate = meta.gate_for("/", &self.config.default_gate).to_string();
        matches!(gate.as_str(), "public" | "authenticated") || self.held_on(user, &app).await.is_some()
    }

    /// Every slug the caller may open, for search.
    async fn visible_slugs(&self, caller: &Caller) -> Vec<String> {
        let mut slugs = Vec::new();
        collect_slugs(&self.config.data_dir, String::new(), &mut slugs).await;
        let mut out = Vec::new();
        for slug in slugs {
            if self.may_see(caller, &slug).await {
                out.push(slug);
            }
        }
        out
    }

    #[tool(
        description = "Find apps and pages on this site by words in their title, slug or notes, plus the platform guide when the question is about toolsite itself. Returns ids for fetch. Only what the caller may open is listed.",
        annotations(title = "Search", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_output::<SearchOutput>()
    )]
    async fn search(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(SearchRequest { query }): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let caller = Self::caller(&ctx);
        let visible = self.visible_slugs(&caller).await;
        let output = knowledge::search(&self.config, &query, &visible).await;
        let value = serde_json::to_value(output).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        description = "Read one app or page as text, by the id search returned: the page's visible words, then the notes kept with the app, with metadata about access and what it declares. 'guide' returns the platform guide.",
        annotations(title = "Fetch", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false),
        output_schema = rmcp::handler::server::tool::schema_for_output::<FetchOutput>()
    )]
    async fn fetch(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(FetchRequest { id }): Parameters<FetchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let caller = Self::caller(&ctx);
        let refused = || CallToolResult::error(vec![ContentBlock::text(format!("nothing to fetch at {id}"))]);
        if id == knowledge::GUIDE_ID {
            let value = serde_json::to_value(knowledge::fetch_guide(&self.config))
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::structured(value));
        }
        if !valid_slug(&id) || !self.may_see(&caller, &id).await {
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
        description = "Keep an app's source in a GitHub repository, with history. The repository is a mirror: publishing the source of a linked app pushes a commit (say why with ?source&message=...), and a push to the repository is pulled into the app's source archive. Nothing is built or run in GitHub; you build and publish from wherever you run, as always. 'create' needs the app's source to have been published with ?source, names the repository toolsite-<app> unless repo says otherwise, and tags it with the toolsite topic. Needs the site to be configured with a GitHub App (TOOLSITE_GITHUB_*); 'installations' tells you whether it is and on which accounts.",
        annotations(title = "Repository", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    pub(crate) async fn app_repo(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(RepoRequest { app, action, repo, branch, directory, installation, public }): Parameters<RepoRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Admin).await {
            return Ok(refused);
        }
        use crate::platform::github;
        if !github::valid_app(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be one path segment of letters, numbers, '-' or '_'",
            )]));
        }
        let config = &self.config;
        let pick_installation = |given: Option<u64>| -> Result<u64, String> {
            if let Some(id) = given {
                return Ok(id);
            }
            let installs = github::installations(config);
            match installs.as_slice() {
                [one] => Ok(one.id),
                [] => Err("the GitHub App is not installed anywhere yet; install it from /admin/github".into()),
                many => Err(format!(
                    "pass installation: one of {}",
                    many.iter().map(|i| format!("{} ({})", i.id, i.account)).collect::<Vec<_>>().join(", ")
                )),
            }
        };
        let outcome: Result<String, String> = match action.as_str() {
            "status" => Ok(github::status_text(config, &app).await),
            "installations" => match github::refresh_installations(config).await {
                Ok(list) if list.is_empty() => Ok("the App is installed nowhere yet".to_string()),
                Ok(list) => Ok(list.iter().map(|i| format!("{}  {} ({})", i.id, i.account, i.kind)).collect::<Vec<_>>().join("\n")),
                Err(why) => Err(why),
            },
            "create" => match pick_installation(installation) {
                Ok(inst) => github::create(config, &app, inst, repo.as_deref(), !public.unwrap_or(false))
                    .await
                    .map(|link| format!("created {} on branch {} with the source of {app}. Publishing the source again pushes a commit; a push there is pulled into the source archive. Building and publishing stay with you.", link.url(), link.branch)),
                Err(why) => Err(why),
            },
            "import" => match (pick_installation(installation), repo.as_deref()) {
                (Ok(inst), Some(repo)) => github::import(config, &app, inst, repo, branch.as_deref(), directory.as_deref())
                    .await
                    .map(|link| format!("connected {} ({}) to {app}; its branch is now the source archive. Fetch it with curl '<upload-url>?source' | tar xz, build, and publish; nothing is built here.", link.url(), link.branch)),
                (Err(why), _) => Err(why),
                (_, None) => Err("import needs repo: owner/name".into()),
            },
            "discover" => github::discover(config).await.map(|found| {
                if found.is_empty() {
                    "no repository tagged toolsite is waiting to be imported".to_string()
                } else {
                    found
                        .iter()
                        .map(|d| {
                            format!(
                                "{}  branch {}  {}  -> app_repo(app: \"{}\", action: \"import\", repo: \"{}\", installation: {})",
                                d.full_name,
                                d.default_branch,
                                if d.private { "private" } else { "public" },
                                d.proposed_app,
                                d.full_name,
                                d.installation_id
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }),
            "sync" | "pull" => github::pull(config, &app)
                .await
                .map(|p| format!("pulled commit {} into the source archive of {app} ({} bytes)", &p.sha[..p.sha.len().min(7)], p.bytes)),
            "disconnect" => {
                let (config2, app2) = (config.clone(), app.clone());
                tokio::task::spawn_blocking(move || github::disconnect(&config2, &app2))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                    .map(|link| format!("{app} is no longer mirrored at {}; the repository is untouched", link.full_name()))
            }
            other => Err(format!("action must be status, installations, discover, create, import, pull or disconnect, not '{other}'")),
        };
        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    #[tool(
        description = "A token that may publish one app and nothing else, for a CI system of your own: PUT <site>/deploy/<app> takes the same flags as an upload ticket (?bundle&spa, ?handler, ?migrations, ?manifest, ?source, ?blob=<key>, or a page) with Authorization: Bearer <token>; add &commit=<sha> to say which commit was published. Returned once, stored only as a hash.",
        annotations(title = "Deploy tokens", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    pub(crate) async fn app_deploy_tokens(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(DeployTokenRequest { app, action, label, id }): Parameters<DeployTokenRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Admin).await {
            return Ok(refused);
        }
        use crate::platform::deploy;
        if !deploy::valid_app(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be one path segment of letters, numbers, '-' or '_'",
            )]));
        }
        let config = self.config.clone();
        let url = deploy::deploy_url(&config, &app);
        let outcome = tokio::task::spawn_blocking(move || match action.as_str() {
            "create" => deploy::create(&config, &app, &label.unwrap_or_default()).map(|(entry, token)| {
                format!(
                    "Token {} for {app} ({}). Shown once:\n\n{token}\n\nUse it as\n\n  tar -czf - -C dist . | curl -f -H 'Authorization: Bearer {token}' -T - '{url}?bundle'",
                    entry.id, entry.label
                )
            }),
            "list" => {
                let tokens = deploy::list(&config, &app);
                if tokens.is_empty() {
                    return Ok(format!("no deploy tokens for {app}"));
                }
                Ok(tokens
                    .iter()
                    .map(|t| format!("{}  {}  last used {}", t.id, t.label, match t.last_used {
                        Some(_) => "recently",
                        None => "never",
                    }))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "revoke" => deploy::revoke(&config, &app, &id.unwrap_or_default())
                .map(|()| "revoked; whatever holds it gets 401 from now on".to_string()),
            other => Err(format!("action must be create, list or revoke, not '{other}'")),
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    #[tool(
        description = "Read-only access to one app's database from outside: a token for GET <site>/export/<app>.sqlite, which answers with a consistent snapshot of the whole SQLite file. For reporting tools that pull SQLite over HTTP (a BI or sync tool). Each token opens one app only and is revocable on its own; the publish token is never accepted there. The token is returned once, by this call, and stored only as a hash.",
        annotations(title = "Export tokens", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    pub(crate) async fn app_exports(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(ExportRequest { app, action, label, id }): Parameters<ExportRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Admin).await {
            return Ok(refused);
        }
        if !crate::platform::export::valid_app(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be one path segment of letters, numbers, '-' or '_'",
            )]));
        }
        let config = self.config.clone();
        let url = crate::platform::export::export_url(&config, &app);
        let outcome = tokio::task::spawn_blocking(move || match action.as_str() {
            "create" => crate::platform::export::create(&config, &app, &label.unwrap_or_default()).map(
                |(entry, token)| {
                    format!(
                        "Token {} for {app} ({}). Shown once:\n\n{token}\n\nUse it as\n\n  curl -H 'Authorization: Bearer {token}' -o {app}.sqlite {url}\n\nIn the reporting tool: a sqlite connection with URL {url} and that bearer token.",
                        entry.id, entry.label
                    )
                },
            ),
            "list" => {
                let tokens = crate::platform::export::list(&config, &app);
                if tokens.is_empty() {
                    return Ok(format!("no export tokens for {app}"));
                }
                Ok(tokens
                    .iter()
                    .map(|t| {
                        format!(
                            "{}  {}  created {} days ago, last used {}",
                            t.id,
                            t.label,
                            crate::platform::export::seconds_since(t.created_at) / 86_400,
                            match t.last_used {
                                Some(at) => format!("{} h ago", crate::platform::export::seconds_since(at) / 3600),
                                None => "never".to_string(),
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "revoke" => crate::platform::export::revoke(&config, &app, &id.unwrap_or_default())
                .map(|()| format!("revoked; the tool holding it will get 401 from now on")),
            other => Err(format!("action must be create, list or revoke, not '{other}'")),
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    // One tool, not five, like app_repo and app_exports: a client lists
    // fewer tools, and the actions share the project they act on and the
    // rule that decides who may act there.
    #[tool(
        description = "Run projects: groups of apps, nested like folders, where access is given once and applies to everything inside. 'list' shows the projects you can see and what you hold at each. 'create' needs admin at the parent. 'move' puts an existing app in another project and needs admin where it is and where it goes; the project must exist. 'rename' changes a project's name and needs admin at its parent; 'move_project' moves a project under another and needs admin at the project, where it is and where it goes. Both carry the apps, the access and the lock along, and the old path keeps working as a link. 'remove' takes away an empty project (admin at its parent); a project with anything inside is refused. 'permissions' lists who holds viewer, editor or admin at a project, set there or above (needs admin there). 'grant' and 'revoke' change that (admin there; you cannot give more than you hold). The same rules as the app browser at /browse/<path>.",
        annotations(title = "Projects", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    pub(crate) async fn projects(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(ProjectsRequest { action, path, name, parent, app, email, scope }): Parameters<ProjectsRequest>,
    ) -> Result<CallToolResult, McpError> {
        use crate::platform::projects as p;
        let caller = Self::caller(&ctx);
        let actor = caller.user.as_ref();
        let path = path.unwrap_or_default();
        let config = &self.config;
        let fail = |text: String| Ok(CallToolResult::error(vec![ContentBlock::text(text)]));
        let need = |value: Option<String>, what: &str| -> Result<String, String> {
            value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).ok_or_else(|| format!("{action} needs {what}"))
        };
        let url = |at: &str| {
            let base = config.base_url.as_deref().unwrap_or(&config.local_base);
            format!("{base}{}", crate::content::browse::browser_url(at))
        };
        let outcome: Result<String, p::Problem> = match action.as_str() {
            "list" => {
                let nodes = p::tree(config, actor).await;
                if nodes.is_empty() {
                    Ok("There are no projects you can see. Every app sits at the top level.".to_string())
                } else {
                    serde_json::to_string_pretty(&nodes).map_err(|e| p::Problem::Invalid(e.to_string()))
                }
            }
            "create" => match need(name, "a name") {
                Ok(name) => p::create(config, actor, &path, &name)
                    .await
                    .map(|folder| format!("Project {} is created: {}", folder.path, url(&folder.path))),
                Err(text) => return fail(text),
            },
            "move" => match need(app, "an app") {
                Ok(app) => p::move_app(config, actor, &app, &path)
                    .await
                    .map(|to| format!("{app} is at {to}: {}", url(path.trim_matches('/')))),
                Err(text) => return fail(text),
            },
            "rename" => match need(name, "a name") {
                Ok(name) => p::rename(config, actor, &path, &name)
                    .await
                    .map(|to| format!("The project is now {to}: {}", url(&to))),
                Err(text) => return fail(text),
            },
            "move_project" => p::move_project(config, actor, &path, parent.as_deref().unwrap_or("").trim_matches('/'))
                .await
                .map(|to| format!("The project is now at {to}: {}", url(&to))),
            "remove" => p::remove(config, actor, &path)
                .await
                .map(|()| format!("Project {} is removed.", path.trim_matches('/'))),
            "permissions" => p::holders(config, actor, &path).await.map(|holders| {
                let place = p::place(path.trim_matches('/'));
                let mut out = format!("At {place}:\n");
                if holders.direct.is_empty() && holders.inherited.is_empty() {
                    out.push_str("nobody holds access here. Site admins hold admin everywhere.\n");
                }
                for row in &holders.direct {
                    out.push_str(&format!("{}  {}  set here\n", row.email, row.scope));
                }
                for row in &holders.inherited {
                    out.push_str(&format!("{}  {}  inherited from {}\n", row.email, row.scope, p::place(&row.prefix)));
                }
                out
            }),
            "grant" => {
                let (email, scope) = match (need(email, "an email"), need(scope, "a scope")) {
                    (Ok(e), Ok(s)) => (e, s),
                    (Err(text), _) | (_, Err(text)) => return fail(text),
                };
                let Some(level) = Scope::parse(&scope) else {
                    return fail("scope is viewer, editor or admin".to_string());
                };
                p::grant(config, actor, &path, &email, level)
                    .await
                    .map(|()| format!("{email} is {level} at {}.", p::place(path.trim_matches('/'))))
            }
            "revoke" => match need(email, "an email") {
                Ok(email) => p::revoke(config, actor, &path, &email)
                    .await
                    .map(|()| format!("{email} has no access of its own at {} now.", p::place(path.trim_matches('/')))),
                Err(text) => return fail(text),
            },
            other => return fail(format!("action is list, create, move, rename, move_project, remove, permissions, grant or revoke, not '{other}'")),
        };
        match outcome {
            Ok(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Err(problem) => fail(problem.message().to_string()),
        }
    }

    #[tool(
        description = "Fallback for a sandbox that cannot reach this host: opens an upload that arrives in base64 chunks through upload_chunk and lands with upload_finish. Same kinds as the upload URL (page, bundle, handler, migrations, manifest, source, icon, blob) and the same rules; the result is identical. Use create_upload and curl whenever the host is reachable, since bytes through a tool call cost tokens. Returns the upload id and the chunk size. Expires in 15 minutes.",
        annotations(title = "Begin an inline upload", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn upload_begin(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(UploadBeginRequest { slug, kind, project, spa, key, page, message, commit }): Parameters<UploadBeginRequest>,
    ) -> Result<CallToolResult, McpError> {
        let refuse = |text: String| Ok(CallToolResult::error(vec![ContentBlock::text(text)]));
        if !valid_slug(&slug) {
            return refuse("slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'".into());
        }
        let kind = match kind.as_str() {
            "page" => UploadKind::Page,
            "icon" => UploadKind::Icon,
            "bundle" => UploadKind::Bundle { spa: spa.unwrap_or(false) },
            "handler" => UploadKind::Handler,
            "migrations" => UploadKind::Migrations,
            "manifest" => UploadKind::Manifest,
            "source" => UploadKind::Source,
            "blob" => match key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
                Some(key) => UploadKind::Blob(key.to_string()),
                None => return refuse("kind 'blob' needs key: where the file is stored, like 'photos/cover.jpg'".into()),
            },
            other => {
                return refuse(format!(
                    "kind must be page, bundle, handler, migrations, manifest, source, icon or blob, not '{other}'"
                ))
            }
        };
        // A page name under the app, exactly as /upload/<ticket>/<page>.
        let target = match page.as_deref().map(|p| p.trim_matches('/').trim_end_matches(".html")).filter(|p| !p.is_empty()) {
            Some(page) if matches!(kind, UploadKind::Page | UploadKind::Icon) => format!("{slug}/{page}"),
            Some(_) => return refuse("page is only for kind 'page' or 'icon'".into()),
            None => slug.clone(),
        };
        if !valid_slug(&target) {
            return refuse("page name must be path segments of letters, numbers, '-' or '_'".into());
        }
        let (caller, folder) = match self.allowed_to_publish(&ctx, &slug, project.as_deref()).await {
            Ok(allowed) => allowed,
            Err(refused) => return Ok(refused),
        };
        let meta = SourceMeta {
            push: true,
            message: message.map(|m| m.trim().to_string()).filter(|m| !m.is_empty()),
            commit: commit.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()),
        };
        let user = caller.user.as_ref().map(|user| user.id.clone());
        let config = self.config.clone();
        let outcome = tokio::task::spawn_blocking(move || inline_upload::begin(&config, target, kind, meta, user, folder))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(match outcome {
            Ok(id) => CallToolResult::success(vec![ContentBlock::text(format!(
                "Upload {id} is open for 15 minutes. Send the file in order with upload_chunk(id, index, data): \
                 index from 0, data standard base64 of at most {} bytes decoded per chunk. Then \
                 upload_finish(id, chunks) with the total count. The reply is what the upload URL would have said.",
                inline_upload::CHUNK_BYTES
            ))]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    #[tool(
        description = "One chunk of an inline upload: standard base64, at most 786432 bytes decoded, with its index from 0. Any order; a repeat of an index replaces it. Returns the bytes received so far and the indexes present.",
        annotations(title = "Send an upload chunk", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn upload_chunk(
        &self,
        Parameters(UploadChunkRequest { id, index, data }): Parameters<UploadChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.config.clone();
        let outcome = tokio::task::spawn_blocking(move || inline_upload::chunk(&config, &id, index, &data))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(match outcome {
            Ok(progress) => CallToolResult::success(vec![ContentBlock::text(format!(
                "{} bytes received; chunks present: {}",
                progress.received,
                progress.present.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
            ))]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }

    #[tool(
        description = "Closes an inline upload: joins the chunks in order and stores the result exactly as the upload URL would, with the same validation and the same reply. Refuses when a chunk is missing, naming it. The id is spent either way.",
        annotations(title = "Finish an inline upload", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn upload_finish(
        &self,
        Parameters(UploadFinishRequest { id, chunks }): Parameters<UploadFinishRequest>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.config.clone();
        let joined = tokio::task::spawn_blocking(move || inline_upload::finish(&config, &id, chunks))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let (upload, bytes) = match joined {
            Ok(joined) => joined,
            Err(message) => return Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        };
        let response = upload::store_for_publisher(
            &self.config,
            &self.runtime,
            upload.slug,
            upload.kind,
            bytes::Bytes::from(bytes),
            upload.meta,
            upload.user,
            upload.project,
        )
        .await;
        let ok = response.status().is_success();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).trim_end().to_string())
            .unwrap_or_default();
        Ok(if ok {
            CallToolResult::success(vec![ContentBlock::text(body)])
        } else {
            CallToolResult::error(vec![ContentBlock::text(format!("{status}: {body}"))])
        })
    }

    #[tool(
        description = "Fallback publisher for when you cannot reach this host from a shell: pastes one page's HTML through this call. If you can run shell commands with network access, use create_upload instead. For anything that is not plain HTML (a built bundle, a handler, migrations, a manifest, the source, a blob) use upload_begin / upload_chunk / upload_finish. Call again with the same slug to update in place. Use a slug like 'myapp/about' to group pages under one app.",
        annotations(title = "Publish a page", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn push_page(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(PushPageRequest { html, slug, project }): Parameters<PushPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = slug.unwrap_or_else(random_slug);
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let (caller, folder) = match self.allowed_to_publish(&ctx, &slug, project.as_deref()).await {
            Ok(allowed) => allowed,
            Err(refused) => return Ok(refused),
        };

        let path = self.config.data_dir.join(format!("{slug}.html"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        fs::write(&path, html)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        self.stamp_new_app(&app, &caller, folder.as_deref()).await;

        let url = page_url(&self.config, &slug);
        Ok(CallToolResult::success(vec![ContentBlock::text(url)]))
    }

    #[tool(
        description = "Look at what you built: renders a page in a real browser on the server and returns the picture, so you can say what you see, with data, before you say it works. Pass as_user (site admins only) to see a gated page as that person. Needs a browser on the server; the error says so when there is none.",
        annotations(title = "Screenshot a page", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn screenshot(
        &self,
        Parameters(ScreenshotRequest { slug, path, as_user, width, full_page }): Parameters<ScreenshotRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        if let Err(refused) = self.allowed(&ctx, &slug, Scope::Editor).await {
            return Ok(refused);
        }
        let options = match crate::platform::screenshot::Options::new(width, full_page.unwrap_or(false)) {
            Ok(options) => options,
            Err(why) => return Ok(CallToolResult::error(vec![ContentBlock::text(why)])),
        };
        // The app is the first segment; a deeper slug is a page within it,
        // which becomes the path unless one was given.
        let (app, rest) = match slug.split_once('/') {
            Some((app, rest)) => (app.to_string(), format!("/{rest}")),
            None => (slug.clone(), "/".to_string()),
        };
        let path = path.unwrap_or(rest);
        if !crate::platform::preview::valid_path(&path) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "path must start with '/' and stay within the app",
            )]));
        }

        // Seeing a page as someone else is impersonation; only a site admin
        // (or the static token) may, and only of an active account.
        let mut as_label = "nobody".to_string();
        let mut user_id = None;
        if let Some(email) = as_user.map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()) {
            if let Err(refused) = Self::root_only(&ctx) {
                return Ok(refused);
            }
            let (config, lookup) = (self.config.clone(), email.clone());
            match tokio::task::spawn_blocking(move || users::account_at_email(&config, &lookup)).await {
                Ok(Ok(users::AtEmail::Active(user))) => {
                    as_label = user.email.clone();
                    user_id = Some(user.id);
                }
                Ok(Ok(users::AtEmail::Disabled)) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!("{email} is disabled."))]));
                }
                Ok(Ok(users::AtEmail::Nobody)) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!("There is no account {email}."))]));
                }
                _ => return Err(McpError::internal_error("account lookup failed", None)),
            }
        }

        match crate::platform::screenshot::render(&self.config, &app, &path, user_id.as_deref(), options).await {
            Ok(shot) => {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(&shot.bytes);
                tracing::info!(app = %app, path = %path, as_user = %as_label, bytes = shot.bytes.len(), "screenshot rendered");
                Ok(CallToolResult::success(vec![
                    ContentBlock::image(data, shot.media_type),
                    ContentBlock::text(format!(
                        "{}x{} of /p/{app}{path} as {as_label}",
                        shot.width, shot.height
                    )),
                ]))
            }
            Err(why) => {
                tracing::warn!(app = %app, path = %path, %why, "screenshot failed");
                Ok(CallToolResult::error(vec![ContentBlock::text(why)]))
            }
        }
    }

    #[tool(description = "Fetch the current HTML source of a previously pushed page by its slug, so it can be edited and pushed back.",
        annotations(title = "Read a page", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn pull_page(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(PullPageRequest { slug }): Parameters<PullPageRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &slug, Scope::Editor).await {
            return Ok(refused);
        }
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
        description = "Fallback publisher for a whole multi-page app when you cannot reach this host from a shell: pastes every page's HTML through this call. If you can run shell commands with network access, use create_upload instead; for a built bundle, a handler or anything that is not plain HTML, use upload_begin / upload_chunk / upload_finish. A page named 'index' is also served at the app's own root URL. Returns each page's URL.",
        annotations(title = "Publish an app", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn push_app(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(PushAppRequest { app, pages, project }): Parameters<PushAppRequest>,
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
        let (caller, folder) = match self.allowed_to_publish(&ctx, &app, project.as_deref()).await {
            Ok(allowed) => allowed,
            Err(refused) => return Ok(refused),
        };
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
        self.stamp_new_app(&app, &caller, folder.as_deref()).await;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            urls.join("\n"),
        )]))
    }

    #[tool(
        description = "The default way to publish. Returns a short-lived upload URL; write the HTML to a local file, then PUT the file to that URL with curl. Do not paste HTML into this call — the point is that the page never passes through the conversation. Works for a single page, a multi-page app, or a whole built front-end (TypeScript/React/Vite — tar the dist folder to ?bundle). The response includes the base path the build must be configured with. If the upload URL turns out to be unreachable from your sandbox, fall back to push_page/push_app.",
        annotations(title = "Create an upload URL", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn create_upload(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(CreateUploadRequest { slug, project }): Parameters<CreateUploadRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = slug.unwrap_or_else(random_slug);
        if !valid_slug(&slug) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "slug must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let (caller, folder) = match self.allowed_to_publish(&ctx, &slug, project.as_deref()).await {
            Ok(allowed) => allowed,
            Err(refused) => return Ok(refused),
        };

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
                    user: caller.user.as_ref().map(|user| user.id.clone()),
                    project: folder,
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
             \nThe app's tables come from numbered .sql files, applied once each, in \
             order, before the handler goes live. The scaffold ships the first one; add \
             002_*.sql for the next change rather than editing it, and never write \
             'create table if not exists' in a handler — once the table exists it does \
             nothing, so a later column never arrives:\n\
             \n  tar -czf - -C migrations . | curl -f -T - '{upload}?migrations'\n\
             \nThe app's settings — its gate, route rules, jobs, icon and allow_http — come \
             from toolsite.toml:\n\
             \n  curl -f -T toolsite.toml '{upload}?manifest'\n\
             \nFiles the app keeps — images, datasets, anything too big or too opaque for \
             a row — are blobs, one namespace per app. Seed one from here (64 MB per PUT; \
             the type comes from the key's extension):\n\
             \n  curl -f -T photo.jpg '{upload}?blob=photos/cover.jpg'\n\
             \nA handler reads and lists them through the blobs import, takes bigger ones \
             from a browser with blobs.upload-url, and sends one by answering with the \
             header x-toolsite-blob: <key> — see {site}/guide.\n\
             \nEvery flag this URL takes: ?bundle, ?spa, ?handler, ?migrations, ?manifest, \
             ?icon, ?source, ?blob=<key>. No flag at all publishes the body as a page. \
             Anything else is refused rather than guessed at.\n\
             \nIf curl cannot reach this host from your sandbox, use upload_begin / \
             upload_chunk / upload_finish with base64 chunks: same kinds, same rules, \
             same reply.\n\
             \nKeep the project with the app, since a bundle cannot be turned back into the \
             sources that built it. Visitors only ever see what the bundle contained:\n\
             \n  tar -czf - --exclude node_modules --exclude target . | curl -f -T - '{upload}?source'\n\
             \nIf the app is linked to a GitHub repository, that upload is also pushed as a commit; \
             say why with '{upload}?source&message=<url-encoded text>', and name the commit the \
             build came from with &commit=<sha> so the Repo tab can tell whether the live app \
             is the repository's head.\n\
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
        description = "Run SQL against one app's own SQLite database — create tables, seed or inspect data. Only reachable over MCP, never from a published page, so it is safe for schema work but is not how an app reads its own data at runtime.",
        annotations(title = "Run SQL", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    pub(crate) async fn run_sql(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(RunSqlRequest { app, sql, params, as_user }): Parameters<RunSqlRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Editor).await {
            return Ok(refused);
        }
        let config = self.config.clone();
        let params = params.unwrap_or_default();
        let as_user = as_user.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        let outcome = tokio::task::spawn_blocking(move || match as_user {
            None => db::run(&config, &app, &sql, &params),
            Some(email) => {
                let user = match crate::accounts::users::account_at_email(&config, &email)? {
                    crate::accounts::users::AtEmail::Active(user) => user,
                    crate::accounts::users::AtEmail::Disabled => {
                        return Err(format!("{email} is disabled; enable the account to run as it"))
                    }
                    crate::accounts::users::AtEmail::Nobody => {
                        return Err(format!("there is no account for {email}"))
                    }
                };
                let identity = db::Identity {
                    role: crate::accounts::users::role_for(&config, &user.id, &app),
                    user_id: user.id,
                    email: user.email.clone(),
                };
                let meta = crate::content::store::read_meta_blocking(&config, &app);
                let scope = db::Scope::of(&meta);
                tracing::info!(app = %app, as_user = %user.email, "admin ran scoped SQL as an account");
                db::run_scoped(&config, &app, Some(&identity), &scope, &sql, &params)
            }
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
                    serde_json::to_string(&body)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Create an account someone can sign in with. Accounts are global to the site; use set_access to say which apps they may reach. There is no public signup, so this is the only way an account comes into being.",
        annotations(title = "Create an account", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    pub(crate) async fn create_user(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(CreateUserRequest {
            email,
            password,
            admin,
        }): Parameters<CreateUserRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = Self::root_only(&ctx) {
            return Ok(refused);
        }
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
        description = "An app's own database schema, as numbered migrations. Each runs once, in order, in a transaction, so adding a column later reaches databases that already exist — which 'create table if not exists' in a handler cannot do. The platform never reads what they create; tables and their meaning are the app's business.",
        annotations(title = "Schema migrations", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn app_migrations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(MigrationsRequest { app, files }): Parameters<MigrationsRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Editor).await {
            return Ok(refused);
        }
        if !valid_slug(&app) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "app must be non-empty path segments (letters, numbers, '-' or '_') separated by '/'",
            )]));
        }
        let config = self.config.clone_for_task();

        let outcome = tokio::task::spawn_blocking(move || match files {
            Some(files) => {
                let files: Vec<(String, String)> = files.into_iter().collect();
                let count = files.len();
                crate::runtime::migrate::store(&config, &app, files)?;
                let (version, ran, notes) = crate::runtime::migrate::apply(&config, &app)?;
                let mut text = format!(
                    "{count} migration(s) stored, {ran} applied, now at version {version}"
                );
                for note in notes {
                    text.push('\n');
                    text.push_str(&note);
                }
                Ok::<_, String>(text)
            }
            None => {
                let stored = crate::runtime::migrate::stored(&config, &app);
                Ok(if stored.is_empty() {
                    format!("{app} has no migrations")
                } else {
                    format!(
                        "{}\n(database at version {})",
                        stored
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        crate::runtime::migrate::apply(&config, &app)
                            .map(|(version, ..)| version)
                            .unwrap_or(0)
                    )
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
        description = "Schedule an app's handler to run on its own — refreshing a cache, pulling from an API, tidying a table. A job is a cron expression and a path, and firing it calls the same handler a request would, with the same sandbox and limits. Give run_now to trigger one immediately, or no name to list them with when each last ran.",
        annotations(title = "Scheduled jobs", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn app_jobs(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(ScheduleRequest {
            app,
            name,
            schedule,
            path,
            run_now,
        }): Parameters<ScheduleRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Editor).await {
            return Ok(refused);
        }
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
        description = "An app's settings — API keys and the like, which its handler reads through the secrets import. Pass link: true to get a URL the owner opens to paste values in, which is the right way: a secret you never see cannot leak through you. Values never come back out: this lists names only, they are absent from the source archive, and no URL serves them. Give a name and value to set, a name alone to remove, neither to list.",
        annotations(title = "App settings", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn app_settings(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(SecretRequest {
            app,
            name,
            value,
            link,
        }): Parameters<SecretRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Editor).await {
            return Ok(refused);
        }
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
        description = "Read or write notes kept with one app for the next session to find. Call it with no notes to read. They are about THIS app: what it is, where things are, its schema, why it was built that way, what is half-finished. Not how the platform behaves — that is at GET /guide, which stays current, while a platform note here is wrong the moment it changes. Read the notes for the app you are changing, not a neighbour's.",
        annotations(title = "App notes", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn app_notes(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(NotesRequest { slug, notes }): Parameters<NotesRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &slug, Scope::Editor).await {
            return Ok(refused);
        }
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
        description = "Turn an account off or back on. A disabled account cannot sign in and its live sessions stop working at once, but it is not deleted.",
        annotations(title = "Enable or disable an account", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn set_user_active(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(SetActiveRequest { email, active }): Parameters<SetActiveRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = Self::root_only(&ctx) {
            return Ok(refused);
        }
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
        description = "Give or take away one account's access to one app. Only matters for apps whose access is 'restricted'.",
        annotations(title = "Grant or revoke access", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn set_access(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(AccessRequest {
            app,
            email,
            allow,
            role,
        }): Parameters<AccessRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Admin).await {
            return Ok(refused);
        }
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
        // The same as the Access tab: access on the app is a View row on it.
        if outcome.is_ok() {
            let path = crate::content::store::logical_path(&self.config, &app).await;
            if allow {
                crate::platform::permissions::give_view_if_absent(&self.config, &email, &path).await;
            } else {
                let (config, who) = (self.config.clone(), email.clone());
                let _ = tokio::task::spawn_blocking(move || crate::accounts::users::revoke_scope(&config, &who, &path)).await;
            }
        }

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
        description = "List published pages: slug, title, URL, when each was last changed, and its visibility. Call this to find out what already exists before editing or reusing a slug.",
        annotations(title = "List apps and pages", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn list_pages(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(ListPagesRequest { include_all }): Parameters<ListPagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let include_all = include_all.unwrap_or(false);
        let caller = Self::caller(&ctx);
        let mut slugs = Vec::new();
        collect_slugs(&self.config.data_dir, String::new(), &mut slugs).await;

        let mut rows = Vec::new();
        for slug in slugs {
            let meta = read_meta(&self.config, &slug).await;
            if !include_all && (meta.hidden || !meta.listed) {
                continue;
            }
            // A signed-in caller sees what it may open: public and signed-in
            // apps, and the ones it holds a scope on.
            if let Some(user) = &caller.user {
                let app = slug.split('/').next().unwrap_or(&slug).to_string();
                let gate = meta.gate_for("/", &self.config.default_gate).to_string();
                let open = matches!(gate.as_str(), "public" | "authenticated")
                    || self.held_on(user, &app).await.is_some();
                if !open {
                    continue;
                }
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
        description = "Take a slug down for good, moving everything belonging to it out of the site. Prefer set_visibility, which hides without removing; this is for junk — a probe published as a page, an app nobody wants. Files are moved aside rather than deleted, so a mistake is recoverable from the server, but nothing on the site refers to them again.",
        annotations(title = "Remove an app or page", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    pub(crate) async fn remove_page(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(RemoveRequest {
            slug,
            confirm,
            page_only,
        }): Parameters<RemoveRequest>,
    ) -> Result<CallToolResult, McpError> {
        if confirm != slug {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "pass confirm: \"{slug}\" to remove it"
            ))]));
        }
        let caller = match self.allowed(&ctx, &slug, Scope::Editor).await {
            Ok(caller) => caller,
            Err(refused) => return Ok(refused),
        };
        if let Some(user) = &caller.user {
            let app = slug.split('/').next().unwrap_or(&slug).to_string();
            let path = crate::content::store::logical_path(&self.config, &app).await;
            let is_admin_here = self.held_on(user, &app).await == Some(Scope::Admin);
            let created_it = read_meta(&self.config, &app).await.created_by.as_deref() == Some(user.id.as_str());
            if !is_admin_here && !created_it {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "{} may remove only apps it created under {path}; removing {app} needs admin there.",
                    user.email
                ))]));
            }
        }
        let config = self.config.clone_for_task();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let page_only = page_only.unwrap_or(false);
        let owned = slug.clone();

        let outcome = tokio::task::spawn_blocking(move || {
            if page_only {
                crate::platform::trash::remove_page_only(&config, &owned, at)
            } else {
                crate::platform::trash::remove(&config, &owned, at)
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        match outcome {
            Ok(moved) => {
                self.runtime.forget(&slug);
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "removed {}; kept under .trash/{at}-{} on the server",
                    moved.join(", "),
                    slug.replace('/', "-")
                ))]))
            }
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Take a page down or restore it, and control whether it appears on the site index. Nothing is ever deleted, so this is the safe way to retract a page published by mistake.",
        annotations(title = "Set visibility and access", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn set_visibility(
        &self,
        ctx: RequestContext<RoleServer>,
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
        let needed = if gate.is_some() || path.is_some() { Scope::Admin } else { Scope::Editor };
        if let Err(refused) = self.allowed(&ctx, &slug, needed).await {
            return Ok(refused);
        }
        let gate = match gate {
            Some(word) if word == "default" && path.is_none() => Some(word),
            Some(word) => match crate::content::store::normalise_gate(&word) {
                Some(level) => Some(level.to_string()),
                None => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "gate must be 'public', 'authenticated' or 'restricted' ('granted' is the old name and still works), or 'default' for the whole app, meaning the site's TOOLSITE_DEFAULT_ACCESS",
                    )]));
                }
            },
            None => None,
        };
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
            (None, Some(gate)) => meta.gate = (gate != "default").then_some(gate),
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
        description = "Set the icon shown next to a page on the site index: an emoji, inline SVG, or data: URI. Pages without one get a generated icon, so this is optional.",
        annotations(title = "Set an icon", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn set_icon(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(SetIconRequest { slug, icon }): Parameters<SetIconRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &slug, Scope::Editor).await {
            return Ok(refused);
        }
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
        description = "Fetch the current HTML for every page in an app namespace, keyed by page name, so the app can be edited and pushed back with push_app.",
        annotations(title = "Read an app", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn pull_app(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(PullAppRequest { app }): Parameters<PullAppRequest>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(refused) = self.allowed(&ctx, &app, Scope::Editor).await {
            return Ok(refused);
        }
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
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
            .with_server_info(Implementation::new("toolsite", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Publishes self-contained HTML pages at public URLs.\n\
                 \n\
                 Check your environment before you promise an app. Run `npm --version`, \
                 `cargo --version` and `curl -sI <site>/guide`. A React or wasm build needs \
                 npm or cargo, and uploading needs either a route to this host or the inline \
                 upload tools. Without a build tool you can still do everything else: pages \
                 of plain HTML with push_page, SQL with run_sql, accounts, access, exports, \
                 repositories and notes. Say which you have before you start, not after.\n\
                 \n\
                 Look at what you built: screenshot(slug) renders the page in a real browser \
                 on the server and returns the picture. Say what you see, with data, before \
                 you say it works.\n\
                 \n\
                 How to publish, in order of preference:\n\
                 1. If you can run shell commands: write the HTML to a file, call \
                 create_upload, then `curl -fT <file> <upload-url>`. Never read the file back \
                 into the conversation — that is the whole point. Emitting a page's HTML as \
                 tool-call arguments when you could have written a file is wasteful, so treat \
                 this as the default path.\n\
                 2. If curl fails because the sandbox has no network access to this host, send \
                 the file in base64 chunks with upload_begin / upload_chunk / upload_finish: \
                 same kinds (bundle, handler, migrations, manifest, source, blob, page), same \
                 rules. push_page / push_app take plain HTML inline.\n\
                 3. If there is no shell at all, build nothing: push_page / push_app for HTML, \
                 or the chunked upload for a file you already have.\n\
                 \n\
                 Build anything interactive as a real front-end project, not as one \
                 hand-written HTML file. If it has state, forms, or more than one screen, \
                 scaffold Vite + React + Tailwind and upload the build: `tar -czf - -C dist \
                 . | curl -f -T - '<upload-url>?bundle&spa'`. A single index.html with an \
                 inline <script> is cheaper only until the second change, after which every \
                 edit is string replacement against markup you cannot test. Plain HTML is \
                 for genuinely static pages.\n\
                 \n\
                 Every app is served from a subpath (/p/<slug>/), never the domain root, so \
                 set the build's base path accordingly — create_upload prints the exact \
                 value, and Vite needs it as `base`. Skipping that step produces a page that \
                 loads but renders blank, because its assets 404. The `toolsite` CLI, if it \
                 is installed, scaffolds this already correct: `toolsite init <name> --react \
                 [--handler]`.\n\
                 \n\
                 GET /guide is how this platform works, kept current. Read it before \
                 building anything with a handler, a schema or a gate.\n\
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
