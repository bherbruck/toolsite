//! Keeping an app's project in a GitHub repository.
//!
//! The repository is a source mirror with history, nothing more. Toolsite
//! does not build and does not run anything in GitHub: an agent or the CLI
//! has a toolchain where it runs and publishes in seconds, and that is where
//! building belongs. What toolsite does:
//!
//! - **Create**: a new repository holding the app's stored source.
//! - **Import**: link a repository you already have and pull its branch into
//!   the app's source archive, so the next session starts from it.
//! - **Push**: publishing the source of a linked app commits it to the
//!   branch, with the message the publisher gave.
//! - **Pull**: a push webhook, or the Pull button, stores the branch as the
//!   app's source archive again.
//! - **Drift**: the link remembers which commit the live app came from, so
//!   the Repo tab can say when the repository has moved ahead of it.
//!
//! Toolsite speaks to GitHub as a GitHub App: a JWT signed with the App's
//! private key buys a short-lived installation token, scoped to the account
//! the App was installed on. No personal token is stored anywhere.
//!
//! What this module remembers lives in files like everything else: the
//! installations in `.site/github.json`, each linked app in `<app>.repo`.

use crate::{
    config::Config,
    content::slug::valid_slug,
    platform::{
        admin::{self, Page},
        deploy,
    },
    ui,
};
use axum::{
    body::Bytes,
    extract::{Form, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use maud::{html, Markup};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A source archive handed to a repository, at most.
const MAX_SOURCE_FILES: usize = 2_000;
const MAX_SOURCE_BYTES: usize = 50 * 1024 * 1024;
/// How much of a branch tarball is taken back as the source archive.
const MAX_PULL_BYTES: usize = crate::platform::upload::MAX_UPLOAD_BYTES;
/// Commit messages a publisher gives: the subject is capped here.
const MAX_SUBJECT: usize = 200;
const TRAILER: &str = "Published from toolsite";
const RECENT_COMMITS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Installation tokens last an hour; one is reused until it is nearly up.
const TOKEN_REUSE: Duration = Duration::from_secs(50 * 60);

// --- the App ----------------------------------------------------------------

/// The GitHub App this deployment speaks as. Present when the four
/// `TOOLSITE_GITHUB_*` variables are set.
pub struct App {
    pub app_id: String,
    key: EncodingKey,
    /// The App's URL slug, for the install link. Optional: without it the
    /// admin page says to install from GitHub's own settings.
    pub slug: Option<String>,
    webhook_secret: Option<String>,
    /// `https://api.github.com`, or a stand-in under test.
    pub api: String,
    tokens: Mutex<HashMap<u64, (String, Instant)>>,
}

impl App {
    /// `private_key` is the App's PEM, raw or base64-encoded (a dashboard
    /// field eats newlines; base64 survives it).
    pub fn new(
        app_id: &str,
        private_key: &str,
        slug: Option<String>,
        webhook_secret: Option<String>,
        api: Option<String>,
    ) -> Result<Self, String> {
        let pem = if private_key.contains("-----BEGIN") {
            private_key.to_string()
        } else {
            let bytes = BASE64
                .decode(private_key.trim())
                .map_err(|_| "TOOLSITE_GITHUB_APP_PRIVATE_KEY is neither a PEM nor base64 of one")?;
            String::from_utf8(bytes).map_err(|_| "TOOLSITE_GITHUB_APP_PRIVATE_KEY did not decode to text")?
        };
        let key = EncodingKey::from_rsa_pem(pem.as_bytes())
            .map_err(|e| format!("TOOLSITE_GITHUB_APP_PRIVATE_KEY is not an RSA private key: {e}"))?;
        Ok(Self {
            app_id: app_id.trim().to_string(),
            key,
            slug: slug.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            webhook_secret: webhook_secret.filter(|s| !s.is_empty()),
            api: api
                .map(|a| a.trim_end_matches('/').to_string())
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| "https://api.github.com".to_string()),
            tokens: Mutex::new(HashMap::new()),
        })
    }

    pub fn install_url(&self) -> Option<String> {
        self.slug
            .as_ref()
            .map(|slug| format!("https://github.com/apps/{slug}/installations/new"))
    }

    /// The App's own credential: ten minutes, signed with its key.
    fn jwt(&self) -> Result<String, String> {
        #[derive(Serialize)]
        struct Claims<'a> {
            iat: u64,
            exp: u64,
            iss: &'a str,
        }
        let now = now();
        let claims = Claims {
            // A minute back, as GitHub recommends, so clock skew does not
            // make the token "from the future".
            iat: now.saturating_sub(60),
            exp: now + 9 * 60,
            iss: &self.app_id,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|e| format!("could not sign the GitHub App JWT: {e}"))
    }

    /// An installation token, reused while it lasts.
    async fn token(&self, installation: u64) -> Result<String, String> {
        if let Some((token, since)) = self.tokens.lock().unwrap().get(&installation)
            && since.elapsed() < TOKEN_REUSE
        {
            return Ok(token.clone());
        }
        let jwt = self.jwt()?;
        let (status, body) = call(
            &self.api,
            &format!("Bearer {jwt}"),
            reqwest::Method::POST,
            &format!("/app/installations/{installation}/access_tokens"),
            None,
        )
        .await?;
        if !status.is_success() {
            return Err(format!(
                "GitHub refused an installation token ({status}): {}. Is the App still installed there?",
                api_message(&body)
            ));
        }
        let token = body["token"]
            .as_str()
            .ok_or("GitHub's token answer had no token in it")?
            .to_string();
        self.tokens
            .lock()
            .unwrap()
            .insert(installation, (token.clone(), Instant::now()));
        Ok(token)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn reason(error: &reqwest::Error) -> String {
    let mut out = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

fn api_message(body: &serde_json::Value) -> String {
    body["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| "no detail".to_string())
}

/// One request to the API. The answer is whatever JSON came back, or an
/// empty object for a bodiless 204.
async fn call(
    api: &str,
    authorization: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(reqwest::StatusCode, serde_json::Value), String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {}", reason(&e)))?;
    let mut request = client
        .request(method, format!("{api}{path}"))
        .header("authorization", authorization)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .header("user-agent", concat!("toolsite/", env!("CARGO_PKG_VERSION")));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("GitHub did not answer: {}", reason(&e)))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("GitHub's answer could not be read: {}", reason(&e)))?;
    let json = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "message": text }))
    };
    Ok((status, json))
}

fn app_of(config: &Config) -> Result<&App, String> {
    config
        .github
        .as_ref()
        .ok_or_else(|| "GitHub is not configured: set TOOLSITE_GITHUB_APP_ID and TOOLSITE_GITHUB_APP_PRIVATE_KEY".to_string())
}

// --- what is remembered ------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Installation {
    pub id: u64,
    /// The account's login: a person or an organisation.
    pub account: String,
    /// "User" or "Organization", which decides where a repository is made.
    pub kind: String,
}

/// The installations GitHub last listed; none, logged, when they cannot be
/// read. Kept in the records store (`.site/github.json` on files).
pub async fn installations(config: &Config) -> Vec<Installation> {
    match crate::platform::records::of(config).installations().await {
        Ok(text) => text.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default(),
        Err(why) => {
            tracing::warn!(%why, "the GitHub installations could not be read");
            Vec::new()
        }
    }
}

async fn write_installations(config: &Config, list: &[Installation]) -> Result<(), String> {
    crate::platform::records::of(config).set_installations(&crate::platform::records::pretty(list)?).await
}

/// Asks GitHub which accounts the App is installed on and remembers them.
pub async fn refresh_installations(config: &Config) -> Result<Vec<Installation>, String> {
    let app = app_of(config)?;
    let jwt = app.jwt()?;
    let (status, body) = call(&app.api, &format!("Bearer {jwt}"), reqwest::Method::GET, "/app/installations?per_page=100", None).await?;
    if !status.is_success() {
        return Err(format!("GitHub would not list installations ({status}): {}", api_message(&body)));
    }
    let list: Vec<Installation> = body
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(Installation {
                        id: item["id"].as_u64()?,
                        account: item["account"]["login"].as_str()?.to_string(),
                        kind: item["account"]["type"].as_str().unwrap_or("User").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    write_installations(config, &list).await?;
    Ok(list)
}

async fn installation(config: &Config, id: u64) -> Result<Installation, String> {
    if let Some(found) = installations(config).await.into_iter().find(|i| i.id == id) {
        return Ok(found);
    }
    refresh_installations(config)
        .await?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| format!("the App is not installed under installation {id}; install it from the GitHub page first"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Push {
    pub at: u64,
    pub sha: String,
}

/// Which commit the live app was published from, when anyone said.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deployed {
    pub sha: String,
    pub at: u64,
}

/// An app's repository, in `<app>.repo`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoLink {
    pub owner: String,
    pub repo: String,
    pub branch: String,
    /// Where the project sits inside the repository; empty for the root.
    #[serde(default)]
    pub directory: String,
    pub installation_id: u64,
    /// A deploy token an older link minted for a workflow. Empty now; kept so
    /// disconnecting an old link still revokes it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token_id: String,
    pub connected_at: u64,
    /// The last commit seen on the branch: ours, or one the webhook told us of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_push: Option<Push>,
    /// The commit the live app came from. Set by our own push, or by a
    /// publisher that said `commit=<sha>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployed: Option<Deployed>,
    /// Kept rather than deleted when disconnected: nothing here destroys a
    /// record, and the history of where an app came from is worth having.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disconnected_at: Option<u64>,
}

impl RepoLink {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}", self.owner, self.repo)
    }
}

