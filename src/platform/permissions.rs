//! The permissions grid: who may do what at a project or an app, set by
//! ticking shaded cells, Tableau style.
//!
//! One component serves both places a person sets access: a project's
//! Permissions tab in the app browser and an app's Access tab. Each row is a
//! person; the columns are View, Edit and Manage, which are cumulative, as
//! the scopes behind them are (viewer, editor, admin). A filled cell is held
//! here, a hatched one is held from a project above, an outlined one is not
//! held. Every cell is a form, so the grid works without script; with
//! script a click saves in place.
//!
//! A project can be locked: then only the rows set on it and above it apply
//! to what is inside, and rows set inside are kept but ignored. The rule
//! itself lives in `users::explain_scope`; this module only draws it and
//! takes the clicks. Granting and revoking go through `projects`, so the
//! limits (admin here, never more than you hold) are the same as everywhere.

use crate::{
    accounts::users::{self, AccessSource, Scope, User},
    config::Config,
    content::store,
    platform::{admin, projects},
    ui,
};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use maud::{html, Markup};
use serde::Deserialize;
use std::sync::Arc;

/// How many accounts the Add people list shows at once.
const PEOPLE_PAGE: usize = 50;

/// What a grid is for.
#[derive(Debug, Clone)]
pub(crate) enum Target {
    /// A project, by path. Empty is the top level.
    Project(String),
    /// An app, by slug, with its path in the tree.
    App { app: String, path: String, roles: Vec<String> },
}

impl Target {
    fn path(&self) -> &str {
        match self {
            Target::Project(path) => path,
            Target::App { path, .. } => path,
        }
    }

    fn app(&self) -> Option<&str> {
        match self {
            Target::Project(_) => None,
            Target::App { app, .. } => Some(app),
        }
    }
}

/// The optional parts of a page that carries a grid.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GridQuery {
    /// An account to explain: "Check a person".
    pub check: Option<String>,
    /// Shows the Add people list in the page, for a browser with no script.
    pub add: Option<String>,
    /// The search in the Add people list.
    pub pq: Option<String>,
    /// The page of the Add people list. Text, because this struct is also
    /// flattened into the browser's query, where numbers arrive as text.
    pub ppage: Option<String>,
}

/// The word a person reads for a scope.
pub(crate) fn level_word(scope: Scope) -> &'static str {
    match scope {
        Scope::Viewer => "View",
        Scope::Editor => "Edit",
        Scope::Admin => "Manage",
    }
}

const LEVELS: [Scope; 3] = [Scope::Viewer, Scope::Editor, Scope::Admin];

fn level_help(scope: Scope) -> &'static str {
    match scope {
        Scope::Viewer => "View: open the apps.",
        Scope::Editor => "Edit: publish and change them, remove what they made.",
        Scope::Admin => "Manage: set access, create projects, exports, repositories.",
    }
}

fn below(scope: Scope) -> Option<Scope> {
    match scope {
        Scope::Viewer => None,
        Scope::Editor => Some(Scope::Viewer),
        Scope::Admin => Some(Scope::Editor),
    }
}

fn place(path: &str) -> String {
    projects::place(path)
}

/// The page where a path's grid is: a project's Permissions tab, or an
/// app's Access tab.
pub(crate) fn grid_url(path: &str, app: Option<&str>) -> String {
    match app {
        Some(app) => format!("/admin/apps/{app}/access"),
        None => format!("{}?tab=permissions", crate::content::browse::browser_url(path)),
    }
}

/// One person in the grid.
struct Person {
    email: String,
    /// Held here: a row at this path, or access given on the app.
    direct: Option<Scope>,
    /// The strongest row above that counts, and where it is.
    inherited: Option<(Scope, String)>,
    /// The role the app reads, for an app.
    role: Option<String>,
}

struct Facts {
    people: Vec<Person>,
    /// The outermost locked project over this path, if it is above the path.
    locked_by: Option<String>,
    /// Whether this project itself is locked.
    locked_here: bool,
}

