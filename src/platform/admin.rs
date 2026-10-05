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
    accounts::users::{self, User},
    config::Config,
    content::{
        slug::valid_slug,
        store::{collect_slugs, read_meta, write_meta, PathRule},
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
    ("granted", "Granted", "Only accounts given access on the Access tab."),
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
        Some(path) if path.starts_with("/admin") && !path.contains("//") => path.to_string(),
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

fn take_flash(headers: &HeaderMap) -> Option<Flash> {
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

fn clear_flash() -> (header::HeaderName, String) {
    (
        header::SET_COOKIE,
        format!("{FLASH_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    )
}

// --- the shell ----------------------------------------------------------------

/// The rail every signed-in page shares, including the index. `active` names
/// the current section so its link is marked rather than followed.
pub(crate) fn sidebar(active: &str, viewer: Option<&User>) -> Markup {
    let is_admin = viewer.is_some_and(|user| user.is_admin);
    html! {
        a."brand" href="/" { span."mark" { "t" } "toolsite" }
        div."nav-group" {
            div."label" { "Site" }
            a."active"[active == "site"] href="/" { "Apps" }
        }
        @if is_admin {
            div."nav-group" {
                div."label" { "Admin" }
                a."active"[active == "apps"] href="/admin/apps" { "Apps" }
                a."active"[active == "accounts"] href="/admin/accounts" { "Accounts" }
                a."active"[active == "exports"] href="/admin/exports" { "Exports" }
                a."active"[active == "github"] href="/admin/github" { "GitHub" }
            }
        }
        div."spacer" {
            @match viewer {
                Some(user) => {
                    div."who" { (user.email) }
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
        sidebar(page.active, Some(admin)),
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
                @if listing.total == 0 { "Nothing matches" }
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

fn matches<T>(all: Vec<T>, q: &str, to_match: impl Fn(&T) -> Match) -> Vec<Match> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    all.into_iter()
        .map(|row| to_match(&row))
        .filter(|m| m.value.to_lowercase().contains(&q) || m.label.to_lowercase().contains(&q))
        .take(MAX_MATCHES)
        .collect()
}

/// `GET /admin/accounts/search?q=`: at most ten emails, for a picker. An
/// empty query finds nothing, so the page never carries every account.
pub async fn search_accounts(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    if let Err(response) = require_admin(&config, &headers).await {
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
    if let Err(response) = require_admin(&config, &headers).await {
        return response;
    }
    let q = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    if q.is_empty() {
        return ([no_store()], Json(Vec::<Match>::new())).into_response();
    }
    let mut found = Vec::new();
    for app in app_names(&config).await {
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
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let mut rows = Vec::new();
    for app in app_names(&config).await {
        rows.push(app_row(&config, &app).await);
    }
    let count = rows.len();
    let listing = paginate(rows, &query, |row| {
        format!("{} {}", row.app, row.title.as_deref().unwrap_or(""))
    });
    admin_page(
        &headers,
        &admin,
        Page {
            active: "apps",
            title: "Apps",
            crumbs: vec![],
            subtitle: Some(html! { (count) " published" }),
            actions: None,
            script: (count > 0).then_some(ui::FILTER_SCRIPT),
            body: html! {
                @if count == 0 {
                    (ui::panel("Nothing published yet", Some("An agent publishes with create_upload; apps appear here as they land."), html! {}))
                } @else {
                    (search_box(&listing, "/admin/apps", "Find an app…"))
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
                    p."no-match" id="no-match" { "No app on this page matches that. Press Enter to search them all." }
                    (pager(&listing, "/admin/apps"))
                }
            },
        },
    )
}

fn gate_badge(gate: &str) -> Markup {
    html! {
        @match gate {
            "public" => span."badge" { "public" },
            "authenticated" => span."badge solid" { "signed in" },
            _ => span."badge solid" { "granted" },
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
) -> Response {
    app_tab(config, headers, app, tab, None).await
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
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    if !export::valid_app(&app) || !TABS.iter().any(|(key, _)| *key == tab) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let exists = app_names(&config).await.iter().any(|name| name == &app);
    if !exists {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    }

    let token = form_token(&config, &admin);
    let meta = read_meta(&config, &app).await;
    let hrefs: Vec<(String, String)> = TABS
        .iter()
        .map(|(key, _)| (key.to_string(), tab_href(&app, key)))
        .collect();
    let tab_items: Vec<(&str, &str, &str)> = TABS
        .iter()
        .zip(hrefs.iter())
        .map(|((key, label), (_, href))| (*key, *label, href.as_str()))
        .collect();
    let back = tab_href(&app, &tab);

    let body = match tab.as_str() {
        "overview" => render_overview(&config, &app, &meta, &token, &back).await,
        "access" => render_access_tab(&config, &app, &meta, &token, &back).await,
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
            crumbs: vec![("Apps", "/admin/apps")],
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
) -> Markup {
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
    html! {
        div."grid-2" {
            (ui::panel("About", None, html! {
                dl."kv" {
                    dt { "Title" } dd { (title.as_deref().unwrap_or("—")) }
                    dt { "Updated" } dd { @match modified { Some(m) => (crate::content::store::relative_time(m)), None => "—" } }
                    dt { "Handler" } dd { @if has_handler { "wasm component" } @else { "none, static files only" } }
                    dt { "Database" } dd { @match db_bytes { Some(b) => (human_bytes(b)), None => "not created yet" } }
                    dt { "Routing" } dd { @if meta.spa { "client-side (spa)" } @else { "files and handler" } }
                    dt { "Outbound" }
                    dd {
                        @if meta.allow_http.is_empty() { "no hosts allowed" }
                        @else { @for (i, host) in meta.allow_http.iter().enumerate() { @if i > 0 { ", " } code { (host) } } }
                    }
                }
            }))
            (ui::panel("Visibility", Some("Nothing here deletes anything. Both are reversible from this page."), html! {
                form."column" method="post" action="/admin/visibility" {
                    (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                    label."choice" {
                        input type="checkbox" name="listed" value="1" checked[meta.listed];
                        strong { "Listed on the index" }
                        span { "Off keeps the URL working but drops it from the front page." }
                    }
                    label."choice" {
                        input type="checkbox" name="hidden" value="1" checked[meta.hidden];
                        strong { "Taken down" }
                        span { "The URL answers 404 and the app leaves the index. Files stay where they are." }
                    }
                    div."actions end" { button type="submit" { "Save" } }
                }
            }))
        }
    }
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
    config: &Config,
    app: &str,
    meta: &crate::content::store::PageMeta,
    token: &str,
    back: &str,
) -> Markup {
    let grants: Vec<(String, String)> = {
        let config = config.clone_for_task();
        let app = app.to_string();
        tokio::task::spawn_blocking(move || users::list_grants(&config))
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default()
            .into_iter()
            .filter(|(granted_app, _, _)| granted_app == &app)
            .map(|(_, email, role)| (email, role))
            .collect()
    };
    html! {
        (ui::panel("Who may open it", Some("Access for the whole app. Route rules below make exceptions by path."), html! {
            form method="post" action="/admin/gate" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                div."choices" {
                    label."choice" {
                        input type="radio" name="gate" value="default" checked[meta.gate.is_none()];
                        strong { "Site default" }
                        span {
                            "Currently " (config.default_gate) ". Set once for the whole site with "
                            code { "TOOLSITE_DEFAULT_ACCESS" } "; apps that have not chosen follow it."
                        }
                    }
                    @for (value, label, help) in GATES {
                        label."choice" {
                            input type="radio" name="gate" value=(value) checked[meta.gate.as_deref() == Some(value)];
                            strong { (label) }
                            span { (help) }
                        }
                    }
                }
                div."actions end" { button type="submit" { "Save access" } }
            }
        }))

        (ui::panel("Route rules", Some("A path prefix with its own access rule. Longest match wins, so a public app can have a private corner."), html! {
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
                                        button."danger quiet sm" type="submit" { "Remove" }
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
        }))

        (ui::panel("Granted accounts", Some("A grant only matters while access, or a rule, says granted. The role is the app's to interpret."), html! {
            form."row" method="post" action="/admin/access" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back)) (hidden("allow", "1"))
                label."small" for="email" { "Add a person" }
                (ui::combobox("email", "/admin/accounts/search", "Start typing an email…"))
                input name="role" value="viewer" placeholder="role" size="8" title="A word the app reads with identity::current-role";
                button type="submit" { "Add" }
            }
            @if grants.is_empty() {
                p."muted" { "Nobody has been granted access yet." }
            } @else {
                table {
                    thead { tr { th { "Account" } th { "Role" } th {} } }
                    tbody {
                        @for (email, role) in &grants {
                            tr {
                                td { a."row-link" href={ "/admin/accounts/" (urlencoding::encode(email)) } { (email) } }
                                td { span."badge" { (role) } }
                                td."actions-cell" {
                                    form method="post" action="/admin/access"
                                         data-confirm={ "Revoke " (email) "?" }
                                         data-confirm-detail="They keep their account and lose this app."
                                         data-confirm-label="Revoke" data-confirm-danger="1" {
                                        (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                                        (hidden("email", email)) (hidden("allow", "0"))
                                        button."danger quiet sm" type="submit" { "Revoke" }
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
            (ui::panel("Your new token", Some("Copy it now. It is not stored and will not be shown again."), html! {
                (ui::secret("fresh-token", fresh))
                p."muted small" {
                    "Point the tool at " code { (url) } " with " code { "Authorization: Bearer <token>" } "."
                }
            }))
        }
        (ui::panel("Export tokens", Some("A read-only snapshot of this app's database for a reporting tool. Each token opens this app and nothing else."), html! {
            @if tokens.is_empty() {
                p."muted" { "No tokens yet." }
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
                                         data-confirm-detail="Whatever holds it gets 401 on its next pull."
                                         data-confirm-label="Revoke" data-confirm-danger="1" {
                                        (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                                        (hidden("action", "revoke")) (hidden("id", &entry.id))
                                        button."danger quiet sm" type="submit" { "Revoke" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            form."row" method="post" action="/admin/exports" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back)) (hidden("action", "create"))
                input name="label" placeholder="What will hold it, e.g. reporting" required;
                button."quiet" type="submit" { "Create token" }
            }
            p."muted small" { "URL: " code { (url) } }
        }))
    }
}

fn render_settings_tab(app: &str, names: &[String], token: &str, back: &str, link: Option<&str>) -> Markup {
    html! {
        @if let Some(link) = link {
            (ui::panel("Entry link", Some("Send this to whoever holds the values. It lasts an hour and takes one NAME=value per line."), html! {
                (ui::secret("settings-link", link))
            }))
        }
        (ui::panel("Settings", Some("Values the handler reads with secrets.get. Sealed at rest; names only here, never values."), html! {
            @if names.is_empty() {
                p."muted" { "Nothing set." }
            } @else {
                ul."stack" { @for name in names { li { code { (name) } } } }
            }
            form."row" method="post" action="/admin/settings-link" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                button."quiet" type="submit" { "Get a link to enter values" }
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
        (ui::panel("Scheduled jobs", Some("Declared in the app's toolsite.toml. Each run goes through the handler like a request."), html! {
            @if jobs.is_empty() {
                p."muted" { "No jobs." }
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
                                        button."quiet sm" type="submit" { "Run now" }
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
        (ui::panel("Notes", Some("What the last session left for the next one. A bundle cannot be turned back into its source, so this may be the only record."), html! {
            form."column" method="post" action="/admin/notes" {
                (hidden("token", token)) (hidden("app", app)) (hidden("back", back))
                textarea name="notes" rows="14" placeholder="Nothing written yet." { (notes.unwrap_or("")) }
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
                    (ui::panel("No accounts yet", Some("Create the first one, or run `toolsite user add` on the machine."), html! {}))
                } @else {
                    (search_box(&listing, "/admin/accounts", "Find an account…"))
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
                                                     data-confirm-detail="Their sessions end now and any connected MCP client stops on its next call. Re-enable any time."
                                                     data-confirm-label="Disable" data-confirm-danger="1" {
                                                    (hidden("token", &token)) (hidden("email", &account.email)) (hidden("active", "0")) (hidden("back", "/admin/accounts"))
                                                    button."danger quiet sm" type="submit" { "Disable" }
                                                }
                                            } @else {
                                                form method="post" action="/admin/active" {
                                                    (hidden("token", &token)) (hidden("email", &account.email)) (hidden("active", "1")) (hidden("back", "/admin/accounts"))
                                                    button."quiet sm" type="submit" { "Enable" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    p."no-match" id="no-match" { "No account on this page matches that. Press Enter to search them all." }
                    (pager(&listing, "/admin/accounts"))
                }
            },
        },
    )
}

/// One account: who they are, what they may open, and the two things an
/// admin does for them — a fresh setup link, and switching them off.
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
    let (account, grants) = {
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
            (account, grants)
        })
        .await
        .unwrap_or((None, Vec::new()))
    };
    let Some(account) = account else {
        return (StatusCode::NOT_FOUND, "no such account").into_response();
    };
    let token = form_token(&config, &admin);
    let back = format!("/admin/accounts/{}", urlencoding::encode(&account.email));
    let is_self = account.email == admin.email;
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
                    (ui::panel("Setup link", Some("Send it to them. It lasts 48 hours, works once, and lets them choose a password."), html! {
                        (ui::secret("setup-link", link))
                    }))
                }
                (ui::panel("Apps they may open", Some("Only matters for apps whose access is granted. The role is a word the app reads; viewer is the usual one."), html! {
                    form."row" method="post" action="/admin/access" {
                        (hidden("token", &token)) (hidden("email", &account.email)) (hidden("back", &back)) (hidden("allow", "1"))
                        label."small" for="app" { "Add to an app" }
                        (ui::combobox("app", "/admin/apps/search", "Start typing an app…"))
                        input name="role" value="viewer" placeholder="role" size="8";
                        button type="submit" { "Add" }
                    }
                    @if grants.is_empty() {
                        p."muted" { "No grants." }
                    } @else {
                        table {
                            thead { tr { th { "App" } th { "Role" } th {} } }
                            tbody {
                                @for (app, role) in &grants {
                                    tr {
                                        td { a."row-link" href={ "/admin/apps/" (app) "/access" } { (app) } }
                                        td { span."badge" { (role) } }
                                        td."actions-cell" {
                                            form method="post" action="/admin/access"
                                                 data-confirm={ "Revoke " (app) "?" }
                                                 data-confirm-detail="They keep their account and lose this app."
                                                 data-confirm-label="Revoke" data-confirm-danger="1" {
                                                (hidden("token", &token)) (hidden("app", app)) (hidden("email", &account.email))
                                                (hidden("allow", "0")) (hidden("back", &back))
                                                button."danger quiet sm" type="submit" { "Revoke" }
                                            }
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
                            button."quiet" type="submit" { "New setup link" }
                        }
                        @if is_self {
                            span."muted small" { "You cannot disable your own account." }
                        } @else if account.is_active {
                            form method="post" action="/admin/active"
                                 data-confirm={ "Disable " (account.email) "?" }
                                 data-confirm-detail="Their sessions end now and any connected MCP client stops on its next call. Re-enable any time."
                                 data-confirm-label="Disable" data-confirm-danger="1" {
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
        Err(_) => redirect_flash(&back, false, "Could not make a setup link."),
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
            body: ui::panel("Account", Some("A password set here is one you have to pass on. Prefer an invite: `toolsite user add` prints a one-time link."), html! {
                form method="post" action="/admin/users" {
                    (hidden("token", &token)) (hidden("back", "/admin/accounts"))
                    div."field" {
                        label for="email" { "Email" }
                        input id="email" name="email" type="email" required autofocus;
                    }
                    div."field" {
                        label for="password" { "Password" }
                        input id="password" name="password" type="password" minlength="8" required;
                        p."help" { "At least 8 characters. They can change it from a setup link later." }
                    }
                    label."choice" {
                        input type="checkbox" name="admin" value="1";
                        strong { "Admin" }
                        span { "Sees this page, manages every app, and may connect an MCP client that publishes." }
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
            subtitle: Some(html! { "Tokens that let a reporting tool pull one app's database. Mint them on the app's Exports tab." }),
            actions: None,
            script: (count > 0).then_some(ui::FILTER_SCRIPT),
            body: html! {
                @if count == 0 {
                    (ui::panel("No export tokens", Some("Open an app and use its Exports tab to create one."), html! {}))
                } @else {
                    (search_box(&listing, "/admin/exports", "Find a token by app or label…"))
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
                    p."no-match" id="no-match" { "No token on this page matches that. Press Enter to search them all." }
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
        return Err((StatusCode::FORBIDDEN, "stale form; reload and try again").into_response());
    }
    Ok(admin)
}

#[derive(Deserialize)]
pub struct NewAccount {
    token: String,
    email: String,
    password: String,
    admin: Option<String>,
    back: Option<String>,
}

pub async fn add_account(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<NewAccount>,
) -> Response {
    if let Err(response) = checked(&config, &headers, &form.token).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/accounts");
    let is_admin = form.admin.is_some();
    let config2 = config.clone();
    let email = form.email.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        users::sign_up_as(&config2, &form.email, &form.password, is_admin)
    })
    .await;
    match outcome {
        Ok(Ok(_)) => redirect_flash(&back, true, format!("Created {email}.")),
        Ok(Err(message)) => redirect_flash("/admin/accounts/new", false, message),
        Err(_) => redirect_flash(&back, false, "Could not add the account."),
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
            if active { format!("{email} can sign in again.") } else { format!("{email} is disabled.") },
        ),
        Ok(Err(message)) => redirect_flash(&back, false, message),
        Err(_) => redirect_flash(&back, false, "Could not change the account."),
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
    if let Err(response) = checked(&config, &headers, &form.token).await {
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
        return redirect_flash(&back, false, "A role is one word: letters, digits, '-' or '_'.");
    }
    let config2 = config.clone();
    let (email, app) = (form.email.clone(), form.app.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        if allow {
            users::grant(&config2, &form.email, &form.app, &role)
        } else {
            users::revoke(&config2, &form.email, &form.app)
        }
    })
    .await;
    match outcome {
        Ok(Ok(())) => redirect_flash(
            &back,
            true,
            if allow { format!("{email} may open {app}.") } else { format!("{email} no longer has {app}.") },
        ),
        Ok(Err(message)) => redirect_flash(&back, false, message),
        Err(_) => redirect_flash(&back, false, "Could not change access."),
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
    if let Err(response) = checked(&config, &headers, &form.token).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if form.gate != "default" && !GATES.iter().any(|(value, ..)| *value == form.gate) {
        return (StatusCode::BAD_REQUEST, "unknown gate").into_response();
    }
    let mut meta = read_meta(&config, &form.app).await;
    meta.gate = (form.gate != "default").then(|| form.gate.clone());
    let said = match meta.gate.as_deref() {
        Some(gate) => format!("{} is now {gate}.", form.app),
        None => format!("{} follows the site default, {}.", form.app, config.default_gate),
    };
    match write_meta(&config, &form.app, &meta).await {
        Ok(()) => redirect_flash(&back, true, said),
        Err(_) => redirect_flash(&back, false, "Could not save access."),
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
    if let Err(response) = checked(&config, &headers, &form.token).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    let prefix = form.prefix.trim().to_string();
    if !prefix.starts_with('/') || prefix.contains("..") {
        return redirect_flash(&back, false, "A rule's prefix is a path within the app, like /admin.");
    }
    let mut meta = read_meta(&config, &form.app).await;
    meta.rules.retain(|rule| rule.prefix != prefix);
    let text = match form.action.as_str() {
        "add" => {
            let gate = form.gate.unwrap_or_default();
            if !GATES.iter().any(|(value, ..)| *value == gate) {
                return (StatusCode::BAD_REQUEST, "unknown gate").into_response();
            }
            meta.rules.push(PathRule {
                prefix: prefix.clone(),
                gate: gate.clone(),
            });
            format!("{prefix} is {gate}.")
        }
        "remove" => format!("Removed the rule for {prefix}."),
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    match write_meta(&config, &form.app, &meta).await {
        Ok(()) => redirect_flash(&back, true, text),
        Err(_) => redirect_flash(&back, false, "Could not save the rule."),
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
    if let Err(response) = checked(&config, &headers, &form.token).await {
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
        Ok(()) => redirect_flash(&back, true, "Visibility saved."),
        Err(_) => redirect_flash(&back, false, "Could not save visibility."),
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
    if let Err(response) = checked(&config, &headers, &form.token).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    match crate::content::store::write_notes(&config, &form.app, &form.notes).await {
        Ok(()) => redirect_flash(&back, true, "Notes saved."),
        Err(_) => redirect_flash(&back, false, "Could not save the notes."),
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
    if let Err(response) = checked(&config, &headers, &form.token).await {
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
    if let Err(response) = checked(&state.config, &headers, &form.token).await {
        return response;
    }
    let back = back_or(form.back.as_deref(), "/admin/apps");
    match crate::platform::schedule::run_job(&state, &form.app, &form.name).await {
        Ok(status) => redirect_flash(&back, true, format!("{} ran: {status}", form.name)),
        Err(message) => redirect_flash(&back, false, format!("{} failed: {message}", form.name)),
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
    let admin = match checked(&config, &headers, &form.token).await {
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
                Err(_) => redirect_flash(&back, false, "Could not create the token."),
            }
        }
        "revoke" => {
            let id = form.id.unwrap_or_default();
            let (config2, app) = (config.clone(), form.app.clone());
            let outcome = tokio::task::spawn_blocking(move || export::revoke(&config2, &app, &id)).await;
            match outcome {
                Ok(Ok(())) => {
                    tracing::info!(admin = %admin.email, app = %form.app, "export token revoked");
                    redirect_flash(&back, true, "Token revoked.")
                }
                Ok(Err(message)) => redirect_flash(&back, false, message),
                Err(_) => redirect_flash(&back, false, "Could not revoke the token."),
            }
        }
        _ => (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    }
}