pub fn valid_app(app: &str) -> bool {
    valid_slug(app) && !app.contains('/')
}

fn parse_link(text: &str) -> Option<RepoLink> {
    serde_json::from_str(text).ok()
}

/// The app's live link, if it has one. Kept in the records store
/// (`<app>.repo` on files).
pub async fn link(config: &Config, app: &str) -> Option<RepoLink> {
    if !valid_app(app) {
        return None;
    }
    match crate::platform::records::of(config).repo_link(app).await {
        Ok(text) => text.as_deref().and_then(parse_link).filter(|link| link.disconnected_at.is_none()),
        Err(why) => {
            tracing::warn!(app, %why, "a repository link could not be read");
            None
        }
    }
}

/// Records a new link for the app, replacing whatever was there.
async fn write_link(config: &Config, app: &str, link: &RepoLink) -> Result<(), String> {
    if !valid_app(app) {
        return Err(format!("invalid app name '{app}'"));
    }
    let text = serde_json::to_string_pretty(link).map_err(|e| e.to_string())?;
    crate::platform::records::of(config).update_repo_link(app, Box::new(move |_| Ok(text))).await?;
    Ok(())
}

/// Changes the app's live link with it held, so two changes at once (a
/// push and a webhook, say) both land. Answers the link as stored, or an
/// error when the app has no live link.
async fn update_link(
    config: &Config,
    app: &str,
    edit: impl FnOnce(&mut RepoLink) + Send,
) -> Result<RepoLink, String> {
    if !valid_app(app) {
        return Err(format!("invalid app name '{app}'"));
    }
    let not_connected = format!("{app} is not connected to a repository");
    let stored = crate::platform::records::of(config)
        .update_repo_link(
            app,
            Box::new(move |current| {
                let mut link = current
                    .and_then(parse_link)
                    .filter(|link| link.disconnected_at.is_none())
                    .ok_or(not_connected)?;
                edit(&mut link);
                serde_json::to_string_pretty(&link).map_err(|e| e.to_string())
            }),
        )
        .await?;
    parse_link(&stored).ok_or_else(|| "a repository link could not be read back".to_string())
}

/// Every live link on the site, by app.
pub async fn linked_apps(config: &Config) -> Vec<(String, RepoLink)> {
    let links = crate::platform::records::of(config).repo_links().await.unwrap_or_else(|why| {
        tracing::warn!(%why, "the repository links could not be listed");
        Vec::new()
    });
    links
        .into_iter()
        .filter_map(|(app, text)| {
            let link = parse_link(&text).filter(|link| link.disconnected_at.is_none())?;
            valid_app(&app).then_some((app, link))
        })
        .collect()
}

async fn link_for_repo(config: &Config, full_name: &str) -> Option<(String, RepoLink)> {
    linked_apps(config)
        .await
        .into_iter()
        .find(|(_, link)| link.full_name().eq_ignore_ascii_case(full_name))
}

// --- names ---------------------------------------------------------------------

fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn valid_owner(name: &str) -> bool {
    !name.is_empty() && name.len() <= 39 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// A subdirectory inside the repository, normalised to `dir/` or empty.
fn clean_directory(directory: Option<&str>) -> Result<String, String> {
    let trimmed = directory.unwrap_or("").trim().trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if !crate::content::slug::valid_asset_path(trimmed) {
        return Err("the directory must be a relative path of plain segments".into());
    }
    Ok(format!("{trimmed}/"))
}

// --- talking to a repository ---------------------------------------------------

/// Every repository this site creates or imports carries this topic, so
/// they are one search away on GitHub.
const TOPIC: &str = "toolsite";

struct Repo<'a> {
    app: &'a App,
    token: String,
    owner: String,
    name: String,
}

