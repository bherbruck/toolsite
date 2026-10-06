//! The site as its owner runs it: apps, accounts, access, exports.
//!
//! This is a platform route rather than a published app on purpose: an app
//! cannot read the account database — that isolation is the thing every other
//! guarantee rests on — so an "admin app" could only exist by breaking it.
//!
//! The shape is lists that show and pages that edit. A list row is a link;
//! the page it leads to has real forms, grouped by concern, each with its own
//! save. Anything that removes or disables asks first, through the shell's
//! dialog. The result of an action comes back as a one-line flash on the page
//! the person was on, carried across the redirect in a short-lived cookie.
//!
//! Every action is a POST carrying a token derived from the caller's own
//! session. Cookies are `SameSite=Lax`, which already refuses a cross-site
//! POST; the token is what stops a page on *this* origin from acting as the
//! admin who happens to be visiting it.

use crate::{
    platform::projects,
    accounts::users::{self, Scope, User},
    config::Config,
    content::{
        slug::valid_slug,
        store::{self, collect_slugs, read_meta, write_meta, PathRule},
    },
    platform::export,
    ui::{self, Flash},
    AppState,
};
use axum::{
    extract::{Form, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use maud::{html, Markup};
use serde::Deserialize;
use std::sync::Arc;

const FLASH_COOKIE: &str = "ts_flash";
const GATES: [(&str, &str, &str); 3] = [
    ("public", "Public", "Anyone with the link."),
    ("authenticated", "Signed in", "Any account on this site."),
    ("restricted", "Restricted", "Only people given access."),
];

/// Resolves an admin from the request, or the response to send instead.
pub(crate) async fn require_admin(config: &Arc<Config>, headers: &HeaderMap) -> Result<User, Response> {
    match users::current_site_user(config, headers).await {
        Some(user) if user.is_admin => Ok(user),
        // Someone signed in but not an admin is told no, not sent to sign in
        // again — that would loop.
        Some(_) => Err((StatusCode::FORBIDDEN, "not an admin").into_response()),
        None => Err(Redirect::to("/auth/login?next=/admin").into_response()),
    }
}

/// What the signed-in account holds at `path`, the one question every
/// admin page and action asks.
pub(crate) async fn held(config: &Arc<Config>, user: &User, path: &str) -> Option<Scope> {
    let (config, user, path) = (config.clone(), user.clone(), path.to_string());
    tokio::task::spawn_blocking(move || {
        let locks = store::locked_prefixes_blocking(&config);
        users::effective_scope(&config, &user, &path, &locks)
    })
        .await
        .ok()
        .flatten()
}

/// Whether the account may manage anything at all, which is what opens the
/// admin pages: a site admin, or editor or admin somewhere in the tree.
pub(crate) async fn manages_something(config: &Arc<Config>, user: &User) -> bool {
    let (config, user) = (config.clone(), user.clone());
    tokio::task::spawn_blocking(move || users::holds_anywhere(&config, &user, Scope::Editor))
        .await
        .unwrap_or(false)
}

/// Anyone who holds Manage somewhere: who may list accounts to give them
/// access. A site admin, or an admin of any project or app.
pub(crate) async fn require_manager(config: &Arc<Config>, headers: &HeaderMap) -> Result<User, Response> {
    let user = match users::current_site_user(config, headers).await {
        Some(user) => user,
        None => return Err(Redirect::to("/auth/login?next=/admin").into_response()),
    };
    let (config2, who) = (config.clone(), user.clone());
    let manages = tokio::task::spawn_blocking(move || users::holds_anywhere(&config2, &who, Scope::Admin))
        .await
        .unwrap_or(false);
    if manages {
        Ok(user)
    } else {
        Err((StatusCode::FORBIDDEN, "not an admin").into_response())
    }
}

/// Anyone who may enter the admin pages. What they then see is decided
/// page by page from what they hold.
pub(crate) async fn require_entry(config: &Arc<Config>, headers: &HeaderMap) -> Result<User, Response> {
    match users::current_site_user(config, headers).await {
        Some(user) if manages_something(config, &user).await => Ok(user),
        Some(_) => Err((StatusCode::FORBIDDEN, "not an admin").into_response()),
        None => Err(Redirect::to("/auth/login?next=/admin").into_response()),
    }
}

/// `needed` at `path`: an app's path in the tree, or a folder.
pub(crate) async fn require_scope(
    config: &Arc<Config>,
    headers: &HeaderMap,
    path: &str,
    needed: Scope,
) -> Result<User, Response> {
    let user = match users::current_site_user(config, headers).await {
        Some(user) => user,
        None => return Err(Redirect::to("/auth/login?next=/admin").into_response()),
    };
    match held(config, &user, path).await {
        Some(have) if have >= needed => Ok(user),
        _ => {
            tracing::warn!(email = %user.email, path = %path, needed = %needed, "admin refused: scope");
            Err((
                StatusCode::FORBIDDEN,
                format!("You need {needed} access at {} for this.", if path.is_empty() { "the site" } else { path }),
            )
                .into_response())
        }
    }
}

/// A folder is open to someone who manages it, or who holds something
/// inside it and needs to walk down to that.
pub(crate) async fn may_see_folder(config: &Arc<Config>, user: &User, folder: &str) -> bool {
    if held(config, user, folder).await.is_some_and(|have| have >= Scope::Editor) {
        return true;
    }
    let (config, user, folder) = (config.clone(), user.clone(), folder.to_string());
    tokio::task::spawn_blocking(move || users::holds_below(&config, &user, &folder))
        .await
        .unwrap_or(false)
}

/// The path scopes are matched against for an app.
pub(crate) async fn app_path(config: &Config, app: &str) -> String {
    store::logical_path(config, app).await
}

/// What the account may do to one app, wherever it sits.
pub(crate) async fn held_on(config: &Arc<Config>, user: &User, app: &str) -> Option<Scope> {
    let folder = store::app_folder(config, app).await;
    let (config, user, app) = (config.clone(), user.clone(), app.to_string());
    tokio::task::spawn_blocking(move || {
        let locks = store::locked_prefixes_blocking(&config);
        users::app_scope(&config, &user, &folder, &app, &locks)
    })
        .await
        .ok()
        .flatten()
}

/// `needed` on one app.
pub(crate) async fn require_app(
    config: &Arc<Config>,
    headers: &HeaderMap,
    app: &str,
    needed: Scope,
) -> Result<User, Response> {
    let user = match users::current_site_user(config, headers).await {
        Some(user) => user,
        None => return Err(Redirect::to("/auth/login?next=/admin").into_response()),
    };
    match held_on(config, &user, app).await {
        Some(have) if have >= needed => Ok(user),
        _ => {
            let path = app_path(config, app).await;
            tracing::warn!(email = %user.email, path = %path, needed = %needed, "admin refused: scope");
            Err((StatusCode::FORBIDDEN, format!("You need {needed} access at {path} for this.")).into_response())
        }
    }
}

/// Ties a form to the session that rendered it. Not the session token itself,
/// so a leaked page cannot be replayed as a credential.
pub(crate) fn form_token(config: &Config, user: &User) -> String {
    users::derive_form_token(config, &user.id)
}

fn check_form_token(config: &Config, user: &User, presented: &str) -> bool {
    // Constant-time is overkill for a value the holder already knows, but
    // comparing lengths first avoids the obvious early-exit.
    let expected = form_token(config, user);
    expected.len() == presented.len() && expected == presented
}

/// Sent on every admin response so the browser will not cache a page listing
/// accounts.
pub fn no_store() -> (header::HeaderName, &'static str) {
    (header::CACHE_CONTROL, "no-store")
}

// --- flash -------------------------------------------------------------------

/// Where a form sends the person back to, if it is one of ours.
pub(crate) fn back_or(back: Option<&str>, default: &str) -> String {
    match back {
        // Ours: an admin page, or a level of the app browser.
        Some(path)
            if (path.starts_with("/admin") || path == "/" || path.starts_with("/?") || path.starts_with("/browse/"))
                && !path.contains("//") =>
        {
            path.to_string()
        }
        _ => default.to_string(),
    }
}

/// Redirects with the outcome in a cookie the next admin page shows once.
pub(crate) fn redirect_flash(to: &str, ok: bool, text: impl Into<String>) -> Response {
    let text: String = text.into();
    let value = format!(
        "{FLASH_COOKIE}={}:{}; Path=/; HttpOnly; SameSite=Lax; Max-Age=60",
        if ok { "ok" } else { "error" },
        urlencoding::encode(&text)
    );
    ([(header::SET_COOKIE, value)], Redirect::to(to)).into_response()
}

pub(crate) fn take_flash(headers: &HeaderMap) -> Option<Flash> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    let raw = cookie
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(name, _)| *name == FLASH_COOKIE)
        .map(|(_, value)| value)?;
    let (kind, text) = raw.split_once(':')?;
    Some(Flash {
        ok: kind == "ok",
        text: urlencoding::decode(text).ok()?.into_owned(),
    })
}