async fn facts(config: &Arc<Config>, target: &Target) -> Facts {
    let (config2, target2) = (config.clone(), target.clone());
    tokio::task::spawn_blocking(move || {
        let path = target2.path().to_string();
        let locks = store::locked_prefixes_blocking(&config2);
        let lock = locks
            .iter()
            .filter(|lock| users::prefix_covers(lock, &path))
            .min_by_key(|lock| lock.len())
            .cloned();
        let locked_here = lock.as_deref() == Some(path.as_str());
        let locked_by = lock.filter(|lock| lock != &path);
        let rows = users::list_scopes(&config2).unwrap_or_default();
        let grants: Vec<(String, String)> = match target2.app() {
            Some(app) => users::list_grants(&config2)
                .unwrap_or_default()
                .into_iter()
                .filter(|(granted, _, _)| granted == app)
                .map(|(_, email, role)| (email, role))
                .collect(),
            None => Vec::new(),
        };
        let mut people: Vec<Person> = Vec::new();
        let find = |people: &mut Vec<Person>, email: &str| -> usize {
            match people.iter().position(|p| p.email == email) {
                Some(at) => at,
                None => {
                    people.push(Person { email: email.to_string(), direct: None, inherited: None, role: None });
                    people.len() - 1
                }
            }
        };
        for row in &rows {
            if row.prefix == path {
                let at = find(&mut people, &row.email);
                people[at].direct = people[at].direct.max(Some(row.scope));
            } else if users::prefix_covers(&row.prefix, &path) {
                // A row inside a lock that is above this path does not count.
                if let Some(lock) = &locked_by
                    && !users::prefix_covers(&row.prefix, lock)
                {
                    continue;
                }
                let at = find(&mut people, &row.email);
                let stronger = match &people[at].inherited {
                    Some((held, from)) => row.scope > *held || (row.scope == *held && row.prefix.len() > from.len()),
                    None => true,
                };
                if stronger {
                    people[at].inherited = Some((row.scope, row.prefix.clone()));
                }
            }
        }
        for (email, role) in grants {
            let at = find(&mut people, &email);
            people[at].direct = people[at].direct.max(Some(Scope::Viewer));
            people[at].role = Some(role);
        }
        people.sort_by(|a, b| a.email.cmp(&b.email));
        Facts { people, locked_by, locked_here }
    })
    .await
    .unwrap_or(Facts { people: Vec::new(), locked_by: None, locked_here: false })
}

