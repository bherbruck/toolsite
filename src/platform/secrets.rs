//! Values the owner sets for one app: API keys, endpoints, anything that must
//! not travel in a bundle a visitor can download.
//!
//! A handler can read them. Nothing else can: they are not in the source
//! archive, not under `/p/`, and no tool returns a value — only names. That
//! asymmetry is the whole point, so it is enforced here rather than left to
//! each caller to remember.

use crate::{
    config::Config,
    platform::{records, tokens::valid_app},
    seal::{open, seal},
};
use std::collections::BTreeMap;

/// Names to sealed values, as stored; none, logged, when they cannot be
/// read. Kept in the records store (`<app>.secrets` on files), beside the
/// app, and refused by the public route.
async fn read_sealed(config: &Config, app: &str) -> BTreeMap<String, String> {
    if !valid_app(app) {
        return BTreeMap::new();
    }
    records::of(config).settings(app).await.unwrap_or_else(|why| {
        tracing::warn!(app, %why, "settings could not be read");
        BTreeMap::new()
    })
}

fn read_sealed_blocking(config: &Config, app: &str) -> BTreeMap<String, String> {
    if !valid_app(app) {
        return BTreeMap::new();
    }
    records::of(config).settings_blocking(app).unwrap_or_else(|why| {
        tracing::warn!(app, %why, "settings could not be read");
        BTreeMap::new()
    })
}

/// Names only. There is deliberately no function returning every value: a
/// handler asks for one at a time, and nothing else asks at all.
pub async fn names(config: &Config, app: &str) -> Vec<String> {
    read_sealed(config, app).await.into_keys().collect()
}

/// `names` for a handler's host call, on a blocking thread.
pub fn names_blocking(config: &Config, app: &str) -> Vec<String> {
    read_sealed_blocking(config, app).into_keys().collect()
}

pub async fn get(config: &Config, app: &str, name: &str) -> Option<String> {
    open(config, read_sealed(config, app).await.get(name)?)
}

/// `get` for a handler's host call, on a blocking thread.
pub fn get_blocking(config: &Config, app: &str, name: &str) -> Option<String> {
    open(config, read_sealed_blocking(config, app).get(name)?)
}

