//! Keeping an app's project in a GitHub repository, and deploying from it.
//!
//! Toolsite does not build. It is one binary with no toolchain in it, and the
//! build belongs where the source is: GitHub Actions runs it, and the result
//! comes back through `PUT /deploy/<app>` with a token that can publish that
//! one app and nothing else. What toolsite does is set that up and keep track:
//!
//! - **Create**: a new repository holding the app's stored source and a
//!   workflow, with the two secrets the workflow needs.
//! - **Import**: the same workflow and secrets added to a repository you
//!   already have, then a first run.
//! - **Watch**: a webhook records the last push and the last deploy, so the
//!   Repo tab can say what happened without a trip to GitHub.
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
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The workflow every connected repository gets. `__APP__`, `__BRANCH__`,
/// `__DIR__` and `__WORKDIR__` are filled in per app.
pub const WORKFLOW_TEMPLATE: &str = include_str!("../../templates/toolsite.yml");
pub const WORKFLOW_PATH: &str = ".github/workflows/toolsite.yml";
const SECRET_URL: &str = "TOOLSITE_URL";
const SECRET_TOKEN: &str = "TOOLSITE_DEPLOY_TOKEN";
/// A source archive handed to a repository, at most.
const MAX_SOURCE_FILES: usize = 2_000;
const MAX_SOURCE_BYTES: usize = 50 * 1024 * 1024;
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

fn installations_path(config: &Config) -> PathBuf {
    config.data_dir.join(".site").join("github.json")
}

