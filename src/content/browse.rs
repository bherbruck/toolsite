//! The app browser at `/`: projects and apps as one list, the way a file
//! browser shows folders and files.
//!
//! One level is shown at a time, chosen with `?project=<path>`. Projects come
//! first, then apps. In the List view a project row opens in place, so its
//! subprojects and apps appear indented beneath it; the name takes you into
//! the project. The Cards view shows the current level as tiles. A search
//! flattens everything below the current level into one list, each result
//! with its path.
//!
//! What is listed follows the same rule as everywhere else: an app the
//! viewer may not open is never shown, and a project is shown only when the
//! viewer may open something in it or holds access on it or below.
//!
//! The browser is also where projects are run from. A person with admin at
//! a project sees New project and a Permissions tab there, and Move on the
//! apps they may move. Every form posts to the admin actions, which check
//! everything again; the controls here only decide what is offered.

use crate::{
    accounts::users::{self, Scope, User},
    config::Config,
    content::{
        serve::{admits, icon_markup},
        store::{
            self, collect_slugs, page_icon, page_path, page_title, read_meta, relative_time, Icon,
        },
    },
    platform::admin,
    ui,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use maud::{html, Markup, PreEscaped};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::SystemTime,
};
use tokio::fs;

#[derive(Deserialize, Default)]
pub struct BrowseQuery {
    /// Only on `/`: the old way of naming a level, redirected to its path.
    project: Option<String>,
    q: Option<String>,
    tab: Option<String>,
    /// Rows open in the List view, relative to this level, comma separated.
    open: Option<String>,
    /// The permissions table's own parts: the rule filter and its page.
    #[serde(flatten)]
    grid: crate::platform::permissions::GridQuery,
}

/// One app the viewer may open.
struct Entry {
    slug: String,
    /// The app the slug belongs to: its first segment.
    app: String,
    project: String,
    title: Option<String>,
    icon: Icon,
    modified: Option<SystemTime>,
    /// What the viewer may do to it beyond opening it.
    scope: Option<Scope>,
    /// Its general access, wherever that comes from.
    gate: String,
}

impl Entry {
    fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.slug)
    }
}

/// Everything the page is drawn from.
struct Tree {
    entries: Vec<Entry>,
    /// Projects the viewer may see, by path.
    projects: BTreeSet<String>,
    /// What the viewer holds on each project it holds something on.
    held: std::collections::BTreeMap<String, Scope>,
    /// Whether the viewer is a site admin, who holds admin at the top level.
    site_admin: bool,
    /// Every project path and every app's project, for "is it empty".
    all_projects: Vec<String>,
    all_app_projects: Vec<String>,
}

impl Tree {
    fn children(&self, parent: &str) -> Vec<&str> {
        self.projects
            .iter()
            .filter(|path| match path.rsplit_once('/') {
                Some((above, _)) => above == parent,
                None => parent.is_empty(),
            })
            .map(String::as_str)
            .collect()
    }

    fn apps_in(&self, project: &str) -> Vec<&Entry> {
        self.entries.iter().filter(|entry| entry.project == project).collect()
    }

    /// No projects below it and no apps in it or below, counting every app,
    /// not only the ones the viewer may see.
    fn is_empty_project(&self, project: &str) -> bool {
        !self.all_projects.iter().any(|p| p.starts_with(&format!("{project}/")))
            && !self.all_app_projects.iter().any(|at| !at.is_empty() && users::prefix_covers(project, at))
    }