pub(crate) fn clear_flash() -> (header::HeaderName, String) {
    (
        header::SET_COOKIE,
        format!("{FLASH_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    )
}

// --- the shell ----------------------------------------------------------------

/// The rail every signed-in page shares, including the index. `active` names
/// the current section so its link is marked rather than followed.
/// `manages` says whether the viewer holds editor or admin somewhere, which
/// is what shows the Admin group to someone who is not a site admin.
pub(crate) fn sidebar(active: &str, viewer: Option<&User>, manages: bool) -> Markup {
    let is_admin = viewer.is_some_and(|user| user.is_admin);
    html! {
        a."brand" href="/" { span."mark" { "t" } "toolsite" }
        div."nav-group" {
            div."label" { "Site" }
            a."active"[active == "site"] href="/" { "Apps" }
        }
        @if is_admin || manages {
            div."nav-group" {
                div."label" { "Admin" }
                a."active"[active == "apps"] href="/admin/apps" { "App settings" }
                @if is_admin {
                    a."active"[active == "accounts"] href="/admin/accounts" { "Accounts" }
                    a."active"[active == "exports"] href="/admin/exports" { "Exports" }
                    a."active"[active == "github"] href="/admin/github" { "GitHub" }
                }
            }
        }
        div."spacer" {
            @match viewer {
                Some(user) => {
                    a."who"."active"[active == "account"] href="/account" title="Your account" { (user.email) }
                    a href="/auth/logout" { "Sign out" }
                }
                None => {
                    a href="/auth/login?next=/" { "Sign in" }
                }
            }
        }
    }
}

pub(crate) struct Page<'a> {
    pub(crate) active: &'a str,
    pub(crate) title: &'a str,
    pub(crate) crumbs: Vec<(&'a str, &'a str)>,
    pub(crate) subtitle: Option<Markup>,
    pub(crate) actions: Option<Markup>,
    pub(crate) body: Markup,
    /// Emitted at the end of the body, for a page that needs behaviour the
    /// shell's own script does not give it.
    pub(crate) script: Option<&'static str>,
}

/// Wraps a section's content, so each page differs only in what it renders.
pub(crate) fn admin_page(headers: &HeaderMap, admin: &User, page: Page<'_>) -> Response {
    let flash = take_flash(headers);
    let markup = ui::shell(
        page.title,
        sidebar(page.active, Some(admin), true),
        html! {
            @if !page.crumbs.is_empty() {
                nav."crumbs" {
                    @for (label, href) in &page.crumbs { span { a href=(href) { (label) } } }
                    span { (page.title) }
                }
            }
            div."title-row" {
                div {
                    h1 { (page.title) }
                    @if let Some(subtitle) = page.subtitle { p."muted" { (subtitle) } }
                }
                @if let Some(actions) = page.actions { div."actions" { (actions) } }
            }
            (ui::flash(flash.as_ref()))
            (page.body)
        },
        page.script,
    );
    let mut response = ([no_store()], Html(markup.into_string())).into_response();
    if flash.is_some() {
        let (name, value) = clear_flash();
        if let Ok(value) = value.parse() {
            response.headers_mut().append(name, value);
        }
    }
    response
}

pub(crate) fn hidden(name: &str, value: &str) -> Markup {
    html! { input type="hidden" name=(name) value=(value); }
}

// --- lists ---------------------------------------------------------------------

/// How many rows a list page shows. Beyond this it pages, so a site with
/// hundreds of apps or accounts is still a page a person can read.
const PAGE_SIZE: usize = 50;

#[derive(Deserialize, Default)]
pub struct ListQuery {
    q: Option<String>,
    page: Option<usize>,
    /// The folder a list is scoped to, for the apps tree.
    folder: Option<String>,
}

struct Listing<T> {
    rows: Vec<T>,
    total: usize,
    page: usize,
    pages: usize,
    q: String,
}

/// Narrows by `q` (case-insensitive, against whatever `text` returns) and
/// takes one page. One helper for every list, so they all behave the same.
fn paginate<T>(all: Vec<T>, query: &ListQuery, text: impl Fn(&T) -> String) -> Listing<T> {
    let q = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let matching: Vec<T> = if q.is_empty() {
        all
    } else {
        all.into_iter().filter(|row| text(row).to_lowercase().contains(&q)).collect()
    };
    let total = matching.len();
    let pages = total.div_ceil(PAGE_SIZE).max(1);
    let page = query.page.unwrap_or(1).clamp(1, pages);
    let rows = matching.into_iter().skip((page - 1) * PAGE_SIZE).take(PAGE_SIZE).collect();
    Listing { rows, total, page, pages, q }
}

/// The search box above a list. Typing narrows the rows already on the
/// page; Enter asks the server, which is what finds things on other pages.
fn search_box(listing: &Listing<impl Sized>, path: &str, placeholder: &str) -> Markup {
    html! {
        form."search" method="get" action=(path) {
            input type="search" id="q" name="q" value=(listing.q) placeholder=(placeholder) autocomplete="off";
        }
    }
}

/// "Showing 1–50 of 120" and the way to the rest.
fn pager(listing: &Listing<impl Sized>, path: &str) -> Markup {
    let first = if listing.total == 0 { 0 } else { (listing.page - 1) * PAGE_SIZE + 1 };
    let last = ((listing.page) * PAGE_SIZE).min(listing.total);
    let link = |page: usize| {
        let mut href = format!("{path}?page={page}");
        if !listing.q.is_empty() {
            href.push_str(&format!("&q={}", urlencoding::encode(&listing.q)));
        }
        href
    };
    html! {
        div."pager" {
            span {
                @if listing.total == 0 { "No match" }
                @else { "Showing " (first) "–" (last) " of " (listing.total) }
            }
            @if listing.pages > 1 {
                div."actions" {
                    @if listing.page > 1 { a."btn quiet sm" href=(link(listing.page - 1)) { "Previous" } }
                    span."muted small" { "Page " (listing.page) " of " (listing.pages) }
                    @if listing.page < listing.pages { a."btn quiet sm" href=(link(listing.page + 1)) { "Next" } }
                }
            }
        }
    }
}

/// One match for a picker: what the form submits, and what the person sees.
#[derive(serde::Serialize)]
pub struct Match {
    value: String,
    label: String,
}

const MAX_MATCHES: usize = 10;

/// At most ten matches. An empty query gives the first ten, so a picker
/// shows something to choose from as soon as it opens, and never the whole
/// list.
fn matches<T>(all: Vec<T>, q: &str, to_match: impl Fn(&T) -> Match) -> Vec<Match> {
    let q = q.trim().to_lowercase();
    all.into_iter()
        .map(|row| to_match(&row))
        .filter(|m| m.value.to_lowercase().contains(&q) || m.label.to_lowercase().contains(&q))
        .take(MAX_MATCHES)
        .collect()
}

/// `GET /admin/accounts/search?q=`: at most ten emails, for a picker. The
/// page never carries every account; the picker asks as it opens.
pub async fn search_accounts(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    if let Err(response) = require_manager(&config, &headers).await {
        return response;
    }
    let accounts = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || users::list_accounts(&config))
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default()
    };
    let found = matches(accounts, query.q.as_deref().unwrap_or(""), |account| Match {
        value: account.email.clone(),
        label: account.email.clone(),
    });
    ([no_store()], Json(found)).into_response()
}