/// The whole permissions panel for `target`, drawn for `viewer`, who holds
/// Manage there (the page checks that before calling).
pub(crate) async fn panel(
    config: &Arc<Config>,
    viewer: &User,
    target: &Target,
    token: &str,
    query: &GridQuery,
) -> Markup {
    let path = target.path().to_string();
    let app = target.app().map(str::to_string);
    let back = grid_url(&path, app.as_deref());
    let facts = facts(config, target).await;
    let mine = if viewer.is_admin { Some(Scope::Admin) } else { admin::held(config, viewer, &path).await };
    let roles: &[String] = match target {
        Target::App { roles, .. } => roles,
        Target::Project(_) => &[],
    };
    let disabled = facts.locked_by.is_some();
    let adding = query.add.is_some();
    let check = match query.check.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        Some(email) => Some((email.to_string(), explain(config, email, &path, app.as_deref()).await)),
        None => None,
    };
    html! {
        div id="perm-grid" {
            @if let Target::Project(project) = target && !project.is_empty() {
                (lock_setting(project, facts.locked_here, token, &back))
            }
            section."panel" id="perm-people" {
                div."panel-head" {
                    h3 { "People with access" }
                    p {
                        "Tick a box to give a level. Each level includes the ones before it. "
                        "Hatched boxes come from a project above. Site admins hold Manage everywhere."
                    }
                    @if let Target::Project(_) = target {
                        p { "Apps here without their own setting have the site default general access: " (crate::platform::admin::gate_label(&config.default_gate)) "." }
                    }
                }
                div."panel-body" {
                    @if let Some(lock) = &facts.locked_by {
                        div."flash" role="status" {
                            span {
                                "Locked by " a href=(grid_url(lock, None)) { (lock) } ": rows set here are ignored. "
                                "Only the permissions of " (lock) " and above apply."
                            }
                        }
                    }
                    @if facts.people.is_empty() {
                        p."muted" { "Nobody has access at " (place(&path)) " yet. Add people below." }
                    } @else {
                        div."table-scroll" {
                            table."perm-grid" {
                                thead {
                                    tr {
                                        th { "Account" }
                                        @for level in LEVELS {
                                            th."cell-head" title=(level_help(level)) { (level_word(level)) }
                                        }
                                        @if !roles.is_empty() { th { "Role in app" } }
                                        th { "From" }
                                    }
                                }
                                tbody {
                                    @for person in &facts.people {
                                        (row(person, &path, app.as_deref(), roles, mine, viewer.is_admin, disabled, token, &back))
                                    }
                                }
                            }
                        }
                    }
                    div."actions perm-actions" {
                        a."btn" href={ (back) (if back.contains('?') { "&" } else { "?" }) "add=1#add-people" }
                          data-dialog="add-people" { "Add people" }
                    }
                    p."muted small" {
                        @for level in LEVELS { (level_help(level)) " " }
                    }
                }
            }
            (add_people(config, &path, app.as_deref(), mine, viewer.is_admin, token, &back, query, adding).await)
            section."panel" {
                div."panel-head" {
                    h3 { "Check a person" }
                    p { "What one account may do at " (place(&path)) ", and why." }
                }
                div."panel-body" {
                    form."row" method="get" action=(base_of(&back)) {
                        @if back.contains("?tab=permissions") { input type="hidden" name="tab" value="permissions"; }
                        (ui::combobox("check", "/admin/accounts/search", "Choose an account"))
                        button."quiet" type="submit" { "Check" }
                    }
                    @if let Some((email, answer)) = &check {
                        p."check-answer" { strong { (email) } ": " (answer) }
                    }
                }
            }
        }
    }
}