impl Repo<'_> {
    fn path(&self, rest: &str) -> String {
        format!("/repos/{}/{}{rest}", self.owner, self.name)
    }

    async fn call(
        &self,
        method: reqwest::Method,
        rest: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(reqwest::StatusCode, serde_json::Value), String> {
        call(&self.app.api, &format!("Bearer {}", self.token), method, &self.path(rest), body).await
    }

    async fn expect(
        &self,
        what: &str,
        method: reqwest::Method,
        rest: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let (status, json) = self.call(method, rest, body).await?;
        if status.is_success() {
            Ok(json)
        } else {
            Err(format!("{what} failed ({status}): {}", api_message(&json)))
        }
    }

    /// Adds the `toolsite` topic, keeping whatever topics the repository
    /// already has. Best effort: a repository that cannot be tagged is still
    /// a repository, so the caller logs and carries on.
    async fn tag_toolsite(&self) -> Result<(), String> {
        let (status, current) = self.call(reqwest::Method::GET, "/topics", None).await?;
        let mut names: Vec<String> = if status.is_success() {
            current["names"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if names.iter().any(|n| n == TOPIC) {
            return Ok(());
        }
        names.push(TOPIC.to_string());
        self.expect(
            "tagging the repository",
            reqwest::Method::PUT,
            "/topics",
            Some(serde_json::json!({ "names": names })),
        )
        .await
        .map(|_| ())
    }

    /// The repository's record, or the reason it cannot be reached.
    async fn get(&self) -> Result<serde_json::Value, String> {
        self.expect("reading the repository", reqwest::Method::GET, "", None).await
    }

    /// The branch tip: commit and tree. A brand-new repository is still being
    /// initialised for a moment after it is created, so the tip is asked for
    /// with patience.
    async fn head(&self, branch: &str) -> Result<(String, String), String> {
        let mut parent = None;
        for attempt in 0..6 {
            let (status, json) = self
                .call(reqwest::Method::GET, &format!("/git/ref/heads/{branch}"), None)
                .await?;
            if status.is_success() {
                parent = json["object"]["sha"].as_str().map(str::to_string);
                break;
            }
            if attempt == 5 {
                return Err(format!("branch {branch} did not appear ({status}): {}", api_message(&json)));
            }
            tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
        }
        let parent = parent.ok_or("the branch has no commit")?;
        let base = self
            .expect("reading the tip commit", reqwest::Method::GET, &format!("/git/commits/{parent}"), None)
            .await?;
        let tree = base["tree"]["sha"].as_str().ok_or("the tip commit has no tree")?.to_string();
        Ok((parent, tree))
    }

    /// Every blob in a tree, as (path, sha).
    async fn tree_entries(&self, tree_sha: &str) -> Result<Vec<(String, String)>, String> {
        let json = self
            .expect("reading the tree", reqwest::Method::GET, &format!("/git/trees/{tree_sha}?recursive=1"), None)
            .await?;
        Ok(json["tree"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item["type"] == "blob")
                    .filter_map(|item| Some((item["path"].as_str()?.to_string(), item["sha"].as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// One commit on top of the branch: `upserts` written, `deletions`
    /// removed. Nothing is committed, and `None` comes back, when the result
    /// would be the tree the branch already has.
    async fn commit_tree(
        &self,
        branch: &str,
        upserts: &[(String, Vec<u8>)],
        deletions: &[String],
        message: &str,
    ) -> Result<Option<String>, String> {
        let (parent, base_tree) = self.head(branch).await?;
        let mut entries = Vec::with_capacity(upserts.len() + deletions.len());
        for (path, content) in upserts {
            let blob = self
                .expect(
                    &format!("storing {path}"),
                    reqwest::Method::POST,
                    "/git/blobs",
                    Some(serde_json::json!({ "content": BASE64.encode(content), "encoding": "base64" })),
                )
                .await?;
            let sha = blob["sha"].as_str().ok_or("blob answer had no sha")?.to_string();
            entries.push(serde_json::json!({ "path": path, "mode": "100644", "type": "blob", "sha": sha }));
        }
        for path in deletions {
            entries.push(serde_json::json!({ "path": path, "mode": "100644", "type": "blob", "sha": serde_json::Value::Null }));
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let tree = self
            .expect(
                "building the tree",
                reqwest::Method::POST,
                "/git/trees",
                Some(serde_json::json!({ "base_tree": base_tree, "tree": entries })),
            )
            .await?;
        let tree_sha = tree["sha"].as_str().ok_or("tree answer had no sha")?.to_string();
        // Trees are content-addressed: the same files give the same sha, so
        // an unchanged project is caught here without a commit.
        if tree_sha == base_tree {
            return Ok(None);
        }
        let commit = self
            .expect(
                "writing the commit",
                reqwest::Method::POST,
                "/git/commits",
                Some(serde_json::json!({ "message": message, "tree": tree_sha, "parents": [parent] })),
            )
            .await?;
        let sha = commit["sha"].as_str().ok_or("commit answer had no sha")?.to_string();
        self.expect(
            "moving the branch",
            reqwest::Method::PATCH,
            &format!("/git/refs/heads/{branch}"),
            Some(serde_json::json!({ "sha": sha, "force": false })),
        )
        .await?;
        Ok(Some(sha))
    }

    /// The branch as a gzipped tar, the way GitHub serves it: one top-level
    /// directory named after the commit, which the caller strips.
    async fn tarball(&self, reference: &str) -> Result<Vec<u8>, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| format!("http client: {}", reason(&e)))?;
        // GitHub answers with a redirect to a signed download URL; reqwest
        // follows it and drops the Authorization header across hosts.
        let response = client
            .get(format!("{}{}", self.app.api, self.path(&format!("/tarball/{reference}"))))
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", concat!("toolsite/", env!("CARGO_PKG_VERSION")))
            .send()
            .await
            .map_err(|e| format!("GitHub did not answer: {}", reason(&e)))?;
        if !response.status().is_success() {
            return Err(format!("downloading the branch failed ({})", response.status()));
        }
        if response.content_length().is_some_and(|len| len as usize > MAX_PULL_BYTES) {
            return Err(format!("the branch tarball is larger than {} MB", MAX_PULL_BYTES / 1024 / 1024));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("the branch tarball could not be read: {}", reason(&e)))?;
        if bytes.len() > MAX_PULL_BYTES {
            return Err(format!("the branch tarball is larger than {} MB", MAX_PULL_BYTES / 1024 / 1024));
        }
        Ok(bytes.to_vec())
    }

    /// The newest commits on a branch, newest first.
    async fn commits(&self, branch: &str, count: usize) -> Result<Vec<Commit>, String> {
        let json = self
            .expect(
                "listing commits",
                reqwest::Method::GET,
                &format!("/commits?sha={branch}&per_page={count}"),
                None,
            )
            .await?;
        Ok(json
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|c| Commit {
                        sha: c["sha"].as_str().unwrap_or("").to_string(),
                        subject: c["commit"]["message"]
                            .as_str()
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string(),
                        author: c["commit"]["author"]["name"].as_str().unwrap_or("").to_string(),
                        date: c["commit"]["author"]["date"].as_str().unwrap_or("").to_string(),
                        url: c["html_url"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// How many commits `head` is ahead of `base`, or nothing when GitHub
    /// cannot compare them (a rewritten branch, a sha it no longer has).
    async fn ahead_by(&self, base: &str, head: &str) -> Option<u64> {
        let (status, json) = self
            .call(reqwest::Method::GET, &format!("/compare/{base}...{head}"), None)
            .await
            .ok()?;
        if !status.is_success() {
            return None;
        }
        json["ahead_by"].as_u64()
    }
}

async fn open_repo<'a>(app: &'a App, installation_id: u64, owner: &str, name: &str) -> Result<Repo<'a>, String> {
    if !valid_owner(owner) || !valid_repo_name(name) {
        return Err(format!("{owner}/{name} is not a repository name"));
    }
    Ok(Repo {
        app,
        token: app.token(installation_id).await?,
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// Repositories the installation can reach whose name contains `q`, at
/// most `limit`. GitHub lists them a page at a time; three pages is enough
/// for a picker, and a name that is not in them is typed in full.
pub async fn search_repos(config: &Config, installation_id: u64, q: &str, limit: usize) -> Result<Vec<String>, String> {
    let app = app_of(config)?;
    let token = app.token(installation_id).await?;
    let needle = q.trim().to_lowercase();
    let mut out = Vec::new();
    for page in 1..=3 {
        let (status, body) = call(
            &app.api,
            &format!("Bearer {token}"),
            reqwest::Method::GET,
            &format!("/installation/repositories?per_page=100&page={page}"),
            None,
        )
        .await?;
        if !status.is_success() {
            return Err(format!("GitHub would not list repositories ({status}): {}", api_message(&body)));
        }
        let items = body["repositories"].as_array().cloned().unwrap_or_default();
        let count = items.len();
        for item in items {
            if let Some(full) = item["full_name"].as_str()
                && (needle.is_empty() || full.to_lowercase().contains(&needle))
            {
                out.push(full.to_string());
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        if count < 100 {
            break;
        }
    }
    Ok(out)
}

/// A repository tagged `toolsite` that no app is linked to yet.
/// One commit on the linked branch, for the Repo tab.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub installation_id: u64,
    pub account: String,
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
    /// The app name to propose: the repository name without a leading
    /// `toolsite-`, made into a slug.
    pub proposed_app: String,
}

/// The app name a repository suggests for itself.
pub fn proposed_app_name(repo_name: &str) -> String {
    let base = repo_name
        .strip_prefix("toolsite-")
        .or_else(|| repo_name.strip_prefix("Toolsite-"))
        .unwrap_or(repo_name);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() { "app".to_string() } else { trimmed }
}

const DISCOVER_PAGES: usize = 10;

/// Every repository the installations can reach that carries the `toolsite`
/// topic and is not linked to an app. Discovery proposes; nothing is
/// imported until someone says so.
pub async fn discover(config: &Config) -> Result<Vec<Discovered>, String> {
    let app = app_of(config)?;
    let linked: Vec<String> = linked_apps(config).await
        .into_iter()
        .map(|(_, link)| link.full_name().to_lowercase())
        .collect();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for inst in installations(config).await {
        let token = app.token(inst.id).await?;
        for page in 1..=DISCOVER_PAGES {
            let (status, body) = call(
                &app.api,
                &format!("Bearer {token}"),
                reqwest::Method::GET,
                &format!("/installation/repositories?per_page=100&page={page}"),
                None,
            )
            .await?;
            if !status.is_success() {
                return Err(format!("GitHub would not list repositories for {} ({status}): {}", inst.account, api_message(&body)));
            }
            let items = body["repositories"].as_array().cloned().unwrap_or_default();
            let count = items.len();
            for item in items {
                let tagged = item["topics"]
                    .as_array()
                    .is_some_and(|topics| topics.iter().any(|t| t.as_str() == Some(TOPIC)));
                let Some(full) = item["full_name"].as_str() else { continue };
                // A repository two installations can both reach is still one
                // repository; the first installation that lists it owns the row.
                if !tagged || linked.iter().any(|l| l == &full.to_lowercase()) || !seen.insert(full.to_lowercase()) {
                    continue;
                }
                let name = item["name"].as_str().unwrap_or(full.rsplit('/').next().unwrap_or(full));
                out.push(Discovered {
                    installation_id: inst.id,
                    account: inst.account.clone(),
                    full_name: full.to_string(),
                    private: item["private"].as_bool().unwrap_or(true),
                    default_branch: item["default_branch"].as_str().unwrap_or("main").to_string(),
                    proposed_app: proposed_app_name(name),
                });
            }
            if count < 100 {
                break;
            }
        }
    }
    out.sort_by_key(|d| d.full_name.to_lowercase());
    Ok(out)
}

// --- the operations ------------------------------------------------------------

/// A publisher's commit message, made safe for a repository: the first line
/// capped, control characters gone, a body kept after a blank line, and a
/// trailer so a reader of the history can tell these commits apart.
pub fn commit_message(given: Option<&str>, default: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .collect::<String>()
    };
    let text = given.map(clean).unwrap_or_default();
    let mut lines = text.lines();
    let subject: String = lines
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().take(MAX_SUBJECT).collect())
        .unwrap_or_else(|| default.to_string());
    let body = lines.collect::<Vec<_>>().join("\n");
    let body = body.trim();
    if body.is_empty() {
        format!("{subject}\n\n{TRAILER}")
    } else {
        format!("{subject}\n\n{body}\n\n{TRAILER}")
    }
}

fn valid_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// Remembers which commit the live app was published from. Nothing happens
/// for an app without a live link or for a sha that is not one.
pub async fn record_deployed(config: &Config, app: &str, sha: &str) -> bool {
    let sha = sha.trim().to_lowercase();
    if !valid_sha(&sha) {
        return false;
    }
    update_link(config, app, |link| link.deployed = Some(Deployed { sha, at: now() })).await.is_ok()
}

/// A README for a repository toolsite made, when the project brought none.
fn readme_for(app: &str, page: &str) -> String {
    format!(
        "# {app}\n\nThe source of `{app}` on toolsite, kept here with its history. Publishing the \
         source from toolsite pushes a commit; a push here is pulled back into the app's source \
         archive. Building and publishing happen wherever the agent or the CLI runs:\n\n\
         ```\ntoolsite deploy --slug {app}\n```\n\nThe app is served at {page}/.\n"
    )
}

/// Makes a repository for `app` out of its stored source.
pub async fn create(
    config: &Config,
    app_name: &str,
    installation_id: u64,
    repo_name: Option<&str>,
    private: bool,
) -> Result<RepoLink, String> {
    let app = app_of(config)?;
    if !valid_app(app_name) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    if let Some(existing) = link(config, app_name).await {
        return Err(format!("{app_name} is already connected to {}; disconnect it first", existing.full_name()));
    }
    // toolsite-<app> by default, so the repositories this site made are
    // recognisable in a long list and sort together.
    let default_name = format!("toolsite-{app_name}");
    let repo_name = repo_name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&default_name);
    if !valid_repo_name(repo_name) {
        return Err(format!("{repo_name} is not a repository name: letters, digits, '-', '_' and '.'"));
    }
    let archive = crate::content::files::read(config, &format!("{app_name}.source")).await.ok_or_else(|| {
        format!(
            "{app_name} has no stored source to put in a repository. Publish the project first: \
             tar -czf - --exclude node_modules --exclude target . | curl -f -T - '<upload-url>?source'"
        )
    })?;
    let mut files = crate::content::bundle::read_all_files(&archive, MAX_SOURCE_FILES, MAX_SOURCE_BYTES)?;
    if files.is_empty() {
        return Err(format!("{app_name}'s stored source archive holds no files"));
    }

    let inst = installation(config, installation_id).await?;
    let token = app.token(installation_id).await?;
    let create_path = if inst.kind.eq_ignore_ascii_case("Organization") {
        format!("/orgs/{}/repos", inst.account)
    } else {
        "/user/repos".to_string()
    };
    let (status, created) = call(
        &app.api,
        &format!("Bearer {token}"),
        reqwest::Method::POST,
        &create_path,
        Some(serde_json::json!({
            "name": repo_name,
            "private": private,
            "auto_init": true,
            "description": format!("{app_name} on toolsite"),
        })),
    )
    .await?;
    if !status.is_success() {
        return Err(format!("GitHub would not create {}/{repo_name} ({status}): {}", inst.account, api_message(&created)));
    }
    let owner = created["owner"]["login"].as_str().unwrap_or(&inst.account).to_string();
    let name = created["name"].as_str().unwrap_or(repo_name).to_string();
    let branch = created["default_branch"].as_str().unwrap_or("main").to_string();
    let repo = open_repo(app, installation_id, &owner, &name).await?;
    let full_name = repo.owner.clone() + "/" + &repo.name;
    if let Err(why) = repo.tag_toolsite().await {
        tracing::warn!(repo = %full_name, %why, "could not add the toolsite topic");
    }

    if !files.iter().any(|(path, _)| path.eq_ignore_ascii_case("README.md")) {
        files.push(("README.md".to_string(), readme_for(app_name, &crate::content::origins::page_url(config, app_name)).into_bytes()));
    }
    let sha = repo
        .commit_tree(&branch, &files, &[], &commit_message(None, &format!("Add {app_name} from toolsite")))
        .await?
        .unwrap_or_default();

    let link = RepoLink {
        owner,
        repo: name,
        branch,
        directory: String::new(),
        installation_id,
        token_id: String::new(),
        connected_at: now(),
        last_push: Some(Push { at: now(), sha: sha.clone() }),
        deployed: Some(Deployed { sha, at: now() }),
        disconnected_at: None,
    };
    write_link(config, app_name, &link).await?;
    tracing::info!(app = %app_name, repo = %link.full_name(), "repository created");
    Ok(link)
}

/// What a pull brought back.
pub struct Pulled {
    pub bytes: usize,
    pub sha: String,
}

/// Links `app` to a repository that already exists and pulls its branch into
/// the app's source archive. Nothing is built and nothing runs in GitHub.
pub async fn import(
    config: &Config,
    app_name: &str,
    installation_id: u64,
    full_name: &str,
    branch: Option<&str>,
    directory: Option<&str>,
) -> Result<RepoLink, String> {
    let app = app_of(config)?;
    if !valid_app(app_name) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    if let Some(existing) = link(config, app_name).await {
        return Err(format!("{app_name} is already connected to {}; disconnect it first", existing.full_name()));
    }
    let (owner, name) = full_name
        .trim()
        .trim_start_matches("https://github.com/")
        .trim_end_matches(".git")
        .split_once('/')
        .ok_or("name the repository as owner/name")?;
    let directory = clean_directory(directory)?;
    installation(config, installation_id).await?;
    let repo = open_repo(app, installation_id, owner, name).await?;
    if let Err(why) = repo.tag_toolsite().await {
        tracing::warn!(repo = %format!("{owner}/{name}"), %why, "could not add the toolsite topic");
    }
    let record = repo.get().await?;
    let branch = match branch.map(str::trim).filter(|b| !b.is_empty()) {
        Some(branch) if valid_branch(branch) => branch.to_string(),
        Some(branch) => return Err(format!("{branch} is not a branch name")),
        None => record["default_branch"].as_str().unwrap_or("main").to_string(),
    };
    let full = format!("{}/{}", repo.owner, repo.name);

    let mut link = RepoLink {
        owner: repo.owner.clone(),
        repo: repo.name.clone(),
        branch,
        directory,
        installation_id,
        token_id: String::new(),
        connected_at: now(),
        last_push: None,
        deployed: None,
        disconnected_at: None,
    };
    let pulled = pull_into(config, app_name, &repo, &link).await?;
    link.last_push = Some(Push { at: now(), sha: pulled.sha });
    write_link(config, app_name, &link).await?;
    tracing::info!(app = %app_name, repo = %full, bytes = pulled.bytes, "repository imported and pulled");
    Ok(link)
}

/// Pulls the linked branch into the app's source archive again.
pub async fn pull(config: &Config, app_name: &str) -> Result<Pulled, String> {
    let app = app_of(config)?;
    let link = link(config, app_name).await.ok_or_else(|| format!("{app_name} is not connected to a repository"))?;
    let repo = open_repo(app, link.installation_id, &link.owner, &link.repo).await?;
    let pulled = pull_into(config, app_name, &repo, &link).await?;
    let sha = pulled.sha.clone();
    update_link(config, app_name, |link| link.last_push = Some(Push { at: now(), sha })).await?;
    Ok(pulled)
}

async fn pull_into(config: &Config, app_name: &str, repo: &Repo<'_>, link: &RepoLink) -> Result<Pulled, String> {
    let (sha, _) = repo.head(&link.branch).await?;
    let tarball = repo.tarball(&link.branch).await?;
    let directory = link.directory.clone();
    let archive = tokio::task::spawn_blocking(move || repack(&tarball, &directory))
        .await
        .map_err(|_| "repacking the branch failed".to_string())??;
    let bytes = archive.len();
    crate::content::files::publish(config, &format!("{app_name}.source"), archive.into())
        .await
        .map_err(|e| format!("could not store the source archive: {e}"))?;
    Ok(Pulled { bytes, sha })
}

/// GitHub's tarball has one top-level directory named after the commit;
/// the stored archive has the project at its root, under `./`, the way an
/// agent tars it. `directory` narrows to a project inside the repository.
fn repack(tarball: &[u8], directory: &str) -> Result<Vec<u8>, String> {
    let decoder = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(decoder);
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut out = tar::Builder::new(encoder);
    let mut total = 0usize;
    for entry in archive.entries().map_err(|e| format!("the branch tarball could not be read: {e}"))? {
        let mut entry = entry.map_err(|e| format!("the branch tarball could not be read: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().to_string();
        let Some((_, rel)) = path.split_once('/') else {
            continue;
        };
        let rel = match rel.strip_prefix(directory) {
            Some(rel) if !rel.is_empty() => rel.to_string(),
            _ => continue,
        };
        let first = rel.split('/').next().unwrap_or("");
        if matches!(first, "node_modules" | "target" | "dist" | ".git") || rel.contains("/node_modules/") {
            continue;
        }
        if !crate::content::slug::valid_asset_path(&rel) && !rel.starts_with(".github/") {
            continue;
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).map_err(|e| e.to_string())?;
        total += bytes.len();
        if total > MAX_PULL_BYTES {
            return Err(format!("the branch holds more than {} MB of files", MAX_PULL_BYTES / 1024 / 1024));
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        out.append_data(&mut header, format!("./{rel}"), bytes.as_slice())
            .map_err(|e| e.to_string())?;
    }
    out.into_inner()
        .and_then(|encoder| encoder.finish())
        .map_err(|e| e.to_string())
}

/// What pushing a source archive did.
pub enum SourcePush {
    Pushed(String),
    Unchanged,
}

/// Commits `archive` to the linked branch: files in it are written, tracked
/// files under the project directory that it lacks are removed, `.github/`
/// is left alone. The commit is also what the live app came from, since the
/// same archive was just stored.
pub async fn push_source(
    config: &Config,
    app_name: &str,
    archive: &[u8],
    message: Option<&str>,
) -> Result<SourcePush, String> {
    let app = app_of(config)?;
    let link = link(config, app_name).await.ok_or_else(|| format!("{app_name} is not connected to a repository"))?;
    let files = crate::content::bundle::read_all_files(archive, MAX_SOURCE_FILES, MAX_SOURCE_BYTES)?;
    if files.is_empty() {
        return Err("the source archive holds no files".into());
    }
    let repo = open_repo(app, link.installation_id, &link.owner, &link.repo).await?;
    let prefix = link.directory.clone();
    let upserts: Vec<(String, Vec<u8>)> = files
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".github/"))
        .map(|(path, bytes)| (format!("{prefix}{path}"), bytes))
        .collect();
    let (_, tree) = repo.head(&link.branch).await?;
    let tracked = repo.tree_entries(&tree).await?;
    let keep: std::collections::HashSet<&str> = upserts.iter().map(|(p, _)| p.as_str()).collect();
    // Tracked files the project no longer has go, so the branch is the
    // project. Two exceptions: anything under .github/, which is the
    // repository's own, and a root README the project did not bring, which
    // toolsite wrote when it made the repository.
    let readme = format!("{prefix}README.md");
    let deletions: Vec<String> = tracked
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| path.starts_with(&prefix))
        .filter(|path| !path[prefix.len()..].starts_with(".github/") && !path.starts_with(".github/"))
        .filter(|path| path != &readme)
        .filter(|path| !keep.contains(path.as_str()))
        .collect();
    let message = commit_message(message, &format!("Update {app_name} from toolsite"));
    match repo.commit_tree(&link.branch, &upserts, &deletions, &message).await? {
        Some(sha) => {
            let pushed = sha.clone();
            update_link(config, app_name, |link| {
                link.last_push = Some(Push { at: now(), sha: pushed.clone() });
                link.deployed = Some(Deployed { sha: pushed, at: now() });
            })
            .await?;
            tracing::info!(app = %app_name, repo = %link.full_name(), %sha, "source pushed");
            Ok(SourcePush::Pushed(sha))
        }
        None => Ok(SourcePush::Unchanged),
    }
}

/// Forgets the link and revokes any token an older link minted. The
/// repository stays.
pub async fn disconnect(config: &Config, app_name: &str) -> Result<RepoLink, String> {
    let link = update_link(config, app_name, |link| link.disconnected_at = Some(now())).await?;
    if !link.token_id.is_empty() {
        let _ = deploy::revoke(config, app_name, &link.token_id).await;
    }
    tracing::info!(app = %app_name, repo = %link.full_name(), "repository disconnected");
    Ok(link)
}

/// Where the branch stands against what is live.
pub struct Drift {
    pub head: String,
    pub deployed: Option<String>,
    /// Commits the branch is ahead of the live app, when they differ and
    /// GitHub could count.
    pub ahead_by: Option<u64>,
}

impl Drift {
    fn live_is_head(&self) -> bool {
        self.deployed
            .as_ref()
            .is_some_and(|d| *d == self.head || self.head.starts_with(d.as_str()))
    }

    pub fn sentence(&self) -> String {
        match (&self.deployed, self.ahead_by) {
            (None, _) => "No publish has named a commit yet, so the live app and the repository cannot be compared.".to_string(),
            _ if self.live_is_head() => "The live app is the repository's head.".to_string(),
            (Some(_), Some(n)) => format!(
                "The repository is {n} commit{} ahead of the live app. Pull the source and run toolsite deploy, or ask the agent to.",
                if n == 1 { "" } else { "s" }
            ),
            (Some(_), None) => "The repository has moved since the live app was published. Pull the source and run toolsite deploy, or ask the agent to.".to_string(),
        }
    }
}

/// The branch head, the recent commits and the drift, for the Repo tab and
/// the status tool.
pub async fn inspect(config: &Config, link: &RepoLink) -> Result<(Drift, Vec<Commit>), String> {
    let app = app_of(config)?;
    let repo = open_repo(app, link.installation_id, &link.owner, &link.repo).await?;
    let (head, _) = repo.head(&link.branch).await?;
    let commits = repo.commits(&link.branch, RECENT_COMMITS).await?;
    let deployed = link.deployed.as_ref().map(|d| d.sha.clone());
    let ahead_by = match &deployed {
        Some(d) if *d != head && !head.starts_with(d.as_str()) => repo.ahead_by(d, &head).await,
        _ => None,
    };
    Ok((Drift { head, deployed, ahead_by }, commits))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// The link, the drift and the newest commits, for a tool call.
pub async fn status_text(config: &Config, app_name: &str) -> String {
    let Some(link) = link(config, app_name).await else {
        return format!("{app_name} is not connected to a repository");
    };
    let mut text = format!("{app_name} is mirrored at {} ({})", link.url(), link.branch);
    if !link.directory.is_empty() {
        text.push_str(&format!(", directory {}", link.directory));
    }
    match &link.last_push {
        Some(push) => text.push_str(&format!("; last push {} ({} h ago)", short(&push.sha), now().saturating_sub(push.at) / 3600)),
        None => text.push_str("; no push seen yet"),
    }
    match &link.deployed {
        Some(d) => text.push_str(&format!("; live app from {}", short(&d.sha))),
        None => text.push_str("; no publish has named a commit"),
    }
    match inspect(config, &link).await {
        Ok((drift, commits)) => {
            text.push_str(&format!("; head {}. {}", short(&drift.head), drift.sentence()));
            if !commits.is_empty() {
                text.push_str("\nRecent commits:");
                for c in commits {
                    text.push_str(&format!("\n  {}  {}  {}  {}", short(&c.sha), c.subject, c.author, c.date));
                }
            }
        }
        Err(why) => text.push_str(&format!("; GitHub did not answer: {why}")),
    }
    text
}

// --- webhook ---------------------------------------------------------------------

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner: Vec<u8> = block.iter().map(|b| b ^ 0x36).collect();
    let outer: Vec<u8> = block.iter().map(|b| b ^ 0x5c).collect();
    let mut hasher = Sha256::new();
    hasher.update(&inner);
    hasher.update(message);
    let inner_hash = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(&outer);
    hasher.update(inner_hash);
    hasher.finalize().into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `signature` (`sha256=<hex>`) is the secret's HMAC of `body`.
pub fn signature_valid(secret: &str, signature: Option<&str>, body: &[u8]) -> bool {
    let Some(presented) = signature.and_then(|s| s.strip_prefix("sha256=")) else {
        return false;
    };
    let expected = hex(&hmac_sha256(secret.as_bytes(), body));
    constant_time_eq(expected.as_bytes(), presented.trim().as_bytes())
}

/// `POST /github/webhook`: a push to a linked branch. Anything unsigned is
/// refused before it is parsed. A push that is not toolsite's own is pulled
/// into the app's source archive, so the next session starts from it.
pub(crate) async fn webhook(State(config): State<Arc<Config>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(secret) = config.github.as_ref().and_then(|app| app.webhook_secret.as_deref()) else {
        tracing::warn!("webhook refused: TOOLSITE_GITHUB_WEBHOOK_SECRET is not set");
        return (StatusCode::NOT_FOUND, "webhooks are not configured\n").into_response();
    };
    let signature = headers.get("x-hub-signature-256").and_then(|v| v.to_str().ok());
    if !signature_valid(secret, signature, &body) {
        tracing::warn!(signed = signature.is_some(), "webhook refused: bad or missing signature");
        return (StatusCode::UNAUTHORIZED, "signature does not match\n").into_response();
    }
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(json) => json,
        Err(_) => return (StatusCode::BAD_REQUEST, "not JSON\n").into_response(),
    };
    let full_name = payload["repository"]["full_name"].as_str().unwrap_or("").to_string();
    match record_event(&config, &event, &full_name, &payload).await {
        Some(Recorded::Ours { full_name }) => {
            tracing::info!(repo = %full_name, "webhook: our own push, nothing to pull");
            (StatusCode::ACCEPTED, "recorded\n").into_response()
        }
        Some(Recorded::Push { app, full_name }) => match pull(&config, &app).await {
            Ok(pulled) => {
                tracing::info!(app = %app, repo = %full_name, bytes = pulled.bytes, sha = %pulled.sha, "webhook: branch pulled into the source archive");
                (StatusCode::ACCEPTED, "recorded and pulled\n").into_response()
            }
            Err(why) => {
                tracing::warn!(app = %app, repo = %full_name, %why, "webhook: push recorded, pull failed");
                (StatusCode::ACCEPTED, "recorded; pull failed\n").into_response()
            }
        },
        None => (StatusCode::ACCEPTED, "ignored\n").into_response(),
    }
}

enum Recorded {
    /// A push whose sha we already know: toolsite made it.
    Ours { full_name: String },
    /// Somebody else's push to the linked branch.
    Push { app: String, full_name: String },
}

async fn record_event(config: &Config, event: &str, full_name: &str, payload: &serde_json::Value) -> Option<Recorded> {
    if event != "push" {
        return None;
    }
    let (app, link) = link_for_repo(config, full_name).await?;
    let reference = payload["ref"].as_str().unwrap_or("");
    if reference != format!("refs/heads/{}", link.branch) {
        return None;
    }
    let sha = payload["after"].as_str().unwrap_or("").to_string();
    if sha.is_empty() {
        return None;
    }
    if link.last_push.as_ref().is_some_and(|p| p.sha == sha) {
        return Some(Recorded::Ours { full_name: full_name.to_string() });
    }
    update_link(config, &app, |link| link.last_push = Some(Push { at: now(), sha })).await.ok()?;
    Some(Recorded::Push { app, full_name: full_name.to_string() })
}

// --- HTTP: setup, admin page, repo tab, actions ---------------------------------

#[derive(Deserialize)]
pub(crate) struct SetupQuery {
    installation_id: Option<u64>,
}

/// Where GitHub sends the person after installing the App. Admin only: an
/// installation is a capability, and recording one is the owner's act.
pub(crate) async fn setup(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<SetupQuery>,
) -> Response {
    let admin = match admin::require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let Some(id) = query.installation_id else {
        return admin::redirect_flash("/admin/github", false, "GitHub sent no installation id.");
    };
    match refresh_installations(&config).await {
        Ok(list) if list.iter().any(|i| i.id == id) => {
            tracing::info!(admin = %admin.email, installation = id, "GitHub App installed");
            admin::redirect_flash("/admin/github", true, "GitHub is connected.")
        }
        Ok(_) => admin::redirect_flash(
            "/admin/github",
            false,
            format!("GitHub does not list installation {id} for this App."),
        ),
        Err(why) => admin::redirect_flash("/admin/github", false, why),
    }
}

pub(crate) async fn github_page(State(config): State<Arc<Config>>, headers: HeaderMap) -> Response {
    let admin = match admin::require_admin(&config, &headers).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let token = admin::form_token(&config, &admin);
    let base = config
        .base_url
        .clone()
        .unwrap_or_else(|| config.local_base.clone());
    let body = match config.github.as_ref() {
        None => setup_guide(&base),
        Some(app) if installations(&config).await.is_empty() => ui::panel(
            "Install the App",
            Some("The App is configured. Install the App on the account or organization that owns the repositories. GitHub returns you to this page."),
            html! {
                div."actions" {
                    @if let Some(url) = app.install_url() {
                        a."btn" href=(url) { "Install on an account" }
                    } @else {
                        span."muted small" { "Set TOOLSITE_GITHUB_APP_SLUG to show an Install button. Or install the App from the GitHub developer settings. GitHub returns you to this page." }
                    }
                    form method="post" action="/admin/repo" {
                        (admin::hidden("token", &token)) (admin::hidden("action", "refresh")) (admin::hidden("app", "-"))
                        button."quiet" type="submit" { "Refresh" }
                    }
                }
                p."muted small" style="margin-top: 1rem" {
                    "Make sure that the webhook URL of the App is " code { (base) "/github/webhook" } "."
                }
            },
        ),
        Some(app) => {
            let installs = installations(&config).await;
            let linked = linked_apps(&config).await;
            let discovered = discover(&config).await;
            html! {
                (ui::panel("Installations", Some("The App can create and read repositories in these accounts."), html! {
                    @if installs.is_empty() {
                        p."muted" { "The App is not installed. Install the App below." }
                    } @else {
                        table {
                            thead { tr { th { "Account" } th { "Kind" } th { "Installation" } } }
                            tbody {
                                @for inst in &installs {
                                    tr { td { (inst.account) } td."muted small" { (inst.kind) } td."muted small" { (inst.id) } }
                                }
                            }
                        }
                    }
                    div."actions" {
                        @if let Some(url) = app.install_url() {
                            a."btn" href=(url) { "Install on an account" }
                        } @else {
                            span."muted small" { "Install the App from the GitHub developer settings. GitHub returns you to this page." }
                        }
                        form method="post" action="/admin/repo" {
                            (admin::hidden("token", &token)) (admin::hidden("action", "refresh")) (admin::hidden("app", "-"))
                            button."quiet" type="submit" { "Refresh" }
                        }
                    }
                }))
                (ui::panel("Import a repository", Some("Connect an existing repository as a new app. Toolsite pulls its branch into the source archive of the app. An agent or the CLI then builds and publishes it."), html! {
                    @if installs.is_empty() {
                        p."muted" { "Install the App first." }
                    } @else {
                        (import_form(&token, &installs, None, "/admin/github"))
                    }
                }))
                (ui::panel("Repositories tagged toolsite", Some("Repositories the App can reach that carry the toolsite topic and are not connected to an app. Nothing is imported until you click Import."), html! {
                    @match &discovered {
                        Err(why) => p."muted" { "GitHub did not answer: " (why) },
                        Ok(found) if found.is_empty() => p."muted" { "No tagged repository is waiting. Repositories that toolsite creates carry the topic. Add the topic to any other repository to see it here." },
                        Ok(found) => {
                            table {
                                thead { tr { th { "Repository" } th { "Branch" } th { "App name" } th {} } }
                                tbody {
                                    @for (i, d) in found.iter().enumerate() {
                                        tr {
                                            td {
                                                a href={ "https://github.com/" (d.full_name) } target="_blank" { (d.full_name) }
                                                " " @if d.private { span."badge" { "private" } } @else { span."badge" { "public" } }
                                            }
                                            td."muted small" { (d.default_branch) }
                                            td {
                                                form method="post" action="/admin/repo" id={ "discover-" (i) } {
                                                    (admin::hidden("token", &token)) (admin::hidden("action", "import")) (admin::hidden("back", "/admin/github"))
                                                    (admin::hidden("installation", &d.installation_id.to_string()))
                                                    (admin::hidden("repo", &d.full_name))
                                                    (admin::hidden("branch", &d.default_branch))
                                                    (ui::combobox_full("app", "/admin/apps/search", "app name", &d.proposed_app, None, &format!("-{i}")))
                                                }
                                            }
                                            td."actions-cell" {
                                                button."quiet sm" type="submit" form={ "discover-" (i) } { "Import" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div."actions" style="margin-top: .75rem" {
                        a."btn quiet sm" href="/admin/github" { "Refresh" }
                    }
                }))
                (ui::panel("Connected apps", None, html! {
                    @if linked.is_empty() {
                        p."muted" { "No app is connected to a repository. Open the Repo tab of an app to create or import a repository." }
                    } @else {
                        table {
                            thead { tr { th { "App" } th { "Repository" } th { "Last push" } th { "Live app" } } }
                            tbody {
                                @for (app, link) in &linked {
                                    tr {
                                        td { a."row-link" href={ "/admin/apps/" (app) "/repo" } { (app) } }
                                        td { a href=(link.url()) target="_blank" { (link.full_name()) } " " span."muted small" { (link.branch) } }
                                        td."muted small" { @match &link.last_push { Some(p) => { code { (short(&p.sha)) } " " (ago(p.at)) }, None => "none" } }
                                        td."muted small" { @match &link.deployed { Some(d) => { code { (short(&d.sha)) } }, None => "unknown" } }
                                    }
                                }
                            }
                        }
                    }
                }))
            }
        }
    };
    admin::admin_page(
        &headers,
        &admin,
        Page {
            active: "github",
            title: "GitHub",
            crumbs: vec![],
            subtitle: Some(html! { "Apps that keep their source in a repository." }),
            actions: None,
            body,
            script: None,
        },
    )
}

fn ago(seconds: u64) -> String {
    let elapsed = now().saturating_sub(seconds);
    match elapsed {
        s if s < 90 => "just now".to_string(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}


/// What to paste where, when no App exists yet. Every value a person types
/// into GitHub or the service's variables is a copy row, and the webhook
/// secret is minted here so the same value lands in both places.
fn setup_guide(base: &str) -> Markup {
    let secret = crate::content::slug::random_token(40);
    let name = format!(
        "toolsite-{}",
        base.trim_start_matches("https://")
            .trim_start_matches("http://")
            .split([':', '/'])
            .next()
            .unwrap_or("site")
            .split('.')
            .next()
            .unwrap_or("site")
    );
    let env_block = format!(
        "TOOLSITE_GITHUB_APP_ID=\nTOOLSITE_GITHUB_APP_PRIVATE_KEY=\nTOOLSITE_GITHUB_APP_SLUG=\nTOOLSITE_GITHUB_WEBHOOK_SECRET={secret}"
    );
    html! {
        (ui::panel("1. Create a GitHub App", Some("Open the GitHub form in a new tab. Paste these values. Keep the other default values."), html! {
            p { a."btn" href="https://github.com/settings/apps/new" target="_blank" rel="noopener" { "Open github.com/settings/apps/new" } }
            dl."kv" style="margin-top: .75rem" {
                dt { "GitHub App name" } dd { (ui::secret("gh-name", &name)) }
                dt { "Homepage URL" } dd { (ui::secret("gh-home", base)) }
                dt { "Setup URL" } dd { (ui::secret("gh-setup", &format!("{base}/github/setup"))) p."muted small" { "Tick \"Redirect on update\"." } }
                dt { "Webhook URL" } dd { (ui::secret("gh-webhook", &format!("{base}/github/webhook"))) }
                dt { "Webhook secret" } dd { (ui::secret("gh-secret", &secret)) p."muted small" { "This secret is new. Paste the same value in step 3." } }
            }
            p."small" style="margin-top: .75rem" { "Repository permissions:" }
            ul."small" {
                li { "Contents: read and write" }
                li { "Administration: read and write" }
                li { "Metadata: read" }
            }
            p."small" { "Subscribe to this event: " code { "push" } ". Set where the App can be installed to your account or to any account." }
        }))
        (ui::panel("2. Generate a private key", Some("After GitHub creates the App, open the App page. Under Private keys, click Generate a private key. A .pem file downloads. Note the App ID at the top of the page and the slug in the page URL."), html! {}))
        (ui::panel("3. Set the variables", Some("Set these variables on the service and restart it. This page then shows an Install button."), html! {
            div."secret" {
                pre id="gh-env" style="flex: 1; margin: 0; background: none; border: 0; padding: 0" { (env_block) }
                button."quiet sm" type="button" data-copy="gh-env" { "Copy" }
            }
            p."muted small" {
                "APP_ID is the number on the App page. PRIVATE_KEY is the content of the .pem file, or its base64, on one line. "
                "SLUG is the name in the App URL. WEBHOOK_SECRET is the secret from step 1."
            }
        }))
    }
}

fn import_form(token: &str, installs: &[Installation], app: Option<&str>, back: &str) -> Markup {
    html! {
        form method="post" action="/admin/repo" {
            (admin::hidden("token", token)) (admin::hidden("action", "import")) (admin::hidden("back", back))
            @if let Some(app) = app {
                (admin::hidden("app", app))
            } @else {
                div."field" {
                    label { "App name" }
                    (ui::combobox("app", "/admin/apps/search", "my-app"))
                    p."help" { "Type to search the apps, or enter a new name. The app is served at /p/<name>/." }
                }
            }
            div."field" {
                label for="import-installation" { "Account" }
                select id="import-installation" name="installation" {
                    @for inst in installs { option value=(inst.id) { (inst.account) } }
                }
            }
            div."field" {
                label { "Repository" }
                (ui::combobox_prefilled("repo", "/admin/github/repos/search", "owner/name", "", Some("installation")))
                p."help" { "Type to search the repositories of the account." }
            }
            div."grid-2" {
                div."field" {
                    label for="import-branch" { "Branch" }
                    input id="import-branch" name="branch" placeholder="default branch";
                }
                div."field" {
                    label for="import-dir" { "Directory" }
                    input id="import-dir" name="directory" placeholder="root of the repository";
                    p."help" { "Enter a directory if the project is not at the repository root." }
                }
            }
            div."actions end" { button type="submit" { "Import repository" } }
        }
    }
}

/// The Repo tab on an app's page.
pub(crate) async fn render_repo_tab(config: &Config, app: &str, token: &str, back: &str, fresh_token: Option<&str>) -> Markup {
    let link = link(config, app).await;
    let installs = installations(config).await;
    let has_source = crate::content::files::path(config, &format!("{app}.source")).await.is_some();
    let tokens = deploy::list(config, app).await;
    let inspected = match &link {
        Some(link) if config.github.is_some() => Some(inspect(config, link).await),
        _ => None,
    };
    html! {
        @if config.github.is_none() {
            (ui::panel("GitHub is not configured", Some("Set the TOOLSITE_GITHUB_* variables to connect repositories. Deploy tokens below work without GitHub, for a CI system of your own."), html! {}))
        } @else if let Some(link) = &link {
            (ui::panel("Repository", Some("The repository holds the source and its history. Publishing the source pushes a commit. A push to the repository is pulled into the source archive. Building and publishing happen where the agent or the CLI runs."), html! {
                dl."kv" {
                    dt { "Repository" } dd { a href=(link.url()) target="_blank" { (link.full_name()) } }
                    dt { "Branch" } dd { code { (link.branch) } @if !link.directory.is_empty() { " in " code { (link.directory) } } }
                    dt { "Connected" } dd { (ago(link.connected_at)) }
                    dt { "Last push" }
                    dd { @match &link.last_push { Some(p) => { code { (short(&p.sha)) } " " span."muted small" { (ago(p.at)) } }, None => "none" } }
                    dt { "Live app" }
                    dd { @match &link.deployed { Some(d) => { code { (short(&d.sha)) } " " span."muted small" { (ago(d.at)) } }, None => span."muted small" { "no publish has named a commit" } } }
                    @if let Some(Ok((drift, _))) = &inspected {
                        dt { "Head" } dd { code { (short(&drift.head)) } }
                    }
                }
                @match &inspected {
                    Some(Ok((drift, _))) => {
                        @if drift.live_is_head() {
                            p."small" style="margin-top:.75rem" { span."badge ok" { "current" } " " (drift.sentence()) }
                        } @else if drift.deployed.is_some() {
                            p."small" style="margin-top:.75rem" { span."badge warn" { "behind" } " " (drift.sentence()) }
                        } @else {
                            p."muted small" style="margin-top:.75rem" { (drift.sentence()) }
                        }
                    }
                    Some(Err(why)) => p."muted small" style="margin-top:.75rem" { "GitHub did not answer: " (why) },
                    None => {}
                }
                div."actions" style="margin-top:1rem" {
                    form method="post" action="/admin/repo"
                         data-confirm="Pull from the repository?"
                         data-confirm-detail="Toolsite stores the branch as the source archive of this app. The live app does not change until someone publishes."
                         data-confirm-label="Pull from repository" {
                        (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "pull"))
                        button type="submit" { "Pull from repository" }
                    }
                    form method="post" action="/admin/repo"
                         data-confirm={ "Disconnect " (link.full_name()) "?" }
                         data-confirm-detail="Toolsite forgets the link. The repository and the app are not changed."
                         data-confirm-label="Disconnect repository" data-confirm-danger="1" {
                        (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "disconnect"))
                        button."danger quiet" type="submit" { "Disconnect repository" }
                    }
                }
            }))
            (ui::panel("Recent commits", Some("The newest commits on the branch."), html! {
                @match &inspected {
                    Some(Ok((_, commits))) if !commits.is_empty() => {
                        table {
                            thead { tr { th { "Commit" } th { "Subject" } th { "Author" } th { "Date" } } }
                            tbody {
                                @for c in commits {
                                    tr {
                                        td { a href=(c.url) target="_blank" { code { (short(&c.sha)) } } }
                                        td { (c.subject) }
                                        td."muted small" { (c.author) }
                                        td."muted small" { (c.date) }
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => p."muted" { "The branch has no commits." },
                    Some(Err(why)) => p."muted" { "GitHub did not answer: " (why) },
                    None => p."muted" { "Not available." },
                }
            }))
        } @else {
            div."grid-2" {
                (ui::panel("Create a repository", Some("Toolsite creates a repository and pushes the source archive of this app. After that, publishing the source pushes a commit, and a push to the repository is pulled into the source archive."), html! {
                    @if installs.is_empty() {
                        p."muted" { "Install the App on an account first, from the " a href="/admin/github" { "GitHub page" } "." }
                    } @else if !has_source {
                        p."muted" { "This app has no source archive. Publish the source with " code { "?source" } " first." }
                    } @else {
                        form method="post" action="/admin/repo" {
                            (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "create"))
                            div."field" {
                                label for="create-installation" { "Account" }
                                select id="create-installation" name="installation" {
                                    @for inst in &installs { option value=(inst.id) { (inst.account) } }
                                }
                            }
                            div."field" {
                                label for="create-name" { "Repository name" }
                                input id="create-name" name="repo" value={ "toolsite-" (app) } required pattern="[A-Za-z0-9._-]+";
                            }
                            label."choice" {
                                input type="checkbox" name="private" value="1" checked;
                                strong { "Private" }
                                span { "Only members of the account can see the repository." }
                            }
                            div."actions end" { button type="submit" { "Create repository" } }
                        }
                    }
                }))
                (ui::panel("Import a repository", Some("Connect an existing repository. Toolsite pulls its branch into the source archive of this app. Nothing is built or published."), html! {
                    @if installs.is_empty() {
                        p."muted" { "Install the App first." }
                    } @else {
                        (import_form(token, &installs, Some(app), back))
                    }
                }))
            }
        }

        @if let Some(fresh) = fresh_token {
            (ui::panel("New deploy token", Some("Copy the token now. The token is shown one time only."), html! {
                (ui::secret("fresh-deploy-token", fresh))
                (ui::secret("fresh-deploy-curl", &format!("tar -czf - -C dist . | curl -f -H 'Authorization: Bearer {fresh}' -T - '{}?bundle'", deploy::deploy_url(config, app))))
            }))
        }
        (ui::panel("Deploy tokens", Some("A deploy token can publish this app only, from a CI system of your own. Use it with PUT /deploy/<app> and the same flags as an upload ticket. Add commit=<sha> to say which commit was published."), html! {
            @if tokens.is_empty() {
                p."muted" { "No deploy tokens. Create one below." }
            } @else {
                table {
                    thead { tr { th { "Label" } th { "Created" } th { "Last used" } th {} } }
                    tbody {
                        @for entry in &tokens {
                            tr {
                                td { (entry.label) " " span."muted small" { (entry.id) } }
                                td."muted small" { (ago(entry.created_at)) }
                                td."muted small" { @match entry.last_used { Some(at) => (ago(at)), None => "never" } }
                                td."actions-cell" {
                                    form method="post" action="/admin/repo"
                                         data-confirm={ "Revoke " (entry.label) "?" }
                                         data-confirm-detail="The system that holds this token gets 401 on the next push."
                                         data-confirm-label="Revoke token" data-confirm-danger="1" {
                                        (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back))
                                        (admin::hidden("action", "token-revoke")) (admin::hidden("id", &entry.id))
                                        button."danger quiet sm" type="submit" { "Revoke token" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            form."row" method="post" action="/admin/repo" {
                (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "token-create"))
                input name="label" placeholder="Label, for example ci" required;
                button."quiet" type="submit" { "Create token" }
            }
        }))
    }
}

#[derive(Deserialize)]
pub(crate) struct RepoForm {
    token: String,
    action: String,
    app: String,
    back: Option<String>,
    installation: Option<u64>,
    repo: Option<String>,
    branch: Option<String>,
    directory: Option<String>,
    private: Option<String>,
    label: Option<String>,
    id: Option<String>,
}

/// Every POST from the GitHub page and the Repo tab.
pub(crate) async fn repo_action(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Form(form): Form<RepoForm>,
) -> Response {
    let admin = match admin::checked(&config, &headers, &form.token).await {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let app = form.app.trim().to_string();
    let tab = format!("/admin/apps/{app}/repo");
    let back = admin::back_or(form.back.as_deref(), &tab);
    let outcome: Result<Option<String>, String> = match form.action.as_str() {
        "refresh" => refresh_installations(&config).await.map(|list| Some(format!("GitHub lists {} installations.", list.len()))),
        "create" => match form.installation {
            Some(inst) => create(&config, &app, inst, form.repo.as_deref(), form.private.is_some())
                .await
                .map(|link| Some(format!("Repository {} is created. The source is pushed.", link.full_name()))),
            None => Err("Choose an account.".into()),
        },
        "import" => match (form.installation, form.repo.as_deref()) {
            (Some(inst), Some(repo)) => import(&config, &app, inst, repo, form.branch.as_deref(), form.directory.as_deref())
                .await
                .map(|link| Some(format!("Repository {} is connected. Its branch is in the source archive. Publish it with toolsite deploy, or ask the agent to.", link.full_name()))),
            _ => Err("Choose an account and a repository.".into()),
        },
        "pull" | "sync" => pull(&config, &app)
            .await
            .map(|pulled| Some(format!("Pulled commit {} into the source archive ({} bytes).", short(&pulled.sha), pulled.bytes))),
        "disconnect" => disconnect(&config, &app)
            .await
            .map(|link| Some(format!("Repository {} is disconnected.", link.full_name()))),
        "token-create" => {
            let label = form.label.unwrap_or_default();
            match deploy::create(&config, &app, &label).await {
                Ok((_, token)) => {
                    tracing::info!(admin = %admin.email, app = %app, "deploy token created");
                    return admin::app_tab(config, headers, app, "repo".into(), Some(admin::Fresh::DeployToken(token))).await;
                }
                Err(why) => Err(why),
            }
        }
        "token-revoke" => {
            let id = form.id.unwrap_or_default();
            deploy::revoke(&config, &app, &id).await.map(|()| Some("The token is revoked.".to_string()))
        }
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    match outcome {
        Ok(Some(text)) => {
            // An import from the GitHub page lands on the app's Repo tab once
            // the app exists; before its first publish there is no app page.
            let app_exists = crate::content::store::app_exists(&config, &app).await;
            let to = if form.action == "import" && back == "/admin/github" && app_exists { tab } else { back };
            admin::redirect_flash(&to, true, text)
        }
        Ok(None) => Redirect::to(&back).into_response(),
        Err(why) => {
            tracing::warn!(admin = %admin.email, app = %app, action = %form.action, %why, "repository action failed");
            admin::redirect_flash(&back, false, why)
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    q: Option<String>,
    installation: Option<u64>,
}

/// Names for the picker, at most twenty, never the whole account.
pub(crate) async fn repos_search(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Response {
    if let Err(response) = admin::require_admin(&config, &headers).await {
        return response;
    }
    let q = query.q.unwrap_or_default();
    if q.trim().is_empty() {
        return Json(Vec::<String>::new()).into_response();
    }
    let installation = match query.installation {
        Some(id) => Some(id),
        None => installations(&config).await.first().map(|i| i.id),
    };
    let installation = match installation {
        Some(id) => id,
        None => return Json(Vec::<String>::new()).into_response(),
    };
    match search_repos(&config, installation, &q, 20).await {
        Ok(names) => Json(names).into_response(),
        Err(why) => {
            tracing::warn!(%why, "repository search failed");
            Json(Vec::<String>::new()).into_response()
        }
    }
}

/// Lets the sidebar know whether to show the GitHub entry at all.
pub fn configured(config: &Config) -> bool {
    config.github.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    const KEY_PEM: &str = include_str!("../../tests/fixtures/oidc-test-key.pem");

    fn app() -> App {
        App::new("12345", KEY_PEM, Some("toolsite".into()), Some("hook-secret".into()), Some("http://127.0.0.1:1".into())).unwrap()
    }

    #[test]
    fn the_private_key_is_taken_raw_or_base64() {
        app();
        let encoded = BASE64.encode(KEY_PEM);
        App::new("1", &encoded, None, None, None).unwrap();
        assert!(App::new("1", "not a key", None, None, None).is_err());
    }

    #[test]
    fn the_app_jwt_is_short_lived_and_names_the_app() {
        let token = app().jwt().unwrap();
        let payload = token.split('.').nth(1).unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert_eq!(claims["iss"], "12345");
        let life = claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap();
        assert!(life <= 600, "a GitHub App JWT may live ten minutes at most, this one {life}s");
    }

    #[test]
    fn a_webhook_signature_is_checked_against_the_body() {
        let body = br#"{"ref":"refs/heads/main"}"#;
        let good = format!("sha256={}", hex(&hmac_sha256(b"hook-secret", body)));
        assert!(signature_valid("hook-secret", Some(&good), body));
        assert!(!signature_valid("hook-secret", Some(&good), br#"{"ref":"refs/heads/other"}"#));
        assert!(!signature_valid("other-secret", Some(&good), body));
        assert!(!signature_valid("hook-secret", None, body));
        assert!(!signature_valid("hook-secret", Some("sha1=abc"), body));
        // RFC 4231 test case 2, so the HMAC itself is known good.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_publishers_message_is_capped_cleaned_and_signed_off() {
        let plain = commit_message(None, "Update shop from toolsite");
        assert_eq!(plain, "Update shop from toolsite\n\nPublished from toolsite");
        let given = commit_message(Some("  Fix the phone field \u{7}\n\nIt was too short.\n"), "x");
        assert_eq!(given, "Fix the phone field\n\nIt was too short.\n\nPublished from toolsite");
        let long = "a".repeat(300);
        let capped = commit_message(Some(&long), "x");
        assert_eq!(capped.lines().next().unwrap().len(), 200);
        assert_eq!(commit_message(Some("   \n"), "fallback").lines().next(), Some("fallback"));
    }

    #[test]
    fn a_pulled_tarball_loses_its_top_directory_and_keeps_the_project() {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        for (path, body) in [
            ("acme-shop-abc123/README.md", &b"# shop"[..]),
            ("acme-shop-abc123/web/package.json", &b"{}"[..]),
            ("acme-shop-abc123/web/node_modules/x/index.js", &b"nope"[..]),
            ("acme-shop-abc123/.github/workflows/old.yml", &b"on: push"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, body).unwrap();
        }
        let tarball = tar.into_inner().unwrap().finish().unwrap();

        let whole = crate::content::bundle::read_all_files(&repack(&tarball, "").unwrap(), 100, 1 << 20).unwrap();
        let paths: Vec<&str> = whole.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["README.md", "web/package.json"], "{paths:?}");

        let narrowed = crate::content::bundle::read_all_files(&repack(&tarball, "web/").unwrap(), 100, 1 << 20).unwrap();
        let paths: Vec<&str> = narrowed.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["package.json"], "{paths:?}");
    }

    #[test]
    fn drift_is_said_plainly_in_each_state() {
        let unknown = Drift { head: "abc".into(), deployed: None, ahead_by: None };
        assert!(unknown.sentence().contains("cannot be compared"));
        let current = Drift { head: "abcdef1234".into(), deployed: Some("abcdef1".into()), ahead_by: None };
        assert_eq!(current.sentence(), "The live app is the repository's head.");
        let behind = Drift { head: "fff".into(), deployed: Some("aaa".into()), ahead_by: Some(3) };
        assert!(behind.sentence().starts_with("The repository is 3 commits ahead of the live app."));
        let one = Drift { head: "fff".into(), deployed: Some("aaa".into()), ahead_by: Some(1) };
        assert!(one.sentence().starts_with("The repository is 1 commit ahead"));
        let moved = Drift { head: "fff".into(), deployed: Some("aaa".into()), ahead_by: None };
        assert!(moved.sentence().contains("has moved"));
    }

    #[test]
    fn names_that_could_reach_outside_a_repository_are_refused() {
        assert_eq!(proposed_app_name("toolsite-shop"), "shop");
        assert_eq!(proposed_app_name("Toolsite-My.App"), "my-app");
        assert_eq!(proposed_app_name("plain"), "plain");
        assert_eq!(proposed_app_name("toolsite-"), "app");
        assert!(valid_repo_name("my-app.v2"));
        assert!(!valid_repo_name("../x"));
        assert!(!valid_repo_name(".git"));
        assert!(!valid_repo_name("a b"));
        assert!(valid_owner("bherbruck"));
        assert!(!valid_owner("a/b"));
        assert!(valid_branch("release/1.2"));
        assert!(!valid_branch("a..b"));
        assert_eq!(clean_directory(Some(" web/ ")).unwrap(), "web/");
        assert_eq!(clean_directory(None).unwrap(), "");
        assert!(clean_directory(Some("../x")).is_err());
    }

    #[tokio::test]
    async fn a_disconnected_link_is_kept_but_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        let (entry, token) = deploy::create(&config, "shop", "github:o/r").await.unwrap();
        write_link(&config, "shop", &RepoLink {
            owner: "o".into(), repo: "r".into(), branch: "main".into(), directory: String::new(),
            installation_id: 1, token_id: entry.id, connected_at: now(), last_push: None, deployed: None, disconnected_at: None,
        }).await.unwrap();
        assert_eq!(linked_apps(&config).await.len(), 1);
        assert_eq!(link_for_repo(&config, "O/R").await.map(|(app, _)| app), Some("shop".to_string()));
        disconnect(&config, "shop").await.unwrap();
        assert!(link(&config, "shop").await.is_none());
        assert!(linked_apps(&config).await.is_empty());
        assert!(dir.path().join("shop.repo").exists(), "the record was destroyed");
        assert!(!deploy::authorize(&config, "shop", &token).await, "an older link's token outlived it");
    }
}
