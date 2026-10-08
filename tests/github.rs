//! Repositories and deploy tokens, end to end over the router, against a
//! GitHub that lives in this process: a small axum app on a loopback port
//! that checks the App JWT's signature, hands out installation tokens, and
//! keeps repositories, trees, commits and tarballs in memory. Nothing here
//! reaches the network.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::{
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use toolsite::{build_router, platform::github::App, platform::upload::UploadTicket, runtime::wasm::Runtime, Config};
use tower::ServiceExt;

const TOKEN: &str = "publish-token";
const SITE: &str = "https://site.test";
const KEY_PEM: &str = include_str!("fixtures/oidc-test-key.pem");

async fn send(config: &Arc<Config>, request: Request<Body>) -> (StatusCode, String, Vec<(String, String)>) {
    let response = build_router(config.clone(), Runtime::new().unwrap())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string(), headers)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn get_as(uri: &str, session: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", format!("ts_session={session}"))
        .body(Body::empty())
        .unwrap()
}

fn post_form(uri: &str, session: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("cookie", format!("ts_session={session}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

fn put_bytes(uri: &str, token: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body))
        .unwrap()
}

fn form_token_from(body: &str) -> String {
    let marker = r#"name="token" value=""#;
    let start = body.find(marker).expect("no form token on the page") + marker.len();
    body[start..].split('"').next().unwrap().to_string()
}

fn location(headers: &[(String, String)]) -> String {
    headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone()).expect("no redirect")
}

fn flash(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .find_map(|(_, v)| v.strip_prefix("ts_flash=").map(|rest| rest.split(';').next().unwrap_or("").to_string()))
        .map(|raw| urlencoding::decode(&raw).unwrap().into_owned())
        .unwrap_or_default()
}

/// A gzipped tar of `files`, the way an agent ships a project with ?source.
fn tgz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for (path, content) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, path, *content).unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}

fn plain_server() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), TOKEN));
    (dir, config)
}

/// A site that knows its address and speaks as a GitHub App to `api`.
fn github_server(api: &str) -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new("12345", KEY_PEM, Some("toolsite-app".into()), Some("hook-secret".into()), Some(api.into())).unwrap();
    let config = Arc::new(Config {
        base_url: Some(SITE.to_string()),
        github: Some(app),
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    (dir, config)
}

fn admin(config: &Config) -> String {
    toolsite::accounts::users::sign_up_as(config, "boss@example.com", "correct horse battery", true).unwrap();
    toolsite::accounts::users::log_in(config, "boss@example.com", "correct horse battery").unwrap().1
}

fn visitor(config: &Config) -> String {
    toolsite::accounts::users::sign_up(config, "reader@example.com", "correct horse battery").unwrap();
    toolsite::accounts::users::log_in(config, "reader@example.com", "correct horse battery").unwrap().1
}

fn publish_app(config: &Config, app: &str) {
    let dir = config.data_dir.join(app);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<title>App</title>").unwrap();
}

fn store_source(config: &Config, app: &str) {
    let archive = tgz(&[
        ("./package.json", br#"{"name":"shop","scripts":{"build":"vite build"}}"#),
        ("./src/main.js", b"console.log('hi')"),
        ("./node_modules/left-pad/index.js", b"nope"),
    ]);
    std::fs::write(config.data_dir.join(format!("{app}.source")), archive).unwrap();
}

/// An upload ticket for `slug`, the way create_upload mints one.
async fn upload_ticket(config: &Config, slug: &str) -> String {
    let ticket = UploadTicket { slug: slug.to_string(), user: None, project: None };
    toolsite::platform::upload::issue_ticket(config, &ticket, Duration::from_secs(60)).await.unwrap()
}

/// A PUT with no bearer: the ticket in the URL is the credential.
fn put_plain(uri: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder().method("PUT").uri(uri).body(Body::from(body)).unwrap()
}

/// The paths in a stored source archive, sorted, `./` stripped.
fn archive_paths(bytes: &[u8]) -> Vec<String> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut paths: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().trim_start_matches("./").to_string())
        .filter(|p| !p.is_empty() && !p.ends_with('/'))
        .collect();
    paths.sort();
    paths
}

/// Installs the App on "acme" as the admin, the way GitHub's redirect does.
async fn install(config: &Arc<Config>, session: &str) {
    let (status, _, headers) = send(config, get_as("/github/setup?installation_id=1", session)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), "/admin/github");
    assert_eq!(flash(&headers), "ok:GitHub is connected.");
}

