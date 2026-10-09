//! The signed-in person's own page: who they are to this site, how they sign
//! in, and a new password if they have one. For every account, admin or not.
//!
//! Lives beside the admin pages because it wears the same shell and uses the
//! same flash, but it is not an admin page: it shows a person their own row
//! and lets them change exactly one thing about it. A reset for someone who
//! has forgotten their password is an admin's setup link, since there is no
//! mailer here to send one.

use crate::{
    accounts::{
        mfa,
        users::{self, User},
    },
    config::Config,
    platform::admin::{clear_flash, redirect_flash, sidebar, take_flash},
    ui,
};
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use maud::{html, Markup, PreEscaped};
use serde::Deserialize;
use std::sync::Arc;

/// Who is asking, from the site cookie, or where to send them instead.
async fn require_user(config: &Arc<Config>, headers: &HeaderMap) -> Result<User, Response> {
    match users::current_site_user(config, headers).await {
        Some(user) => Ok(user),
        None => Err(Redirect::to("/auth/login?next=/account").into_response()),
    }
}

/// The raw site session token the request carries.
fn site_token(config: &Config, headers: &HeaderMap) -> Option<String> {
    users::token_from_cookies(config, headers.get(header::COOKIE).and_then(|v| v.to_str().ok()))
}

fn form_token(config: &Config, user: &User) -> String {
    users::derive_form_token(config, &user.id)
}