    fn apps_below(&self, project: &str) -> usize {
        self.entries
            .iter()
            .filter(|entry| users::prefix_covers(project, &entry.project))
            .count()
    }
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Where a level lives: `/` for the root, `/browse/<path>` below it. Paths
/// are slug segments, so they need no escaping.
pub(crate) fn browser_url(project: &str) -> String {
    if project.is_empty() {
        "/".to_string()
    } else {
        format!("/browse/{project}")
    }
}

fn permissions_url(project: &str) -> String {
    format!("{}?tab=permissions", browser_url(project))
}

fn count_label(n: usize) -> String {
    match n {
        1 => "1 app".to_string(),
        n => format!("{n} apps"),
    }
}

async fn gather(config: &Arc<Config>, viewer: Option<&User>) -> Tree {
    let mut slugs = Vec::new();
    collect_slugs(&config.data_dir, String::new(), &mut slugs).await;

    let mut entries = Vec::with_capacity(slugs.len());
    for slug in &slugs {
        let meta = read_meta(config, slug).await;
        let app = slug.split('/').next().unwrap_or(slug).to_string();
        // Judged by the app, as serving judges it: the app's meta decides
        // hidden and access, with its rule for this page's path. A loose
        // page with no meta of its own must not read as open.
        let app_meta = read_meta(config, &app).await;
        if meta.hidden || app_meta.hidden || !meta.listed {
            continue;
        }
        let within = match slug.split_once('/') {
            Some((_, rest)) => format!("/{rest}"),
            None => "/".to_string(),
        };
        // The app's own rules, its projects', the site's: one resolution,
        // with this page's path inside the app.
        let gate = crate::content::store::effective_gate(config, &app, &within).await.gate;
        if !admits(config, &gate, &app, viewer).await {
            continue;
        }
        let project = store::app_folder(config, &app).await;
        let path = page_path(config, slug).await;
        let title = match &path {
            Some(path) => page_title(path).await,
            None => None,
        };
        let modified = match &path {
            Some(path) => fs::metadata(path).await.ok().and_then(|m| m.modified().ok()),
            None => None,
        };
        let scope = match viewer {
            Some(user) => admin::held_on(config, user, &app).await,
            None => None,
        };
        entries.push(Entry {
            slug: slug.clone(),
            app,
            project,
            title,
            icon: page_icon(config, slug).await,
            modified,
            scope,
            gate,
        });
    }
    // Newest first, the way the index always listed them.
    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.slug.cmp(&b.slug)));

    // A project is shown when something visible sits in it or below it, or
    // when the viewer holds access on it or below and so may need to walk
    // there to manage it.
    let mut projects = BTreeSet::new();
    let mut held = std::collections::BTreeMap::new();
    for folder in store::list_folders(config).await {
        let has_app = entries.iter().any(|entry| users::prefix_covers(&folder.path, &entry.project));
        let here = match viewer {
            Some(user) => admin::held(config, user, &folder.path).await,
            None => None,
        };
        if let Some(scope) = here {
            held.insert(folder.path.clone(), scope);
        }
        let holds = match viewer {
            Some(user) => {
                here.is_some() || {
                    let (config, user, path) = (config.clone(), user.clone(), folder.path.clone());
                    tokio::task::spawn_blocking(move || users::holds_below(&config, &user, &path))
                        .await
                        .unwrap_or(false)
                }
            }
            None => false,
        };
        if has_app || holds {
            projects.insert(folder.path);
        }
    }
    // A visible project's parents are visible too, or it could not be reached.
    let mut with_parents = projects.clone();
    for path in &projects {
        for above in store::folder_chain(&format!("{path}/x")) {
            with_parents.insert(above);
        }
    }
    let all_projects: Vec<String> = store::list_folders(config).await.into_iter().map(|f| f.path).collect();
    let all_app_projects: Vec<String> = store::apps_with_folders(config).await.into_iter().map(|(_, at)| at).collect();
    Tree {
        entries,
        projects: with_parents,
        held,
        site_admin: viewer.is_some_and(|user| user.is_admin),
        all_projects,
        all_app_projects,
    }
}

/// The projects `viewer` may see, each with the apps in it or below that it
/// may see. The rule the browser draws by, for the `projects` tool.
pub(crate) async fn visible_projects(config: &Arc<Config>, viewer: Option<&User>) -> Vec<(String, usize)> {
    let tree = gather(config, viewer).await;
    tree.projects.iter().map(|path| (path.clone(), tree.apps_below(path))).collect()
}

/// `GET /`: the top level. `?project=` was the old way to name a level and
/// is sent on to its path.
pub(crate) async fn index(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<BrowseQuery>,
) -> Response {
    if let Some(project) = query.project.as_deref().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()) {
        if !users::valid_prefix(project) {
            return (StatusCode::NOT_FOUND, "There is no such project.").into_response();
        }
        let mut to = browser_url(project);
        let rest: Vec<String> = [("q", &query.q), ("tab", &query.tab), ("open", &query.open)]
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", urlencoding::encode(v))))
            .collect();
        if !rest.is_empty() {
            to = format!("{to}?{}", rest.join("&"));
        }
        return Redirect::permanent(&to).into_response();
    }
    render(config, headers, String::new(), query).await
}

