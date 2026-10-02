//! Accounts and access, for whoever runs the site.
//!
//! This is a platform route rather than a published app on purpose: an app
//! cannot read the account database — that isolation is the thing every other
//! guarantee rests on — so an "admin app" could only exist by breaking it.
//!
//! Every action here is a POST carrying a token derived from the caller's own
//! session. Cookies are `SameSite=Lax`, which already refuses a cross-site
//! POST; the token is what stops a page on *this* origin from acting as the
//! admin who happens to be visiting it.

use crate::{
    accounts::users::{self, User},
    config::Config,
    content::{slug::valid_slug, store::collect_slugs, store::read_meta},
};
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use maud::{html, Markup};
use serde::Deserialize;
use std::sync::Arc;

/// Resolves an admin from the request, or the response to send instead.
async fn require_admin(config: &Arc<Config>, headers: &HeaderMap) -> Result<User, Response> {
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
fn form_token(config: &Config, user: &User) -> String {
    users::derive_form_token(config, &user.id)
}

fn check_form_token(config: &Config, user: &User, presented: &str) -> bool {
    // Constant-time is overkill for a value the holder already knows, but
    // comparing lengths first avoids the obvious early-exit.
    let expected = form_token(config, user);
    expected.len() == presented.len() && expected == presented
}

/// The rail every admin page shares. `active` names the current page so the
/// link to it can be marked rather than followed.
fn sidebar(active: &str) -> Markup {
    html! {
        div."brand" { "Admin" }
        a."active"[active == "accounts"] href="/admin" { "Accounts" }
        a."active"[active == "apps"] href="/admin/apps" { "Apps" }
        a."active"[active == "access"] href="/admin/access" { "Access" }
        a."active"[active == "exports"] href="/admin/exports" { "Exports" }
        div."spacer" {
            a href="/" { "Pages" }
            a href="/auth/logout" { "Sign out" }
        }
    }
}

/// Wraps a section's content, so each page differs only in what it renders.
fn admin_page(active: &str, heading: &str, admin: &User, body: Markup) -> Response {
    let markup = crate::ui::shell(
        heading,
        sidebar(active),
        html! {
            h1 { (heading) }
            p."muted" { "Signed in as " (admin.email) }
            (body)
        },
        None,
    );
    ([no_store()], Html(markup.into_string())).into_response()
}

pub async fn page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
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

    let token = form_token(&config, &admin);
    admin_page("accounts", "Accounts", &admin, render_accounts(&accounts, &token))
}

pub async fn apps_page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };

    // Apps, with the gate each one is behind.
    let mut apps = Vec::new();
    let mut slugs = Vec::new();
    collect_slugs(&config.data_dir, String::new(), &mut slugs).await;
    for slug in slugs {
        let app = slug.split('/').next().unwrap_or(&slug).to_string();
        if apps.iter().any(|(name, _)| name == &app) {
            continue;
        }
        let gate = read_meta(&config, &app).await.gate;
        apps.push((app, gate));
    }
    apps.sort();

    let token = form_token(&config, &admin);
    admin_page("apps", "Apps", &admin, render_apps(&apps, &token))
}

pub async fn access_page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };

    let grants = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || users::list_grants(&config))
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default()
    };

    let token = form_token(&config, &admin);
    admin_page("access", "Access", &admin, render_access(&grants, &token))
}

