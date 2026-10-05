//! The signed-in person's own page: who they are to this site, how they sign
//! in, and a new password if they have one. For every account, admin or not.
//!
//! Lives beside the admin pages because it wears the same shell and uses the
//! same flash, but it is not an admin page: it shows a person their own row
//! and lets them change exactly one thing about it. A reset for someone who
//! has forgotten their password is an admin's setup link, since there is no
//! mailer here to send one.

use crate::{
    accounts::users::{self, User},
    config::Config,
    platform::admin::{clear_flash, redirect_flash, sidebar, take_flash},
    ui,
};
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use maud::html;
use serde::Deserialize;
use std::sync::Arc;

/// Who is asking, from the site cookie, or where to send them instead.
async fn require_user(config: &Arc<Config>, headers: &HeaderMap) -> Result<User, Response> {
    match users::current_site_user(config, headers).await {
        Some(user) => Ok(user),
        None => Err(Redirect::to("/auth/login?next=/account").into_response()),
    }
}

fn form_token(config: &Config, user: &User) -> String {
    users::derive_form_token(config, &user.id)
}

pub async fn page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let user = match require_user(&config, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (has_password, providers) = {
        let (config, id) = (config.clone(), user.id.clone());
        tokio::task::spawn_blocking(move || {
            (users::has_password(&config, &id), users::identities_for(&config, &id))
        })
        .await
        .unwrap_or((false, Vec::new()))
    };
    let token = form_token(&config, &user);
    let flash = take_flash(&headers);

    let markup = ui::shell(
        "Your account",
        sidebar("account", Some(&user)),
        html! {
            div."title-row" {
                div {
                    h1 { "Your account" }
                    p."muted" { (user.email) }
                }
                div."actions" {
                    @if user.is_admin { span."badge solid" { "admin" } } @else { span."badge" { "visitor" } }
                }
            }
            (ui::flash(flash.as_ref()))

            (ui::panel("How you sign in", None, html! {
                dl."kv" {
                    dt { "Email" } dd { (user.email) }
                    dt { "Ways in" }
                    dd {
                        @if !has_password && providers.is_empty() {
                            "None set up yet. Ask an admin for a setup link."
                        }
                        @if has_password { "A password" }
                        @for (i, provider) in providers.iter().enumerate() {
                            @if i > 0 || has_password { ", " }
                            "Sign in with " (provider)
                        }
                    }
                }
            }))

            @if has_password {
                (ui::panel(
                    "Change password",
                    Some("Your other sessions end when you do. The one you are using now stays signed in."),
                    html! {
                        form method="post" action="/account/password" {
                            input type="hidden" name="token" value=(token);
                            div."field" {
                                label for="current" { "Current password" }
                                input id="current" name="current" type="password"
                                      autocomplete="current-password" required;
                            }
                            div."field" {
                                label for="new" { "New password" }
                                input id="new" name="new" type="password"
                                      autocomplete="new-password" minlength="8" required;
                                p."help" { "At least 8 characters." }
                            }
                            div."field" {
                                label for="confirm" { "New password again" }
                                input id="confirm" name="confirm" type="password"
                                      autocomplete="new-password" minlength="8" required;
                            }
                            div."actions end" { button type="submit" { "Change password" } }
                        }
                    },
                ))
            } @else if !providers.is_empty() {
                p."muted" {
                    "You sign in through "
                    @for (i, provider) in providers.iter().enumerate() {
                        @if i > 0 { " and " }
                        (provider)
                    }
                    ", so there is no password here to change."
                }
            }
        },
        None,
    );
    let mut response = (
        [(header::CACHE_CONTROL, "no-store")],
        Html(markup.into_string()),
    )
        .into_response();
    if flash.is_some() {
        let (name, value) = clear_flash();
        if let Ok(value) = value.parse() {
            response.headers_mut().append(name, value);
        }
    }
    response
}

#[derive(Deserialize)]
pub struct PasswordChange {
    token: String,
    current: String,
    new: String,
    confirm: String,
}

pub async fn change_password(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<PasswordChange>,
) -> Response {
    let user = match require_user(&config, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let expected = form_token(&config, &user);
    if expected.len() != form.token.len() || expected != form.token {
        return (StatusCode::FORBIDDEN, "stale form; reload and try again").into_response();
    }
    if form.new != form.confirm {
        return redirect_flash("/account", false, "The two new passwords do not match.");
    }
    // The session to keep is the one that sent this form.
    let Some(session) = users::token_from_cookies(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
    ) else {
        return Redirect::to("/auth/login?next=/account").into_response();
    };

    let (worker, id, email) = (config.clone(), user.id.clone(), user.email.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        users::change_password(&worker, &id, &form.current, &form.new, &session)
    })
    .await;
    match outcome {
        Ok(Ok(())) => {
            tracing::info!(%email, "password changed");
            redirect_flash("/account", true, "Password changed. Your other sessions have been signed out.")
        }
        Ok(Err(message)) => {
            tracing::warn!(%email, %message, "password change refused");
            redirect_flash("/account", false, message)
        }
        Err(_) => redirect_flash("/account", false, "Could not change the password."),
    }
}