/// The address part of a URL, without its query.
fn base_of(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

#[allow(clippy::too_many_arguments)]
fn row(
    person: &Person,
    path: &str,
    app: Option<&str>,
    roles: &[String],
    mine: Option<Scope>,
    site_admin: bool,
    disabled: bool,
    token: &str,
    back: &str,
) -> Markup {
    let inherited = person.inherited.as_ref().map(|(scope, _)| *scope);
    html! {
        tr data-email=(person.email) {
            td."who" { (person.email) }
            @for level in LEVELS {
                td."cell-col" {
                    @if person.direct.is_some_and(|held| held >= level) {
                        @let lower = below(level);
                        (cell(path, app, &person.email, level, "on", lower, mine, site_admin, disabled, token, back))
                    } @else if inherited.is_some_and(|held| held >= level) {
                        @let from = person.inherited.as_ref().map(|(_, from)| from.as_str()).unwrap_or("");
                        span."cell inherited" title={ (level_word(level)) " from " (place(from)) }
                             aria-label={ (level_word(level)) " from " (place(from)) } {}
                    } @else {
                        (cell(path, app, &person.email, level, "off", Some(level), mine, site_admin, disabled, token, back))
                    }
                }
            }
            @if !roles.is_empty() {
                td {
                    @if let Some(app) = app {
                        form."inline" method="post" action="/admin/access" {
                            (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("email", &person.email))
                            (admin::hidden("allow", "1")) (admin::hidden("back", back))
                            select name="role" data-autosubmit aria-label={ "Role in app for " (person.email) } {
                                option value="" selected[person.role.is_none()] { "None" }
                                @for role in roles {
                                    option value=(role) selected[person.role.as_deref() == Some(role.as_str())] { (role) }
                                }
                                // A role the app no longer declares still shows, as it is.
                                @if let Some(role) = &person.role && !roles.contains(role) {
                                    option value=(role) selected { (role) }
                                }
                            }
                            noscript { button."quiet sm" type="submit" { "Save" } }
                        }
                    }
                }
            }
            td."muted small from" {
                @match (&person.direct, &person.inherited) {
                    (Some(_), Some((_, from))) => { "here, and " a href=(grid_url(from, None)) { (place(from)) } }
                    (Some(_), None) => "here",
                    (None, Some((_, from))) => a href=(grid_url(from, None)) { (place(from)) },
                    (None, None) => "",
                }
            }
        }
    }
}

/// One cell as a form. `to` is the level a click sets: `None` removes the
/// person's own permission here, which asks first.
#[allow(clippy::too_many_arguments)]
fn cell(
    path: &str,
    app: Option<&str>,
    email: &str,
    level: Scope,
    state: &str,
    to: Option<Scope>,
    mine: Option<Scope>,
    site_admin: bool,
    disabled: bool,
    token: &str,
    back: &str,
) -> Markup {
    // A manager may not set anyone above what the manager holds here.
    let beyond = !site_admin && to.is_some_and(|to| mine.is_none_or(|mine| to > mine));
    let off = disabled || beyond;
    let label = match (state, to) {
        ("on", Some(to)) => format!("{email} holds {}. Click to lower to {}.", level_word(level), level_word(to)),
        ("on", None) => format!("{email} holds View. Click to remove access here."),
        (_, Some(to)) => format!("Give {email} {}.", level_word(to)),
        _ => String::new(),
    };
    let to_word = to.map(|to| to.as_str()).unwrap_or("none");
    html! {
        form."cell-form" method="post" action="/admin/permissions/cell" data-cell
             data-cell-confirm=[to.is_none().then(|| format!("Remove access for {email} at {}?", place(path)))] {
            (admin::hidden("token", token)) (admin::hidden("path", path))
            @if let Some(app) = app { (admin::hidden("app", app)) }
            (admin::hidden("email", email)) (admin::hidden("level", to_word))
            (admin::hidden("back", back)) (admin::hidden("confirm", ""))
            button type="submit" class={ "cell " (state) } disabled[off] title=(label) aria-label=(label) {}
        }
    }
}

/// Locked or Customizable, on a project's own tab.
fn lock_setting(project: &str, locked: bool, token: &str, back: &str) -> Markup {
    html! {
        section."panel" {
            div."panel-head" {
                h3 { "Apps and projects inside" }
                p {
                    @if locked {
                        "Locked: only the permissions set here and above apply inside " (project) ". Rows set inside are kept but ignored."
                    } @else {
                        "Customizable: apps and projects inside follow these permissions, and can add their own."
                    }
                }
            }
            div."panel-body" {
                form method="post" action="/admin/permissions/lock" {
                    (admin::hidden("token", token)) (admin::hidden("path", project)) (admin::hidden("back", back))
                    span."seg" role="radiogroup" aria-label="Inheritance" {
                        button type="submit" name="locked" value="0" aria-pressed=(if locked { "false" } else { "true" }) { "Customizable" }
                        button type="submit" name="locked" value="1" aria-pressed=(if locked { "true" } else { "false" }) { "Locked" }
                    }
                }
            }
        }
    }
}

/// The Add people list. Inside a dialog for a browser with script; drawn in
/// the page when `?add=1` is in the address, for one without.
#[allow(clippy::too_many_arguments)]
async fn add_people(
    config: &Arc<Config>,
    path: &str,
    app: Option<&str>,
    mine: Option<Scope>,
    site_admin: bool,
    token: &str,
    back: &str,
    query: &GridQuery,
    inline: bool,
) -> Markup {
    let q = query.pq.clone().unwrap_or_default();
    let page = query.ppage.as_deref().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
    let listing = if inline {
        people_page(config, &q, page).await
    } else {
        PeoplePage { items: Vec::new(), total: 0, page: 1, pages: 1 }
    };
    let body = html! {
        form method="get" action=(base_of(back)) class="people-search" {
            @if back.contains("?tab=permissions") { input type="hidden" name="tab" value="permissions"; }
            input type="hidden" name="add" value="1";
            input type="search" name="pq" value=(q) placeholder="Search accounts" autocomplete="off"
                  data-people-search="/admin/permissions/people" aria-label="Search accounts";
            noscript { button."quiet sm" type="submit" { "Search" } }
        }
        form method="post" action="/admin/permissions/add" id="add-people-form" {
            (admin::hidden("token", token)) (admin::hidden("path", path))
            @if let Some(app) = app { (admin::hidden("app", app)) }
            (admin::hidden("back", back))
            ul."people-list" data-people-list {
                // In the dialog the list loads when it opens, so a page never
                // carries the accounts unless someone asks for them.
                @if inline {
                    @for email in &listing.items {
                        li { label { input type="checkbox" name="email" value=(email); " " (email) } }
                    }
                    @if listing.items.is_empty() { li."muted" { "No accounts match." } }
                } @else {
                    li."muted" { "Loading accounts." }
                }
            }
            p."muted small" data-people-count {
                @if inline { "Showing " (listing.items.len()) " of " (listing.total) "." }
                @if listing.pages > 1 {
                    " Page " (listing.page) " of " (listing.pages) "."
                    @if listing.page > 1 {
                        " " a href={ (base_of(back)) "?" (if back.contains("?tab=permissions") { "tab=permissions&" } else { "" }) "add=1&pq=" (urlencoding::encode(&q)) "&ppage=" (listing.page - 1) "#add-people" } { "Previous" }
                    }
                    @if listing.page < listing.pages {
                        " " a href={ (base_of(back)) "?" (if back.contains("?tab=permissions") { "tab=permissions&" } else { "" }) "add=1&pq=" (urlencoding::encode(&q)) "&ppage=" (listing.page + 1) "#add-people" } { "Next" }
                    }
                }
            }
            div."field" {
                label { "Give them" }
                span."seg" role="radiogroup" {
                    @for level in LEVELS {
                        @let allowed = site_admin || mine.is_some_and(|mine| level <= mine);
                        label."seg-item" {
                            input type="radio" name="scope" value=(level.as_str()) checked[level == Scope::Viewer] disabled[!allowed];
                            " " (level_word(level))
                        }
                    }
                }
            }
            div."actions end" {
                @if !inline { button."quiet" type="button" data-close { "Cancel" } }
                button type="submit" { "Add people" }
            }
        }
    };
    html! {
        @if inline {
            section."panel" id="add-people" {
                div."panel-head" { h3 { "Add people" } p { "Pick one or more accounts, choose a level, and add them." } }
                div."panel-body" { (body) }
            }
        } @else {
            dialog id="add-people" class="wide" {
                h3 { "Add people" }
                p { "Pick one or more accounts, choose a level, and add them." }
                (body)
            }
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct PeoplePage {
    items: Vec<String>,
    total: usize,
    page: usize,
    pages: usize,
}

async fn people_page(config: &Arc<Config>, q: &str, page: usize) -> PeoplePage {
    let q = q.trim().to_lowercase();
    let accounts = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || users::list_accounts(&config).unwrap_or_default())
            .await
            .unwrap_or_default()
    };
    let matching: Vec<String> = accounts
        .into_iter()
        .filter(|account| account.is_active)
        .map(|account| account.email)
        .filter(|email| q.is_empty() || email.contains(&q))
        .collect();
    let total = matching.len();
    let pages = total.div_ceil(PEOPLE_PAGE).max(1);
    let page = page.clamp(1, pages);
    let items = matching.into_iter().skip((page - 1) * PEOPLE_PAGE).take(PEOPLE_PAGE).collect();
    PeoplePage { items, total, page, pages }
}

/// `GET /admin/permissions/people?q=&page=`: one page of accounts for the
/// Add people list. For anyone who manages something.
pub async fn people(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<PeopleQuery>,
) -> Response {
    if let Err(response) = admin::require_manager(&config, &headers).await {
        return response;
    }
    let listing = people_page(&config, query.q.as_deref().unwrap_or(""), query.page.unwrap_or(1)).await;
    ([admin::no_store()], Json(listing)).into_response()
}

#[derive(Deserialize)]
pub struct PeopleQuery {
    q: Option<String>,
    page: Option<usize>,
}

/// The answer to "Check a person", from the same rule the server uses.
pub(crate) async fn explain(config: &Arc<Config>, email: &str, path: &str, app: Option<&str>) -> String {
    let (config2, email2, path2, app2) = (config.clone(), email.trim().to_lowercase(), path.to_string(), app.map(str::to_string));
    tokio::task::spawn_blocking(move || {
        let Some(user) = users::user_by_email(&config2, &email2) else {
            return "There is no active account with that email.".to_string();
        };
        let locks = store::locked_prefixes_blocking(&config2);
        match users::explain_scope(&config2, &user, &path2, app2.as_deref(), &locks) {
            None => {
                let lock = locks.iter().filter(|l| users::prefix_covers(l, &path2)).min_by_key(|l| l.len());
                match lock {
                    Some(lock) if lock != &path2 => format!("No access. {lock} is locked, so only its permissions and those above it apply."),
                    _ => "No access. Nothing gives this account a level here or above.".to_string(),
                }
            }
            Some((scope, AccessSource::SiteAdmin)) => format!("{} everywhere, as a site admin.", level_word(scope)),
            Some((scope, AccessSource::AppAccess)) => format!("{} here, from access given on this app.", level_word(scope)),
            Some((scope, AccessSource::Row(prefix))) if prefix == path2 => format!("{} here, set here.", level_word(scope)),
            Some((scope, AccessSource::Row(prefix))) => format!("{} here, from {}.", level_word(scope), place(&prefix)),
        }
    })
    .await
    .unwrap_or_else(|_| "The check did not finish.".to_string())
}

// --- actions --------------------------------------------------------------------

/// Whether the request came from the grid's script, which wants JSON back.
fn from_script(headers: &HeaderMap) -> bool {
    headers.get("x-toolsite-fetch").is_some()
}

fn answer(headers: &HeaderMap, back: &str, ok: bool, text: String) -> Response {
    if from_script(headers) {
        let status = if ok { StatusCode::OK } else { StatusCode::BAD_REQUEST };
        (status, [admin::no_store()], Json(serde_json::json!({ "ok": ok, "message": text }))).into_response()
    } else {
        admin::redirect_flash(back, ok, text)
    }
}

/// Refuses a change at a path that sits inside a locked project.
fn locked_out(config: &Config, path: &str) -> Option<String> {
    let locks = store::locked_prefixes_blocking(config);
    locks
        .iter()
        .filter(|lock| users::prefix_covers(lock, path) && *lock != path)
        .min_by_key(|lock| lock.len())
        .map(|lock| format!("Locked by {lock}: rows set here are ignored. Change access on {lock}, or set it to Customizable."))
}

#[derive(Deserialize)]
pub struct CellChange {
    token: String,
    path: String,
    app: Option<String>,
    email: String,
    level: String,
    back: Option<String>,
    confirm: Option<String>,
}

/// `POST /admin/permissions/cell`: sets one person's own level at a path,
/// or removes it. Removing asks first: without `confirm=1` a browser with no
/// script gets a page that asks.
pub async fn change_cell(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    axum::extract::Form(form): axum::extract::Form<CellChange>,
) -> Response {
    let path = form.path.trim_matches('/').to_string();
    if !users::valid_prefix(&path) {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    let actor = match admin::checked_at(&config, &headers, &form.token, &path, Scope::Admin).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let back = admin::back_or(form.back.as_deref(), &grid_url(&path, form.app.as_deref()));
    if let Some(message) = locked_out(&config, &path) {
        return answer(&headers, &back, false, message);
    }
    let email = form.email.trim().to_lowercase();
    if form.level == "none" {
        if form.confirm.as_deref() != Some("1") && !from_script(&headers) {
            return confirm_page(&headers, &actor, &form, &back, &path);
        }
        // Their own row here, and access given on the app, both go.
        let revoked = projects::revoke(&config, Some(&actor), &path, &email).await;
        let mut gone = revoked.is_ok();
        if let Some(app) = form.app.as_deref() {
            let (config2, who, app) = (config.clone(), email.clone(), app.to_string());
            let had = {
                let config3 = config2.clone();
                let (who2, app2) = (who.clone(), app.clone());
                tokio::task::spawn_blocking(move || {
                    users::list_grants(&config3)
                        .unwrap_or_default()
                        .iter()
                        .any(|(granted, holder, _)| granted == &app2 && holder == &who2)
                })
                .await
                .unwrap_or(false)
            };
            if had {
                let _ = tokio::task::spawn_blocking(move || users::revoke(&config2, &who, &app)).await;
                gone = true;
            }
        }
        return match (gone, revoked) {
            (true, _) => answer(&headers, &back, true, format!("{email} has no access of their own at {} now.", place(&path))),
            (false, Err(problem)) => answer(&headers, &back, false, problem.message().to_string()),
            (false, Ok(())) => answer(&headers, &back, true, format!("{email} has no access of their own at {} now.", place(&path))),
        };
    }
    let Some(scope) = Scope::parse(&form.level) else {
        return answer(&headers, &back, false, "Choose View, Edit or Manage.".to_string());
    };
    match projects::grant(&config, Some(&actor), &path, &email, scope).await {
        Ok(()) => answer(&headers, &back, true, format!("{email} has {} at {}.", level_word(scope), place(&path))),
        Err(problem) => answer(&headers, &back, false, problem.message().to_string()),
    }
}

fn confirm_page(headers: &HeaderMap, actor: &User, form: &CellChange, back: &str, path: &str) -> Response {
    admin::admin_page(
        headers,
        actor,
        admin::Page {
            active: "apps",
            title: "Remove access",
            crumbs: vec![],
            subtitle: None,
            actions: None,
            script: None,
            body: ui::panel(
                "Remove access",
                Some("The account keeps any access given on a project above. Its own access here goes."),
                html! {
                    p { "Remove the access of " strong { (form.email) } " at " (place(path)) "?" }
                    form method="post" action="/admin/permissions/cell" {
                        (admin::hidden("token", &form.token)) (admin::hidden("path", path))
                        @if let Some(app) = &form.app { (admin::hidden("app", app)) }
                        (admin::hidden("email", &form.email)) (admin::hidden("level", "none"))
                        (admin::hidden("back", back)) (admin::hidden("confirm", "1"))
                        div."actions" {
                            a."btn quiet" href=(back) { "Cancel" }
                            button."danger" type="submit" { "Remove access" }
                        }
                    }
                },
            ),
        },
    )
}

/// `POST /admin/permissions/add`: gives several accounts one level at a
/// path. The body carries `email` once per account, so it is read by hand.
pub async fn add(State(config): State<Arc<Config>>, headers: HeaderMap, body: String) -> Response {
    let mut token = String::new();
    let mut path = String::new();
    let mut app: Option<String> = None;
    let mut back: Option<String> = None;
    let mut level = String::from("viewer");
    let mut emails: Vec<String> = Vec::new();
    for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
        match key.as_ref() {
            "token" => token = value.into_owned(),
            "path" => path = value.trim_matches('/').to_string(),
            "app" => app = Some(value.into_owned()),
            "back" => back = Some(value.into_owned()),
            "scope" => level = value.into_owned(),
            "email" => {
                let email = value.trim().to_lowercase();
                if !email.is_empty() && !emails.contains(&email) {
                    emails.push(email);
                }
            }
            _ => {}
        }
    }
    if !users::valid_prefix(&path) {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    let actor = match admin::checked_at(&config, &headers, &token, &path, Scope::Admin).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let back = admin::back_or(back.as_deref(), &grid_url(&path, app.as_deref()));
    if let Some(message) = locked_out(&config, &path) {
        return admin::redirect_flash(&back, false, message);
    }
    if emails.is_empty() {
        return admin::redirect_flash(&back, false, "Pick at least one account.");
    }
    let Some(scope) = Scope::parse(&level) else {
        return admin::redirect_flash(&back, false, "Choose View, Edit or Manage.");
    };
    let mut added = 0;
    for email in &emails {
        match projects::grant(&config, Some(&actor), &path, email, scope).await {
            Ok(()) => added += 1,
            Err(problem) => return admin::redirect_flash(&back, false, format!("{email}: {}", problem.message())),
        }
    }
    let who = if added == 1 { emails[0].clone() } else { format!("{added} accounts") };
    admin::redirect_flash(&back, true, format!("{who} now have {} at {}.", level_word(scope), place(&path)))
}

#[derive(Deserialize)]
pub struct LockChange {
    token: String,
    path: String,
    locked: String,
    back: Option<String>,
}

/// `POST /admin/permissions/lock`: Locked or Customizable for a project.
pub async fn change_lock(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    axum::extract::Form(form): axum::extract::Form<LockChange>,
) -> Response {
    let path = form.path.trim_matches('/').to_string();
    if path.is_empty() || !users::valid_prefix(&path) {
        return (StatusCode::BAD_REQUEST, "only a project can be locked").into_response();
    }
    let actor = match admin::checked_at(&config, &headers, &form.token, &path, Scope::Admin).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let back = admin::back_or(form.back.as_deref(), &grid_url(&path, None));
    let locked = form.locked == "1";
    match store::set_locked(&config, &path, locked).await {
        Ok(()) => {
            tracing::info!(by = %actor.email, project = %path, locked, "project lock changed");
            admin::redirect_flash(
                &back,
                true,
                if locked {
                    format!("{path} is locked. Only its permissions and those above it apply inside.")
                } else {
                    format!("{path} is customizable. Apps and projects inside can add their own permissions.")
                },
            )
        }
        Err(message) => admin::redirect_flash(&back, false, message),
    }
}

// --- the old per-app grants -------------------------------------------------------

/// Turns every per-app grant into a View row on that app, once. The grant
/// itself stays, because it carries the role the app reads.
pub async fn adopt_grants(config: &Arc<Config>) {
    let marker = config.data_dir.join(".site").join("grants-adopted");
    if tokio::fs::metadata(&marker).await.is_ok() {
        return;
    }
    let grants = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || users::list_grants(&config).unwrap_or_default())
            .await
            .unwrap_or_default()
    };
    let mut adopted = 0;
    for (app, email, _) in grants {
        let path = store::logical_path(config, &app).await;
        if give_view_if_absent(config, &email, &path).await {
            adopted += 1;
        }
    }
    if let Some(parent) = marker.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(&marker, format!("{adopted}\n")).await;
    if adopted > 0 {
        tracing::info!(adopted, "per-app grants became View rows");
    }
}

/// Gives View at `path` unless the account already holds a row there.
/// Returns whether a row was added.
pub(crate) async fn give_view_if_absent(config: &Arc<Config>, email: &str, path: &str) -> bool {
    let (config, email, path) = (config.clone(), email.trim().to_lowercase(), path.to_string());
    tokio::task::spawn_blocking(move || {
        let exists = users::list_scopes(&config)
            .unwrap_or_default()
            .iter()
            .any(|row| row.email == email && row.prefix == path);
        if exists {
            return false;
        }
        users::grant_scope(&config, &email, &path, Scope::Viewer, None).is_ok()
    })
    .await
    .unwrap_or(false)
}
