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

    let grants = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || users::list_grants(&config))
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default()
    };

    let token = form_token(&config, &admin);
    (
        [no_store()],
        Html(render(&admin, &accounts, &apps, &grants, &token).into_string()),
    )
        .into_response()
}

fn render(
    admin: &User,
    accounts: &[users::Account],
    apps: &[(String, String)],
    grants: &[(String, String)],
    token: &str,
) -> Markup {
    crate::ui::page(
        "Admin",
        html! {
            h1 { "Admin" }
            p."muted" { "Signed in as " (admin.email) " · " a href="/auth/logout" { "sign out" } }

            section {
                    h2 { "Accounts" }
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

            section {
                    h2 { "Apps" }
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

            section {
                    h2 { "Access" }
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
        },
        None,
    )
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
        Ok(Ok(())) => Redirect::to("/admin").into_response(),
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
        Ok(()) => Redirect::to("/admin").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not set gate").into_response(),
    }
}

/// Sent on every admin response so the browser will not cache a page listing
/// accounts.
pub fn no_store() -> (header::HeaderName, &'static str) {
    (header::CACHE_CONTROL, "no-store")
}