/// Setting an existing name replaces it; passing no value removes it.
pub async fn set(config: &Config, app: &str, name: &str, value: Option<&str>) -> Result<(), String> {
    if !valid_app(app) {
        return Err(format!("invalid app name '{app}'"));
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("a setting's name must be letters, numbers or '_'".into());
    }
    let sealed = value.map(|value| seal(config, value)).transpose()?;
    let had = records::of(config).set_setting(app, name, sealed.as_deref()).await?;
    if value.is_none() && !had {
        return Err(format!("{app} has no setting called {name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "test-token");
        (dir, config)
    }

    #[tokio::test]
    async fn a_setting_round_trips_and_can_be_replaced() {
        let (_dir, config) = config();
        set(&config, "app", "API_KEY", Some("first")).await.unwrap();
        assert_eq!(get(&config, "app", "API_KEY").await.as_deref(), Some("first"));
        set(&config, "app", "API_KEY", Some("second")).await.unwrap();
        assert_eq!(get(&config, "app", "API_KEY").await.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn listing_gives_names_and_never_values() {
        let (_dir, config) = config();
        set(&config, "app", "API_KEY", Some("hunter2")).await.unwrap();
        set(&config, "app", "ENDPOINT", Some("https://example.com")).await.unwrap();

        let listed = names(&config, "app").await;
        assert_eq!(listed, ["API_KEY", "ENDPOINT"]);
        assert!(
            !format!("{listed:?}").contains("hunter2"),
            "a value came back from a listing"
        );
    }

    #[tokio::test]
    async fn one_app_cannot_see_anothers() {
        let (_dir, config) = config();
        set(&config, "mine", "API_KEY", Some("hunter2")).await.unwrap();
        assert!(get(&config, "theirs", "API_KEY").await.is_none());
        assert!(names(&config, "theirs").await.is_empty());
    }

    #[tokio::test]
    async fn removing_says_so_when_there_was_nothing_there() {
        let (_dir, config) = config();
        set(&config, "app", "API_KEY", Some("hunter2")).await.unwrap();
        set(&config, "app", "API_KEY", None).await.unwrap();
        assert!(get(&config, "app", "API_KEY").await.is_none());
        assert!(set(&config, "app", "API_KEY", None).await.is_err());
    }

    #[tokio::test]
    async fn a_value_is_not_readable_from_the_file_it_is_stored_in() {
        let (dir, config) = config();
        set(&config, "app", "API_KEY", Some("hunter2")).await.unwrap();

        // Whoever gets hold of the volume gets ciphertext, not keys.
        let stored = std::fs::read_to_string(dir.path().join("app.secrets")).unwrap();
        assert!(stored.contains("API_KEY"), "names are not secret, values are");
        assert!(!stored.contains("hunter2"), "the value was stored in the clear");
        assert_eq!(get(&config, "app", "API_KEY").await.as_deref(), Some("hunter2"));
    }

    #[tokio::test]
    async fn the_same_value_stored_twice_does_not_look_the_same() {
        let (dir, config) = config();
        set(&config, "one", "K", Some("same")).await.unwrap();
        set(&config, "two", "K", Some("same")).await.unwrap();

        let first = std::fs::read_to_string(dir.path().join("one.secrets")).unwrap();
        let second = std::fs::read_to_string(dir.path().join("two.secrets")).unwrap();
        assert_ne!(first, second, "identical values produced identical ciphertext");
    }

    #[tokio::test]
    async fn a_name_that_could_leave_the_data_directory_is_refused() {
        let (_dir, config) = config();
        assert!(set(&config, "../etc", "API_KEY", Some("x")).await.is_err());
        assert!(set(&config, "app", "../../escape", Some("x")).await.is_err());
        assert!(set(&config, "app", "has space", Some("x")).await.is_err());
    }
}

// --- handing entry to a person -----------------------------------------

use crate::AppState;
use axum::{
    extract::{Form, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use crate::state::tickets::Kind;
use std::time::Duration;

/// Long enough to reach someone, short enough that a forgotten link is not a
/// standing way to rewrite an app's credentials.
const ENTRY_TTL: Duration = Duration::from_secs(60 * 60);

/// A link the owner opens to type values in. An agent can create one and pass
/// it on without ever handling a secret itself, which is the point: values
/// that never enter a conversation cannot leak from one.
pub async fn create_entry(config: &Config, app: &str) -> Result<String, String> {
    if !valid_app(app) {
        return Err("invalid app name".into());
    }
    let token = config.stores.tickets.put(Kind::SettingsLink, ENTRY_TTL, &app).await?;

    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    Ok(format!("{base}/settings/{token}"))
}

async fn entry_app(config: &Config, token: &str) -> Option<String> {
    match config.stores.tickets.get(Kind::SettingsLink, token).await {
        Ok(app) => app,
        Err(why) => {
            tracing::error!(%why, "settings link could not be read");
            None
        }
    }
}

#[derive(serde::Deserialize)]
pub struct EntryToken {
    token: String,
}

pub async fn entry_form(
    State(state): State<AppState>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    let Some(app) = entry_app(&state.config, &token).await else {
        return (
            StatusCode::GONE,
            "This link has expired. Ask for a new link.",
        )
            .into_response();
    };
    let existing = names(&state.config, &app).await;

    let markup = crate::ui::form_page(
        &format!("Settings for {app}"),
        maud::html! {
            form."column" method="post" action="/settings" {
                h1 { "Settings for " (app) }
                p."muted" {
                    "Enter one setting per line as NAME=value. Only the code of "
                    (app) " can read these values."
                }
                @if !existing.is_empty() {
                    p."muted" { "Set: " (existing.join(", ")) }
                }
                input type="hidden" name="token" value=(token);
                textarea name="pasted" rows="8" placeholder="API_KEY=…\nENDPOINT=https://…"
                         autofocus required {}
                button type="submit" { "Save" }
            }
        },
    );
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Html(markup.into_string()),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub struct PastedSettings {
    token: String,
    pasted: String,
}

pub async fn entry_submit(
    State(state): State<AppState>,
    Form(form): Form<PastedSettings>,
) -> Response {
    let Some(app) = entry_app(&state.config, &form.token).await else {
        return (StatusCode::GONE, "This link has expired. Ask for a new link.").into_response();
    };

    let mut saved = 0usize;
    for (name, value) in parse_pasted(&form.pasted) {
        if let Err(message) = set(&state.config, &app, &name, Some(&value)).await {
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
        saved += 1;
    }

    if saved == 0 {
        return (StatusCode::BAD_REQUEST, "No line has the format NAME=value. Check the format.").into_response();
    }
    // Back to the form, which now lists the names — and never the values.
    Redirect::to(&format!("/settings/{}", form.token)).into_response()
}

/// `NAME=value` per line, the way a hosting dashboard accepts them. Blank
/// lines and `#` comments are ignored, quotes around a value are stripped,
/// and `export ` prefixes are tolerated so a .env file can be pasted whole.
fn parse_pasted(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (name, value) = line.split_once('=')?;
            let name = name.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            (!name.is_empty() && !value.is_empty())
                .then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

/// Unused by the form, but the query form keeps `/settings?token=` working
/// for anything that builds links that way.
pub async fn entry_form_query(
    state: State<AppState>,
    Query(params): Query<EntryToken>,
) -> Response {
    entry_form(state, axum::extract::Path(params.token)).await
}

#[cfg(test)]
mod paste_tests {
    use super::parse_pasted;

    #[test]
    fn a_dotenv_file_can_be_pasted_whole() {
        let pasted = "# comment\n\nexport API_KEY=\"hunter2\"\nENDPOINT = https://example.com \n\
                      QUOTED='single'\nnot a setting\nEMPTY=\n";
        assert_eq!(
            parse_pasted(pasted),
            vec![
                ("API_KEY".to_string(), "hunter2".to_string()),
                ("ENDPOINT".to_string(), "https://example.com".to_string()),
                ("QUOTED".to_string(), "single".to_string()),
            ]
        );
    }

    #[test]
    fn a_value_containing_an_equals_sign_survives() {
        assert_eq!(
            parse_pasted("TOKEN=abc=def=="),
            vec![("TOKEN".to_string(), "abc=def==".to_string())]
        );
    }
}