pub async fn page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let user = match require_user(&config, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (has_password, providers, two_step, setup_secret) = {
        let (config, id, session) = (config.clone(), user.id.clone(), site_token(&config, &headers).unwrap_or_default());
        tokio::task::spawn_blocking(move || {
            (
                users::has_password(&config, &id),
                users::identities_for(&config, &id),
                mfa::status(&config, &id),
                mfa::setup_secret(&config, &id, &session),
            )
        })
        .await
        .unwrap_or_default()
    };
    let required = config.mfa.policy.requires(&user);
    let token = form_token(&config, &user);
    let flash = take_flash(&headers);
    let manages = crate::platform::admin::manages_something(&config, &user).await;

    let markup = ui::shell(
        "Your account",
        sidebar("account", Some(&user), manages),
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

            (ui::panel("Sign-in methods", None, html! {
                dl."kv" {
                    dt { "Email" } dd { (user.email) }
                    dt { "Methods" }
                    dd {
                        @if !has_password && providers.is_empty() {
                            "No sign-in method is set. Ask an admin for a setup link."
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
                    Some("When you change the password, your other sessions end. This session stays signed in."),
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
                                p."help" { "Enter at least 8 characters." }
                            }
                            div."field" {
                                label for="confirm" { "Confirm new password" }
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
                    ". This account has no password."
                }
            }

            (two_step_panel(&config, &user, &token, &two_step, setup_secret.as_deref(), required, has_password))
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
        return (StatusCode::FORBIDDEN, "The form is out of date. Reload the page and try again.").into_response();
    }
    if form.new != form.confirm {
        return redirect_flash("/account", false, "The new passwords do not match.");
    }
    // The session to keep is the one that sent this form.
    let Some(session) = users::token_from_cookies(
        &config,
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
            revoke_clients(&config, &user.id, &email, "password changed").await;
            redirect_flash("/account", true, "The password is changed. Your other sessions and connected MCP clients are signed out.")
        }
        Ok(Err(message)) => {
            tracing::warn!(%email, %message, "password change refused");
            redirect_flash("/account", false, message)
        }
        Err(_) => redirect_flash("/account", false, "The password was not changed."),
    }
}

// --- two-step sign-in ----------------------------------------------------

/// The QR code and the key for setup, with the steps to follow.
fn setup_steps(config: &Config, email: &str, secret: &str) -> Markup {
    let uri = mfa::otpauth_uri(config, email, secret);
    html! {
        ol style="margin: 0 0 .75rem; padding-left: 1.25rem" {
            li { "Open an authenticator app on your phone, for example Google Authenticator, Microsoft Authenticator or 1Password." }
            li { "Scan this QR code with the app." }
            li { "Enter the 6-digit code that the app shows." }
        }
        div."qr" role="img" aria-label="QR code for your authenticator app" { (PreEscaped(mfa::qr_svg(&uri))) }
        p."muted" style="margin: 0 0 .4rem; font-size: .85rem" { "If you cannot scan the code, enter this key in the app:" }
        (ui::secret("mfa-key", &mfa::grouped(secret)))
    }
}

/// Recovery codes, shown once, with copy and download.
fn recovery_codes(email: &str, codes: &[String]) -> Markup {
    let text = format!(
        "Recovery codes for {email}\n\n{}\n\nEach code works one time. Keep this file in a safe place.\n",
        codes.join("\n")
    );
    let download = format!("data:text/plain;charset=utf-8,{}", urlencoding::encode(&text));
    html! {
        p {
            "Save these recovery codes in a safe place. If you do not have your phone, use a recovery code to sign in. "
            "Each code works one time. You will not see these codes again."
        }
        ul."codes" id="recovery-codes" {
            @for code in codes { li { (code) } }
        }
        div."actions" style="margin-bottom: .75rem" {
            button."quiet" type="button" data-copy="recovery-codes" { "Copy codes" }
            a."btn quiet" href=(download) download="recovery-codes.txt" { "Download as .txt" }
        }
    }
}

fn code_field(id: &str, label: &str) -> Markup {
    html! {
        div."field" {
            label for=(id) { (label) }
            input id=(id) name="code" type="text" inputmode="numeric" autocomplete="one-time-code"
                  placeholder="123456" maxlength="20" required;
        }
    }
}

fn two_step_panel(
    config: &Config,
    user: &User,
    token: &str,
    status: &mfa::Status,
    setup_secret: Option<&str>,
    required: bool,
    has_password: bool,
) -> Markup {
    let hidden_token = html! { input type="hidden" name="token" value=(token); };
    let description = "After your password, you also enter a code from an authenticator app on your phone.";
    html! {
        section id="two-step" {
            @if status.enabled {
                (ui::panel("Two-step sign-in", Some(description), html! {
                    dl."kv" style="margin-bottom: 1rem" {
                        dt { "Status" } dd { span."badge ok" { "On" } }
                        dt { "Recovery codes" } dd { (status.recovery_left) " of 10 left" }
                    }
                    details style="margin-bottom: .75rem" {
                        summary { "Get new recovery codes" }
                        form method="post" action="/account/mfa/recovery" style="margin-top: .75rem" {
                            (hidden_token)
                            p."muted" style="font-size: .85rem" { "Your old recovery codes stop working." }
                            (code_field("recovery-code", "Code from your authenticator app"))
                            div."actions end" { button type="submit" { "Get new codes" } }
                        }
                    }
                    @if required {
                        p."muted" style="font-size: .85rem" { "This site requires two-step sign-in for your account. You cannot turn it off." }
                    } @else {
                        details {
                            summary { "Turn off two-step sign-in" }
                            form method="post" action="/account/mfa/off" style="margin-top: .75rem" {
                                (hidden_token)
                                (code_field("off-code", "Code from your authenticator app, or a recovery code"))
                                div."actions end" { button."danger" type="submit" { "Turn off" } }
                            }
                        }
                    }
                }))
            } @else if let (true, Some(secret)) = (status.setting_up, setup_secret) {
                (ui::panel("Two-step sign-in", Some(description), html! {
                    (setup_steps(config, &user.email, secret))
                    form method="post" action="/account/mfa/confirm" {
                        (hidden_token)
                        (code_field("confirm-code", "Code from your authenticator app"))
                        @if has_password {
                            div."field" {
                                label for="confirm-password" { "Your password" }
                                input id="confirm-password" name="password" type="password"
                                      autocomplete="current-password" required;
                                p."help" { "Your other sessions end when two-step sign-in turns on." }
                            }
                        }
                        div."actions end" { button type="submit" { "Turn on" } }
                    }
                    form method="post" action="/account/mfa/cancel" {
                        (hidden_token)
                        button."quiet sm" type="submit" { "Cancel setup" }
                    }
                }))
            } @else {
                (ui::panel("Two-step sign-in", Some(description), html! {
                    p { "Two-step sign-in is off." }
                    @if required {
                        p."muted" style="font-size: .85rem" { "This site requires two-step sign-in for your account. You must set it up." }
                    }
                    @if !has_password && !config.mfa.for_providers {
                        p."muted" style="font-size: .85rem" {
                            "You sign in through a provider, which asks for its own second step. Two-step sign-in here applies only when you sign in with a password."
                        }
                    }
                    form method="post" action="/account/mfa/start" {
                        (hidden_token)
                        div."actions end" { button type="submit" { "Set up two-step sign-in" } }
                    }
                }))
            }
        }
    }
}

/// The checks every two-step form on the account page shares: a signed-in
/// person, and the form token their page carried.
async fn checked_user(config: &Arc<Config>, headers: &HeaderMap, token: &str) -> Result<User, Response> {
    let user = require_user(config, headers).await?;
    let expected = form_token(config, &user);
    if expected.len() != token.len() || expected != token {
        tracing::warn!(email = %user.email, "two-step form refused: the form token does not match");
        return Err((StatusCode::FORBIDDEN, "The form is out of date. Reload the page and try again.").into_response());
    }
    Ok(user)
}

/// A page on the account shell, never cached: it carries recovery codes.
async fn account_shell(config: &Arc<Config>, user: &User, title: &str, body: Markup) -> Response {
    let manages = crate::platform::admin::manages_something(config, user).await;
    let markup = ui::shell(title, sidebar("account", Some(user), manages), body, None);
    ([(header::CACHE_CONTROL, "no-store")], Html(markup.into_string())).into_response()
}

/// Ends every connection a client holds for this account. Turning two-step
/// sign-in on does it, so a client connected with a password alone must
/// sign in again with a code; so do a new password and an admin's reset,
/// which close whatever the old password or the lost phone opened, a setup
/// link, which is a reset, and an account turned off, so turning it back on
/// does not revive them. `why` says which, for the log.
pub(crate) async fn revoke_clients(config: &Arc<Config>, user_id: &str, email: &str, why: &str) {
    let (worker, id) = (config.clone(), user_id.to_string());
    match tokio::task::spawn_blocking(move || crate::platform::oauth_store::revoke_for_user(&worker, &id)).await {
        Ok(Ok(count)) => tracing::info!(%email, revoked = count, "{why}: OAuth tokens of this account revoked"),
        Ok(Err(error)) => tracing::warn!(%email, %error, "{why}: OAuth tokens were not revoked"),
        Err(_) => tracing::warn!(%email, "{why}: OAuth tokens were not revoked"),
    }
}

#[derive(Deserialize)]
pub struct NewPassword {
    token: String,
    password: String,
}

/// `POST /auth/setup`: a setup link spent. Here rather than in `accounts`
/// because the link is also the reset for a forgotten or leaked password,
/// and the clients the old password connected are the platform's to end.
pub async fn setup_submit(State(config): State<Arc<Config>>, Form(form): Form<NewPassword>) -> Response {
    let worker = config.clone();
    let outcome = tokio::task::spawn_blocking(move || users::accept_invite(&worker, &form.token, &form.password)).await;
    match outcome {
        // Signed in on the spot: having just proved they hold the link and
        // chosen the password, asking them to type it again is theatre. A
        // code, though, is still owed if the account has two-step sign-in.
        Ok(Ok(user)) => {
            revoke_clients(&config, &user.id, &user.email, "password set by link").await;
            let next = if user.is_admin { "/admin" } else { "/" };
            mfa::sign_in(&config, user, mfa::Primary::Password, next).await
        }
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, message).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "The password was not set.").into_response(),
    }
}

#[derive(Deserialize)]
pub struct TokenOnly {
    token: String,
}

#[derive(Deserialize)]
pub struct WithCode {
    token: String,
    code: String,
}

#[derive(Deserialize)]
pub struct Confirm {
    token: String,
    code: String,
    /// Required of an account with a password: turning it on ends every
    /// other session, so a stolen cookie alone must not be able to put the
    /// thief's phone on the account and sign the owner out.
    password: Option<String>,
}

pub async fn mfa_start(State(config): State<Arc<Config>>, headers: HeaderMap, Form(form): Form<TokenOnly>) -> Response {
    let user = match checked_user(&config, &headers, &form.token).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let Some(session) = site_token(&config, &headers) else {
        return Redirect::to("/auth/login?next=/account").into_response();
    };
    let (worker, id) = (config.clone(), user.id.clone());
    match tokio::task::spawn_blocking(move || mfa::begin_setup(&worker, &id, &session)).await {
        Ok(Ok(_)) => Redirect::to("/account#two-step").into_response(),
        Ok(Err(message)) => redirect_flash("/account", false, message),
        Err(_) => redirect_flash("/account", false, "Setup did not start."),
    }
}

pub async fn mfa_cancel(State(config): State<Arc<Config>>, headers: HeaderMap, Form(form): Form<TokenOnly>) -> Response {
    let user = match checked_user(&config, &headers, &form.token).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (worker, id) = (config.clone(), user.id.clone());
    let _ = tokio::task::spawn_blocking(move || mfa::cancel_setup(&worker, &id)).await;
    redirect_flash("/account", true, "Setup is cancelled. Two-step sign-in is off.")
}

pub async fn mfa_confirm(State(config): State<Arc<Config>>, headers: HeaderMap, Form(form): Form<Confirm>) -> Response {
    let user = match checked_user(&config, &headers, &form.token).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    // The session to keep is the one that sent this form.
    let Some(session) = site_token(&config, &headers) else {
        return Redirect::to("/auth/login?next=/account").into_response();
    };
    let (worker, who) = (config.clone(), user.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        if users::has_password(&worker, &who.id)
            && !users::password_matches(&worker, &who.id, form.password.as_deref().unwrap_or_default())
        {
            tracing::warn!(email = %who.email, "two-step setup refused: the password is not correct");
            return Err("The password is not correct.".to_string());
        }
        mfa::confirm_setup(&worker, &who, &form.code, &session)
    })
    .await;
    match outcome {
        Ok(Ok(codes)) => {
            revoke_clients(&config, &user.id, &user.email, "two-step sign-in on").await;
            account_shell(&config, &user, "Two-step sign-in is on", html! {
                div."title-row" { div { h1 { "Two-step sign-in is on" } p."muted" { (user.email) } } }
                div."flash ok" role="status" {
                    span { "Two-step sign-in is on. Your other sessions are signed out, and connected MCP clients must sign in again." }
                }
                (ui::panel("Recovery codes", None, html! {
                    (recovery_codes(&user.email, &codes))
                    a."btn" href="/account" { "Done" }
                }))
            })
            .await
        }
        Ok(Err(message)) => redirect_flash("/account#two-step", false, message),
        Err(_) => redirect_flash("/account", false, "Two-step sign-in was not turned on."),
    }
}