/// `GET /browse/<path>`: one project's level.
pub(crate) async fn browse(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Response {
    let project = path.trim_matches('/').to_string();
    if project.is_empty() {
        return Redirect::permanent("/").into_response();
    }
    if !users::valid_prefix(&project) {
        return (StatusCode::NOT_FOUND, "There is no such project.").into_response();
    }
    // A project that was renamed or moved still answers at its old path. The
    // query goes along so a shared link opens the same view.
    if let Some(to) = store::renamed_path(&config, &project).await {
        let rest: Vec<String> = [("q", &query.q), ("tab", &query.tab), ("open", &query.open)]
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", urlencoding::encode(v))))
            .collect();
        let mut url = browser_url(&to);
        if !rest.is_empty() {
            url = format!("{url}?{}", rest.join("&"));
        }
        return Redirect::permanent(&url).into_response();
    }
    render(config, headers, project, query).await
}

async fn render(config: Arc<Config>, headers: HeaderMap, project: String, query: BrowseQuery) -> Response {
    let viewer = users::current_site_user(&config, &headers).await;
    let q = query.q.clone().unwrap_or_default().trim().to_string();
    let open: BTreeSet<String> = query
        .open
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|p| p.trim().trim_matches('/').to_string())
        .filter(|p| !p.is_empty() && users::valid_prefix(p))
        .collect();

    let tree = gather(&config, viewer.as_ref()).await;
    // A project the viewer may not see reads exactly like one that does not
    // exist.
    if !project.is_empty() && !(users::valid_prefix(&project) && tree.projects.contains(&project)) {
        return (StatusCode::NOT_FOUND, "There is no such project.").into_response();
    }

    let here = match &viewer {
        Some(user) => admin::held(&config, user, &project).await,
        None => None,
    };
    let admin_here = here == Some(Scope::Admin);
    let tab = match query.tab.as_deref() {
        Some("permissions") if admin_here => "permissions",
        _ => "apps",
    };
    let token = viewer.as_ref().map(|user| admin::form_token(&config, user)).unwrap_or_default();
    let back = if tab == "permissions" { permissions_url(&project) } else { browser_url(&project) };
    let ctx = Ctx { token: &token, back: &back };

    let title = if project.is_empty() { "Apps" } else { name_of(&project) };
    let shown = if q.is_empty() { tree.apps_in(&project).len() } else { 0 };

    let body = html! {
        @if !project.is_empty() {
            nav."crumbs" {
                span { a href="/" { "Apps" } }
                @for above in store::folder_chain(&format!("{project}/x")).into_iter().filter(|p| p != &project) {
                    span { a href=(browser_url(&above)) { (name_of(&above)) } }
                }
                span { (title) }
            }
        }
        div."title-row" {
            div {
                h1 { (title) }
                p."muted" {
                    @if project.is_empty() { (count_label(tree.entries.len())) }
                    @else { code { (project) } " · " (count_label(tree.apps_below(&project))) }
                }
            }
            div."actions" {
                @if tab == "apps" {
                    div."seg icons" role="group" aria-label="View" {
                        button type="button" data-view="cards" aria-pressed="true" aria-label="Cards" title="Cards" { (PreEscaped(ICON_CARDS)) }
                        button type="button" data-view="list" aria-pressed="false" aria-label="List" title="List" { (PreEscaped(ICON_LIST)) }
                    }
                }
                @if admin_here {
                    (new_project_control(&project, &token, &back))
                }
            }
        }
        (ui::flash(admin::take_flash(&headers).as_ref()))
        @if admin_here {
            (ui::tabs(
                &[("apps", "Apps", browser_url(&project).as_str()), ("permissions", "Permissions", permissions_url(&project).as_str())],
                tab,
            ))
        }
        @if tab == "permissions" {
            @if let Some(user) = &viewer {
                (crate::platform::permissions::panel(
                    &config,
                    user,
                    &crate::platform::permissions::Target::Project(project.clone()),
                    &token,
                    &query.grid,
                ).await)
            }
        } @else {
            form."search" method="get" action=(browser_url(&project)) {
                (ui::search_field("q", &q, &(if project.is_empty() { "Search apps and projects".to_string() } else { format!("Search in {title}") }), Some("q")))
            }
            @if q.is_empty() {
                (level(&tree, &project, shown, &open, &ctx))
            } @else {
                (results(&tree, &project, &q, &ctx))
            }
            p."no-match" id="no-match" { "Nothing on this page matches. Press Enter to search below this level." }
        }
    };

    let manages = match &viewer {
        Some(user) => admin::manages_something(&config, user).await,
        None => false,
    };
    let flash = admin::take_flash(&headers);
    let markup = ui::shell(
        title,
        admin::sidebar("site", viewer.as_ref(), manages),
        body,
        (tab == "apps").then_some(ui::FILTER_SCRIPT),
    );
    let mut response = ([admin::no_store()], Html(markup.into_string())).into_response();
    if flash.is_some() {
        let (name, value) = admin::clear_flash();
        if let Ok(value) = value.parse() {
            response.headers_mut().append(name, value);
        }
    }
    response
}