fn render_accounts(accounts: &[users::Account], token: &str) -> Markup {
    html! {
        @if accounts.is_empty() {
            p."muted" { "No accounts yet." }
        } @else {
            table {
                thead { tr { th { "Email" } th { "Created" } th { "Admin" } th { "Status" } th {} } }
                tbody {
                    @for account in accounts {
                        tr {
                            td { (account.email) }
                            td."muted" { (account.created) }
                            td { @if account.is_admin { "yes" } @else { "" } }
                            td { @if account.is_active { "active" } @else { "disabled" } }
                            td {
                                form."row" method="post" action="/admin/active" {
                                    input type="hidden" name="token" value=(token);
                                    input type="hidden" name="email" value=(account.email);
                                    input type="hidden" name="active"
                                          value=(if account.is_active { "0" } else { "1" });
                                    @if account.is_active {
                                        button."danger" type="submit" { "Disable" }
                                    } @else {
                                        button type="submit" { "Enable" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        form."row" method="post" action="/admin/users" {
            input type="hidden" name="token" value=(token);
            input name="email" type="email" placeholder="Email" required;
            input name="password" type="password" placeholder="Password (8+)" required;
            label { input type="checkbox" name="admin" value="1"; " admin" }
            button type="submit" { "Add account" }
        }
    }
}

fn render_apps(apps: &[(String, String)], token: &str) -> Markup {
    html! {
        @if apps.is_empty() {
            p."muted" { "Nothing published yet." }
        } @else {
            table {
                thead { tr { th { "App" } th { "Gate" } th {} } }
                tbody {
                    @for (app, gate) in apps {
                        tr {
                            td { a href={ "/p/" (app) "/" } { (app) } }
                            td { code { (gate) } }
                            td {
                                form."row" method="post" action="/admin/gate" {
                                    input type="hidden" name="token" value=(token);
                                    input type="hidden" name="app" value=(app);
                                    select name="gate" {
                                        @for option in ["public", "authenticated", "granted"] {
                                            option value=(option) selected[option == gate] { (option) }
                                        }
                                    }
                                    button type="submit" { "Set" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_access(grants: &[(String, String)], token: &str) -> Markup {
    html! {
        p."muted" { "Only matters for apps gated " code { "granted" } "." }
        @if grants.is_empty() {
            p."muted" { "No grants." }
        } @else {
            table {
                thead { tr { th { "App" } th { "Account" } th {} } }
                tbody {
                    @for (app, email) in grants {
                        tr {
                            td { (app) }
                            td { (email) }
                            td {
                                form."row" method="post" action="/admin/access" {
                                    input type="hidden" name="token" value=(token);
                                    input type="hidden" name="app" value=(app);
                                    input type="hidden" name="email" value=(email);
                                    input type="hidden" name="allow" value="0";
                                    button."danger" type="submit" { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }

        form."row" method="post" action="/admin/access" {
            input type="hidden" name="token" value=(token);
            input type="hidden" name="allow" value="1";
            input name="app" placeholder="App" required;
            input name="email" type="email" placeholder="Account email" required;
            button type="submit" { "Grant" }
        }
    }
}

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

pub async fn exports_page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let apps = app_names(&config).await;
    let tokens = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || crate::platform::export::list_all(&config))
            .await
            .unwrap_or_default()
    };
    let token = form_token(&config, &admin);
    admin_page("exports", "Exports", &admin, render_exports(&config, &apps, &tokens, &token, None))
}

fn ago(seconds: u64) -> String {
    let elapsed = crate::platform::export::seconds_since(seconds);
    match elapsed {
        s if s < 90 => "just now".to_string(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

/// `fresh` is a token minted by the request that rendered this page: the one
/// time it is ever shown.
fn render_exports(
    config: &Config,
    apps: &[String],
    tokens: &[(String, crate::platform::export::ExportToken)],
    form_token: &str,
    fresh: Option<(&str, &str)>,
) -> Markup {
    html! {
        p."muted" {
            "A read-only copy of one app's database, for a reporting tool that pulls SQLite over HTTP. "
            "Each token opens one app and nothing else; revoke it here when the tool goes."
        }
        @if let Some((app, token)) = fresh {
            section {
                h2 { "New token for " (app) }
                p { "Copy it now. It is not stored and will not be shown again." }
                pre { code { (token) } }
                p."muted" {
                    "Point the tool at " code { (crate::platform::export::export_url(config, app)) }
                    " with " code { "Authorization: Bearer " } "that token."
                }
            }
        }
        @if tokens.is_empty() {
            p."muted" { "No export tokens." }
        } @else {
            table {
                thead { tr { th { "App" } th { "Label" } th { "Created" } th { "Last used" } th {} } }
                tbody {
                    @for (app, entry) in tokens {
                        tr {
                            td { (app) }
                            td { (entry.label) " " span."muted" { "(" (entry.id) ")" } }
                            td."muted" { (ago(entry.created_at)) }
                            td."muted" {
                                @match entry.last_used {
                                    Some(at) => (ago(at)),
                                    None => "never",
                                }
                            }
                            td {
                                form."row" method="post" action="/admin/exports" {
                                    input type="hidden" name="token" value=(form_token);
                                    input type="hidden" name="action" value="revoke";
                                    input type="hidden" name="app" value=(app);
                                    input type="hidden" name="id" value=(entry.id);
                                    button."danger" type="submit" { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }

        form."row" method="post" action="/admin/exports" {
            input type="hidden" name="token" value=(form_token);
            input type="hidden" name="action" value="create";
            @if apps.is_empty() {
                input name="app" placeholder="App" required;
            } @else {
                select name="app" required {
                    @for app in apps { option value=(app) { (app) } }
                }
            }
            input name="label" placeholder="What will hold it, e.g. reporting" required;
            button type="submit" { "Create token" }
        }
    }
}

#[derive(Deserialize)]
pub struct ExportChange {
    token: String,
    action: String,
    app: String,
    label: Option<String>,
    id: Option<String>,
}

pub async fn change_export(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<ExportChange>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    if !check_form_token(&config, &admin, &form.token) {
        return (StatusCode::FORBIDDEN, "stale form; reload and try again").into_response();
    }

    match form.action.as_str() {
        "create" => {
            let label = form.label.unwrap_or_default();
            let (config2, app) = (config.clone(), form.app.clone());
            let outcome = tokio::task::spawn_blocking(move || {
                crate::platform::export::create(&config2, &app, &label)
            })
            .await;
            match outcome {
                Ok(Ok((_, token))) => {
                    tracing::info!(admin = %admin.email, app = %form.app, "export token created");
                    // Rendered, not redirected: the token exists in this
                    // response and nowhere else.
                    let apps = app_names(&config).await;
                    let tokens = {
                        let config = config.clone();
                        tokio::task::spawn_blocking(move || crate::platform::export::list_all(&config))
                            .await
                            .unwrap_or_default()
                    };
                    let form_token = form_token(&config, &admin);
                    admin_page(
                        "exports",
                        "Exports",
                        &admin,
                        render_exports(&config, &apps, &tokens, &form_token, Some((&form.app, &token))),
                    )
                }
                Ok(Err(message)) => (StatusCode::BAD_REQUEST, message).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not create the token").into_response(),
            }
        }
        "revoke" => {
            let id = form.id.unwrap_or_default();
            let (config2, app) = (config.clone(), form.app.clone());
            let outcome = tokio::task::spawn_blocking(move || {
                crate::platform::export::revoke(&config2, &app, &id)
            })
            .await;
            match outcome {
                Ok(Ok(())) => {
                    tracing::info!(admin = %admin.email, app = %form.app, "export token revoked");
                    Redirect::to("/admin/exports").into_response()
                }
                Ok(Err(message)) => (StatusCode::BAD_REQUEST, message).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not revoke the token").into_response(),
            }
        }
        _ => (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    }
}

#[derive(Deserialize)]
pub struct NewAccount {
    token: String,
    email: String,
    password: String,
    admin: Option<String>,
}

pub async fn add_account(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<NewAccount>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    if !check_form_token(&config, &admin, &form.token) {
        return (StatusCode::FORBIDDEN, "stale form; reload and try again").into_response();
    }

    let is_admin = form.admin.is_some();
    let config2 = config.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        users::sign_up_as(&config2, &form.email, &form.password, is_admin)
    })
    .await;

    match outcome {
        Ok(Ok(_)) => Redirect::to("/admin").into_response(),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not add account").into_response(),
    }
}

#[derive(Deserialize)]
pub struct ActiveChange {
    token: String,
    email: String,
    active: String,
}

pub async fn change_active(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<ActiveChange>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    if !check_form_token(&config, &admin, &form.token) {
        return (StatusCode::FORBIDDEN, "stale form; reload and try again").into_response();
    }
    // Disabling yourself would lock the last admin out of this page.
    if form.email.trim().eq_ignore_ascii_case(&admin.email) && form.active != "1" {
        return (StatusCode::BAD_REQUEST, "you cannot disable your own account").into_response();
    }

    let active = form.active == "1";
    let config2 = config.clone();
    let outcome =
        tokio::task::spawn_blocking(move || users::set_active(&config2, &form.email, active)).await;

    match outcome {
        Ok(Ok(())) => Redirect::to("/admin").into_response(),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not change the account").into_response(),
    }
}

#[derive(Deserialize)]
pub struct AccessChange {
    token: String,
    app: String,
    email: String,
    allow: String,
}

pub async fn change_access(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<AccessChange>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    if !check_form_token(&config, &admin, &form.token) {
        return (StatusCode::FORBIDDEN, "stale form; reload and try again").into_response();
    }

    let allow = form.allow == "1";
    let config2 = config.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        if allow {
            users::grant(&config2, &form.email, &form.app, "viewer")
        } else {
            users::revoke(&config2, &form.email, &form.app)
        }
    })
    .await;

    match outcome {
        Ok(Ok(())) => Redirect::to("/admin/access").into_response(),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not change access").into_response(),
    }
}

#[derive(Deserialize)]
pub struct GateChange {
    token: String,
    app: String,
    gate: String,
}

pub async fn change_gate(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<GateChange>,
) -> Response {
    let admin = match require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    if !check_form_token(&config, &admin, &form.token) {
        return (StatusCode::FORBIDDEN, "stale form; reload and try again").into_response();
    }
    if !valid_slug(&form.app) {
        return (StatusCode::BAD_REQUEST, "invalid app name").into_response();
    }
    if !matches!(
        form.gate.as_str(),
        "public" | "authenticated" | "granted"
    ) {
        return (StatusCode::BAD_REQUEST, "unknown gate").into_response();
    }

    let mut meta = read_meta(&config, &form.app).await;
    meta.gate = form.gate;
    match crate::content::store::write_meta(&config, &form.app, &meta).await {
        Ok(()) => Redirect::to("/admin/apps").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not set gate").into_response(),
    }
}

/// Sent on every admin response so the browser will not cache a page listing
/// accounts.
pub fn no_store() -> (header::HeaderName, &'static str) {
    (header::CACHE_CONTROL, "no-store")
}