pub async fn mfa_recovery(State(config): State<Arc<Config>>, headers: HeaderMap, Form(form): Form<WithCode>) -> Response {
    let user = match checked_user(&config, &headers, &form.token).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (worker, who) = (config.clone(), user.clone());
    let outcome = tokio::task::spawn_blocking(move || mfa::regenerate_recovery_codes(&worker, &who, &form.code)).await;
    match outcome {
        Ok(Ok(codes)) => {
            account_shell(&config, &user, "New recovery codes", html! {
                div."title-row" { div { h1 { "New recovery codes" } p."muted" { (user.email) } } }
                (ui::panel("Recovery codes", Some("Your old recovery codes do not work now."), html! {
                    (recovery_codes(&user.email, &codes))
                    a."btn" href="/account" { "Done" }
                }))
            })
            .await
        }
        Ok(Err(message)) => redirect_flash("/account#two-step", false, message),
        Err(_) => redirect_flash("/account", false, "No new recovery codes were made."),
    }
}

pub async fn mfa_off(State(config): State<Arc<Config>>, headers: HeaderMap, Form(form): Form<WithCode>) -> Response {
    let user = match checked_user(&config, &headers, &form.token).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (worker, who) = (config.clone(), user.clone());
    let outcome = tokio::task::spawn_blocking(move || mfa::turn_off(&worker, &who, &form.code)).await;
    match outcome {
        Ok(Ok(())) => redirect_flash("/account", true, "Two-step sign-in is off."),
        Ok(Err(message)) => redirect_flash("/account#two-step", false, message),
        Err(_) => redirect_flash("/account", false, "Two-step sign-in was not turned off."),
    }
}