/// A project marker, outline only, in the muted colour of the icon box it
/// sits in, so project rows line up with app rows. Closed and open shapes;
/// CSS shows the open one while a row is expanded.
const FOLDER_CLOSED: &str = r#"<svg class="folder-closed" viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linejoin="round"><path d="M3.5 7A1.5 1.5 0 0 1 5 5.5h4.4l2 2.2H19A1.5 1.5 0 0 1 20.5 9.2v8.3A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5z"/></svg>"#;
const FOLDER_OPEN: &str = r#"<svg class="folder-open" viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linejoin="round"><path d="M3.5 17.5V7A1.5 1.5 0 0 1 5 5.5h4.4l2 2.2H18A1.5 1.5 0 0 1 19.5 9.2V10.5"/><path d="M3.5 17.5 6 11.2A1.5 1.5 0 0 1 7.4 10.2H20.6a1 1 0 0 1 .9 1.4l-2.3 5.9A2.4 2.4 0 0 1 17 19H5a1.5 1.5 0 0 1-1.5-1.5z"/></svg>"#;

fn folder_icon(with_open: bool) -> Markup {
    html! {
        span."icon folder-icon" {
            (PreEscaped(FOLDER_CLOSED))
            @if with_open { (PreEscaped(FOLDER_OPEN)) }
        }
    }
}

const ICON_CARDS: &str = r#"<svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" fill="currentColor"><rect x="1.5" y="1.5" width="5.5" height="5.5" rx="1.2"/><rect x="9" y="1.5" width="5.5" height="5.5" rx="1.2"/><rect x="1.5" y="9" width="5.5" height="5.5" rx="1.2"/><rect x="9" y="9" width="5.5" height="5.5" rx="1.2"/></svg>"#;
const ICON_LIST: &str = r#"<svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" fill="currentColor"><rect x="1.5" y="2.5" width="13" height="2" rx="1"/><rect x="1.5" y="7" width="13" height="2" rx="1"/><rect x="1.5" y="11.5" width="13" height="2" rx="1"/></svg>"#;

/// What a row's actions menu needs besides the row: the form token and
/// where a change returns to.
struct Ctx<'a> {
    token: &'a str,
    back: &'a str,
}

/// An element id from a slug or path, unique per place on the page.
fn menu_id(kind: &str, place: &str, name: &str) -> String {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    format!("menu-{kind}-{place}-{safe}")
}

/// The button that opens a menu. Popover is declarative, so the menu opens
/// and closes with no script; CSS anchors it under the button where the
/// browser can, and the shared script places it otherwise.
fn menu_button(id: &str, label: &str) -> Markup {
    html! {
        button."ghost sm menu-btn" type="button" popovertarget=(id) aria-haspopup="menu"
               aria-label=(label) title=(label) style={ "anchor-name:--" (id) } { "\u{22EF}" }
    }
}

