//! The permissions rules: who may do what at a project or an app, edited in
//! place, the way Tableau's permissions dialog works.
//!
//! One component serves both places a person sets access: a project's
//! Permissions tab in the app browser and an app's Access tab. A rule is one
//! person and a level. The table lists only the people who hold a rule here
//! or above, never the whole directory: a site can have hundreds of
//! accounts. A person is added from the row at the top of the table, a
//! search box that asks the server for at most ten matches. Each rule row
//! has a level select and the View, Edit and Manage cells shaded to match;
//! a click on a cell sets that level. Rules held from a project above are
//! their own rows, greyed and read only. Removing a rule asks in the row.
//! Every control is a form, so it all works without script; with script a
//! change saves in place.
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
use std::{ops::Not, sync::Arc};

/// How many rules the table shows at once.
const RULES_PAGE: usize = 50;

/// How many accounts the add row's search offers at once.
const MAX_CANDIDATES: usize = 10;

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

/// The optional parts of a page that carries the rules table.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GridQuery {
    /// A filter on the rules, by email.
    pub rq: Option<String>,
    /// The page of the rules table. Text, because this struct is also
    /// flattened into the browser's query, where numbers arrive as text.
    pub rpage: Option<String>,
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

/// One row of the rules table.
enum Rule<'a> {
    /// A rule set here: the person's own level at this path.
    Own(&'a Person),
    /// A rule held from a project above, read only.
    Above(&'a Person),
}

impl Rule<'_> {
    fn email(&self) -> &str {
        match self {
            Rule::Own(person) | Rule::Above(person) => &person.email,
        }
    }
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

    // Own rules first, then the ones from above, each by email.
    let mut rules: Vec<Rule> = facts.people.iter().filter(|p| p.direct.is_some()).map(Rule::Own).collect();
    rules.extend(facts.people.iter().filter(|p| p.inherited.is_some()).map(Rule::Above));
    let filter = query.rq.as_deref().map(str::trim).unwrap_or("").to_lowercase();
    if !filter.is_empty() {
        rules.retain(|rule| rule.email().contains(&filter));
    }
    let total = rules.len();
    let pages = total.div_ceil(RULES_PAGE).max(1);
    let page = query.rpage.as_deref().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).clamp(1, pages);
    let shown: Vec<&Rule> = rules.iter().skip((page - 1) * RULES_PAGE).take(RULES_PAGE).collect();
    let many = facts.people.iter().filter(|p| p.direct.is_some()).count()
        + facts.people.iter().filter(|p| p.inherited.is_some()).count()
        > RULES_PAGE;

    // What each person on this page may do here, and why: the same answer
    // the server gives, shown when the row's name is opened.
    let mut why: Vec<(String, String)> = Vec::new();
    for rule in &shown {
        if !why.iter().any(|(email, _)| email == rule.email()) {
            why.push((rule.email().to_string(), explain(config, rule.email(), &path, app.as_deref()).await));
        }
    }
    let why_of = |email: &str| why.iter().find(|(e, _)| e == email).map(|(_, w)| w.clone()).unwrap_or_default();

    let page_link = |n: usize| -> String {
        let mut link = format!("{}{}rpage={n}", back, if back.contains('?') { "&" } else { "?" });
        if !filter.is_empty() {
            link.push_str(&format!("&rq={}", urlencoding::encode(&filter)));
        }
        link
    };
    let columns = 4 + LEVELS.len() + usize::from(!roles.is_empty());

    html! {
        div id="perm-grid" {
            @if let Target::Project(project) = target && !project.is_empty() {
                (project_access(config, project, facts.locked_by.as_deref(), token, &back).await)
                (lock_setting(project, facts.locked_here, token, &back))
            }
            @if let Target::Project(project) = target && project.is_empty() {
                section."panel" {
                    div."panel-head" {
                        h3 { "General access" }
                        p { "Apps and projects without their own setting: " (crate::platform::admin::gate_label(&config.default_gate)) ". Set it for the whole site with " code { "TOOLSITE_DEFAULT_ACCESS" } "." }
                    }
                }
            }
            section."panel" id="perm-people" {
                div."panel-head" {
                    h3 { "People with access" }
                    p {
                        "Add a person above the table, then set their level. Each level includes the ones before it. "
                        "Grey rows come from a project above. Site admins hold Manage everywhere."
                    }

                }
                div."panel-body" {
                    @if let Some(lock) = &facts.locked_by {
                        div."flash" role="status" {
                            span {
                                "Locked by " a href=(grid_url(lock, None)) { (lock) } ": rules set here are ignored. "
                                "Only the permissions of " (lock) " and above apply."
                            }
                        }
                    }
                    @if many {
                        form."row rules-filter" method="get" action=(base_of(&back)) {
                            @if back.contains("?tab=permissions") { input type="hidden" name="tab" value="permissions"; }
                            input type="search" name="rq" value=(filter) placeholder="Filter people" aria-label="Filter people";
                            button."quiet sm" type="submit" { "Filter" }
                        }
                    }
                    @if !disabled {
                        (add_toolbar(&path, app.as_deref(), mine, viewer.is_admin, token, &back))
                    }
                    div."table-scroll" {
                        table."perm-grid perm-rules" {
                            thead {
                                tr {
                                    th { "Account" }
                                    th { "Level" }
                                    @for level in LEVELS {
                                        th."cell-head" title=(level_help(level)) { (level_word(level)) }
                                    }
                                    @if !roles.is_empty() { th { "Role in app" } }
                                    th { "From" }
                                    th."remove-col" { span."sr" { "Remove" } }
                                }
                            }
                            tbody {
                                @if shown.is_empty() {
                                    tr { td colspan=(columns) class="muted" {
                                        @if filter.is_empty() { "Nobody has access at " (place(&path)) " yet. Add a person above the table." }
                                        @else { "Nobody here matches " (filter) "." }
                                    } }
                                }
                                @for rule in &shown {
                                    @match rule {
                                        Rule::Own(person) => (own_row(person, &path, app.as_deref(), roles, mine, viewer.is_admin, disabled, token, &back, &why_of(&person.email))),
                                        Rule::Above(person) => (above_row(person, roles, &why_of(&person.email))),
                                    }
                                }
                            }
                        }
                    }
                    @if pages > 1 {
                        div."actions pager" {
                            @if page > 1 { a."btn quiet sm" href=(page_link(page - 1)) { "Previous" } }
                            span."muted small" { "Showing " ((page - 1) * RULES_PAGE + 1) "\u{2013}" (((page - 1) * RULES_PAGE + shown.len())) " of " (total) }
                            @if page < pages { a."btn quiet sm" href=(page_link(page + 1)) { "Next" } }
                        }
                    }
                    p."muted small" {
                        @for level in LEVELS { (level_help(level)) " " }
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

/// The line of controls above the table: a search for one account and a
/// level. It sits outside the table so the column headers label rules, not
/// the form. The search asks the server for at most ten matches, leaving out
/// people who already hold a rule here. Pressing Add keeps the focus in the
/// search, so several people can be added one after another.
fn add_toolbar(
    path: &str,
    app: Option<&str>,
    mine: Option<Scope>,
    site_admin: bool,
    token: &str,
    back: &str,
) -> Markup {
    html! {
        form."add-rule-form" method="post" action="/admin/permissions/add" data-perm-add {
            (admin::hidden("token", token)) (admin::hidden("path", path))
            @if let Some(app) = app { (admin::hidden("app", app)) } @else { (admin::hidden("app", "")) }
            (admin::hidden("back", back))
            span."add-rule-label" { "Add a person" }
            (ui::combobox_full("email", "/admin/permissions/candidates", "Type a name or email", "", Some("path,app"), "-add"))
            select name="scope" aria-label="Level for the new person" {
                @for level in LEVELS {
                    @let allowed = site_admin || mine.is_some_and(|mine| level <= mine);
                    option value=(level.as_str()) selected[level == Scope::Viewer] disabled[!allowed] { (level_word(level)) }
                }
            }
            button type="submit" { "Add" }
        }
    }
}

/// The person's name, which opens to say what they may do here and why.
fn who_cell(email: &str, why: &str) -> Markup {
    html! {
        td."who" {
            details."rule-who" {
                summary { (email) }
                p."muted small rule-why" { (why) }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn own_row(
    person: &Person,
    path: &str,
    app: Option<&str>,
    roles: &[String],
    mine: Option<Scope>,
    site_admin: bool,
    disabled: bool,
    token: &str,
    back: &str,
    why: &str,
) -> Markup {
    let held = person.direct.unwrap_or(Scope::Viewer);
    html! {
        tr."rule own" data-email=(person.email) {
            (who_cell(&person.email, why))
            td."level" {
                form."inline" method="post" action="/admin/permissions/cell" data-cell {
                    (admin::hidden("token", token)) (admin::hidden("path", path))
                    @if let Some(app) = app { (admin::hidden("app", app)) }
                    (admin::hidden("email", &person.email)) (admin::hidden("back", back)) (admin::hidden("confirm", ""))
                    select name="level" data-autosubmit disabled[disabled] aria-label={ "Level for " (person.email) } {
                        @for level in LEVELS {
                            @let allowed = site_admin || mine.is_some_and(|mine| level <= mine);
                            option value=(level.as_str()) selected[level == held] disabled[!allowed] { (level_word(level)) }
                        }
                    }
                    noscript { button."quiet sm" type="submit" disabled[disabled] { "Save" } }
                }
            }
            @for level in LEVELS {
                td."cell-col" {
                    (cell(path, app, &person.email, level, if held >= level { "on" } else { "off" }, mine, site_admin, disabled, token, back))
                }
            }
            @if !roles.is_empty() { (role_cell(person, app, roles, token, back)) }
            td."muted small from" { "here" }
            td."remove-col" {
                @if !disabled {
                    details."remove-rule" {
                        summary aria-label={ "Remove " (person.email) } title="Remove" { "\u{2715}" }
                        div."remove-ask" {
                            span { "Remove?" }
                            form."inline" method="post" action="/admin/permissions/cell" data-cell {
                                (admin::hidden("token", token)) (admin::hidden("path", path))
                                @if let Some(app) = app { (admin::hidden("app", app)) }
                                (admin::hidden("email", &person.email)) (admin::hidden("level", "none"))
                                (admin::hidden("back", back)) (admin::hidden("confirm", "1"))
                                button."danger sm" type="submit" { "Yes" }
                            }
                            button."quiet sm" type="button" data-remove-no { "No" }
                        }
                    }
                }
            }
        }
    }
}

fn above_row(person: &Person, roles: &[String], why: &str) -> Markup {
    let (held, from) = person.inherited.clone().unwrap_or((Scope::Viewer, String::new()));
    html! {
        tr."rule above" data-email-above=(person.email) {
            (who_cell(&person.email, why))
            td."level muted" { (level_word(held)) }
            @for level in LEVELS {
                td."cell-col" {
                    @if held >= level {
                        span."cell inherited" title={ (level_word(level)) " from " (place(&from)) }
                             aria-label={ (level_word(level)) " from " (place(&from)) } {}
                    } @else {
                        span."cell empty" aria-hidden="true" {}
                    }
                }
            }
            @if !roles.is_empty() { td."muted small" { (person.role.as_deref().unwrap_or("")) } }
            td."muted small from" { a href=(grid_url(&from, None)) { (place(&from)) } }
            td {}
        }
    }
}

fn role_cell(person: &Person, app: Option<&str>, roles: &[String], token: &str, back: &str) -> Markup {
    html! {
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
}

/// One cell as a form: a click sets the person's level here to this cell.
#[allow(clippy::too_many_arguments)]
fn cell(
    path: &str,
    app: Option<&str>,
    email: &str,
    level: Scope,
    state: &str,
    mine: Option<Scope>,
    site_admin: bool,
    disabled: bool,
    token: &str,
    back: &str,
) -> Markup {
    // A manager may not set anyone above what the manager holds here.
    let beyond = !site_admin && mine.is_none_or(|mine| level > mine);
    let off = disabled || beyond;
    let label = format!("Set {email} to {}.", level_word(level));
    html! {
        form."cell-form" method="post" action="/admin/permissions/cell" data-cell {
            (admin::hidden("token", token)) (admin::hidden("path", path))
            @if let Some(app) = app { (admin::hidden("app", app)) }
            (admin::hidden("email", email)) (admin::hidden("level", level.as_str()))
            (admin::hidden("back", back)) (admin::hidden("confirm", ""))
            button type="submit" class={ "cell " (state) } disabled[off] title=(label) aria-label=(label) {}
        }
    }
}

/// Locked or Customizable, on a project's own tab.
/// A project's general access: its own setting, or "Use the project above",
/// which shows what that gives. Under a lock above it the control is off,
/// since a setting here would be ignored.
async fn project_access(config: &Arc<Config>, project: &str, locked_by: Option<&str>, token: &str, back: &str) -> Markup {
    let own = store::list_folders(config).await.into_iter().find(|f| f.path == project).and_then(|f| f.gate);
    let parent = project.rsplit_once('/').map(|(above, _)| above.to_string()).unwrap_or_default();
    let (above, source) = store::project_gate(config, &parent).await;
    let label = crate::platform::admin::gate_label;
    html! {
        section."panel" {
            div."panel-head" {
                h3 { "General access" }
                p { "Who may open the apps in this project that set nothing of their own. People with access always may." }
            }
            div."panel-body" {
                form method="post" action="/admin/project" {
                    input type="hidden" name="token" value=(token);
                    input type="hidden" name="action" value="access";
                    input type="hidden" name="path" value=(project);
                    input type="hidden" name="back" value=(back);
                    span."seg" role="radiogroup" aria-label="General access" {
                        @for (value, name) in [("public", "Public"), ("authenticated", "Signed in"), ("restricted", "Restricted")] {
                            button type="submit" name="gate" value=(value) disabled[locked_by.is_some()]
                                   aria-pressed=(if own.as_deref() == Some(value) && locked_by.is_none() { "true" } else { "false" }) { (name) }
                        }
                        button type="submit" name="gate" value="inherit" disabled[locked_by.is_some()]
                               aria-pressed=(if own.is_none() || locked_by.is_some() { "true" } else { "false" }) { "Use the project above" }
                    }
                }
                p."muted small" {
                    @if let Some(lock) = locked_by {
                        "Locked by " a href=(crate::content::browse::browser_url(lock)) { (lock) } ": its general access applies here."
                    } @else if own.is_none() {
                        @match &source {
                            store::GateSource::Project(path) => { "Follows " (path) ": " (label(&above)) "." }
                            _ => { "Follows the site default: " (label(&above)) "." }
                        }
                    } @else {
                        "Apps here without their own setting are " (label(own.as_deref().unwrap_or("restricted"))) "."
                    }
                }
            }
        }
    }
}

fn lock_setting(project: &str, locked: bool, token: &str, back: &str) -> Markup {
    html! {
        section."panel" {
            div."panel-head" {
                h3 { "Apps and projects inside" }
                p {
                    @if locked {
                        "Locked: only the permissions set here and above apply inside " (project) ". Rules set inside are kept but ignored."
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

/// One account the add row may offer.
#[derive(serde::Serialize)]
pub(crate) struct Candidate {
    value: String,
    label: String,
}

#[derive(Deserialize)]
pub struct CandidateQuery {
    q: Option<String>,
    path: Option<String>,
    app: Option<String>,
}

/// `GET /admin/permissions/candidates?q=&path=&app=`: at most ten active
/// accounts for the add row, leaving out anyone who already holds a rule at
/// that path (or access given on that app). For a manager of the path.
pub async fn candidates(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<CandidateQuery>,
) -> Response {
    let path = query.path.as_deref().unwrap_or("").trim_matches('/').to_string();
    if !users::valid_prefix(&path) {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    let viewer = match admin::require_manager(&config, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let manages = viewer.is_admin || admin::held(&config, &viewer, &path).await.is_some_and(|held| held >= Scope::Admin);
    if !manages {
        return (StatusCode::FORBIDDEN, "you do not manage access here").into_response();
    }
    let q = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let app = query.app.filter(|a| !a.is_empty());
    let config2 = config.clone();
    let found = tokio::task::spawn_blocking(move || {
        let mut taken: Vec<String> = users::list_scopes(&config2)
            .unwrap_or_default()
            .into_iter()
            .filter(|row| row.prefix == path)
            .map(|row| row.email)
            .collect();
        if let Some(app) = &app {
            taken.extend(
                users::list_grants(&config2)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(granted, _, _)| granted == app)
                    .map(|(_, email, _)| email),
            );
        }
        users::list_accounts(&config2)
            .unwrap_or_default()
            .into_iter()
            .filter(|account| account.is_active && !taken.contains(&account.email))
            .filter(|account| q.is_empty() || account.email.contains(&q))
            .take(MAX_CANDIDATES)
            .map(|account| Candidate { value: account.email.clone(), label: account.email })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    ([admin::no_store()], Json(found)).into_response()
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
    let app = app.filter(|a| !a.is_empty());
    let back = admin::back_or(back.as_deref(), &grid_url(&path, app.as_deref()));
    if let Some(message) = locked_out(&config, &path) {
        return answer(&headers, &back, false, message);
    }
    if emails.is_empty() {
        return answer(&headers, &back, false, "Choose an account first.".to_string());
    }
    let Some(scope) = Scope::parse(&level) else {
        return answer(&headers, &back, false, "Choose View, Edit or Manage.".to_string());
    };
    let mut added = 0;
    for email in &emails {
        if users_exists(&config, email).await.not() {
            return answer(&headers, &back, false, format!("There is no active account {email}."));
        }
        match projects::grant(&config, Some(&actor), &path, email, scope).await {
            Ok(()) => added += 1,
            Err(problem) => return answer(&headers, &back, false, format!("{email}: {}", problem.message())),
        }
    }
    let who = if added == 1 { emails[0].clone() } else { format!("{added} accounts") };
    answer(&headers, &back, true, format!("{who} now has {} at {}.", level_word(scope), place(&path)))
}

/// Whether an active account has this email.
async fn users_exists(config: &Arc<Config>, email: &str) -> bool {
    let (config, email) = (config.clone(), email.to_string());
    tokio::task::spawn_blocking(move || users::user_by_email(&config, &email).is_some())
        .await
        .unwrap_or(false)
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