// --- setup the site requires, at sign-in ---------------------------------
//
// Here rather than in `accounts` because turning two-step sign-in on also
// revokes the account's OAuth tokens, which are the platform's.

fn forced_setup_page(config: &Config, pending: &mfa::Pending, secret: &str, error: Option<&str>) -> Markup {
    ui::form_page_with_script(
        "Set up two-step sign-in",
        html! {
            h1 { "Set up two-step sign-in" }
            p { "This site requires two-step sign-in for " strong { (pending.user.email) } ". Set it up to finish signing in." }
            @if let Some(error) = error {
                div."flash error" role="alert" { span { (error) } }
            }
            (setup_steps(config, &pending.user.email, secret))
            form."column" method="post" action="/auth/mfa/setup" {
                label for="code" { "Code from your authenticator app" }
                input id="code" name="code" type="text" inputmode="numeric" autocomplete="one-time-code"
                      placeholder="123456" maxlength="20" required autofocus;
                button type="submit" { "Turn on and continue" }
            }
            p."muted" style="margin: .75rem 0 0; font-size: .8rem" {
                a href="/auth/login" { "Cancel and sign in again" }
            }
        },
        Some(ui::COPY_SCRIPT),
    )
}

/// `GET /auth/mfa/setup`: the setup a pending sign-in owes.
pub async fn forced_setup_form(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let Some(token) = mfa::pending_token(&config, &headers) else {
        return Redirect::to("/auth/login").into_response();
    };
    let worker = config.clone();
    let found = tokio::task::spawn_blocking(move || {
        let setup = mfa::pending_setup(&worker, &token);
        let stage = setup.is_none().then(|| mfa::pending(&worker, &token).map(|p| p.stage)).flatten();
        (setup, stage)
    })
    .await;
    match found {
        Ok((Some((pending, secret)), _)) => mfa::with_cookies(
            Html(forced_setup_page(&config, &pending, &secret, None).into_string()).into_response(),
            &[],
        ),
        Ok((None, Some(stage))) if stage == "code" => Redirect::to("/auth/mfa").into_response(),
        _ => mfa::ended_page(&config, &mfa::Refusal::Expired),
    }
}