/// `GET /admin/apps/search?q=`: at most ten apps by slug or title.
pub async fn search_apps(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let viewer = match require_entry(&config, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let q = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    if q.is_empty() {
        return ([no_store()], Json(Vec::<Match>::new())).into_response();
    }
    let mut found = Vec::new();
    for app in app_names(&config).await {
        if !viewer.is_admin && !held_on(&config, &viewer, &app).await.is_some_and(|have| have >= Scope::Editor) {
            continue;
        }
        let path = crate::content::store::page_path(&config, &app).await;
        let title = match &path {
            Some(path) => crate::content::store::page_title(path).await,
            None => None,
        };
        let label = title.clone().unwrap_or_else(|| app.clone());
        if app.to_lowercase().contains(&q) || label.to_lowercase().contains(&q) {
            found.push(Match { value: app, label });
            if found.len() == MAX_MATCHES {
                break;
            }
        }
    }
    ([no_store()], Json(found)).into_response()
}

/// `GET /admin/projects/search?q=`: at most ten projects the caller may move
/// an app into, which is where it holds admin. `/` is the top level.
pub async fn search_projects(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let viewer = match require_entry(&config, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let q = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    if q.is_empty() {
        return ([no_store()], Json(Vec::<Match>::new())).into_response();
    }
    let mut found = Vec::new();
    if (q == "/" || "top level".contains(&q)) && held(&config, &viewer, "").await == Some(Scope::Admin) {
        found.push(Match { value: "/".to_string(), label: "Top level".to_string() });
    }
    for folder in store::list_folders(&config).await {
        if found.len() == MAX_MATCHES {
            break;
        }
        if !folder.path.to_lowercase().contains(&q) {
            continue;
        }
        if held(&config, &viewer, &folder.path).await != Some(Scope::Admin) {
            continue;
        }
        found.push(Match { value: folder.path.clone(), label: folder.path });
    }
    ([no_store()], Json(found)).into_response()
}

// --- apps ----------------------------------------------------------------------

/// The apps that exist, by their top-level directory.
async fn app_names(config: &Config) -> Vec<String> {
    let mut slugs = Vec::new();
    collect_slugs(&config.data_dir, String::new(), &mut slugs).await;
    let mut apps: Vec<String> = slugs
        .into_iter()
        .map(|slug| slug.split('/').next().unwrap_or(&slug).to_string())
        .collect();
    apps.sort();
    apps.dedup();
    apps
}

struct AppRow {
    app: String,
    title: Option<String>,
    gate: String,
    follows_default: bool,
    hidden: bool,
    has_handler: bool,
    modified: Option<std::time::SystemTime>,
}

async fn app_row(config: &Config, app: &str) -> AppRow {
    let meta = read_meta(config, app).await;
    let path = crate::content::store::page_path(config, app).await;
    let title = match &path {
        Some(path) => crate::content::store::page_title(path).await,
        None => None,
    };
    let modified = match &path {
        Some(path) => tokio::fs::metadata(path).await.ok().and_then(|m| m.modified().ok()),
        None => None,
    };
    AppRow {
        app: app.to_string(),
        title,
        gate: meta.gate(&config.default_gate).to_string(),
        follows_default: meta.gate.is_none(),
        hidden: meta.hidden,
        has_handler: config.data_dir.join(app).join("handler.wasm").is_file(),
        modified,
    }
}

pub async fn apps_page(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let viewer = match require_entry(&config, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let folder = query.folder.clone().unwrap_or_default();
    let folder = folder.trim_matches('/').to_string();
    if !users::valid_prefix(&folder) || !store::folder_exists(&config, &folder).await {
        return (StatusCode::NOT_FOUND, "no such folder").into_response();
    }
    if !may_see_folder(&config, &viewer, &folder).await {
        return (StatusCode::FORBIDDEN, format!("You hold no access under {}.", if folder.is_empty() { "the site" } else { &folder })).into_response();
    }
    let here = held(&config, &viewer, &folder).await;
    let is_admin_here = here == Some(Scope::Admin);

    // Subfolders the viewer may enter, with what sits in each.
    let all_apps = store::apps_with_folders(&config).await;
    let mut folders = Vec::new();
    for sub in store::subfolders(&config, &folder).await {
        if !may_see_folder(&config, &viewer, &sub.path).await {
            continue;
        }
        let apps_below = all_apps.iter().filter(|(_, in_folder)| users::prefix_covers(&sub.path, in_folder)).count();
        folders.push((sub, apps_below));
    }

    // Apps directly in this folder that the viewer may manage.
    let mut rows = Vec::new();
    for (app, in_folder) in &all_apps {
        if in_folder != &folder {
            continue;
        }
        if !held_on(&config, &viewer, app).await.is_some_and(|have| have >= Scope::Editor) {
            continue;
        }
        rows.push(app_row(&config, app).await);
    }
    let count = rows.len();
    let listing = paginate(rows, &query, |row| {
        format!("{} {}", row.app, row.title.as_deref().unwrap_or(""))
    });
    let list_path = if folder.is_empty() {
        "/admin/apps".to_string()
    } else {
        format!("/admin/apps?folder={}", urlencoding::encode(&folder))
    };

    let title = if folder.is_empty() { "App settings".to_string() } else { folder.rsplit('/').next().unwrap_or(&folder).to_string() };
    let crumbs: Vec<(String, String)> = std::iter::once(("App settings".to_string(), "/admin/apps".to_string()))
        .chain(store::folder_chain(&format!("{folder}/x")).into_iter().filter(|chain| chain != &folder).map(|chain| {
            let name = chain.rsplit('/').next().unwrap_or(&chain).to_string();
            (name, format!("/admin/apps?folder={}", urlencoding::encode(&chain)))
        }))
        .collect();
    let crumb_refs: Vec<(&str, &str)> = if folder.is_empty() {
        Vec::new()
    } else {
        crumbs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
    };
    let subtitle = if folder.is_empty() {
        html! { (count) " apps at the root" }
    } else {
        html! { code { (folder) } " · " (count) " apps" }
    };

    admin_page(
        &headers,
        &viewer,
        Page {
            active: "apps",
            title: &title,
            crumbs: crumb_refs,
            subtitle: Some(subtitle),
            actions: Some(html! {
                @match here {
                    Some(scope) => span."badge solid" { "you: " (scope) },
                    None => span."badge" { "you: inside only" },
                }
            }),
            script: (count > 0).then_some(ui::FILTER_SCRIPT),
            body: html! {
                @if !folders.is_empty() {
                    section."panel" {
                        div."panel-head" { h3 { "Folders" } }
                        table {
                            thead { tr { th { "Folder" } th { "Path" } th."num" { "Apps below" } } }
                            tbody {
                                @for (sub, apps_below) in &folders {
                                    tr {
                                        td { a."row-link" href={ "/admin/apps?folder=" (urlencoding::encode(&sub.path)) } { (sub.name) } }
                                        td { code { (sub.path) } }
                                        td."num" { (apps_below) }
                                    }
                                }
                            }
                        }
                    }
                }
                @if count == 0 && folders.is_empty() {
                    (ui::panel("No apps", Some("There are no apps here. An agent publishes an app with create_upload. An admin can add a folder below."), html! {}))
                } @else if count > 0 {
                    (search_box(&listing, &list_path, "Search apps"))
                    section."panel" {
                        table {
                            thead { tr { th { "App" } th { "Access" } th { "Handler" } th { "Updated" } th {} } }
                            tbody id="list" {
                                @for row in &listing.rows {
                                    tr data-slug=(row.app.to_lowercase())
                                       data-title=(row.title.as_deref().unwrap_or_default().to_lowercase()) {
                                        td {
                                            a."row-link" href={ "/admin/apps/" (row.app) } { (row.title.as_deref().unwrap_or(&row.app)) }
                                            @if row.title.is_some() { " " span."muted small" { (row.app) } }
                                        }
                                        td {
                                            (gate_badge(&row.gate))
                                            @if row.follows_default { " " span."muted small" { "site default" } }
                                            @if row.hidden { " " span."badge warn" { "hidden" } }
                                        }
                                        td { @if row.has_handler { span."badge" { "wasm" } } @else { span."muted small" { "static" } } }
                                        td."muted small" {
                                            @if let Some(modified) = row.modified { (crate::content::store::relative_time(modified)) }
                                        }
                                        td."actions-cell" { a."btn quiet sm" href={ "/p/" (row.app) "/" } target="_blank" { "Open" } }
                                    }
                                }
                            }
                        }
                    }
                    p."no-match" id="no-match" { "No app on this page matches. Press Enter to search all apps." }
                    (pager(&listing, &list_path))
                }

                (ui::panel("Projects and permissions", Some("New projects and who may do what in each are set in the app browser, where the apps are."), html! {
                    a."btn quiet" href=(crate::content::browse::browser_url(&folder)) { "Open in the app browser" }
                    @if is_admin_here {
                        " "
                        a."btn quiet" href={ (crate::content::browse::browser_url(&folder)) "?tab=permissions" } { "Permissions" }
                    }
                }))
            },
        },
    )
}

/// How a person reads an access level: the plain name, never the stored word.
pub(crate) fn gate_label(gate: &str) -> &'static str {
    match crate::content::store::normalise_gate(gate) {
        Some("public") => "Public",
        Some("authenticated") => "Signed in",
        _ => "Restricted",
    }
}

fn gate_badge(gate: &str) -> Markup {
    html! {
        @match gate {
            "public" => span."badge" { "Public" },
            "authenticated" => span."badge solid" { "Signed in" },
            _ => span."badge solid" { "Restricted" },
        }
    }
}

const TABS: [(&str, &str); 7] = [
    ("overview", "Overview"),
    ("access", "Access"),
    ("repo", "Repo"),
    ("exports", "Exports"),
    ("settings", "Settings"),
    ("jobs", "Jobs"),
    ("notes", "Notes"),
];

fn tab_href(app: &str, tab: &str) -> String {
    if tab == "overview" {
        format!("/admin/apps/{app}")
    } else {
        format!("/admin/apps/{app}/{tab}")
    }
}

pub async fn app_overview(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Path(app): Path<String>,
) -> Response {
    app_tab(config, headers, app, "overview".to_string(), None).await
}

pub async fn app_tab_page(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Path((app, tab)): Path<(String, String)>,
    Query(grid): Query<crate::platform::permissions::GridQuery>,
) -> Response {
    app_tab_with(config, headers, app, tab, None, grid).await
}

/// Something minted by the request that rendered this page, shown this once.
pub(crate) enum Fresh {
    ExportToken(String),
    SettingsLink(String),
    DeployToken(String),
}

pub(crate) async fn app_tab(
    config: Arc<Config>,
    headers: HeaderMap,
    app: String,
    tab: String,
    fresh: Option<Fresh>,
) -> Response {
    app_tab_with(config, headers, app, tab, fresh, Default::default()).await
}

async fn app_tab_with(
    config: Arc<Config>,
    headers: HeaderMap,
    app: String,
    tab: String,
    fresh: Option<Fresh>,
    grid: crate::platform::permissions::GridQuery,
) -> Response {
    if !export::valid_app(&app) || !TABS.iter().any(|(key, _)| *key == tab) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let exists = app_names(&config).await.iter().any(|name| name == &app);
    if !exists {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    }
    let needed = if matches!(tab.as_str(), "access" | "exports" | "repo") { Scope::Admin } else { Scope::Editor };
    let admin = match require_app(&config, &headers, &app, needed).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let is_admin_here = held_on(&config, &admin, &app).await == Some(Scope::Admin);

    let token = form_token(&config, &admin);
    let meta = read_meta(&config, &app).await;
    let hrefs: Vec<(String, String)> = TABS
        .iter()
        .map(|(key, _)| (key.to_string(), tab_href(&app, key)))
        .collect();
    let tab_items: Vec<(&str, &str, &str)> = TABS
        .iter()
        .zip(hrefs.iter())
        .filter(|((key, _), _)| is_admin_here || !matches!(*key, "access" | "exports" | "repo"))
        .map(|((key, label), (_, href))| (*key, *label, href.as_str()))
        .collect();
    let back = tab_href(&app, &tab);

    let body = match tab.as_str() {
        "overview" => render_overview(&config, &app, &meta, &token, &back, is_admin_here).await,
        "access" => render_access_tab(&config, &admin, &app, &meta, &token, &back, &grid).await,
        "repo" => {
            let fresh_token = match &fresh {
                Some(Fresh::DeployToken(value)) => Some(value.as_str()),
                _ => None,
            };
            crate::platform::github::render_repo_tab(&config, &app, &token, &back, fresh_token).await
        }
        "exports" => {
            let tokens = {
                let (config, app) = (config.clone(), app.clone());
                tokio::task::spawn_blocking(move || export::list(&config, &app))
                    .await
                    .unwrap_or_default()
            };
            let fresh_token = match &fresh {
                Some(Fresh::ExportToken(value)) => Some(value.as_str()),
                _ => None,
            };
            render_exports_tab(&config, &app, &tokens, &token, &back, fresh_token)
        }
        "settings" => {
            let names = crate::platform::secrets::names(&config, &app);
            let link = match &fresh {
                Some(Fresh::SettingsLink(url)) => Some(url.as_str()),
                _ => None,
            };
            render_settings_tab(&app, &names, &token, &back, link)
        }
        "jobs" => {
            let jobs = {
                let (config, app) = (config.clone(), app.clone());
                tokio::task::spawn_blocking(move || crate::platform::schedule::read_jobs(&config, &app))
                    .await
                    .unwrap_or_default()
            };
            render_jobs_tab(&app, &jobs, &token, &back)
        }
        _ => {
            let notes = crate::content::store::read_notes(&config, &app).await;
            render_notes_tab(&app, notes.as_deref(), &token, &back)
        }
    };

    let title = app.clone();
    let url = crate::content::store::page_url(&config, &app);
    admin_page(
        &headers,
        &admin,
        Page {
            active: "apps",
            title: &title,
            crumbs: vec![("App settings", "/admin/apps")],
            subtitle: Some(html! { a href=(url) target="_blank" { (url) } }),
            actions: Some(html! {
                (gate_badge(meta.gate(&config.default_gate)))
                @if meta.hidden { span."badge warn" { "hidden" } }
                @if !meta.listed { span."badge" { "unlisted" } }
            }),
            script: None,
            body: html! {
                (ui::tabs(&tab_items, &tab))
                (body)
            },
        },
    )
}

async fn render_overview(
    config: &Config,
    app: &str,
    meta: &crate::content::store::PageMeta,
    token: &str,
    back: &str,
    is_admin_here: bool,
) -> Markup {
    let folder = meta.project.clone().unwrap_or_default();
    let dir = config.data_dir.join(app);
    let has_handler = dir.join("handler.wasm").is_file();
    let db_bytes = tokio::fs::metadata(dir.join("data.db")).await.ok().map(|m| m.len());
    let page = crate::content::store::page_path(config, app).await;
    let title = match &page {
        Some(path) => crate::content::store::page_title(path).await,
        None => None,
    };
    let modified = match &page {
        Some(path) => tokio::fs::metadata(path).await.ok().and_then(|m| m.modified().ok()),
        None => None,
    };
    let source = tokio::fs::metadata(config.data_dir.join(format!("{app}.source")))
        .await
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    let page_url = crate::content::store::page_url(config, app);
    html! {
        div."grid-2" {
            (ui::panel("About", None, html! {
                dl."kv" {
                    dt { "Project" }
                    dd {
                        @if folder.is_empty() { a href="/" { "the top level" } } @else {
                            a href=(crate::content::browse::browser_url(&folder)) { (folder) }
                        }
                    }
                    dt { "Title" } dd { (title.as_deref().unwrap_or("—")) }
                    dt { "Updated" } dd { @match modified { Some(m) => (crate::content::store::relative_time(m)), None => "—" } }
                    dt { "Handler" } dd { @if has_handler { "wasm component" } @else { "none" } }
                    dt { "Database" } dd { @match db_bytes { Some(b) => (human_bytes(b)), None => "none" } }
                    dt { "Routing" } dd { @if meta.spa { "client-side" } @else { "files and handler" } }
                    dt { "Outbound" }
                    dd {
                        @if meta.allow_http.is_empty() { "none" }
                        @else { @for (i, host) in meta.allow_http.iter().enumerate() { @if i > 0 { ", " } code { (host) } } }
                    }
                }
            }))
            (ui::panel("Visibility", Some("These settings do not delete files. You can change them again on this page."), html! {
                form."column" method="post" action="/admin/visibility" {
                    (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                    label."choice" {
                        input type="checkbox" name="listed" value="1" checked[meta.listed];
                        strong { "Show on the index" }
                        span { "If off, the URL continues to work. The app does not show on the index." }
                    }
                    label."choice" {
                        input type="checkbox" name="hidden" value="1" checked[meta.hidden];
                        strong { "Take down" }
                        span { "The URL returns 404. The app does not show on the index. The files stay." }
                    }
                    div."actions end" { button type="submit" { "Save visibility" } }
                }
            }))
        }
        @if is_admin_here {
            (ui::panel("Folder", Some("Move the app to another folder. The URL does not change. Access given on the app follows it; access from the folders changes to the new folders."), html! {
                form."row" method="post" action="/admin/move" {
                    (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                    input name="folder" value=(folder) placeholder="Folder path, empty for the root" pattern="[A-Za-z0-9_/-]*";
                    button."quiet" type="submit" { "Move app" }
                }
            }))
        }
        (ui::panel("Source archive", Some("The agent stored this source archive. The bundle cannot give back the source. Keep this copy."), html! {
            @match source {
                Some((bytes, modified)) => {
                    dl."kv" {
                        dt { "Archive" } dd { (human_bytes(bytes)) ", gzipped tar" }
                        dt { "Stored" } dd { @match modified { Some(m) => (crate::content::store::relative_time(m)), None => "—" } }
                    }
                    div."actions" style="margin-top: .75rem" {
                        a."btn" href={ "/admin/apps/" (app) "/source" } { "Download source" }
                        a."btn quiet" href=(page_url) target="_blank" { "Open the app as served" }
                    }
                }
                None => {
                    p."muted" {
                        "There is no source archive. Ask the agent to publish the source with " code { "?source" }
                        " on the upload URL. The " a href="/guide" { "guide" } " gives the steps. "
                        "The app as served is at "
                        a href=(page_url) target="_blank" { (page_url) } "."
                    }
                }
            }
        }))
    }
}

/// `GET /admin/apps/<app>/source`: the stored project archive, for an admin
/// and nobody else. The public site refuses `.source` outright; this is the
/// one door to it.
pub async fn download_source(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Path(app): Path<String>,
) -> Response {
    if !export::valid_app(&app) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if let Err(response) = require_app(&config, &headers, &app, Scope::Editor).await {
        return response;
    }
    let path = config.data_dir.join(format!("{app}.source"));
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return (StatusCode::NOT_FOUND, "no source stored for this app").into_response(),
    };
    let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = tokio_util::io::ReaderStream::new(file);
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(header::CONTENT_LENGTH, size)
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{app}-source.tar.gz\""),
        )
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

async fn render_access_tab(
    config: &Arc<Config>,
    viewer: &User,
    app: &str,
    meta: &crate::content::store::PageMeta,
    token: &str,
    back: &str,
    grid: &crate::platform::permissions::GridQuery,
) -> Markup {
    let path = app_path(config, app).await;
    let target = crate::platform::permissions::Target::App { app: app.to_string(), path, roles: meta.roles.clone() };
    let current = meta.gate.as_deref();
    html! {
        (ui::panel("General access", Some("Who may open the app at all. People with access below always may, unless it is Public, when anyone may."), html! {
            form method="post" action="/admin/gate" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                span."seg" role="radiogroup" aria-label="General access" {
                    @for (value, label, _) in GATES {
                        button type="submit" name="gate" value=(value) aria-pressed=(if current == Some(value) { "true" } else { "false" }) { (label) }
                    }
                    button type="submit" name="gate" value="default" aria-pressed=(if current.is_none() { "true" } else { "false" }) { "Site default" }
                }
            }
            p."muted small" {
                @match current {
                    Some(level) => { (gate_label(level)) ": " (GATES.iter().find(|(v, ..)| *v == level).map(|(_, _, help)| *help).unwrap_or("")) }
                    None => { "Follows the site default (" (gate_label(&config.default_gate)) "). Set it for the whole site with " code { "TOOLSITE_DEFAULT_ACCESS" } "." }
                }
            }
        }))

        (crate::platform::permissions::panel(config, viewer, &target, token, grid).await)

        (ui::panel("Shared data", Some("What a person may query from outside the app, as this account, through /me/mcp. Declared in toolsite.toml under [access]."), html! {
            @if meta.queryable.is_empty() && meta.policies.is_empty() {
                p."muted" { "This app shares no data. Add [access] to toolsite.toml to share views." }
            } @else {
                table {
                    thead { tr { th { "View" } th { "Mode" } th { "Source" } } }
                    tbody {
                        @for view in &meta.queryable {
                            tr { td { code { (view) } } td { "read" } td."muted small" { "hand-written view" } }
                        }
                        @for policy in &meta.policies {
                            tr {
                                td { code { (policy.view) } }
                                td { @if policy.write { "read, write" } @else { "read" } }
                                td."muted small" {
                                    "rows of " code { (policy.table) } " where " code { (policy.where_) }
                                    @if !meta.generated.iter().any(|g| g.eq_ignore_ascii_case(&policy.view)) {
                                        " " span."badge warn" { "not generated yet" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }))

        details."panel advanced" open[!meta.rules.is_empty()] {
            summary."panel-head" { h3 { "Advanced: route rules" } }
            div."panel-body" {
                p."muted small" { "A rule sets general access for one path prefix of the app. The longest matching prefix applies." }
                @if !meta.rules.is_empty() {
                    table {
                        thead { tr { th { "Prefix" } th { "Access" } th {} } }
                        tbody {
                            @for rule in &meta.rules {
                                tr {
                                    td { code { (rule.prefix) } }
                                    td { (gate_badge(&rule.gate)) }
                                    td."actions-cell" {
                                        form method="post" action="/admin/rule" {
                                            (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                                            (hidden("prefix", &rule.prefix)) (hidden("action", "remove"))
                                            button."danger quiet sm" type="submit" { "Remove rule" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                form."row" method="post" action="/admin/rule" {
                    (hidden("token", token)) (hidden("app", app)) (hidden("back", back)) (hidden("action", "add"))
                    input name="prefix" placeholder="/admin" required pattern="/.*";
                    select name="gate" { @for (value, label, _) in GATES { option value=(value) { (label) } } }
                    button."quiet" type="submit" { "Add rule" }
                }
            }
        }
    }
}

fn render_exports_tab(
    config: &Config,
    app: &str,
    tokens: &[export::ExportToken],
    token: &str,
    back: &str,
    fresh: Option<&str>,
) -> Markup {
    let url = export::export_url(config, app);
    html! {
        @if let Some(fresh) = fresh {
            (ui::panel("New export token", Some("Copy the token now. The token is shown one time only."), html! {
                p."small" { "Token:" }
                (ui::secret("fresh-token", fresh))
                p."small" { "Command:" }
                (ui::secret(
                    "fresh-token-curl",
                    &format!("curl -H 'Authorization: Bearer {fresh}' -o {app}.sqlite {url}"),
                ))
                p."muted small" {
                    "In a reporting tool, use a sqlite connection with the URL " code { (url) } " and this token as the bearer token."
                }
            }))
        }
        (ui::panel("Export tokens", Some("An export token lets a reporting tool read a copy of the database of this app. One token opens one app."), html! {
            @if tokens.is_empty() {
                p."muted" { "No export tokens. Create one below." }
            } @else {
                table {
                    thead { tr { th { "Label" } th { "Created" } th { "Last used" } th {} } }
                    tbody {
                        @for entry in tokens {
                            tr {
                                td { (entry.label) " " span."muted small" { (entry.id) } }
                                td."muted small" { (ago(entry.created_at)) }
                                td."muted small" { @match entry.last_used { Some(at) => (ago(at)), None => "never" } }
                                td."actions-cell" {
                                    form method="post" action="/admin/exports"
                                         data-confirm={ "Revoke " (entry.label) "?" }
                                         data-confirm-detail="The tool that holds this token gets 401 on the next request."
                                         data-confirm-label="Revoke token" data-confirm-danger="1" {
                                        (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                                        (hidden("action", "revoke")) (hidden("id", &entry.id))
                                        button."danger quiet sm" type="submit" { "Revoke token" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            form."row" method="post" action="/admin/exports" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back)) (hidden("action", "create"))
                input name="label" placeholder="Label, for example reporting" required;
                button."quiet" type="submit" { "Create token" }
            }
            p."muted small" { "URL: " code { (url) } }
        }))
    }
}

fn render_settings_tab(app: &str, names: &[String], token: &str, back: &str, link: Option<&str>) -> Markup {
    html! {
        @if let Some(link) = link {
            (ui::panel("Entry link", Some("Send this link to the person who has the values. The link is valid for one hour. Enter one NAME=value per line."), html! {
                (ui::secret("settings-link", link))
            }))
        }
        (ui::panel("Settings", Some("The handler reads these values with secrets.get. This page shows the names only."), html! {
            @if names.is_empty() {
                p."muted" { "No settings. Get an entry link below." }
            } @else {
                ul."stack" { @for name in names { li { code { (name) } } } }
            }
            form."row" method="post" action="/admin/settings-link" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                button."quiet" type="submit" { "Get entry link" }
            }
        }))
    }
}

fn render_jobs_tab(
    app: &str,
    jobs: &std::collections::BTreeMap<String, crate::platform::schedule::Job>,
    token: &str,
    back: &str,
) -> Markup {
    html! {
        (ui::panel("Scheduled jobs", Some("The app declares jobs in toolsite.toml. Each run sends a request to the handler."), html! {
            @if jobs.is_empty() {
                p."muted" { "No jobs. The app declares jobs in toolsite.toml." }
            } @else {
                table {
                    thead { tr { th { "Name" } th { "Schedule" } th { "Path" } th { "Last run" } th { "Status" } th {} } }
                    tbody {
                        @for (name, job) in jobs {
                            tr {
                                td { (name) }
                                td { code { (job.schedule) } }
                                td { code { (job.path) } }
                                td."muted small" { @match job.last_run { Some(at) => (ago(at)), None => "never" } }
                                td {
                                    @match job.last_status.as_deref() {
                                        Some(status) if status.starts_with("ok") || status.starts_with("200") => span."badge ok" { (status) },
                                        Some(status) => span."badge warn" { (status) },
                                        None => span."muted small" { "—" },
                                    }
                                }
                                td."actions-cell" {
                                    form method="post" action="/admin/job-run" {
                                        (hidden("token", token)) (hidden("app", app)) (hidden("back", back)) (hidden("name", name))
                                        button."quiet sm" type="submit" { "Run job" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }))
    }
}

fn render_notes_tab(app: &str, notes: Option<&str>, token: &str, back: &str) -> Markup {
    html! {
        (ui::panel("Notes", Some("Notes from the last session for the next session. The bundle cannot give back the source. Keep the notes."), html! {
            form."column" method="post" action="/admin/notes" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                textarea name="notes" rows="14" placeholder="No notes." { (notes.unwrap_or("")) }
                div."actions end" { button type="submit" { "Save notes" } }
            }
        }))
    }
}

fn ago(seconds: u64) -> String {
    let elapsed = export::seconds_since(seconds);
    match elapsed {
        s if s < 90 => "just now".to_string(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

// --- accounts ---------------------------------------------------------------------

pub async fn accounts_page(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let accounts = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || users::list_accounts(&config))
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default()
    };
    let count = accounts.len();
    let listing = paginate(accounts, &query, |account| account.email.clone());
    let token = form_token(&config, &admin);
    admin_page(
        &headers,
        &admin,
        Page {
            active: "accounts",
            title: "Accounts",
            crumbs: vec![],
            subtitle: Some(html! { (count) " accounts" }),
            actions: Some(html! { a."btn" href="/admin/accounts/new" { "New account" } }),
            script: (count > 0).then_some(ui::FILTER_SCRIPT),
            body: html! {
                @if count == 0 {
                    (ui::panel("No accounts", Some("There are no accounts. Click New account, or run toolsite user add on the server."), html! {}))
                } @else {
                    (search_box(&listing, "/admin/accounts", "Search accounts"))
                    section."panel" {
                        table {
                            thead { tr { th { "Email" } th { "Role" } th { "Status" } th { "Created" } th {} } }
                            tbody id="list" {
                                @for account in &listing.rows {
                                    tr data-slug=(account.email.to_lowercase()) {
                                        td { a."row-link" href={ "/admin/accounts/" (urlencoding::encode(&account.email)) } { (account.email) } }
                                        td { @if account.is_admin { span."badge solid" { "admin" } } @else { span."muted small" { "visitor" } } }
                                        td { @if account.is_active { span."badge ok" { "active" } } @else { span."badge warn" { "disabled" } } }
                                        td."muted small" { (account.created) }
                                        td."actions-cell" {
                                            @if account.is_active {
                                                form method="post" action="/admin/active"
                                                     data-confirm={ "Disable " (account.email) "?" }
                                                     data-confirm-detail="The sessions of this account end now. A connected MCP client stops on the next call. You can enable the account again."
                                                     data-confirm-label="Disable account" data-confirm-danger="1" {
                                                    (hidden("token", &token)) (hidden("email", &account.email)) (hidden("active", "0")) (hidden("back", "/admin/accounts"))
                                                    button."danger quiet sm" type="submit" { "Disable account" }
                                                }
                                            } @else {
                                                form method="post" action="/admin/active" {
                                                    (hidden("token", &token)) (hidden("email", &account.email)) (hidden("active", "1")) (hidden("back", "/admin/accounts"))
                                                    button."quiet sm" type="submit" { "Enable account" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    p."no-match" id="no-match" { "No account on this page matches. Press Enter to search all accounts." }
                    (pager(&listing, "/admin/accounts"))
                }
            },
        },
    )
}

/// One account: who they are, what they may open, and the two things an
/// admin does for them — a fresh setup link, and switching them off.
/// One place an account holds a level, for its read-only summary.
struct AccessLine {
    label: String,
    level: &'static str,
    source: &'static str,
    link: String,
}

/// An account's rows and app access, grouped by the project they sit in.
async fn access_summary(
    config: &Arc<Config>,
    scopes: &[users::ScopeGrant],
    grants: &[(String, String)],
) -> Vec<(String, Vec<AccessLine>)> {
    use crate::platform::permissions::{grid_url, level_word};
    let folders: Vec<String> = store::list_folders(config).await.into_iter().map(|f| f.path).collect();
    let mut lines: Vec<(String, AccessLine)> = Vec::new();
    for row in scopes {
        if row.prefix.is_empty() || folders.contains(&row.prefix) {
            let group = row.prefix.split('/').next().unwrap_or("").to_string();
            lines.push((group, AccessLine {
                label: if row.prefix.is_empty() { "The whole site".to_string() } else { row.prefix.clone() },
                level: level_word(row.scope),
                source: "project",
                link: grid_url(&row.prefix, None),
            }));
        } else {
            let app = row.prefix.rsplit('/').next().unwrap_or(&row.prefix).to_string();
            let group = row.prefix.split('/').next().filter(|first| *first != app).unwrap_or("").to_string();
            lines.push((group, AccessLine {
                label: row.prefix.clone(),
                level: level_word(row.scope),
                source: "app",
                link: grid_url(&row.prefix, Some(&app)),
            }));
        }
    }
    for (app, _) in grants {
        let path = app_path(config, app).await;
        if scopes.iter().any(|row| row.prefix == path) {
            continue;
        }
        let group = path.split('/').next().filter(|first| first != app).unwrap_or("").to_string();
        lines.push((group, AccessLine {
            label: path.clone(),
            level: level_word(Scope::Viewer),
            source: "access on the app",
            link: grid_url(&path, Some(app)),
        }));
    }
    lines.sort_by(|a, b| (&a.0, &a.1.label).cmp(&(&b.0, &b.1.label)));
    let mut groups: Vec<(String, Vec<AccessLine>)> = Vec::new();
    for (group, line) in lines {
        match groups.last_mut() {
            Some((last, rows)) if *last == group => rows.push(line),
            _ => groups.push((group, vec![line])),
        }
    }
    groups
}

pub async fn account_page(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Path(email): Path<String>,
) -> Response {
    render_account_page(config, headers, email, None).await
}

async fn render_account_page(
    config: Arc<Config>,
    headers: HeaderMap,
    email: String,
    fresh_link: Option<String>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let wanted = email.trim().to_lowercase();
    let (account, grants, scopes) = {
        let (config, wanted) = (config.clone(), wanted.clone());
        tokio::task::spawn_blocking(move || {
            let account = users::list_accounts(&config)
                .unwrap_or_default()
                .into_iter()
                .find(|account| account.email == wanted);
            let grants: Vec<(String, String)> = users::list_grants(&config)
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, who, _)| who == &wanted)
                .map(|(app, _, role)| (app, role))
                .collect();
            let scopes: Vec<users::ScopeGrant> = users::list_scopes(&config)
                .unwrap_or_default()
                .into_iter()
                .filter(|row| row.email == wanted)
                .collect();
            (account, grants, scopes)
        })
        .await
        .unwrap_or((None, Vec::new(), Vec::new()))
    };
    let Some(account) = account else {
        return (StatusCode::NOT_FOUND, "no such account").into_response();
    };
    let token = form_token(&config, &admin);
    let back = format!("/admin/accounts/{}", urlencoding::encode(&account.email));
    let is_self = account.email == admin.email;
    let access = access_summary(&config, &scopes, &grants).await;
    let title = account.email.clone();
    admin_page(
        &headers,
        &admin,
        Page {
            active: "accounts",
            title: &title,
            crumbs: vec![("Accounts", "/admin/accounts")],
            subtitle: Some(html! { "Created " (account.created) }),
            actions: Some(html! {
                @if account.is_admin { span."badge solid" { "admin" } } @else { span."badge" { "visitor" } }
                @if account.is_active { span."badge ok" { "active" } } @else { span."badge warn" { "disabled" } }
            }),
            script: None,
            body: html! {
                @if let Some(link) = &fresh_link {
                    (ui::panel("Setup link", Some("Send this link to the account owner. The link is valid for 48 hours and works one time. The owner sets a password with it."), html! {
                        (ui::secret("setup-link", link))
                    }))
                }
                (ui::panel("Has access to", Some("Every place this account holds a level of its own. Change it on that project or app's permissions."), html! {
                    @if account.is_admin {
                        p { "This account is a site admin and holds Manage everywhere." }
                    }
                    @if access.is_empty() {
                        @if !account.is_admin { p."muted" { "No access of its own. Add it from a project's Permissions tab or an app's Access tab." } }
                    } @else {
                        @for (group, rows) in &access {
                            h4."group" { @if group.is_empty() { "Top level" } @else { (group) } }
                            table {
                                thead { tr { th { "Where" } th { "Level" } th { "Given as" } } }
                                tbody {
                                    @for row in rows {
                                        tr {
                                            td { a."row-link" href=(row.link) { (row.label) } }
                                            td { span."badge solid" { (row.level) } }
                                            td."muted small" { (row.source) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }))
                (ui::panel("Account", None, html! {
                    div."actions" {
                        form method="post" action="/admin/reinvite" {
                            (hidden("token", &token)) (hidden("email", &account.email)) (hidden("back", &back))
                            button."quiet" type="submit" { "Create setup link" }
                        }
                        @if is_self {
                            span."muted small" { "You cannot disable your own account." }
                        } @else if account.is_active {
                            form method="post" action="/admin/active"
                                 data-confirm={ "Disable " (account.email) "?" }
                                 data-confirm-detail="The sessions of this account end now. A connected MCP client stops on the next call. You can enable the account again."
                                 data-confirm-label="Disable account" data-confirm-danger="1" {
                                (hidden("token", &token)) (hidden("email", &account.email)) (hidden("active", "0")) (hidden("back", &back))
                                button."danger quiet" type="submit" { "Disable account" }
                            }
                        } @else {
                            form method="post" action="/admin/active" {
                                (hidden("token", &token)) (hidden("email", &account.email)) (hidden("active", "1")) (hidden("back", &back))
                                button."quiet" type="submit" { "Enable account" }
                            }
                        }
                    }
                }))
            },
        },
    )
}

#[derive(Deserialize)]
pub struct Reinvite {
    token: String,
    email: String,
    back: Option<String>,
}

/// Mints a fresh setup link and shows it on the account page, once.
pub async fn reinvite(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<Reinvite>,
) -> Response {
    let admin = match checked(&config, &headers, &form.token).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), "/admin/accounts");
    let (config2, email) = (config.clone(), form.email.clone());
    let outcome = tokio::task::spawn_blocking(move || users::reinvite(&config2, &email)).await;
    match outcome {
        Ok(Ok(invite)) => {
            tracing::info!(admin = %admin.email, email = %form.email, "setup link reissued");
            let url = users::invite_url(&config, &invite);
            render_account_page(config, headers, form.email, Some(url)).await
        }
        Ok(Err(message)) => redirect_flash(&back, false, message),
        Err(_) => redirect_flash(&back, false, "The setup link was not created."),
    }
}

pub async fn new_account_page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let token = form_token(&config, &admin);
    admin_page(
        &headers,
        &admin,
        Page {
            active: "accounts",
            title: "New account",
            crumbs: vec![("Accounts", "/admin/accounts")],
            subtitle: None,
            actions: None,
            script: None,
            body: ui::panel("New account", Some("By default the account owner gets a setup link and chooses the password. Set a password here only for a shared login."), html! {
                form method="post" action="/admin/users" {
                    (hidden("token", &token)) (hidden("back", "/admin/accounts"))
                    div."field" {
                        label for="email" { "Email" }
                        input id="email" name="email" type="email" required autofocus;
                    }
                    div."choices" {
                        label."choice" {
                            input type="radio" name="mode" value="invite" checked;
                            strong { "Send a setup link" }
                            span { "You get a link to send. The owner opens it and chooses a password. The link works one time and for 48 hours." }
                        }
                        label."choice" {
                            input type="radio" name="mode" value="generate";
                            strong { "Generate a strong password" }
                            span { "For a login that a group shares. Toolsite makes a 20-character password and shows it one time." }
                        }
                        label."choice" {
                            input type="radio" name="mode" value="password";
                            strong { "Set a password now" }
                            span { "Enter the password yourself. You must give it to the people who use it." }
                        }
                    }
                    div."field" {
                        label for="password" { "Password" }
                        input id="password" name="password" type="password" minlength="8" autocomplete="new-password";
                        p."help" { "Only used with \"Set a password now\". Enter at least 8 characters." }
                    }
                    label."choice" {
                        input type="checkbox" name="admin" value="1";
                        strong { "Admin" }
                        span { "An admin can open this page, manage all apps, and connect an MCP client that publishes." }
                    }
                    div."actions end" {
                        a."btn quiet" href="/admin/accounts" { "Cancel" }
                        button type="submit" { "Create account" }
                    }
                }
            }),
        },
    )
}

// --- cross-cutting lists ---------------------------------------------------------

pub async fn exports_page(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let tokens = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || export::list_all(&config))
            .await
            .unwrap_or_default()
    };
    let count = tokens.len();
    let listing = paginate(tokens, &query, |(app, entry)| format!("{app} {} {}", entry.label, entry.id));
    admin_page(
        &headers,
        &admin,
        Page {
            active: "exports",
            title: "Exports",
            crumbs: vec![],
            subtitle: Some(html! { "An export token lets a reporting tool read the database of one app. Create tokens on the Exports tab of the app." }),
            actions: None,
            script: (count > 0).then_some(ui::FILTER_SCRIPT),
            body: html! {
                @if count == 0 {
                    (ui::panel("No export tokens", Some("There are no export tokens. Open an app and create one on the Exports tab."), html! {}))
                } @else {
                    (search_box(&listing, "/admin/exports", "Search tokens"))
                    section."panel" {
                        table {
                            thead { tr { th { "App" } th { "Label" } th { "Created" } th { "Last used" } } }
                            tbody id="list" {
                                @for (app, entry) in &listing.rows {
                                    tr data-slug=(app.to_lowercase()) data-title=(entry.label.to_lowercase()) {
                                        td { a."row-link" href={ "/admin/apps/" (app) "/exports" } { (app) } }
                                        td { (entry.label) " " span."muted small" { (entry.id) } }
                                        td."muted small" { (ago(entry.created_at)) }
                                        td."muted small" { @match entry.last_used { Some(at) => (ago(at)), None => "never" } }
                                    }
                                }
                            }
                        }
                    }
                    p."no-match" id="no-match" { "No token on this page matches. Press Enter to search all tokens." }
                    (pager(&listing, "/admin/exports"))
                }
            },
        },
    )
}

// --- actions ----------------------------------------------------------------------

/// Everything a POST needs before it may do anything.
pub(crate) async fn checked(config: &Arc<Config>, headers: &HeaderMap, token: &str) -> Result<User, Response> {
    let admin = require_admin(config, headers).await?;
    if !check_form_token(config, &admin, token) {
        return Err((StatusCode::FORBIDDEN, "The form is out of date. Reload the page and try again.").into_response());
    }
    Ok(admin)
}

/// The same, for an action on one app: the form token, and `needed` there.
pub(crate) async fn checked_app(
    config: &Arc<Config>,
    headers: &HeaderMap,
    token: &str,
    app: &str,
    needed: Scope,
) -> Result<User, Response> {
    let user = require_app(config, headers, app, needed).await?;
    if !check_form_token(config, &user, token) {
        return Err((StatusCode::FORBIDDEN, "The form is out of date. Reload the page and try again.").into_response());
    }
    Ok(user)
}

/// The same, for an action at a folder: the form token, and `needed` there.
pub(crate) async fn checked_at(
    config: &Arc<Config>,
    headers: &HeaderMap,
    token: &str,
    path: &str,
    needed: Scope,
) -> Result<User, Response> {
    let user = require_scope(config, headers, path, needed).await?;
    if !check_form_token(config, &user, token) {
        return Err((StatusCode::FORBIDDEN, "The form is out of date. Reload the page and try again.").into_response());
    }
    Ok(user)
}

#[derive(Deserialize)]
pub struct NewAccount {
    token: String,
    email: String,
    /// "invite" (the default) or "password". A password with no mode means
    /// "password", which is what the older form sent.
    mode: Option<String>,
    password: Option<String>,
    admin: Option<String>,
    back: Option<String>,
}

/// Twenty characters from letters and digits, which every password field
/// and every chat window accepts without mangling. About 119 bits.
fn strong_password() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..20)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

pub async fn add_account(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<NewAccount>,
) -> Response {
    let admin = match checked(&config, &headers, &form.token).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), "/admin/accounts");
    let is_admin = form.admin.is_some();
    let email = form.email.trim().to_lowercase();
    let password = form.password.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let mode = match form.mode.as_deref() {
        Some(mode) => mode.to_string(),
        None if password.is_some() => "password".to_string(),
        None => "invite".to_string(),
    };

    if mode == "password" || mode == "generate" {
        let generated = mode == "generate";
        let password = if generated {
            strong_password()
        } else {
            match password {
                Some(password) => password.to_string(),
                None => return redirect_flash("/admin/accounts/new", false, "Enter a password, or choose the setup link."),
            }
        };
        let config2 = config.clone();
        let (for_login, to_hash) = (email.clone(), password.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            users::sign_up_as(&config2, &for_login, &to_hash, is_admin)
        })
        .await;
        return match outcome {
            Ok(Ok(_)) if generated => {
                tracing::info!(admin = %admin.email, account = %email, "account created with a generated password");
                admin_page(
                    &headers,
                    &admin,
                    Page {
                        active: "accounts",
                        title: "Account created",
                        crumbs: vec![("Accounts", "/admin/accounts")],
                        subtitle: Some(html! { (email) }),
                        actions: None,
                        script: None,
                        body: html! {
                            (ui::panel("Password", Some("Give this password to the people who use the login. It is shown one time only."), html! {
                                (ui::secret("generated-password", &password))
                                p."muted small" { "If the password is lost, open the account and create a new setup link." }
                            }))
                            div."actions" {
                                a."btn" href={ "/admin/accounts/" (email) } { "Open account" }
                                a."btn quiet" href="/admin/accounts" { "Back to accounts" }
                            }
                        },
                    },
                )
            }
            Ok(Ok(_)) => redirect_flash(&back, true, format!("Account {email} is created.")),
            Ok(Err(message)) => redirect_flash("/admin/accounts/new", false, message),
            Err(_) => redirect_flash(&back, false, "The account was not created."),
        };
    }

    let config2 = config.clone();
    let for_invite = email.clone();
    let outcome =
        tokio::task::spawn_blocking(move || users::invite(&config2, &for_invite, is_admin)).await;
    match outcome {
        // Rendered, not redirected: the link is in this response and nowhere
        // else, like a token.
        Ok(Ok((_, setup_token))) => {
            let link = users::invite_url(&config, &setup_token);
            tracing::info!(admin = %admin.email, account = %email, "account created with a setup link");
            admin_page(
                &headers,
                &admin,
                Page {
                    active: "accounts",
                    title: "Account created",
                    crumbs: vec![("Accounts", "/admin/accounts")],
                    subtitle: Some(html! { (email) }),
                    actions: None,
                    script: None,
                    body: html! {
                        (ui::panel("Setup link", Some("Send this link to the account owner. The link works one time and for 48 hours. It is shown one time only."), html! {
                            (ui::secret("setup-link", &link))
                            p."muted small" { "If the link is lost, open the account and create a new setup link." }
                        }))
                        div."actions" {
                            a."btn" href={ "/admin/accounts/" (email) } { "Open account" }
                            a."btn quiet" href="/admin/accounts" { "Back to accounts" }
                        }
                    },
                },
            )
        }
        Ok(Err(message)) => redirect_flash("/admin/accounts/new", false, message),
        Err(_) => redirect_flash(&back, false, "The account was not created."),
    }
}

#[derive(Deserialize)]
pub struct ActiveChange {
    token: String,
    email: String,
    active: String,
    back: Option<String>,
}

pub async fn change_active(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<ActiveChange>,
) -> Response {
    let admin = match checked(&config, &headers, &form.token).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), "/admin/accounts");
    // Disabling yourself would lock the last admin out of this page.
    if form.email.trim().eq_ignore_ascii_case(&admin.email) && form.active != "1" {
        return redirect_flash(&back, false, "You cannot disable your own account.");
    }
    let active = form.active == "1";
    let config2 = config.clone();
    let email = form.email.clone();
    let outcome =
        tokio::task::spawn_blocking(move || users::set_active(&config2, &form.email, active)).await;
    match outcome {
        Ok(Ok(())) => redirect_flash(
            &back,
            true,
            if active { format!("Account {email} is enabled.") } else { format!("Account {email} is disabled.") },
        ),
        Ok(Err(message)) => redirect_flash(&back, false, message),
        Err(_) => redirect_flash(&back, false, "The account was not changed."),
    }
}

#[derive(Deserialize)]
pub struct AccessChange {
    token: String,
    app: String,
    email: String,
    allow: String,
    role: Option<String>,
    back: Option<String>,
}

pub async fn change_access(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<AccessChange>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if let Err(response) = checked_app(&config, &headers, &form.token, &form.app, Scope::Admin).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/accounts");
    let allow = form.allow == "1";
    let role = form
        .role
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty())
        .unwrap_or("viewer")
        .to_string();
    if role.len() > 40 || !role.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        return redirect_flash(&back, false, "Enter the role as one word. Use letters, digits, - or _.");
    }
    let config2 = config.clone();
    let (email, app) = (form.email.clone(), form.app.clone());
    let path = app_path(&config, &app).await;
    let outcome = tokio::task::spawn_blocking(move || {
        if allow {
            users::grant(&config2, &form.email, &form.app, &role)
        } else {
            users::revoke(&config2, &form.email, &form.app)
        }
    })
    .await;
    // Access given on the app is a View row on it too, so the grid shows
    // it; taking access away takes that row with it.
    if matches!(outcome, Ok(Ok(()))) {
        if allow {
            crate::platform::permissions::give_view_if_absent(&config, &email, &path).await;
        } else {
            let (config3, who, at) = (config.clone(), email.clone(), path.clone());
            let _ = tokio::task::spawn_blocking(move || users::revoke_scope(&config3, &who, &at)).await;
        }
    }
    match outcome {
        Ok(Ok(())) => redirect_flash(
            &back,
            true,
            if allow { format!("Account {email} has a grant on {app}.") } else { format!("The grant of {email} on {app} is revoked.") },
        ),
        Ok(Err(message)) => redirect_flash(&back, false, message),
        Err(_) => redirect_flash(&back, false, "Access was not changed."),
    }
}

#[derive(Deserialize)]
pub struct GateChange {
    token: String,
    app: String,
    gate: String,
    back: Option<String>,
}

pub async fn change_gate(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<GateChange>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if let Err(response) = checked_app(&config, &headers, &form.token, &form.app, Scope::Admin).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    let level = if form.gate == "default" {
        None
    } else {
        match crate::content::store::normalise_gate(&form.gate) {
            Some(level) => Some(level.to_string()),
            None => return (StatusCode::BAD_REQUEST, "unknown access level").into_response(),
        }
    };
    let mut meta = read_meta(&config, &form.app).await;
    meta.gate = level;
    let said = match meta.gate.as_deref() {
        Some(gate) => format!("Access for {} is {}.", form.app, gate_label(gate)),
        None => format!("Access for {} is the site default, {}.", form.app, gate_label(&config.default_gate)),
    };
    match write_meta(&config, &form.app, &meta).await {
        Ok(()) => redirect_flash(&back, true, said),
        Err(_) => redirect_flash(&back, false, "Access was not saved."),
    }
}

#[derive(Deserialize)]
pub struct RuleChange {
    token: String,
    app: String,
    action: String,
    prefix: String,
    gate: Option<String>,
    back: Option<String>,
}

pub async fn change_rule(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<RuleChange>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if let Err(response) = checked_app(&config, &headers, &form.token, &form.app, Scope::Admin).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    let prefix = form.prefix.trim().to_string();
    if !prefix.starts_with('/') || prefix.contains("..") {
        return redirect_flash(&back, false, "Enter the prefix as a path in the app, for example /admin.");
    }
    let mut meta = read_meta(&config, &form.app).await;
    meta.rules.retain(|rule| rule.prefix != prefix);
    let text = match form.action.as_str() {
        "add" => {
            let Some(gate) = crate::content::store::normalise_gate(&form.gate.unwrap_or_default()).map(str::to_string) else {
                return (StatusCode::BAD_REQUEST, "unknown access level").into_response();
            };
            meta.rules.push(PathRule {
                prefix: prefix.clone(),
                gate: gate.clone(),
            });
            format!("Access for {prefix} is {}.", gate_label(&gate))
        }
        "remove" => format!("The rule for {prefix} is removed."),
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    match write_meta(&config, &form.app, &meta).await {
        Ok(()) => redirect_flash(&back, true, text),
        Err(_) => redirect_flash(&back, false, "The rule was not saved."),
    }
}

#[derive(Deserialize)]
pub struct VisibilityChange {
    token: String,
    app: String,
    listed: Option<String>,
    hidden: Option<String>,
    back: Option<String>,
}

pub async fn change_visibility(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<VisibilityChange>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if let Err(response) = checked_app(&config, &headers, &form.token, &form.app, Scope::Editor).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    let mut meta = read_meta(&config, &form.app).await;
    meta.listed = form.listed.is_some();
    meta.hidden = form.hidden.is_some();
    match write_meta(&config, &form.app, &meta).await {
        Ok(()) => redirect_flash(&back, true, "Visibility is saved."),
        Err(_) => redirect_flash(&back, false, "Visibility was not saved."),
    }
}

#[derive(Deserialize)]
pub struct NotesChange {
    token: String,
    app: String,
    notes: String,
    back: Option<String>,
}

pub async fn change_notes(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<NotesChange>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if let Err(response) = checked_app(&config, &headers, &form.token, &form.app, Scope::Editor).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    match crate::content::store::write_notes(&config, &form.app, &form.notes).await {
        Ok(()) => redirect_flash(&back, true, "Notes are saved."),
        Err(_) => redirect_flash(&back, false, "Notes were not saved."),
    }
}

#[derive(Deserialize)]
pub struct AppOnly {
    token: String,
    app: String,
    back: Option<String>,
}

/// Mints an entry link and shows it on the Settings tab, once.
pub async fn settings_link(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<AppOnly>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if let Err(response) = checked_app(&config, &headers, &form.token, &form.app, Scope::Editor).await {
        return response;
    }
    match crate::platform::secrets::create_entry(&config, &form.app) {
        Ok(url) => app_tab(config, headers, form.app, "settings".into(), Some(Fresh::SettingsLink(url))).await,
        Err(message) => redirect_flash(&back_or(form.back.as_deref(), "/admin/apps"), false, message),
    }
}

#[derive(Deserialize)]
pub struct JobRun {
    token: String,
    app: String,
    name: String,
    back: Option<String>,
}

pub async fn run_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<JobRun>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if let Err(response) = checked_app(&state.config, &headers, &form.token, &form.app, Scope::Editor).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    match crate::platform::schedule::run_job(&state, &form.app, &form.name).await {
        Ok(status) => redirect_flash(&back, true, format!("Job {} ran. Status: {status}", form.name)),
        Err(message) => redirect_flash(&back, false, format!("Job {} failed. {message}", form.name)),
    }
}

#[derive(Deserialize)]
pub struct ExportChange {
    token: String,
    action: String,
    app: String,
    label: Option<String>,
    id: Option<String>,
    back: Option<String>,
}

pub async fn change_export(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<ExportChange>,
) -> Response {
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    let admin = match checked_app(&config, &headers, &form.token, &form.app, Scope::Admin).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), &format!("/admin/apps/{}/exports", form.app));
    match form.action.as_str() {
        "create" => {
            let label = form.label.unwrap_or_default();
            let (config2, app) = (config.clone(), form.app.clone());
            let outcome = tokio::task::spawn_blocking(move || export::create(&config2, &app, &label)).await;
            match outcome {
                Ok(Ok((_, token))) => {
                    tracing::info!(admin = %admin.email, app = %form.app, "export token created");
                    // Rendered, not redirected: the token exists in this
                    // response and nowhere else.
                    app_tab(config, headers, form.app, "exports".into(), Some(Fresh::ExportToken(token))).await
                }
                Ok(Err(message)) => redirect_flash(&back, false, message),
                Err(_) => redirect_flash(&back, false, "The token was not created."),
            }
        }
        "revoke" => {
            let id = form.id.unwrap_or_default();
            let (config2, app) = (config.clone(), form.app.clone());
            let outcome = tokio::task::spawn_blocking(move || export::revoke(&config2, &app, &id)).await;
            match outcome {
                Ok(Ok(())) => {
                    tracing::info!(admin = %admin.email, app = %form.app, "export token revoked");
                    redirect_flash(&back, true, "The token is revoked.")
                }
                Ok(Err(message)) => redirect_flash(&back, false, message),
                Err(_) => redirect_flash(&back, false, "The token was not revoked."),
            }
        }
        _ => (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    }
}

// --- projects: folders, scopes, moves -------------------------------------------

#[derive(Deserialize)]
pub struct ScopeChange {
    token: String,
    action: String,
    prefix: String,
    email: String,
    scope: Option<String>,
    back: Option<String>,
}

/// Gives or takes a scope at a folder or an app path. The caller needs admin
/// there, may give at most what it holds there, and never grants above it,
/// because the prefix in the form is the one the page was for.
pub async fn change_scope(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<ScopeChange>,
) -> Response {
    let prefix = form.prefix.trim_matches('/').to_string();
    if !users::valid_prefix(&prefix) {
        return (StatusCode::BAD_REQUEST, "invalid folder").into_response();
    }
    let admin = match checked_at(&config, &headers, &form.token, &prefix, Scope::Admin).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), "/admin/apps");
    let email = form.email.trim().to_lowercase();
    let where_ = projects::place(&prefix);
    let outcome = match form.action.as_str() {
        "grant" => {
            let Some(scope) = form.scope.as_deref().and_then(Scope::parse) else {
                return redirect_flash(&back, false, "Choose viewer, editor or admin.");
            };
            projects::grant(&config, Some(&admin), &prefix, &email, scope)
                .await
                .map(|()| format!("{email} is {scope} at {where_}."))
        }
        "revoke" => projects::revoke(&config, Some(&admin), &prefix, &email)
            .await
            .map(|()| format!("{email} has no access of its own at {where_} now.")),
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    match outcome {
        Ok(text) => redirect_flash(&back, true, text),
        Err(problem) => redirect_flash(&back, false, problem.message()),
    }
}

#[derive(Deserialize)]
pub struct NewFolder {
    token: String,
    parent: String,
    name: String,
    back: Option<String>,
}

pub async fn new_folder(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<NewFolder>,
) -> Response {
    let parent = form.parent.trim_matches('/').to_string();
    if !users::valid_prefix(&parent) {
        return (StatusCode::BAD_REQUEST, "invalid folder").into_response();
    }
    let admin = match checked_at(&config, &headers, &form.token, &parent, Scope::Admin).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), "/admin/apps");
    match projects::create(&config, Some(&admin), &parent, &form.name).await {
        Ok(folder) => {
            // Back where the form was, inside the new project: the browser
            // when it came from there, the admin list otherwise.
            let to = if back.starts_with("/admin") {
                format!("/admin/apps?folder={}", urlencoding::encode(&folder.path))
            } else {
                crate::content::browse::browser_url(&folder.path)
            };
            redirect_flash(&to, true, format!("Project {} is created.", folder.path))
        }
        Err(problem) => redirect_flash(&back, false, problem.message()),
    }
}

#[derive(Deserialize)]
pub struct ProjectChange {
    token: String,
    /// rename, move or remove.
    action: String,
    path: String,
    name: Option<String>,
    parent: Option<String>,
    back: Option<String>,
}

/// Renames, moves or removes a project. The door is admin at the project;
/// the projects module then asks for what each change needs (the parent for
/// a rename or removal, all three places for a move).
pub async fn change_project(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<ProjectChange>,
) -> Response {
    let path = form.path.trim_matches('/').to_string();
    if path.is_empty() || !users::valid_prefix(&path) {
        return (StatusCode::BAD_REQUEST, "invalid project").into_response();
    }
    let admin = match checked_at(&config, &headers, &form.token, &path, Scope::Admin).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), &crate::content::browse::browser_url(&path));
    let outcome = match form.action.as_str() {
        "rename" => projects::rename(&config, Some(&admin), &path, form.name.as_deref().unwrap_or(""))
            .await
            .map(|to| (crate::content::browse::browser_url(&to), format!("The project is now {to}."))),
        "move" => projects::move_project(&config, Some(&admin), &path, form.parent.as_deref().unwrap_or("").trim_matches('/'))
            .await
            .map(|to| (crate::content::browse::browser_url(&to), format!("The project is now at {to}."))),
        "remove" => {
            let parent = path.rsplit_once('/').map(|(above, _)| above.to_string()).unwrap_or_default();
            projects::remove(&config, Some(&admin), &path)
                .await
                .map(|()| (crate::content::browse::browser_url(&parent), format!("Project {path} is removed.")))
        }
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    match outcome {
        Ok((to, message)) => redirect_flash(&to, true, message),
        Err(projects::Problem::Refused(message)) => (StatusCode::FORBIDDEN, message).into_response(),
        Err(problem) => redirect_flash(&back, false, problem.message()),
    }
}

#[derive(Deserialize)]
pub struct MoveApp {
    token: String,
    app: String,
    folder: String,
    back: Option<String>,
}

/// Moves an app to another folder. Admin at both ends, since access from
/// the old folders stops and access from the new ones starts.
pub async fn move_app(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<MoveApp>,
) -> Response {
    if !export::valid_app(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    let target = form.folder.trim_matches('/').to_string();
    if !users::valid_prefix(&target) {
        return (StatusCode::BAD_REQUEST, "invalid folder").into_response();
    }
    let admin = match checked_app(&config, &headers, &form.token, &form.app, Scope::Admin).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let back = back_or(form.back.as_deref(), &format!("/admin/apps/{}", form.app));
    let from = app_path(&config, &form.app).await;
    match projects::move_app(&config, Some(&admin), &form.app, &target).await {
        Ok(to) if to == from => redirect_flash(&back, true, "The app is already there."),
        Ok(to) => redirect_flash(&back, true, format!("{} is now at {to}.", form.app)),
        Err(projects::Problem::Refused(message)) => (StatusCode::FORBIDDEN, message).into_response(),
        Err(problem) => redirect_flash(&back, false, problem.message()),
    }
}