pub fn installations(config: &Config) -> Vec<Installation> {
    std::fs::read_to_string(installations_path(config))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_installations(config: &Config, list: &[Installation]) -> Result<(), String> {
    let path = installations_path(config);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(list).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
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
    write_installations(config, &list)?;
    Ok(list)
}

async fn installation(config: &Config, id: u64) -> Result<Installation, String> {
    if let Some(found) = installations(config).into_iter().find(|i| i.id == id) {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deploy {
    pub at: u64,
    /// GitHub's word: success, failure, cancelled, …
    pub conclusion: String,
    pub url: String,
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
    /// The deploy token the workflow holds, by id, so it can be rotated and
    /// revoked without touching any other.
    pub token_id: String,
    pub connected_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_push: Option<Push>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deploy: Option<Deploy>,
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

fn link_path(config: &Config, app: &str) -> Option<PathBuf> {
    valid_app(app).then(|| config.data_dir.join(format!("{app}.repo")))
}

fn read_link_raw(config: &Config, app: &str) -> Option<RepoLink> {
    let path = link_path(config, app)?;
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// The app's live link, if it has one.
pub fn link(config: &Config, app: &str) -> Option<RepoLink> {
    read_link_raw(config, app).filter(|link| link.disconnected_at.is_none())
}

fn write_link(config: &Config, app: &str, link: &RepoLink) -> Result<(), String> {
    let path = link_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    std::fs::write(&path, serde_json::to_string_pretty(link).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// Every live link on the site, by app.
pub fn linked_apps(config: &Config) -> Vec<(String, RepoLink)> {
    let Ok(entries) = std::fs::read_dir(&config.data_dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, RepoLink)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let app = name.strip_suffix(".repo")?.to_string();
            let link = link(config, &app)?;
            Some((app, link))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn link_for_repo(config: &Config, full_name: &str) -> Option<(String, RepoLink)> {
    linked_apps(config)
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

pub fn workflow_for(app: &str, branch: &str, directory: &str) -> String {
    let workdir = if directory.is_empty() {
        ".".to_string()
    } else {
        directory.trim_end_matches('/').to_string()
    };
    WORKFLOW_TEMPLATE
        .replace("__APP__", app)
        .replace("__BRANCH__", branch)
        .replace("__WORKDIR__", &workdir)
        .replace("__DIR__", directory)
}

// --- talking to a repository ---------------------------------------------------

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

    /// The repository's record, or the reason it cannot be reached.
    async fn get(&self) -> Result<serde_json::Value, String> {
        self.expect("reading the repository", reqwest::Method::GET, "", None).await
    }

    /// Sets a repository secret, sealed to the repository's public key the
    /// way the Actions API demands.
    async fn put_secret(&self, name: &str, value: &str) -> Result<(), String> {
        let key = self
            .expect("fetching the repository's public key", reqwest::Method::GET, "/actions/secrets/public-key", None)
            .await?;
        let key_id = key["key_id"].as_str().ok_or("public key answer had no key_id")?.to_string();
        let public = key["key"].as_str().ok_or("public key answer had no key")?;
        let sealed = seal(public, value)?;
        self.expect(
            &format!("setting the secret {name}"),
            reqwest::Method::PUT,
            &format!("/actions/secrets/{name}"),
            Some(serde_json::json!({ "encrypted_value": sealed, "key_id": key_id })),
        )
        .await?;
        Ok(())
    }

    /// Creates or updates one file through the Contents API.
    async fn put_file(&self, path: &str, content: &[u8], message: &str, branch: &str) -> Result<(), String> {
        let (status, existing) = self
            .call(reqwest::Method::GET, &format!("/contents/{path}?ref={branch}"), None)
            .await?;
        let mut body = serde_json::json!({
            "message": message,
            "content": BASE64.encode(content),
            "branch": branch,
        });
        if status.is_success()
            && let Some(sha) = existing["sha"].as_str()
        {
            body["sha"] = serde_json::Value::String(sha.to_string());
        }
        self.expect(&format!("writing {path}"), reqwest::Method::PUT, &format!("/contents/{path}"), Some(body))
            .await?;
        Ok(())
    }

    /// One commit with every file, on top of the branch's tip. A brand-new
    /// repository is still being initialised for a moment after it is
    /// created, so the tip is asked for with patience.
    async fn commit_files(&self, branch: &str, files: &[(String, Vec<u8>)], message: &str) -> Result<String, String> {
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
        let base_tree = base["tree"]["sha"].as_str().ok_or("the tip commit has no tree")?.to_string();

        let mut entries = Vec::with_capacity(files.len());
        for (path, content) in files {
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
        let tree = self
            .expect(
                "building the tree",
                reqwest::Method::POST,
                "/git/trees",
                Some(serde_json::json!({ "base_tree": base_tree, "tree": entries })),
            )
            .await?;
        let tree_sha = tree["sha"].as_str().ok_or("tree answer had no sha")?.to_string();
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
        Ok(sha)
    }

    async fn dispatch(&self, branch: &str) -> Result<(), String> {
        self.expect(
            "starting the workflow",
            reqwest::Method::POST,
            &format!("/actions/workflows/{}/dispatches", WORKFLOW_PATH.rsplit('/').next().unwrap_or("toolsite.yml")),
            Some(serde_json::json!({ "ref": branch })),
        )
        .await?;
        Ok(())
    }
}

/// libsodium's sealed box, which is what the Actions secrets API accepts.
fn seal(public_key_b64: &str, value: &str) -> Result<String, String> {
    let bytes = BASE64
        .decode(public_key_b64)
        .map_err(|_| "the repository's public key is not base64")?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "the repository's public key is not 32 bytes")?;
    let public = crypto_box::PublicKey::from_bytes(key);
    let sealed = public
        .seal(&mut crypto_box::aead::OsRng, value.as_bytes())
        .map_err(|_| "sealing the secret failed")?;
    Ok(BASE64.encode(sealed))
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

// --- the operations ------------------------------------------------------------

fn site_url(config: &Config) -> Result<String, String> {
    config
        .base_url
        .clone()
        .ok_or_else(|| "TOOLSITE_BASE_URL must be set: the workflow needs to know where to deploy to".to_string())
}

fn mint_deploy_token(config: &Config, app: &str, full_name: &str) -> Result<(String, String), String> {
    let (entry, token) = deploy::create(config, app, &format!("github:{full_name}"))?;
    Ok((entry.id, token))
}

/// Makes a repository for `app` out of its stored source, with the workflow
/// and secrets a deploy needs.
pub async fn create(
    config: &Config,
    app_name: &str,
    installation_id: u64,
    repo_name: Option<&str>,
    private: bool,
) -> Result<RepoLink, String> {
    let app = app_of(config)?;
    let site = site_url(config)?;
    if !valid_app(app_name) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    if let Some(existing) = link(config, app_name) {
        return Err(format!("{app_name} is already connected to {}; disconnect it first", existing.full_name()));
    }
    let repo_name = repo_name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(app_name);
    if !valid_repo_name(repo_name) {
        return Err(format!("{repo_name} is not a repository name: letters, digits, '-', '_' and '.'"));
    }
    let archive = std::fs::read(config.data_dir.join(format!("{app_name}.source"))).map_err(|_| {
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

    let (token_id, deploy_token) = mint_deploy_token(config, app_name, &full_name)?;
    repo.put_secret(SECRET_URL, &site).await?;
    repo.put_secret(SECRET_TOKEN, &deploy_token).await?;

    files.retain(|(path, _)| path != WORKFLOW_PATH);
    files.push((WORKFLOW_PATH.to_string(), workflow_for(app_name, &branch, "").into_bytes()));
    let sha = repo
        .commit_files(&branch, &files, &format!("Add {app_name} from toolsite, with its deploy workflow"))
        .await?;

    let link = RepoLink {
        owner,
        repo: name,
        branch,
        directory: String::new(),
        installation_id,
        token_id,
        connected_at: now(),
        last_push: Some(Push { at: now(), sha }),
        last_deploy: None,
        disconnected_at: None,
    };
    write_link(config, app_name, &link)?;
    tracing::info!(app = %app_name, repo = %link.full_name(), "repository created");
    Ok(link)
}

/// Connects `app` to a repository that already exists: workflow, secrets,
/// and a first run. Nothing is cloned and nothing is built here.
pub async fn import(
    config: &Config,
    app_name: &str,
    installation_id: u64,
    full_name: &str,
    branch: Option<&str>,
    directory: Option<&str>,
) -> Result<RepoLink, String> {
    let app = app_of(config)?;
    let site = site_url(config)?;
    if !valid_app(app_name) {
        return Err("app must be one path segment of letters, numbers, '-' or '_'".into());
    }
    if let Some(existing) = link(config, app_name) {
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
    let record = repo.get().await?;
    let branch = match branch.map(str::trim).filter(|b| !b.is_empty()) {
        Some(branch) if valid_branch(branch) => branch.to_string(),
        Some(branch) => return Err(format!("{branch} is not a branch name")),
        None => record["default_branch"].as_str().unwrap_or("main").to_string(),
    };
    let full = format!("{}/{}", repo.owner, repo.name);

    let (token_id, deploy_token) = mint_deploy_token(config, app_name, &full)?;
    repo.put_secret(SECRET_URL, &site).await?;
    repo.put_secret(SECRET_TOKEN, &deploy_token).await?;
    repo.put_file(
        WORKFLOW_PATH,
        workflow_for(app_name, &branch, &directory).as_bytes(),
        &format!("Deploy {app_name} to toolsite on push"),
        &branch,
    )
    .await?;
    // The workflow file has to exist before it can be dispatched; GitHub
    // registers it a moment after the commit lands.
    let mut started = Err(String::new());
    for attempt in 0..5 {
        started = repo.dispatch(&branch).await;
        if started.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(600 * (attempt + 1))).await;
    }
    if let Err(why) = started {
        tracing::warn!(app = %app_name, repo = %full, %why, "connected, but the first run did not start");
    }

    let link = RepoLink {
        owner: repo.owner.clone(),
        repo: repo.name.clone(),
        branch,
        directory,
        installation_id,
        token_id,
        connected_at: now(),
        last_push: None,
        last_deploy: None,
        disconnected_at: None,
    };
    write_link(config, app_name, &link)?;
    tracing::info!(app = %app_name, repo = %full, "repository imported");
    Ok(link)
}

/// Runs the workflow now.
pub async fn sync(config: &Config, app_name: &str) -> Result<(), String> {
    let app = app_of(config)?;
    let link = link(config, app_name).ok_or_else(|| format!("{app_name} is not connected to a repository"))?;
    let repo = open_repo(app, link.installation_id, &link.owner, &link.repo).await?;
    repo.dispatch(&link.branch).await
}

/// A new deploy token in the repository's secret, the old one dead.
pub async fn rotate(config: &Config, app_name: &str) -> Result<String, String> {
    let app = app_of(config)?;
    let mut link = link(config, app_name).ok_or_else(|| format!("{app_name} is not connected to a repository"))?;
    let repo = open_repo(app, link.installation_id, &link.owner, &link.repo).await?;
    let (token_id, token) = mint_deploy_token(config, app_name, &link.full_name())?;
    repo.put_secret(SECRET_TOKEN, &token).await?;
    let _ = deploy::revoke(config, app_name, &link.token_id);
    link.token_id = token_id;
    write_link(config, app_name, &link)?;
    Ok(token)
}

/// Forgets the link and revokes its token. The repository stays.
pub fn disconnect(config: &Config, app_name: &str) -> Result<RepoLink, String> {
    let mut link = link(config, app_name).ok_or_else(|| format!("{app_name} is not connected to a repository"))?;
    let _ = deploy::revoke(config, app_name, &link.token_id);
    link.disconnected_at = Some(now());
    write_link(config, app_name, &link)?;
    tracing::info!(app = %app_name, repo = %link.full_name(), "repository disconnected");
    Ok(link)
}

/// One line about the link, for a tool call.
pub fn describe(config: &Config, app_name: &str) -> String {
    match link(config, app_name) {
        None => format!("{app_name} is not connected to a repository"),
        Some(link) => {
            let mut text = format!("{app_name} deploys from {} ({})", link.url(), link.branch);
            if !link.directory.is_empty() {
                text.push_str(&format!(", directory {}", link.directory));
            }
            match &link.last_push {
                Some(push) => text.push_str(&format!("; last push {} ({} h ago)", &push.sha[..push.sha.len().min(7)], now().saturating_sub(push.at) / 3600)),
                None => text.push_str("; no push seen yet"),
            }
            match &link.last_deploy {
                Some(deploy) => text.push_str(&format!("; last deploy {} ({})", deploy.conclusion, deploy.url)),
                None => text.push_str("; no deploy seen yet"),
            }
            text
        }
    }
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

/// `POST /github/webhook`: push and workflow_run, for repositories an app
/// is linked to. Anything unsigned is refused before it is parsed.
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
    let outcome = tokio::task::spawn_blocking(move || record_event(&config, &event, &full_name, &payload)).await;
    match outcome {
        Ok(Some(what)) => {
            tracing::info!(%what, "webhook recorded");
            (StatusCode::ACCEPTED, "recorded\n").into_response()
        }
        Ok(None) => (StatusCode::ACCEPTED, "ignored\n").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not record the event\n").into_response(),
    }
}

fn record_event(config: &Config, event: &str, full_name: &str, payload: &serde_json::Value) -> Option<String> {
    let (app, mut link) = link_for_repo(config, full_name)?;
    match event {
        "push" => {
            let reference = payload["ref"].as_str().unwrap_or("");
            if reference != format!("refs/heads/{}", link.branch) {
                return None;
            }
            let sha = payload["after"].as_str().unwrap_or("").to_string();
            if sha.is_empty() {
                return None;
            }
            link.last_push = Some(Push { at: now(), sha });
            write_link(config, &app, &link).ok()?;
            Some(format!("push to {full_name}"))
        }
        "workflow_run" => {
            if payload["action"].as_str() != Some("completed") {
                return None;
            }
            let run = &payload["workflow_run"];
            let path = run["path"].as_str().unwrap_or("");
            if !path.ends_with("toolsite.yml") {
                return None;
            }
            link.last_deploy = Some(Deploy {
                at: now(),
                conclusion: run["conclusion"].as_str().unwrap_or("unknown").to_string(),
                url: run["html_url"].as_str().unwrap_or("").to_string(),
            });
            write_link(config, &app, &link).ok()?;
            Some(format!("deploy of {full_name}"))
        }
        _ => None,
    }
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
        Some(app) if installations(&config).is_empty() => ui::panel(
            "Install the App",
            Some("The App is configured. Install it on the account or organisation whose repositories it should reach; GitHub sends you back here."),
            html! {
                div."actions" {
                    @if let Some(url) = app.install_url() {
                        a."btn" href=(url) { "Install on an account" }
                    } @else {
                        span."muted small" { "Set TOOLSITE_GITHUB_APP_SLUG for an install button, or install from GitHub's developer settings; it sends you back here." }
                    }
                    form method="post" action="/admin/repo" {
                        (admin::hidden("token", &token)) (admin::hidden("action", "refresh")) (admin::hidden("app", "-"))
                        button."quiet" type="submit" { "Refresh" }
                    }
                }
                p."muted small" style="margin-top: 1rem" {
                    "Double-check the App's webhook URL is " code { (base) "/github/webhook" } "."
                }
            },
        ),
        Some(app) => {
            let installs = installations(&config);
            let linked = linked_apps(&config);
            html! {
                (ui::panel("Installed on", Some("Accounts the App may create and read repositories in."), html! {
                    @if installs.is_empty() {
                        p."muted" { "Not installed anywhere yet." }
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
                            span."muted small" { "Install the App from GitHub's developer settings; it sends you back here." }
                        }
                        form method="post" action="/admin/repo" {
                            (admin::hidden("token", &token)) (admin::hidden("action", "refresh")) (admin::hidden("app", "-"))
                            button."quiet" type="submit" { "Refresh" }
                        }
                    }
                }))
                (ui::panel("Import a repository", Some("Connect a repository you already have as a new app. Toolsite adds the deploy workflow and secrets and runs it once."), html! {
                    @if installs.is_empty() {
                        p."muted" { "Install the App first." }
                    } @else {
                        (import_form(&token, &installs, None, "/admin/github"))
                    }
                }))
                (ui::panel("Connected apps", None, html! {
                    @if linked.is_empty() {
                        p."muted" { "No app deploys from a repository yet. Open an app's Repo tab to create or import one." }
                    } @else {
                        table {
                            thead { tr { th { "App" } th { "Repository" } th { "Last push" } th { "Last deploy" } } }
                            tbody {
                                @for (app, link) in &linked {
                                    tr {
                                        td { a."row-link" href={ "/admin/apps/" (app) "/repo" } { (app) } }
                                        td { a href=(link.url()) target="_blank" { (link.full_name()) } " " span."muted small" { (link.branch) } }
                                        td."muted small" { @match &link.last_push { Some(p) => (ago(p.at)), None => "—" } }
                                        td { (deploy_badge(link.last_deploy.as_ref())) }
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
            subtitle: Some(html! { "Apps that live in a repository and deploy from it." }),
            actions: None,
            body,
            script: Some(PICKER_SCRIPT),
        },
    )
}

fn deploy_badge(deploy: Option<&Deploy>) -> Markup {
    html! {
        @match deploy {
            Some(d) if d.conclusion == "success" => a."badge ok" href=(d.url) target="_blank" { "success" },
            Some(d) => a."badge warn" href=(d.url) target="_blank" { (d.conclusion) },
            None => span."muted small" { "—" },
        }
    }
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

/// Fills a repository picker from `/admin/github/repos/search` as the
/// person types. Without script the field still takes `owner/name`.
const PICKER_SCRIPT: &str = r#"
<script>
document.querySelectorAll('input[data-repo-picker]').forEach((input) => {
  const list = document.getElementById(input.getAttribute('list'));
  const installation = () => {
    const select = input.form && input.form.querySelector('select[name=installation]');
    return select ? select.value : '';
  };
  let timer = null;
  input.addEventListener('input', () => {
    clearTimeout(timer);
    const q = input.value.trim();
    if (!q) return;
    timer = setTimeout(async () => {
      try {
        const res = await fetch('/admin/github/repos/search?installation=' + encodeURIComponent(installation()) + '&q=' + encodeURIComponent(q));
        if (!res.ok) return;
        const names = await res.json();
        list.innerHTML = '';
        names.forEach((name) => { const o = document.createElement('option'); o.value = name; list.appendChild(o); });
      } catch {}
    }, 150);
  });
});
</script>
"#;

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
        (ui::panel("1. Create a GitHub App", Some("Open the form below in a new tab and paste these values in. Leave everything else as GitHub sets it."), html! {
            p { a."btn" href="https://github.com/settings/apps/new" target="_blank" rel="noopener" { "Open github.com/settings/apps/new" } }
            dl."kv" style="margin-top: .75rem" {
                dt { "GitHub App name" } dd { (ui::secret("gh-name", &name)) }
                dt { "Homepage URL" } dd { (ui::secret("gh-home", base)) }
                dt { "Setup URL" } dd { (ui::secret("gh-setup", &format!("{base}/github/setup"))) p."muted small" { "Tick \"Redirect on update\"." } }
                dt { "Webhook URL" } dd { (ui::secret("gh-webhook", &format!("{base}/github/webhook"))) }
                dt { "Webhook secret" } dd { (ui::secret("gh-secret", &secret)) p."muted small" { "Made for you just now. Paste this same value in step 3." } }
            }
            p."small" style="margin-top: .75rem" { "Repository permissions:" }
            ul."small" {
                li { "Contents: read and write" }
                li { "Administration: read and write" }
                li { "Secrets: read and write" }
                li { "Actions: read and write" }
                li { "Workflows: read and write" }
                li { "Metadata: read" }
            }
            p."small" { "Subscribe to events: " code { "push" } ", " code { "workflow_run" } ". Where it can be installed: your account, or any." }
        }))
        (ui::panel("2. Generate a private key", Some("On the App's page after it is created: Private keys, Generate a private key. A .pem file downloads. Note the App ID at the top of that page and the slug in its URL."), html! {}))
        (ui::panel("3. Set these variables on the service", Some("Then restart. This page shows an Install button once the App is configured."), html! {
            div."secret" {
                pre id="gh-env" style="flex: 1; margin: 0; background: none; border: 0; padding: 0" { (env_block) }
                button."quiet sm" type="button" data-copy="gh-env" { "Copy" }
            }
            p."muted small" {
                "APP_ID is the number on the App's page. PRIVATE_KEY is the .pem's contents, or base64 of it, on one line. "
                "SLUG is the name in the App's URL. The secret is the one from step 1."
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
                    label for="import-app" { "App name" }
                    input id="import-app" name="app" placeholder="my-app" required pattern="[A-Za-z0-9_-]+";
                    p."help" { "The slug it will be served at: /p/<name>/." }
                }
            }
            div."field" {
                label for="import-installation" { "Account" }
                select id="import-installation" name="installation" {
                    @for inst in installs { option value=(inst.id) { (inst.account) } }
                }
            }
            div."field" {
                label for="import-repo" { "Repository" }
                input id="import-repo" name="repo" list="repo-options" placeholder="owner/name" required autocomplete="off" data-repo-picker;
                datalist id="repo-options" {}
                p."help" { "Type to search the account's repositories." }
            }
            div."grid-2" {
                div."field" {
                    label for="import-branch" { "Branch" }
                    input id="import-branch" name="branch" placeholder="default branch";
                }
                div."field" {
                    label for="import-dir" { "Directory" }
                    input id="import-dir" name="directory" placeholder="root of the repository";
                    p."help" { "If the project is in a subfolder." }
                }
            }
            div."actions end" { button type="submit" { "Import and deploy" } }
        }
    }
}

/// The Repo tab on an app's page.
pub(crate) async fn render_repo_tab(config: &Config, app: &str, token: &str, back: &str, fresh_token: Option<&str>) -> Markup {
    let link = link(config, app);
    let installs = installations(config);
    let has_source = config.data_dir.join(format!("{app}.source")).is_file();
    let tokens = deploy::list(config, app);
    html! {
        @if config.github.is_none() {
            (ui::panel("GitHub is not configured", Some("Set the TOOLSITE_GITHUB_* variables to connect repositories. Deploy tokens below work regardless, for any CI you already run."), html! {}))
        } @else if let Some(link) = &link {
            (ui::panel("Repository", None, html! {
                dl."kv" {
                    dt { "Repository" } dd { a href=(link.url()) target="_blank" { (link.full_name()) } }
                    dt { "Branch" } dd { code { (link.branch) } @if !link.directory.is_empty() { " in " code { (link.directory) } } }
                    dt { "Connected" } dd { (ago(link.connected_at)) }
                    dt { "Last push" }
                    dd { @match &link.last_push { Some(p) => { code { (&p.sha[..p.sha.len().min(7)]) } " " span."muted small" { (ago(p.at)) } }, None => "none seen yet" } }
                    dt { "Last deploy" }
                    dd { (deploy_badge(link.last_deploy.as_ref())) @if let Some(d) = &link.last_deploy { " " span."muted small" { (ago(d.at)) } } }
                }
                div."actions" style="margin-top:1rem" {
                    form method="post" action="/admin/repo" {
                        (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "sync"))
                        button type="submit" { "Sync now" }
                    }
                    form method="post" action="/admin/repo"
                         data-confirm="Rotate the deploy token?"
                         data-confirm-detail="The repository's secret is replaced and the old token stops working at once."
                         data-confirm-label="Rotate" {
                        (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "rotate"))
                        button."quiet" type="submit" { "Rotate deploy token" }
                    }
                    form method="post" action="/admin/repo"
                         data-confirm={ "Disconnect " (link.full_name()) "?" }
                         data-confirm-detail="The deploy token is revoked and pushes stop deploying. The repository itself is left alone."
                         data-confirm-label="Disconnect" data-confirm-danger="1" {
                        (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "disconnect"))
                        button."danger quiet" type="submit" { "Disconnect" }
                    }
                }
            }))
        } @else {
            div."grid-2" {
                (ui::panel("Create a repository", Some("A new repository holding this app's stored source and a deploy workflow. Pushes to it deploy here."), html! {
                    @if installs.is_empty() {
                        p."muted" { "Install the App on an account first, from the " a href="/admin/github" { "GitHub page" } "." }
                    } @else if !has_source {
                        p."muted" { "This app has no stored source. Publish the project first with " code { "?source" } ", then come back." }
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
                                input id="create-name" name="repo" value=(app) required pattern="[A-Za-z0-9._-]+";
                            }
                            label."choice" {
                                input type="checkbox" name="private" value="1" checked;
                                strong { "Private" }
                                span { "Only the account's members see it." }
                            }
                            div."actions end" { button type="submit" { "Create repository" } }
                        }
                    }
                }))
                (ui::panel("Import a repository", Some("Connect a repository you already have. Toolsite adds the workflow and secrets, then runs it."), html! {
                    @if installs.is_empty() {
                        p."muted" { "Install the App first." }
                    } @else {
                        (import_form(token, &installs, Some(app), back))
                    }
                }))
            }
        }

        @if let Some(fresh) = fresh_token {
            (ui::panel("New deploy token", Some("Copy it now; it is not stored and will not be shown again."), html! {
                (ui::secret("fresh-deploy-token", fresh))
                p."muted small" { "Use it as " code { "Authorization: Bearer <token>" } " on " code { "PUT " (deploy::deploy_url(config, app)) } "." }
            }))
        }
        (ui::panel("Deploy tokens", Some("Each one may publish this app and nothing else: the same flags as an upload ticket, on PUT /deploy/<app>. The workflow holds one; mint another for any other CI."), html! {
            @if tokens.is_empty() {
                p."muted" { "No deploy tokens." }
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
                                         data-confirm-detail="Whatever holds it gets 401 on its next push."
                                         data-confirm-label="Revoke" data-confirm-danger="1" {
                                        (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back))
                                        (admin::hidden("action", "token-revoke")) (admin::hidden("id", &entry.id))
                                        button."danger quiet sm" type="submit" { "Revoke" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            form."row" method="post" action="/admin/repo" {
                (admin::hidden("token", token)) (admin::hidden("app", app)) (admin::hidden("back", back)) (admin::hidden("action", "token-create"))
                input name="label" placeholder="What will hold it, e.g. ci" required;
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
        "refresh" => refresh_installations(&config).await.map(|list| Some(format!("{} installation(s).", list.len()))),
        "create" => match form.installation {
            Some(inst) => create(&config, &app, inst, form.repo.as_deref(), form.private.is_some())
                .await
                .map(|link| Some(format!("Created {} and pushed the project.", link.full_name()))),
            None => Err("choose an account".into()),
        },
        "import" => match (form.installation, form.repo.as_deref()) {
            (Some(inst), Some(repo)) => import(&config, &app, inst, repo, form.branch.as_deref(), form.directory.as_deref())
                .await
                .map(|link| Some(format!("Connected {}; the first deploy is running.", link.full_name()))),
            _ => Err("choose an account and a repository".into()),
        },
        "sync" => sync(&config, &app).await.map(|()| Some("Workflow started.".to_string())),
        "rotate" => match rotate(&config, &app).await {
            Ok(token) => {
                tracing::info!(admin = %admin.email, app = %app, "deploy token rotated");
                return admin::app_tab(config, headers, app, "repo".into(), Some(admin::Fresh::DeployToken(token))).await;
            }
            Err(why) => Err(why),
        },
        "disconnect" => {
            let (config2, app2) = (config.clone(), app.clone());
            match tokio::task::spawn_blocking(move || disconnect(&config2, &app2)).await {
                Ok(result) => result.map(|link| Some(format!("Disconnected from {}.", link.full_name()))),
                Err(_) => Err("could not disconnect".into()),
            }
        }
        "token-create" => {
            let label = form.label.unwrap_or_default();
            let (config2, app2) = (config.clone(), app.clone());
            match tokio::task::spawn_blocking(move || deploy::create(&config2, &app2, &label)).await {
                Ok(Ok((_, token))) => {
                    tracing::info!(admin = %admin.email, app = %app, "deploy token created");
                    return admin::app_tab(config, headers, app, "repo".into(), Some(admin::Fresh::DeployToken(token))).await;
                }
                Ok(Err(why)) => Err(why),
                Err(_) => Err("could not create the token".into()),
            }
        }
        "token-revoke" => {
            let id = form.id.unwrap_or_default();
            let (config2, app2) = (config.clone(), app.clone());
            match tokio::task::spawn_blocking(move || deploy::revoke(&config2, &app2, &id)).await {
                Ok(result) => result.map(|()| Some("Token revoked.".to_string())),
                Err(_) => Err("could not revoke the token".into()),
            }
        }
        _ => return (StatusCode::BAD_REQUEST, "unknown action").into_response(),
    };
    match outcome {
        Ok(Some(text)) => {
            // An import from the GitHub page lands on the app's Repo tab once
            // the app exists; before its first deploy there is no app page.
            let app_exists = config.data_dir.join(&app).is_dir();
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
    let installation = match query.installation.or_else(|| installations(&config).first().map(|i| i.id)) {
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
    fn the_workflow_is_filled_in_per_app_and_directory() {
        let root = workflow_for("shop", "main", "");
        assert!(root.contains("TOOLSITE_APP: \"shop\""));
        assert!(root.contains("branches: [\"main\"]"));
        assert!(root.contains("working-directory: \".\""));
        assert!(root.contains("hashFiles('package.json')"));
        let sub = workflow_for("shop", "trunk", "web/");
        assert!(sub.contains("working-directory: \"web\""));
        assert!(sub.contains("hashFiles('web/package.json')"));
        assert!(!sub.contains("__"), "a placeholder survived: {sub}");
    }

    #[test]
    fn names_that_could_reach_outside_a_repository_are_refused() {
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

    #[test]
    fn a_sealed_secret_opens_only_with_the_repositorys_key() {
        let secret = crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng);
        let public = BASE64.encode(secret.public_key().as_bytes());
        let sealed = seal(&public, "tsd_abc").unwrap();
        let opened = secret.unseal(&BASE64.decode(sealed).unwrap()).unwrap();
        assert_eq!(opened, b"tsd_abc");
        let other = crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng);
        assert!(other.unseal(&BASE64.decode(seal(&public, "x").unwrap()).unwrap()).is_err());
    }

    #[test]
    fn a_disconnected_link_is_kept_but_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "t");
        let (entry, token) = deploy::create(&config, "shop", "github:o/r").unwrap();
        write_link(&config, "shop", &RepoLink {
            owner: "o".into(), repo: "r".into(), branch: "main".into(), directory: String::new(),
            installation_id: 1, token_id: entry.id, connected_at: now(), last_push: None, last_deploy: None, disconnected_at: None,
        }).unwrap();
        assert_eq!(linked_apps(&config).len(), 1);
        assert_eq!(link_for_repo(&config, "O/R").map(|(app, _)| app), Some("shop".to_string()));
        disconnect(&config, "shop").unwrap();
        assert!(link(&config, "shop").is_none());
        assert!(linked_apps(&config).is_empty());
        assert!(dir.path().join("shop.repo").exists(), "the record was destroyed");
        assert!(!deploy::authorize(&config, "shop", &token), "the workflow's token outlived the link");
    }
}