/// `POST /auth/mfa/setup`: the first code turns it on, and the session and
/// the recovery codes follow.
pub async fn forced_setup_submit(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<mfa::CodeForm>,
) -> Response {
    let Some(token) = mfa::pending_token(&config, &headers) else {
        tracing::warn!("two-step setup refused: no pending sign-in cookie");
        return mfa::ended_page(&config, &mfa::Refusal::Expired);
    };
    let worker = config.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let result = mfa::finish_setup(&worker, &token, form.code());
        let again = match &result {
            Err(mfa::Refusal::Wrong(_)) => mfa::pending_setup(&worker, &token),
            _ => None,
        };
        (result, again)
    })
    .await;
    match outcome {
        Ok((Ok(done), _)) => {
            revoke_clients(&config, &done.user.id, &done.user.email, "two-step sign-in on").await;
            let markup = ui::form_page_with_script(
                "Two-step sign-in is on",
                html! {
                    h1 { "Two-step sign-in is on" }
                    (recovery_codes(&done.user.email, &done.recovery_codes))
                    a."btn" href=(done.next) { "Continue" }
                },
                Some(ui::COPY_SCRIPT),
            );
            mfa::with_cookies(
                Html(markup.into_string()).into_response(),
                &[users::set_cookie_header(&config, &done.session), mfa::clear_pending_cookie_header(&config)],
            )
        }
        Ok((Err(refusal @ mfa::Refusal::Wrong(_)), Some((pending, secret)))) => mfa::with_cookies(
            (
                StatusCode::UNAUTHORIZED,
                Html(forced_setup_page(&config, &pending, &secret, Some(&refusal.message())).into_string()),
            )
                .into_response(),
            &[],
        ),
        Ok((Err(refusal), _)) => mfa::ended_page(&config, &refusal),
        Err(_) => mfa::ended_page(&config, &mfa::Refusal::Failed("the task did not finish".into())),
    }
}