fn sign(secret: &str, body: &[u8]) -> String {
    // HMAC-SHA256 by hand, so this side does not lean on the code under test.
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    block[..secret.len()].copy_from_slice(secret.as_bytes());
    let inner: Vec<u8> = block.iter().map(|b| b ^ 0x36).collect();
    let outer: Vec<u8> = block.iter().map(|b| b ^ 0x5c).collect();
    let inner_hash = Sha256::new().chain_update(&inner).chain_update(body).finalize();
    let mac = Sha256::new().chain_update(&outer).chain_update(inner_hash).finalize();
    format!("sha256={}", mac.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn webhook(event: &str, signature: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/github/webhook")
        .header("content-type", "application/json")
        .header("x-github-event", event);
    if let Some(signature) = signature {
        builder = builder.header("x-hub-signature-256", signature);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

// --- the fake GitHub -----------------------------------------------------------

mod fake_github {
    use axum::{
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::{get, post},
        Json, Router,
    };
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use sha2::{Digest, Sha256};
    use std::{
        collections::{BTreeMap, HashMap},
        sync::{Arc, Mutex},
    };

    const JWKS: &str = include_str!("fixtures/oidc-test-jwks.json");

    #[derive(Default)]
    pub struct Repo {
        pub owner: String,
        pub name: String,
        pub default_branch: String,
        pub private: bool,
        pub files: BTreeMap<String, Vec<u8>>,
        pub head: String,
        pub commits_made: usize,
        pub topics: Vec<String>,
        /// (sha, message), newest first.
        pub history: Vec<(String, String)>,
        pub tarball_downloads: usize,
    }

    pub struct Fake {
        pub base: String,
        pub installations: Vec<(u64, &'static str, &'static str)>,
        pub repos: BTreeMap<String, Repo>,
        blobs: HashMap<String, Vec<u8>>,
        trees: HashMap<String, Vec<(String, String)>>,
        /// commit sha -> (tree sha, message)
        commits: HashMap<String, (String, String)>,
        pub last_jwt_claims: Option<serde_json::Value>,
        pub token_requests: usize,
    }

    pub type Shared = Arc<Mutex<Fake>>;

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect::<String>()[..40].to_string()
    }

    impl Fake {
        pub fn add_repo(&mut self, owner: &str, name: &str, private: bool) {
            let readme = b"# hello".to_vec();
            let blob = sha(&readme);
            self.blobs.insert(blob.clone(), readme.clone());
            let tree = sha(format!("tree:{owner}/{name}").as_bytes());
            self.trees.insert(tree.clone(), vec![("README.md".into(), blob)]);
            let commit = sha(format!("commit:{owner}/{name}").as_bytes());
            self.commits.insert(commit.clone(), (tree, "Initial commit".into()));
            let mut files = BTreeMap::new();
            files.insert("README.md".to_string(), readme);
            self.repos.insert(
                format!("{owner}/{name}").to_lowercase(),
                Repo {
                    owner: owner.into(),
                    name: name.into(),
                    default_branch: "main".into(),
                    private,
                    files,
                    head: commit.clone(),
                    commits_made: 0,
                    topics: Vec::new(),
                    history: vec![(commit, "Initial commit".into())],
                    tarball_downloads: 0,
                },
            );
        }
        pub fn repo(&self, full: &str) -> &Repo {
            self.repos.get(&full.to_lowercase()).expect("no such repo in the fake")
        }
        /// Somebody else commits to a repository: the files change and the
        /// branch moves, as a push from a laptop would.
        pub fn commit_from_outside(&mut self, full: &str, path: &str, content: &[u8], message: &str) -> String {
            let blob = sha(content);
            self.blobs.insert(blob.clone(), content.to_vec());
            let r = self.repos.get_mut(&full.to_lowercase()).unwrap();
            r.files.insert(path.to_string(), content.to_vec());
            let mut entries: Vec<(String, String)> = r.files.iter().map(|(p, b)| (p.clone(), sha(b))).collect();
            entries.sort();
            for b in r.files.values() {
                self.blobs.insert(sha(b), b.clone());
            }
            let tree = sha(format!("{entries:?}").as_bytes());
            let commit = sha(format!("{tree}{message}{}", r.history.len()).as_bytes());
            r.head = commit.clone();
            r.history.insert(0, (commit.clone(), message.to_string()));
            self.trees.insert(tree.clone(), entries);
            self.commits.insert(commit.clone(), (tree, message.to_string()));
            commit
        }
    }

    pub async fn start() -> (Shared, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let fake: Shared = Arc::new(Mutex::new(Fake {
            base: base.clone(),
            installations: vec![(1, "acme", "Organization"), (2, "octocat", "User")],
            repos: BTreeMap::new(),
            blobs: HashMap::new(),
            trees: HashMap::new(),
            commits: HashMap::new(),
            last_jwt_claims: None,
            token_requests: 0,
        }));
        let app = Router::new()
            .route("/app/installations", get(list_installations))
            .route("/app/installations/{id}/access_tokens", post(access_token))
            .route("/installation/repositories", get(installation_repos))
            .route("/orgs/{org}/repos", post(create_org_repo))
            .route("/user/repos", post(create_user_repo))
            .route("/repos/{owner}/{repo}", get(get_repo))
            .route("/repos/{owner}/{repo}/topics", get(get_topics).put(put_topics))
            .route("/repos/{owner}/{repo}/git/ref/heads/{branch}", get(get_ref))
            .route("/repos/{owner}/{repo}/git/commits/{sha}", get(get_commit))
            .route("/repos/{owner}/{repo}/git/blobs", post(create_blob))
            .route("/repos/{owner}/{repo}/git/trees", post(create_tree))
            .route("/repos/{owner}/{repo}/git/commits", post(create_commit))
            .route("/repos/{owner}/{repo}/git/refs/heads/{branch}", axum::routing::patch(update_ref))
            .route("/repos/{owner}/{repo}/git/trees/{sha}", get(get_tree))
            .route("/repos/{owner}/{repo}/contents/{*path}", get(get_contents).put(put_contents))
            .route("/repos/{owner}/{repo}/tarball/{reference}", get(tarball))
            .route("/codeload/{owner}/{repo}/{sha}", get(codeload))
            .route("/repos/{owner}/{repo}/commits", get(list_commits))
            .route("/repos/{owner}/{repo}/compare/{basehead}", get(compare))
            .with_state(fake.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (fake, base)
    }

    fn bearer(headers: &HeaderMap) -> Option<String> {
        headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")
            .map(str::to_string)
    }

    /// Only an installation token this fake handed out opens a repository.
    fn installed(headers: &HeaderMap) -> Result<(), Response> {
        match bearer(headers) {
            Some(token) if token.starts_with("inst-") && token.ends_with("-token") => Ok(()),
            _ => Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "message": "Bad credentials" }))).into_response()),
        }
    }

    fn verify_jwt(token: &str) -> Result<serde_json::Value, String> {
        let jwks: serde_json::Value = serde_json::from_str(JWKS).unwrap();
        let key = &jwks["keys"][0];
        let decoding = jsonwebtoken::DecodingKey::from_rsa_components(key["n"].as_str().unwrap(), key["e"].as_str().unwrap())
            .map_err(|e| e.to_string())?;
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "iat"]);
        validation.validate_aud = false;
        let data = jsonwebtoken::decode::<serde_json::Value>(token, &decoding, &validation).map_err(|e| e.to_string())?;
        Ok(data.claims)
    }

    async fn list_installations(State(fake): State<Shared>, headers: HeaderMap) -> Response {
        let Some(jwt) = bearer(&headers) else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        if verify_jwt(&jwt).is_err() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let list: Vec<serde_json::Value> = fake
            .lock()
            .unwrap()
            .installations
            .iter()
            .map(|(id, login, kind)| serde_json::json!({ "id": id, "account": { "login": login, "type": kind } }))
            .collect();
        Json(list).into_response()
    }

    async fn access_token(State(fake): State<Shared>, Path(id): Path<u64>, headers: HeaderMap) -> Response {
        let Some(jwt) = bearer(&headers) else {
            return StatusCode::UNAUTHORIZED.into_response();
        };
        let claims = match verify_jwt(&jwt) {
            Ok(claims) if claims["iss"] == "12345" => claims,
            Ok(_) => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "message": "wrong app" }))).into_response(),
            Err(why) => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "message": why }))).into_response(),
        };
        let mut fake = fake.lock().unwrap();
        fake.last_jwt_claims = Some(claims);
        fake.token_requests += 1;
        if !fake.installations.iter().any(|(i, ..)| *i == id) {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "message": "Not Found" }))).into_response();
        }
        (StatusCode::CREATED, Json(serde_json::json!({ "token": format!("inst-{id}-token"), "expires_at": "2099-01-01T00:00:00Z" }))).into_response()
    }

    async fn installation_repos(State(fake): State<Shared>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        let repos: Vec<serde_json::Value> = fake
            .repos
            .values()
            .map(|r| serde_json::json!({ "name": r.name, "full_name": format!("{}/{}", r.owner, r.name), "owner": { "login": r.owner }, "default_branch": r.default_branch, "private": r.private, "topics": r.topics }))
            .collect();
        Json(serde_json::json!({ "total_count": repos.len(), "repositories": repos })).into_response()
    }

    fn repo_json(r: &Repo) -> serde_json::Value {
        serde_json::json!({ "name": r.name, "full_name": format!("{}/{}", r.owner, r.name), "owner": { "login": r.owner }, "default_branch": r.default_branch, "private": r.private })
    }

    async fn create_org_repo(State(fake): State<Shared>, Path(org): Path<String>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let name = body["name"].as_str().unwrap_or("").to_string();
        let mut fake = fake.lock().unwrap();
        if fake.repos.contains_key(&format!("{org}/{name}").to_lowercase()) {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({ "message": "name already exists on this account" }))).into_response();
        }
        assert_eq!(body["auto_init"], true, "a new repository must be initialised or there is no branch to commit on");
        fake.add_repo(&org, &name, body["private"].as_bool().unwrap_or(false));
        (StatusCode::CREATED, Json(repo_json(fake.repo(&format!("{org}/{name}"))))).into_response()
    }

    async fn create_user_repo(State(fake): State<Shared>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let name = body["name"].as_str().unwrap_or("").to_string();
        let mut fake = fake.lock().unwrap();
        fake.add_repo("octocat", &name, body["private"].as_bool().unwrap_or(false));
        (StatusCode::CREATED, Json(repo_json(fake.repo(&format!("octocat/{name}"))))).into_response()
    }

    async fn get_repo(State(fake): State<Shared>, Path((owner, repo)): Path<(String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        match fake.repos.get(&format!("{owner}/{repo}").to_lowercase()) {
            Some(r) => Json(repo_json(r)).into_response(),
            None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "message": "Not Found" }))).into_response(),
        }
    }

    async fn get_topics(State(fake): State<Shared>, Path((owner, repo)): Path<(String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        match fake.repos.get(&format!("{owner}/{repo}").to_lowercase()) {
            Some(r) => Json(serde_json::json!({ "names": r.topics })).into_response(),
            None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "message": "Not Found" }))).into_response(),
        }
    }

    async fn put_topics(State(fake): State<Shared>, Path((owner, repo)): Path<(String, String)>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let mut fake = fake.lock().unwrap();
        let Some(r) = fake.repos.get_mut(&format!("{owner}/{repo}").to_lowercase()) else {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "message": "Not Found" }))).into_response();
        };
        r.topics = body["names"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Json(serde_json::json!({ "names": r.topics })).into_response()
    }

    async fn get_ref(State(fake): State<Shared>, Path((owner, repo, branch)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        let Some(r) = fake.repos.get(&format!("{owner}/{repo}").to_lowercase()) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if r.default_branch != branch {
            return StatusCode::NOT_FOUND.into_response();
        }
        Json(serde_json::json!({ "ref": format!("refs/heads/{branch}"), "object": { "sha": r.head, "type": "commit" } })).into_response()
    }

    async fn get_commit(State(fake): State<Shared>, Path((_o, _r, sha)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        match fake.commits.get(&sha) {
            Some((tree, _)) => Json(serde_json::json!({ "sha": sha, "tree": { "sha": tree } })).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn create_blob(State(fake): State<Shared>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        assert_eq!(body["encoding"], "base64");
        let bytes = BASE64.decode(body["content"].as_str().unwrap()).unwrap();
        let id = sha(&bytes);
        fake.lock().unwrap().blobs.insert(id.clone(), bytes);
        (StatusCode::CREATED, Json(serde_json::json!({ "sha": id }))).into_response()
    }

    async fn create_tree(State(fake): State<Shared>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let mut fake = fake.lock().unwrap();
        let mut entries: Vec<(String, String)> = body["base_tree"]
            .as_str()
            .and_then(|base| fake.trees.get(base).cloned())
            .unwrap_or_default();
        for item in body["tree"].as_array().unwrap() {
            let path = item["path"].as_str().unwrap().to_string();
            entries.retain(|(p, _)| p != &path);
            // A null sha deletes the path, as the Git Data API has it.
            if let Some(blob) = item["sha"].as_str() {
                assert!(fake.blobs.contains_key(blob), "tree names a blob that was never stored");
                entries.push((path, blob.to_string()));
            }
        }
        // Git trees are sorted and content-addressed: the same files give
        // the same sha, whichever order they arrived in.
        entries.sort();
        let id = sha(format!("{entries:?}").as_bytes());
        fake.trees.insert(id.clone(), entries);
        (StatusCode::CREATED, Json(serde_json::json!({ "sha": id }))).into_response()
    }

    async fn create_commit(State(fake): State<Shared>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let tree = body["tree"].as_str().unwrap().to_string();
        let parents = body["parents"].as_array().cloned().unwrap_or_default();
        let message = body["message"].as_str().unwrap_or("").to_string();
        let id = sha(format!("{tree}{parents:?}{message}").as_bytes());
        let mut fake = fake.lock().unwrap();
        assert!(fake.trees.contains_key(&tree), "commit names a tree that was never stored");
        fake.commits.insert(id.clone(), (tree, message));
        (StatusCode::CREATED, Json(serde_json::json!({ "sha": id }))).into_response()
    }

    async fn update_ref(State(fake): State<Shared>, Path((owner, repo, _branch)): Path<(String, String, String)>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let sha = body["sha"].as_str().unwrap().to_string();
        let mut fake = fake.lock().unwrap();
        let (tree, message) = fake.commits.get(&sha).cloned().expect("ref moved to an unknown commit");
        let entries = fake.trees.get(&tree).cloned().unwrap();
        let files: BTreeMap<String, Vec<u8>> = entries
            .into_iter()
            .map(|(path, blob)| (path, fake.blobs.get(&blob).cloned().unwrap()))
            .collect();
        let r = fake.repos.get_mut(&format!("{owner}/{repo}").to_lowercase()).unwrap();
        r.head = sha.clone();
        r.files = files;
        r.commits_made += 1;
        r.history.insert(0, (sha, message));
        Json(serde_json::json!({ "object": { "sha": r.head } })).into_response()
    }

    async fn get_contents(State(fake): State<Shared>, Path((owner, repo, path)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        let r = fake.repo(&format!("{owner}/{repo}"));
        match r.files.get(&path) {
            Some(bytes) => Json(serde_json::json!({ "sha": sha(bytes), "content": BASE64.encode(bytes) })).into_response(),
            None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "message": "Not Found" }))).into_response(),
        }
    }

    async fn put_contents(State(fake): State<Shared>, Path((owner, repo, path)): Path<(String, String, String)>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let bytes = BASE64.decode(body["content"].as_str().unwrap()).unwrap();
        let mut fake = fake.lock().unwrap();
        let r = fake.repos.get_mut(&format!("{owner}/{repo}").to_lowercase()).unwrap();
        if r.files.contains_key(&path) && body["sha"].is_null() {
            return (StatusCode::CONFLICT, Json(serde_json::json!({ "message": "sha required to update" }))).into_response();
        }
        r.files.insert(path, bytes);
        r.commits_made += 1;
        (StatusCode::CREATED, Json(serde_json::json!({ "content": {} }))).into_response()
    }

    async fn get_tree(State(fake): State<Shared>, Path((_o, _r, sha)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        match fake.trees.get(&sha) {
            Some(entries) => {
                let tree: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|(path, blob)| serde_json::json!({ "path": path, "type": "blob", "sha": blob, "mode": "100644" }))
                    .collect();
                Json(serde_json::json!({ "sha": sha, "tree": tree, "truncated": false })).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    /// GitHub answers a tarball request with a redirect to a signed URL.
    async fn tarball(State(fake): State<Shared>, Path((owner, repo, reference)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        let Some(r) = fake.repos.get(&format!("{owner}/{repo}").to_lowercase()) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if reference != r.default_branch && reference != r.head {
            return StatusCode::NOT_FOUND.into_response();
        }
        let to = format!("{}/codeload/{owner}/{repo}/{}", fake.base, r.head);
        (StatusCode::FOUND, [(axum::http::header::LOCATION, to)]).into_response()
    }

    async fn codeload(State(fake): State<Shared>, Path((owner, repo, sha)): Path<(String, String, String)>) -> Response {
        let mut fake = fake.lock().unwrap();
        let Some(r) = fake.repos.get_mut(&format!("{owner}/{repo}").to_lowercase()) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        r.tarball_downloads += 1;
        let top = format!("{owner}-{repo}-{}", &sha[..7]);
        let files: Vec<(String, Vec<u8>)> = r.files.iter().map(|(p, b)| (format!("{top}/{p}"), b.clone())).collect();
        let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
        let body = super::tgz(&borrowed);
        ([(axum::http::header::CONTENT_TYPE, "application/x-gzip")], body).into_response()
    }

    async fn list_commits(State(fake): State<Shared>, Path((owner, repo)): Path<(String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let fake = fake.lock().unwrap();
        let r = fake.repo(&format!("{owner}/{repo}"));
        let list: Vec<serde_json::Value> = r
            .history
            .iter()
            .take(5)
            .map(|(sha, message)| {
                serde_json::json!({
                    "sha": sha,
                    "html_url": format!("https://github.com/{owner}/{repo}/commit/{sha}"),
                    "commit": { "message": message, "author": { "name": "toolsite", "date": "2026-10-05T12:00:00Z" } }
                })
            })
            .collect();
        Json(list).into_response()
    }

    async fn compare(State(fake): State<Shared>, Path((owner, repo, basehead)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let Some((base, head)) = basehead.split_once("...") else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let fake = fake.lock().unwrap();
        let r = fake.repo(&format!("{owner}/{repo}"));
        let position = |wanted: &str| r.history.iter().position(|(sha, _)| sha.starts_with(wanted));
        match (position(base), position(head)) {
            (Some(b), Some(h)) => Json(serde_json::json!({ "ahead_by": b.saturating_sub(h), "behind_by": h.saturating_sub(b) })).into_response(),
            _ => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "message": "Not Found" }))).into_response(),
        }
    }
}

// --- deploy tokens -------------------------------------------------------------

#[tokio::test]
async fn a_deploy_token_publishes_one_app_and_nothing_else() {
    let (_dir, config) = plain_server();
    let (_, token) = toolsite::platform::deploy::create(&config, "shop", "ci").unwrap();
    let bundle = tgz(&[("./index.html", b"<title>Shop</title>")]);

    let (status, body, _) = send(&config, put_bytes("/deploy/shop?bundle", &token, bundle.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, page, _) = send(&config, get("/p/shop/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Shop"));

    // A page of the app, through the sub route.
    let (status, ..) = send(&config, put_bytes("/deploy/shop/about", &token, b"<h1>about</h1>".to_vec())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ..) = send(&config, get("/p/shop/about")).await;
    assert_eq!(status, StatusCode::OK);

    // Not another app, not with the publish token, not without one.
    let (status, ..) = send(&config, put_bytes("/deploy/other?bundle", &token, bundle.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "shop's token wrote to other");
    assert!(!config.data_dir.join("other").exists());
    let (status, ..) = send(&config, put_bytes("/deploy/shop?bundle", TOKEN, bundle.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the publish token deployed");
    let (status, ..) = send(&config, Request::builder().method("PUT").uri("/deploy/shop?bundle").body(Body::from(bundle.clone())).unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // An unknown flag is refused rather than guessed at.
    let (status, ..) = send(&config, put_bytes("/deploy/shop?config", &token, b"x".to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Revoked is refused.
    let id = toolsite::platform::deploy::list(&config, "shop")[0].id.clone();
    toolsite::platform::deploy::revoke(&config, "shop", &id).unwrap();
    let (status, ..) = send(&config, put_bytes("/deploy/shop?bundle", &token, bundle)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The sidecar is never served.
    let (status, ..) = send(&config, get("/p/shop.deploys")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- repositories --------------------------------------------------------------

#[tokio::test]
async fn creating_a_repository_pushes_the_source_and_a_readme_in_one_commit() {
    let (fake, api) = fake_github::start().await;
    let (_dir, config) = github_server(&api);
    publish_app(&config, "shop");
    store_source(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;

    let (status, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains(r#"value="toolsite-shop""#), "the form does not propose toolsite-<app>");
    assert!(page.contains("Create a repository"));
    let token = form_token_from(&page);

    let (status, _, headers) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&private=1&back=/admin/apps/shop/repo")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(flash(&headers).starts_with("ok:"), "{}", flash(&headers));

    let head = {
        let fake = fake.lock().unwrap();
        let claims = fake.last_jwt_claims.as_ref().expect("no App JWT was presented");
        assert_eq!(claims["iss"], "12345");
        let repo = fake.repo("acme/shop");
        assert!(repo.private);
        assert_eq!(repo.topics, ["toolsite"], "the repository was not tagged");
        assert_eq!(repo.commits_made, 1, "the project should land in one commit");
        assert_eq!(repo.files.get("src/main.js").map(|b| b.as_slice()), Some(&b"console.log('hi')"[..]));
        assert!(repo.files.contains_key("package.json"));
        let readme = String::from_utf8(repo.files["README.md"].clone()).unwrap();
        assert!(readme.contains("shop") && readme.contains("toolsite deploy"), "no README of ours: {readme}");
        assert!(!repo.files.keys().any(|k| k.contains("node_modules")), "node_modules was pushed");
        assert!(!repo.files.keys().any(|k| k.starts_with(".github/")), "a workflow was pushed: {:?}", repo.files.keys());
        assert_eq!(repo.history[0].1.lines().next(), Some("Add shop from toolsite"));
        assert!(repo.history[0].1.ends_with("Published from toolsite"));
        repo.head.clone()
    };
    assert!(toolsite::platform::deploy::list(&config, "shop").is_empty(), "a deploy token was minted for a mirror");

    let link = toolsite::platform::github::link(&config, "shop").expect("no link recorded");
    assert_eq!((link.owner.as_str(), link.repo.as_str(), link.branch.as_str()), ("acme", "shop", "main"));
    assert_eq!(link.deployed.as_ref().map(|d| d.sha.as_str()), Some(head.as_str()), "the live app should be the commit just pushed");
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert!(page.contains("acme/shop"));
    assert!(page.contains("Pull from repository"));
    assert!(page.contains("The live app is the repository's head."), "{page}");
    assert!(page.contains("Add shop from toolsite"), "the commit list is missing");
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    assert!(page.contains("acme/shop"));

    // Twice is refused, and the repository is not touched again.
    let (_, _, headers) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&back=/admin/apps/shop/repo")),
    )
    .await;
    assert!(flash(&headers).contains("already connected"));
    assert_eq!(fake.lock().unwrap().repo("acme/shop").commits_made, 1);
}

#[tokio::test]
async fn an_app_without_stored_source_cannot_become_a_repository() {
    let (fake, api) = fake_github::start().await;
    let (_dir, config) = github_server(&api);
    publish_app(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert!(page.contains("no source archive"));
    let token = form_token_from(&page);
    let (_, _, headers) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop")),
    )
    .await;
    assert!(flash(&headers).contains("no stored source"), "{}", flash(&headers));
    assert!(fake.lock().unwrap().repos.is_empty(), "a repository was created with nothing to put in it");
    assert!(toolsite::platform::github::link(&config, "shop").is_none());
}

#[tokio::test]
async fn importing_a_repository_pulls_its_branch_into_the_source_archive() {
    let (fake, api) = fake_github::start().await;
    {
        let mut f = fake.lock().unwrap();
        f.add_repo("acme", "dashboard", true);
        f.commit_from_outside("acme/dashboard", "web/package.json", br#"{"name":"dash"}"#, "Add the web app");
        f.commit_from_outside("acme/dashboard", "web/node_modules/x/index.js", b"nope", "Oops");
        f.commit_from_outside("acme/dashboard", "docs/notes.md", b"# notes", "Notes");
    }
    let (dir, config) = github_server(&api);
    let session = admin(&config);
    install(&config, &session).await;

    // From the GitHub page, as a new app, from a subdirectory.
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    assert!(page.contains("Import a repository"));
    let token = form_token_from(&page);
    let (status, _, headers) = send(
        &config,
        post_form(
            "/admin/repo",
            &session,
            format!("token={token}&action=import&app=dash&installation=1&repo=acme/dashboard&branch=&directory=web&back=/admin/github"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(flash(&headers).starts_with("ok:"), "{}", flash(&headers));
    assert!(flash(&headers).contains("source archive"), "{}", flash(&headers));
    // No app exists yet, so the person is kept on the GitHub page.
    assert_eq!(location(&headers), "/admin/github");

    {
        let fake = fake.lock().unwrap();
        let repo = fake.repo("acme/dashboard");
        assert_eq!(repo.commits_made, 0, "an import must not touch the repository");
        assert_eq!(repo.tarball_downloads, 1, "the branch should be pulled once");
        assert!(repo.topics.iter().any(|t| t == "toolsite"), "an imported repository was not tagged");
    }
    let archive = std::fs::read(dir.path().join("dash.source")).expect("the branch was not stored as the source archive");
    assert_eq!(archive_paths(&archive), ["package.json"], "only the project directory, without node_modules");
    let link = toolsite::platform::github::link(&config, "dash").unwrap();
    assert_eq!(link.directory, "web/");
    assert_eq!(link.branch, "main");
    assert!(link.deployed.is_none(), "nothing was published, so nothing is live");
    assert!(link.token_id.is_empty(), "a mirror needs no deploy token");
    let status = toolsite::platform::github::status_text(&config, "dash").await;
    assert!(status.contains("cannot be compared"), "{status}");
    assert!(status.contains("Notes"), "the newest commit is not listed: {status}");

    // A repository the installation cannot reach is refused without a link.
    let (_, _, headers) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=import&app=ghost&installation=1&repo=acme/nothing&back=/admin/github")),
    )
    .await;
    assert!(flash(&headers).starts_with("error:"));
    assert!(toolsite::platform::github::link(&config, "ghost").is_none());
}

#[tokio::test]
async fn publishing_source_to_a_linked_app_pushes_one_commit_with_the_publishers_message() {
    let (fake, api) = fake_github::start().await;
    let (dir, config) = github_server(&api);
    publish_app(&config, "shop");
    store_source(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&back=/admin/apps/shop/repo"))).await;
    assert_eq!(fake.lock().unwrap().repo("acme/shop").commits_made, 1);

    // The next publish: a new file, an old one gone, a message on the URL.
    let next = tgz(&[
        ("./package.json", br#"{"name":"shop","scripts":{"build":"vite build"}}"#),
        ("./src/App.tsx", b"export default () => <p>phone</p>"),
    ]);
    let ticket = upload_ticket(&config, "shop").await;
    let (status, body, _) = send(
        &config,
        put_plain(&format!("/upload/{ticket}?source&message=Fix%20the%20phone%20field%0A%0AIt%20was%20too%20short."), next.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("pushed to acme/shop@main as "), "{body}");
    let head = {
        let fake = fake.lock().unwrap();
        let repo = fake.repo("acme/shop");
        assert_eq!(repo.commits_made, 2, "one commit per publish");
        assert!(repo.files.contains_key("src/App.tsx"));
        assert!(!repo.files.contains_key("src/main.js"), "a file the project dropped is still in the repository");
        assert!(repo.files.contains_key("README.md"), "toolsite's own README should survive a push that lacks one");
        let (sha, message) = &repo.history[0];
        assert_eq!(message, "Fix the phone field\n\nIt was too short.\n\nPublished from toolsite");
        sha.clone()
    };
    assert_eq!(std::fs::read(dir.path().join("shop.source")).unwrap(), next, "the archive was not stored");
    let link = toolsite::platform::github::link(&config, "shop").unwrap();
    assert_eq!(link.last_push.as_ref().map(|p| p.sha.as_str()), Some(head.as_str()));
    assert_eq!(link.deployed.as_ref().map(|d| d.sha.as_str()), Some(head.as_str()), "what was just published is what is live");

    // The same archive again: stored, nothing to commit.
    let ticket = upload_ticket(&config, "shop").await;
    let (status, body, _) = send(&config, put_plain(&format!("/upload/{ticket}?source"), next)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("unchanged"), "{body}");
    assert_eq!(fake.lock().unwrap().repo("acme/shop").commits_made, 2);

    // The message may also travel as a header.
    let third = tgz(&[("./package.json", b"{}"), ("./src/App.tsx", b"v3")]);
    let ticket = upload_ticket(&config, "shop").await;
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/{ticket}?source"))
        .header("x-toolsite-message", "Third pass")
        .body(Body::from(third))
        .unwrap();
    let (status, ..) = send(&config, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fake.lock().unwrap().repo("acme/shop").history[0].1.lines().next(), Some("Third pass"));
}

#[tokio::test]
async fn a_source_archive_arriving_by_deploy_token_is_stored_but_never_pushed_back() {
    let (fake, api) = fake_github::start().await;
    let (dir, config) = github_server(&api);
    publish_app(&config, "shop");
    store_source(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&back=/admin/apps/shop/repo"))).await;
    let (_, deploy_token) = toolsite::platform::deploy::create(&config, "shop", "ci").unwrap();

    let from_ci = tgz(&[("./package.json", b"{}"), ("./src/ci.js", b"built elsewhere")]);
    let (status, body, _) = send(&config, put_bytes("/deploy/shop?source&commit=0123456789abcdef0123456789abcdef01234567", &deploy_token, from_ci.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("pushed to"), "a pipeline's archive was pushed back: {body}");
    assert_eq!(fake.lock().unwrap().repo("acme/shop").commits_made, 1, "the repository must not gain a commit");
    assert_eq!(std::fs::read(dir.path().join("shop.source")).unwrap(), from_ci);
    // The pipeline said which commit it built: that is what is live now.
    let link = toolsite::platform::github::link(&config, "shop").unwrap();
    assert_eq!(link.deployed.as_ref().map(|d| d.sha.as_str()), Some("0123456789abcdef0123456789abcdef01234567"));
    // Something that is not a sha is ignored, not stored.
    let (status, ..) = send(&config, put_bytes("/deploy/shop?bundle&commit=not-a-sha", &deploy_token, tgz(&[("./index.html", b"<title>x</title>")]))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(toolsite::platform::github::link(&config, "shop").unwrap().deployed.unwrap().sha, "0123456789abcdef0123456789abcdef01234567");
}

#[tokio::test]
async fn a_disconnected_link_does_not_push() {
    let (fake, api) = fake_github::start().await;
    let (_dir, config) = github_server(&api);
    publish_app(&config, "shop");
    store_source(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&back=/admin/apps/shop/repo"))).await;
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=disconnect&app=shop&back=/admin/apps/shop/repo"))).await;
    assert!(toolsite::platform::github::link(&config, "shop").is_none());

    let ticket = upload_ticket(&config, "shop").await;
    let (status, body, _) = send(&config, put_plain(&format!("/upload/{ticket}?source"), tgz(&[("./package.json", b"{}"), ("./new.js", b"1")]))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("pushed to"), "{body}");
    assert_eq!(fake.lock().unwrap().repo("acme/shop").commits_made, 1);
}

#[tokio::test]
async fn the_repo_tab_says_when_the_repository_is_ahead_of_the_live_app() {
    let (fake, api) = fake_github::start().await;
    let (_dir, config) = github_server(&api);
    publish_app(&config, "shop");
    store_source(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&back=/admin/apps/shop/repo"))).await;

    // Somebody commits twice from a laptop.
    let newest = {
        let mut f = fake.lock().unwrap();
        f.commit_from_outside("acme/shop", "src/a.js", b"a", "Add a");
        f.commit_from_outside("acme/shop", "src/b.js", b"b", "Add b")
    };
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert!(page.contains("The repository is 2 commits ahead of the live app."), "{page}");
    assert!(page.contains("toolsite deploy"), "the page does not say what to do");
    assert!(page.contains("Add b") && page.contains("Add a"), "the new commits are not listed");
    let status = toolsite::platform::github::status_text(&config, "shop").await;
    assert!(status.contains("2 commits ahead"), "{status}");

    // A publish that names the head catches up.
    let (_, deploy_token) = toolsite::platform::deploy::create(&config, "shop", "laptop").unwrap();
    let (status, ..) = send(&config, put_bytes(&format!("/deploy/shop?bundle&commit={newest}"), &deploy_token, tgz(&[("./index.html", b"<title>v2</title>")]))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert!(page.contains("The live app is the repository's head."), "{page}");
}

#[tokio::test]
async fn a_webhook_must_be_signed_and_a_push_from_elsewhere_is_pulled_in() {
    let (fake, api) = fake_github::start().await;
    fake.lock().unwrap().add_repo("acme", "dashboard", true);
    let (dir, config) = github_server(&api);
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=import&app=dash&installation=1&repo=acme/dashboard&back=/admin/github"))).await;
    assert_eq!(fake.lock().unwrap().repo("acme/dashboard").tarball_downloads, 1, "the import pulls once");
    let first_head = fake.lock().unwrap().repo("acme/dashboard").head.clone();

    // Somebody pushes from a laptop; GitHub tells us.
    let pushed = fake.lock().unwrap().commit_from_outside("acme/dashboard", "app.js", b"v2", "Work from a laptop");
    let push = format!(r#"{{"ref":"refs/heads/main","after":"{pushed}","repository":{{"full_name":"acme/dashboard"}}}}"#);
    let (status, ..) = send(&config, webhook("push", None, &push)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "an unsigned webhook was taken");
    let (status, ..) = send(&config, webhook("push", Some(&sign("wrong-secret", push.as_bytes())), &push)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a webhook signed with another secret was taken");
    let (status, ..) = send(&config, webhook("push", Some(&sign("hook-secret", b"{}")), &push)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a signature over a different body was taken");
    assert_eq!(toolsite::platform::github::link(&config, "dash").unwrap().last_push.unwrap().sha, first_head);
    assert_eq!(fake.lock().unwrap().repo("acme/dashboard").tarball_downloads, 1);

    let (status, body, _) = send(&config, webhook("push", Some(&sign("hook-secret", push.as_bytes())), &push)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.contains("pulled"), "{body}");
    assert_eq!(toolsite::platform::github::link(&config, "dash").unwrap().last_push.unwrap().sha, pushed);
    assert_eq!(fake.lock().unwrap().repo("acme/dashboard").tarball_downloads, 2);
    let archive = std::fs::read(dir.path().join("dash.source")).unwrap();
    assert_eq!(archive_paths(&archive), ["README.md", "app.js"], "the pushed branch is not the source archive");
    assert_eq!(fake.lock().unwrap().repo("acme/dashboard").commits_made, 0, "a pull must not push back");

    // The same sha again is our own echo, or a replay: not pulled twice.
    let (status, body, _) = send(&config, webhook("push", Some(&sign("hook-secret", push.as_bytes())), &push)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(!body.contains("pulled"), "{body}");
    assert_eq!(fake.lock().unwrap().repo("acme/dashboard").tarball_downloads, 2);

    // Another branch is noise.
    let other = r#"{"ref":"refs/heads/feature","after":"ffff","repository":{"full_name":"acme/dashboard"}}"#;
    send(&config, webhook("push", Some(&sign("hook-secret", other.as_bytes())), other)).await;
    assert_eq!(toolsite::platform::github::link(&config, "dash").unwrap().last_push.unwrap().sha, pushed);

    // A repository nobody linked is acknowledged and ignored.
    let stranger = r#"{"ref":"refs/heads/main","after":"1","repository":{"full_name":"someone/else"}}"#;
    let (status, body, _) = send(&config, webhook("push", Some(&sign("hook-secret", stranger.as_bytes())), stranger)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.contains("ignored"));
}

#[tokio::test]
async fn pulling_refreshes_the_source_archive_and_disconnecting_leaves_the_repository_alone() {
    let (fake, api) = fake_github::start().await;
    fake.lock().unwrap().add_repo("acme", "dashboard", true);
    let (dir, config) = github_server(&api);
    publish_app(&config, "dash");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/dash/repo", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=import&app=dash&installation=1&repo=acme/dashboard&back=/admin/apps/dash/repo"))).await;
    assert_eq!(archive_paths(&std::fs::read(dir.path().join("dash.source")).unwrap()), ["README.md"]);

    // The branch moves without a webhook reaching us; Pull catches up.
    fake.lock().unwrap().commit_from_outside("acme/dashboard", "index.html", b"<title>d</title>", "Add a page");
    let (status, _, headers) = send(&config, post_form("/admin/repo", &session, format!("token={token}&action=pull&app=dash&back=/admin/apps/dash/repo"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(flash(&headers).starts_with("ok:Pulled commit"), "{}", flash(&headers));
    assert_eq!(archive_paths(&std::fs::read(dir.path().join("dash.source")).unwrap()), ["README.md", "index.html"]);
    assert_eq!(fake.lock().unwrap().repo("acme/dashboard").commits_made, 0, "a pull must not write to the repository");

    // Disconnect: link gone from the live set, repository untouched.
    let (status, _, headers) = send(&config, post_form("/admin/repo", &session, format!("token={token}&action=disconnect&app=dash&back=/admin/apps/dash/repo"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(flash(&headers).starts_with("ok:"));
    assert!(toolsite::platform::github::link(&config, "dash").is_none());
    assert!(fake.lock().unwrap().repo("acme/dashboard").files.contains_key("index.html"));
    let (_, page, _) = send(&config, get_as("/admin/apps/dash/repo", &session)).await;
    assert!(page.contains("Create a repository"), "the tab should offer to connect again");
}

#[tokio::test]
async fn the_github_pages_are_for_admins_and_the_search_is_capped() {
    let (fake, api) = fake_github::start().await;
    {
        let mut fake = fake.lock().unwrap();
        for i in 0..30 {
            fake.add_repo("acme", &format!("repo-{i:02}"), false);
        }
    }
    let (_dir, config) = github_server(&api);
    let session = admin(&config);
    let reader = visitor(&config);
    install(&config, &session).await;

    for uri in ["/admin/github", "/admin/github/repos/search?q=repo", "/github/setup?installation_id=1"] {
        let (status, ..) = send(&config, get(uri)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri} without a session");
        let (status, ..) = send(&config, get_as(uri, &reader)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} as a visitor");
    }

    let (status, body, _) = send(&config, get_as("/admin/github/repos/search?q=repo&installation=1", &session)).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<String> = serde_json::from_str(&body).unwrap();
    assert_eq!(names.len(), 20, "the picker must never get everything");
    let (_, body, _) = send(&config, get_as("/admin/github/repos/search?q=repo-1&installation=1", &session)).await;
    let names: Vec<String> = serde_json::from_str(&body).unwrap();
    assert_eq!(names.len(), 10);
    assert!(names.iter().all(|n| n.starts_with("acme/repo-1")));
    let (_, body, _) = send(&config, get_as("/admin/github/repos/search?q=&installation=1", &session)).await;
    assert_eq!(body, "[]", "an empty query lists nothing");

    // The GitHub page names the installation and does not list every repository.
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    assert!(page.contains("acme"));
    assert!(!page.contains("repo-05"));
}

#[tokio::test]
async fn without_a_github_app_the_pages_say_so_and_the_webhook_is_closed() {
    let (_dir, config) = plain_server();
    let session = admin(&config);
    publish_app(&config, "shop");
    let (status, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("not configured"));
    assert!(page.contains("Deploy tokens"), "deploy tokens work without GitHub");
    let (status, ..) = send(&config, webhook("push", Some("sha256=00"), "{}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}


#[tokio::test]
async fn the_unconfigured_github_page_walks_through_the_setup_with_every_value_copyable() {
    let (_dir, plain) = plain_server();
    let config = Arc::new(Config {
        base_url: Some(SITE.to_string()),
        ..Config::local(plain.data_dir.clone(), TOKEN)
    });
    let session = admin(&config);
    let (status, page, _) = send(&config, get_as("/admin/github", &session)).await;
    assert_eq!(status, StatusCode::OK);
    for needle in [
        "https://github.com/settings/apps/new",
        &format!("{SITE}/github/setup"),
        &format!("{SITE}/github/webhook"),
        "Redirect on update",
        "TOOLSITE_GITHUB_APP_ID=",
        "TOOLSITE_GITHUB_WEBHOOK_SECRET=",
        "Contents: read and write",
        "Administration: read and write",
        "Metadata: read",
        r#"data-copy="gh-env""#,
    ] {
        assert!(page.contains(needle), "setup page lacks {needle}");
    }
    // The secret in step 1 is the one in step 3.
    let secret = page
        .split(r#"id="gh-secret">"#)
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("a generated webhook secret");
    assert_eq!(secret.len(), 40);
    assert!(page.contains(&format!("TOOLSITE_GITHUB_WEBHOOK_SECRET={secret}")));
    assert!(!page.contains("Actions: read") && !page.contains("workflow_run"), "the setup still asks for Actions");
}

#[tokio::test]
async fn tagged_repositories_are_proposed_for_import_and_linked_or_untagged_ones_are_not() {
    let (fake, api) = fake_github::start().await;
    {
        let mut f = fake.lock().unwrap();
        f.add_repo("acme", "toolsite-shop", true);
        f.add_repo("acme", "toolsite-crm", false);
        f.add_repo("acme", "notes", false);
        f.repos.get_mut("acme/toolsite-shop").unwrap().topics.push("toolsite".into());
        f.repos.get_mut("acme/toolsite-crm").unwrap().topics.push("toolsite".into());
    }
    let (_dir, config) = github_server(&api);
    let session = admin(&config);
    install(&config, &session).await;

    // crm is connected already, so discovery must leave it out.
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    let token = form_token_from(&page);
    let (status, ..) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=import&app=crm&installation=1&repo=acme/toolsite-crm&back=/admin/github")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    let panel = page
        .split("Repositories tagged toolsite")
        .nth(1)
        .and_then(|rest| rest.split("Connected apps").next())
        .expect("no discovery panel");
    assert!(panel.contains("acme/toolsite-shop"), "a tagged, unlinked repository is missing");
    assert!(panel.contains(r#"value="shop""#), "the proposed app name is not shop: {panel}");
    assert!(!panel.contains("acme/toolsite-crm"), "a connected repository was proposed again");
    assert!(!panel.contains("acme/notes"), "an untagged repository was proposed");
    assert!(!page.contains("<datalist"), "a datalist is still on the page");
    assert!(page.contains(r#"role="combobox""#));

    // The same list, as the tool's discover action reads it.
    let found = toolsite::platform::github::discover(&config).await.unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].full_name, "acme/toolsite-shop");
    assert_eq!(found[0].proposed_app, "shop");
    assert_eq!(found[0].installation_id, 1);
    assert!(found[0].private);

    // The row's Import button performs the import; afterwards nothing waits.
    let (status, _, headers) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=import&app=shop&installation=1&repo=acme/toolsite-shop&branch=main&back=/admin/github")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(flash(&headers).starts_with("ok:"), "{}", flash(&headers));
    assert!(toolsite::platform::github::link(&config, "shop").is_some());
    assert!(toolsite::platform::github::discover(&config).await.unwrap().is_empty());
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    assert!(page.contains("No tagged repository is waiting"));
}

/// One JSON-RPC call to `/mcp` with the static token, answered as the SSE
/// data line or plain JSON. Enough to drive the inline upload tools.
async fn mcp_call(config: &Arc<Config>, name: &str, arguments: serde_json::Value) -> (bool, String) {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text, _) = send(config, request).await;
    assert_eq!(status, StatusCode::OK, "{name}: {text}");
    let raw = text.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim).last().unwrap_or(text.trim()).to_string();
    let reply: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|_| panic!("not JSON-RPC: {text}"));
    let result = &reply["result"];
    (
        result["isError"].as_bool().unwrap_or(false),
        result["content"][0]["text"].as_str().unwrap_or("").to_string(),
    )
}

#[tokio::test]
async fn a_source_archive_sent_in_chunks_pushes_with_its_message_like_the_put() {
    use base64::Engine as _;
    let (fake, api) = fake_github::start().await;
    let (_dir, config) = github_server(&api);
    publish_app(&config, "shop");
    store_source(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&back=/admin/apps/shop/repo"))).await;
    assert_eq!(fake.lock().unwrap().repo("acme/shop").commits_made, 1);

    let next = tgz(&[("./package.json", b"{}"), ("./src/Phone.tsx", b"export default () => <p>phone</p>")]);
    let (is_error, text) = mcp_call(
        &config,
        "upload_begin",
        serde_json::json!({ "slug": "shop", "kind": "source", "message": "Fix the phone field" }),
    )
    .await;
    assert!(!is_error, "{text}");
    let id = text.split_whitespace().nth(1).unwrap().to_string();
    for (index, part) in next.chunks(500).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(part);
        let (is_error, text) = mcp_call(&config, "upload_chunk", serde_json::json!({ "id": id, "index": index, "data": data })).await;
        assert!(!is_error, "{text}");
    }
    let count = next.chunks(500).count();
    let (is_error, reply) = mcp_call(&config, "upload_finish", serde_json::json!({ "id": id, "chunks": count })).await;
    assert!(!is_error, "{reply}");
    assert!(reply.contains("pushed to acme/shop@main as "), "{reply}");

    let fake = fake.lock().unwrap();
    let repo = fake.repo("acme/shop");
    assert_eq!(repo.commits_made, 2, "one commit per publish");
    assert!(repo.files.contains_key("src/Phone.tsx"));
    let (_, message) = &repo.history[0];
    assert_eq!(message, "Fix the phone field\n\nPublished from toolsite");
}