/// An app's actions: Open, Settings and, for an admin, Permissions and a
/// move that opens in place. Shown to an editor or more; for anyone else
/// the row's own link is the only thing to do.
fn app_menu(entry: &Entry, place: &str, ctx: &Ctx) -> Markup {
    let id = menu_id("app", place, &entry.slug);
    html! {
        @if entry.scope.is_some_and(|scope| scope >= Scope::Editor) {
            span."row-tools" {
                (menu_button(&id, &format!("Actions for {}", entry.slug)))
                div."menu" popover id=(id) role="menu" style={ "position-anchor:--" (id) } {
                    a role="menuitem" href={ "/p/" (entry.slug) "/" } target="_blank" rel="noopener" { "Open" }
                    a role="menuitem" href={ "/admin/apps/" (entry.app) } { "Settings" }
                    @if entry.scope == Some(Scope::Admin) {
                        a role="menuitem" href={ "/admin/apps/" (entry.app) "/access" } {
                            "Permissions" span."muted small" { " in Settings" }
                        }
                        details."menu-sub" {
                            summary role="menuitem" { "Move to project\u{2026}" }
                            form."menu-form" method="post" action="/admin/move" {
                                (admin::hidden("token", ctx.token)) (admin::hidden("app", &entry.app)) (admin::hidden("back", ctx.back))
                                (ui::combobox_full("folder", "/admin/projects/search", "Type a project, or / for the top level", "", None, &format!("-{id}")))
                                button."sm" type="submit" { "Move" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A project's actions, for an admin there: Open, Permissions, and a new
/// project inside it that opens in place.
fn project_menu(tree: &Tree, path: &str, place: &str, ctx: &Ctx) -> Markup {
    let id = menu_id("project", place, path);
    html! {
        @if tree.held.get(path) == Some(&Scope::Admin) {
            div."menu" popover id=(id) role="menu" style={ "position-anchor:--" (id) } {
                a role="menuitem" href=(browser_url(path)) { "Open" }
                a role="menuitem" href=(permissions_url(path)) { "Permissions" }
                details."menu-sub" {
                    summary role="menuitem" { "New project inside\u{2026}" }
                    form."menu-form" method="post" action="/admin/folder" {
                        (admin::hidden("token", ctx.token)) (admin::hidden("parent", path)) (admin::hidden("back", ctx.back))
                        input name="name" placeholder="Project name" aria-label="Project name" required pattern="[A-Za-z0-9_-]+" autocomplete="off";
                        button."sm" type="submit" { "Create" }
                    }
                }
                // Rename and remove change the parent's contents, so they
                // are offered to an admin there; the server checks again.
                @if admin_at_parent(tree, path) {
                    details."menu-sub" {
                        summary role="menuitem" { "Rename\u{2026}" }
                        form."menu-form" method="post" action="/admin/project" {
                            (admin::hidden("token", ctx.token)) (admin::hidden("action", "rename")) (admin::hidden("path", path)) (admin::hidden("back", ctx.back))
                            input name="name" value=(name_of(path)) aria-label="New name" required pattern="[A-Za-z0-9_-]+" autocomplete="off";
                            button."sm" type="submit" { "Rename" }
                        }
                    }
                    details."menu-sub" {
                        summary role="menuitem" { "Move to project\u{2026}" }
                        form."menu-form" method="post" action="/admin/project" {
                            (admin::hidden("token", ctx.token)) (admin::hidden("action", "move")) (admin::hidden("path", path)) (admin::hidden("back", ctx.back))
                            (ui::combobox_full("parent", "/admin/projects/search", "Type a project, or / for the top level", "", None, &format!("-{id}-move")))
                            button."sm" type="submit" { "Move" }
                        }
                    }
                    @if tree.is_empty_project(path) {
                        details."menu-sub" {
                            summary role="menuitem" { "Remove" }
                            form."menu-form" method="post" action="/admin/project" {
                                (admin::hidden("token", ctx.token)) (admin::hidden("action", "remove")) (admin::hidden("path", path)) (admin::hidden("back", ctx.back))
                                span."small" { "Remove? " }
                                button."sm danger" type="submit" { "Yes" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Whether the viewer is admin where `path` sits: its parent project, or
/// the top level, which only a site admin holds as admin.
fn admin_at_parent(tree: &Tree, path: &str) -> bool {
    match path.rsplit_once('/') {
        Some((parent, _)) => tree.held.get(parent) == Some(&Scope::Admin),
        None => tree.site_admin,
    }
}

/// The button for a project's menu, or nothing when it has none.
fn project_menu_button(tree: &Tree, path: &str, place: &str) -> Markup {
    let id = menu_id("project", place, path);
    html! {
        @if tree.held.get(path) == Some(&Scope::Admin) {
            span."row-tools" { (menu_button(&id, &format!("Actions for {}", name_of(path)))) }
        }
    }
}

/// New project in the title row: a small form under the button.
fn new_project_control(project: &str, token: &str, back: &str) -> Markup {
    html! {
        button."quiet" type="button" popovertarget="new-project" style="anchor-name:--new-project" { "New project" }
        div."menu menu-wide" popover id="new-project" style="position-anchor:--new-project" {
            form."menu-form" method="post" action="/admin/folder" {
                (admin::hidden("token", token)) (admin::hidden("parent", project)) (admin::hidden("back", back))
                label."small" for="new-project-name" { "New project" }
                input id="new-project-name" name="name" placeholder="Project name" required pattern="[A-Za-z0-9_-]+" autocomplete="off";
                button."sm" type="submit" { "Create" }
            }
        }
    }
}

fn app_row(entry: &Entry, show_path: bool, ctx: &Ctx) -> Markup {
    let menu = entry.scope.is_some_and(|scope| scope >= Scope::Editor).then(|| menu_id("app", "row", &entry.slug));
    html! {
        div."row app" data-slug=(entry.slug.to_lowercase()) data-title=(entry.label().to_lowercase()) data-menu=[menu] {
            span."chev-space" {}
            (icon_markup(&entry.icon))
            a."row-name" href={ "/p/" (entry.slug) } { (entry.label()) }
            span."row-meta" {
                @if show_path && !entry.project.is_empty() { code { (entry.project) } " · " }
                @if entry.title.is_some() { (entry.slug) }
                @if let Some(modified) = entry.modified { span."when" { (relative_time(modified)) } }
            }
            // Who may open it, for the people who could change that.
            @if entry.scope.is_some_and(|scope| scope >= Scope::Editor) {
                span."badge" title="General access" { (crate::platform::admin::gate_label(&entry.gate)) }
            }
            (app_menu(entry, "row", ctx))
        }
    }
}

/// A project row that opens in place. Its children are rendered inside, so
/// opening needs no script and no request; depth is as deep as the tree.
fn project_row(tree: &Tree, level: &str, path: &str, open: &BTreeSet<String>, ctx: &Ctx) -> Markup {
    let children = tree.children(path);
    let apps = tree.apps_in(path);
    // The row's path relative to the level on screen, which is how `?open=`
    // names it.
    let rel = if level.is_empty() { path.to_string() } else { path[level.len() + 1..].to_string() };
    let menu = (tree.held.get(path) == Some(&Scope::Admin)).then(|| menu_id("project", "row", path));
    html! {
        // The menu sits beside the row, not inside it: inside a closed
        // `details` it would not render, and inside `summary` a click on it
        // would fold the row.
        (project_menu(tree, path, "row", ctx))
        details."project" data-rel=(rel) open[open.contains(&rel)] data-slug=(name_of(path).to_lowercase()) data-title=(path.to_lowercase()) {
            summary."row" data-menu=[menu] {
                span."chev" aria-hidden="true" {}
                (folder_icon(true))
                a."row-name" href=(browser_url(path)) { (name_of(path)) }
                span."row-meta" { (count_label(tree.apps_below(path))) }
                (project_menu_button(tree, path, "row"))
            }
            div."children" {
                @for child in &children { (project_row(tree, level, child, open, ctx)) }
                @for entry in &apps { (app_row(entry, false, ctx)) }
                @if children.is_empty() && apps.is_empty() {
                    p."muted small empty-row" { "Nothing here yet." }
                }
            }
        }
    }
}

fn app_tile(entry: &Entry, show_path: bool, ctx: &Ctx) -> Markup {
    let menu = entry.scope.is_some_and(|scope| scope >= Scope::Editor).then(|| menu_id("app", "tile", &entry.slug));
    html! {
        li."tile" data-slug=(entry.slug.to_lowercase()) data-title=(entry.label().to_lowercase()) data-menu=[menu] {
            a."card" href={ "/p/" (entry.slug) } {
                (icon_markup(&entry.icon))
                span."meta" {
                    span."title" { (entry.label()) }
                    span."slug" {
                        @if show_path && !entry.project.is_empty() { (entry.project) "/" }
                        @if entry.title.is_some() || show_path { (entry.slug) }
                        @if let Some(modified) = entry.modified { span."when" { (relative_time(modified)) } }
                    }
                }
            }
            (app_menu(entry, "tile", ctx))
        }
    }
}

fn project_tile(tree: &Tree, path: &str, show_path: bool, ctx: &Ctx) -> Markup {
    let place = if show_path { "hit" } else { "tile" };
    let menu = (tree.held.get(path) == Some(&Scope::Admin)).then(|| menu_id("project", place, path));
    html! {
        li."tile" data-slug=(name_of(path).to_lowercase()) data-title=(path.to_lowercase()) data-menu=[menu] {
            a."card project-card" href=(browser_url(path)) {
                (folder_icon(false))
                span."meta" {
                    span."title" { (name_of(path)) }
                    span."slug" {
                        @if show_path { (path) " · " }
                        (count_label(tree.apps_below(path)))
                    }
                }
            }
            (project_menu_button(tree, path, place))
            (project_menu(tree, path, place, ctx))
        }
    }
}

/// The current level, in both views.
fn level(tree: &Tree, project: &str, shown: usize, open: &BTreeSet<String>, ctx: &Ctx) -> Markup {
    let children = tree.children(project);
    let apps = tree.apps_in(project);
    html! {
        @if children.is_empty() && apps.is_empty() {
            p."empty" {
                @if project.is_empty() { "There are no apps. Publish an app to show it here." }
                @else { "This project is empty. Publish an app into it, or move one here." }
            }
        } @else {
            div id="list" class="browse" data-shown=(shown) {
                div."rows list-only" {
                    @for child in &children { (project_row(tree, project, child, open, ctx)) }
                    @for entry in &apps { (app_row(entry, false, ctx)) }
                }
                ul."tiles cards-only" {
                    @for child in &children { (project_tile(tree, child, false, ctx)) }
                    @for entry in &apps { (app_tile(entry, false, ctx)) }
                }
            }
        }
    }
}

/// Everything below `project` that matches `q`, flat, each with its path.
fn results(tree: &Tree, project: &str, q: &str, ctx: &Ctx) -> Markup {
    let needle = q.to_lowercase();
    let projects: Vec<&str> = tree
        .projects
        .iter()
        .filter(|path| path.as_str() != project && users::prefix_covers(project, path))
        .filter(|path| path.to_lowercase().contains(&needle))
        .map(String::as_str)
        .collect();
    let mut apps: Vec<&Entry> = tree
        .entries
        .iter()
        .filter(|entry| users::prefix_covers(project, &entry.project))
        .filter(|entry| {
            entry.slug.to_lowercase().contains(&needle) || entry.label().to_lowercase().contains(&needle)
        })
        .collect();
    // Names that start with the query first.
    apps.sort_by_key(|entry| !entry.label().to_lowercase().starts_with(&needle) && !entry.slug.to_lowercase().starts_with(&needle));
    let total = projects.len() + apps.len();
    html! {
        p."muted small" { (total) " results for " strong { (q) } " · " a href=(browser_url(project)) { "Clear search" } }
        @if total == 0 {
            p."empty" { "Nothing matches." }
        } @else {
            div id="list" class="browse" {
                div."rows list-only" {
                    @for path in &projects {
                        div."row project-hit" data-slug=(name_of(path).to_lowercase()) data-title=(path.to_lowercase()) {
                            span."chev-space" {}
                            (folder_icon(false))
                            a."row-name" href=(browser_url(path)) { (name_of(path)) }
                            span."row-meta" { code { (path) } " · " (count_label(tree.apps_below(path))) }
                        }
                    }
                    @for entry in &apps { (app_row(entry, true, ctx)) }
                }
                ul."tiles cards-only" {
                    @for path in &projects { (project_tile(tree, path, true, ctx)) }
                    @for entry in &apps { (app_tile(entry, true, ctx)) }
                }
            }
        }
    }
}

