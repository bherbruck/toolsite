//! Repositories and deploy tokens, end to end over the router, against a
//! GitHub that lives in this process: a small axum app on a loopback port
//! that checks the App JWT's signature, hands out installation tokens,
//! keeps repositories, trees and secrets in memory, and opens sealed secrets
//! with the key it published. Nothing here reaches the network.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::Arc;
use tempfile::TempDir;
use toolsite::{build_router, platform::github::App, runtime::wasm::Runtime, Config};
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
        /// Plaintext, having been opened with the repository's key.
        pub secrets: HashMap<String, String>,
        pub dispatches: Vec<String>,
        pub head: String,
        pub commits_made: usize,
        pub topics: Vec<String>,
    }

    pub struct Fake {
        pub secret: crypto_box::SecretKey,
        pub installations: Vec<(u64, &'static str, &'static str)>,
        pub repos: BTreeMap<String, Repo>,
        blobs: HashMap<String, Vec<u8>>,
        trees: HashMap<String, Vec<(String, String)>>,
        commits: HashMap<String, String>,
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
            self.commits.insert(commit.clone(), tree);
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
                    secrets: HashMap::new(),
                    dispatches: Vec::new(),
                    head: commit,
                    commits_made: 0,
                    topics: Vec::new(),
                },
            );
        }
        pub fn repo(&self, full: &str) -> &Repo {
            self.repos.get(&full.to_lowercase()).expect("no such repo in the fake")
        }
    }

    pub async fn start() -> (Shared, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let fake: Shared = Arc::new(Mutex::new(Fake {
            secret: crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng),
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
            .route("/repos/{owner}/{repo}/contents/{*path}", get(get_contents).put(put_contents))
            .route("/repos/{owner}/{repo}/actions/secrets/public-key", get(public_key))
            .route("/repos/{owner}/{repo}/actions/secrets/{name}", axum::routing::put(put_secret))
            .route("/repos/{owner}/{repo}/actions/workflows/{file}/dispatches", post(dispatch))
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
            .map(|r| serde_json::json!({ "name": r.name, "full_name": format!("{}/{}", r.owner, r.name), "owner": { "login": r.owner }, "default_branch": r.default_branch, "private": r.private }))
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
            Some(tree) => Json(serde_json::json!({ "sha": sha, "tree": { "sha": tree } })).into_response(),
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
            let blob = item["sha"].as_str().unwrap().to_string();
            assert!(fake.blobs.contains_key(&blob), "tree names a blob that was never stored");
            entries.retain(|(p, _)| p != &path);
            entries.push((path, blob));
        }
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
        let id = sha(format!("{tree}{parents:?}{}", body["message"]).as_bytes());
        let mut fake = fake.lock().unwrap();
        assert!(fake.trees.contains_key(&tree), "commit names a tree that was never stored");
        fake.commits.insert(id.clone(), tree);
        (StatusCode::CREATED, Json(serde_json::json!({ "sha": id }))).into_response()
    }

    async fn update_ref(State(fake): State<Shared>, Path((owner, repo, _branch)): Path<(String, String, String)>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let sha = body["sha"].as_str().unwrap().to_string();
        let mut fake = fake.lock().unwrap();
        let tree = fake.commits.get(&sha).cloned().expect("ref moved to an unknown commit");
        let entries = fake.trees.get(&tree).cloned().unwrap();
        let files: BTreeMap<String, Vec<u8>> = entries
            .into_iter()
            .map(|(path, blob)| (path, fake.blobs.get(&blob).cloned().unwrap()))
            .collect();
        let r = fake.repos.get_mut(&format!("{owner}/{repo}").to_lowercase()).unwrap();
        r.head = sha;
        r.files = files;
        r.commits_made += 1;
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

    async fn public_key(State(fake): State<Shared>, headers: HeaderMap) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let public = fake.lock().unwrap().secret.public_key();
        Json(serde_json::json!({ "key_id": "key-1", "key": BASE64.encode(public.as_bytes()) })).into_response()
    }

    async fn put_secret(State(fake): State<Shared>, Path((owner, repo, name)): Path<(String, String, String)>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        assert_eq!(body["key_id"], "key-1");
        let sealed = BASE64.decode(body["encrypted_value"].as_str().unwrap()).unwrap();
        let mut fake = fake.lock().unwrap();
        let plain = fake.secret.unseal(&sealed).expect("the secret was not sealed to the repository's key");
        let r = fake.repos.get_mut(&format!("{owner}/{repo}").to_lowercase()).unwrap();
        r.secrets.insert(name, String::from_utf8(plain).unwrap());
        StatusCode::CREATED.into_response()
    }

    async fn dispatch(State(fake): State<Shared>, Path((owner, repo, file)): Path<(String, String, String)>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response {
        if let Err(r) = installed(&headers) {
            return r;
        }
        let mut fake = fake.lock().unwrap();
        let r = fake.repos.get_mut(&format!("{owner}/{repo}").to_lowercase()).unwrap();
        if !r.files.contains_key(&format!(".github/workflows/{file}")) {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "message": "workflow not found" }))).into_response();
        }
        r.dispatches.push(body["ref"].as_str().unwrap_or("").to_string());
        StatusCode::NO_CONTENT.into_response()
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
async fn creating_a_repository_pushes_the_project_the_workflow_and_sealed_secrets() {
    let (fake, api) = fake_github::start().await;
    let (_dir, config) = github_server(&api);
    publish_app(&config, "shop");
    store_source(&config, "shop");
    let session = admin(&config);
    install(&config, &session).await;

    let (status, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert!(page.contains(r#"value="toolsite-shop""#), "the form does not propose toolsite-<app>");
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("Create a repository"));
    let token = form_token_from(&page);

    let (status, _, headers) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&private=1&back=/admin/apps/shop/repo")),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(flash(&headers).starts_with("ok:"), "{}", flash(&headers));

    {
        let fake = fake.lock().unwrap();
        let claims = fake.last_jwt_claims.as_ref().expect("no App JWT was presented");
        assert_eq!(claims["iss"], "12345");
        let repo = fake.repo("acme/shop");
        assert!(repo.private);
        assert_eq!(repo.topics, ["toolsite"], "the repository was not tagged");
        assert_eq!(repo.commits_made, 1, "the project should land in one commit");
        assert_eq!(repo.files.get("src/main.js").map(|b| b.as_slice()), Some(&b"console.log('hi')"[..]));
        assert!(repo.files.contains_key("package.json"));
        assert!(repo.files.contains_key("README.md"), "the initial commit was lost");
        assert!(!repo.files.keys().any(|k| k.contains("node_modules")), "node_modules was pushed");
        let workflow = String::from_utf8(repo.files[".github/workflows/toolsite.yml"].clone()).unwrap();
        assert!(workflow.contains("TOOLSITE_APP: \"shop\""));
        assert!(workflow.contains("/deploy/$TOOLSITE_APP"));
        assert_eq!(repo.secrets.get("TOOLSITE_URL").map(String::as_str), Some(SITE));
        let deploy_token = repo.secrets.get("TOOLSITE_DEPLOY_TOKEN").expect("no deploy token secret");
        assert!(toolsite::platform::deploy::authorize(&config, "shop", deploy_token), "the secret is not a live deploy token");
        assert!(!toolsite::platform::deploy::authorize(&config, "other", deploy_token));
    }

    let link = toolsite::platform::github::link(&config, "shop").expect("no link recorded");
    assert_eq!((link.owner.as_str(), link.repo.as_str(), link.branch.as_str()), ("acme", "shop", "main"));
    let (_, page, _) = send(&config, get_as("/admin/apps/shop/repo", &session)).await;
    assert!(page.contains("acme/shop"));
    assert!(page.contains("Sync repository"));
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    assert!(page.contains("acme/shop"));

    // Twice is refused, and the repository is not touched again.
    let (_, _, headers) = send(
        &config,
        post_form("/admin/repo", &session, format!("token={token}&action=create&app=shop&installation=1&repo=shop&back=/admin/apps/shop/repo")),
    )
    .await;
    assert!(flash(&headers).contains("already connected"));
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
async fn importing_a_repository_adds_the_workflow_sets_secrets_and_runs_it() {
    let (fake, api) = fake_github::start().await;
    fake.lock().unwrap().add_repo("acme", "dashboard", true);
    let (_dir, config) = github_server(&api);
    let session = admin(&config);
    install(&config, &session).await;

    // From the GitHub page, as a new app.
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
    // No app exists yet, so the person is kept on the GitHub page.
    assert_eq!(location(&headers), "/admin/github");

    {
        let fake = fake.lock().unwrap();
        let repo = fake.repo("acme/dashboard");
        let workflow = String::from_utf8(repo.files[".github/workflows/toolsite.yml"].clone()).unwrap();
        assert!(workflow.contains("TOOLSITE_APP: \"dash\""));
        assert!(workflow.contains("working-directory: \"web\""));
        assert!(workflow.contains("hashFiles('web/package.json')"));
        assert_eq!(repo.files.get("README.md").map(|b| b.as_slice()), Some(&b"# hello"[..]), "an import must not touch the project");
        assert_eq!(repo.secrets.get("TOOLSITE_URL").map(String::as_str), Some(SITE));
        let deploy_token = repo.secrets.get("TOOLSITE_DEPLOY_TOKEN").unwrap();
        assert!(toolsite::platform::deploy::authorize(&config, "dash", deploy_token));
        assert_eq!(repo.dispatches, vec!["main".to_string()], "the first deploy was not started");
        assert!(repo.topics.iter().any(|t| t == "toolsite"), "an imported repository was not tagged");
    }
    let link = toolsite::platform::github::link(&config, "dash").unwrap();
    assert_eq!(link.directory, "web/");
    assert_eq!(link.branch, "main");

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
async fn a_webhook_must_be_signed_and_records_pushes_and_deploys() {
    let (fake, api) = fake_github::start().await;
    fake.lock().unwrap().add_repo("acme", "dashboard", true);
    let (_dir, config) = github_server(&api);
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=import&app=dash&installation=1&repo=acme/dashboard&back=/admin/github"))).await;

    let push = r#"{"ref":"refs/heads/main","after":"abcdef1234567890","repository":{"full_name":"acme/dashboard"}}"#;
    let (status, ..) = send(&config, webhook("push", None, push)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "an unsigned webhook was taken");
    let (status, ..) = send(&config, webhook("push", Some(&sign("wrong-secret", push.as_bytes())), push)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a webhook signed with another secret was taken");
    let (status, ..) = send(&config, webhook("push", Some(&sign("hook-secret", b"{}")), push)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a signature over a different body was taken");
    assert!(toolsite::platform::github::link(&config, "dash").unwrap().last_push.is_none());

    let (status, ..) = send(&config, webhook("push", Some(&sign("hook-secret", push.as_bytes())), push)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(toolsite::platform::github::link(&config, "dash").unwrap().last_push.unwrap().sha, "abcdef1234567890");

    // Another branch is noise.
    let other = r#"{"ref":"refs/heads/feature","after":"ffff","repository":{"full_name":"acme/dashboard"}}"#;
    send(&config, webhook("push", Some(&sign("hook-secret", other.as_bytes())), other)).await;
    assert_eq!(toolsite::platform::github::link(&config, "dash").unwrap().last_push.unwrap().sha, "abcdef1234567890");

    let run = r#"{"action":"completed","repository":{"full_name":"acme/dashboard"},"workflow_run":{"path":".github/workflows/toolsite.yml","conclusion":"success","html_url":"https://github.com/acme/dashboard/actions/runs/1"}}"#;
    let (status, ..) = send(&config, webhook("workflow_run", Some(&sign("hook-secret", run.as_bytes())), run)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let deploy = toolsite::platform::github::link(&config, "dash").unwrap().last_deploy.unwrap();
    assert_eq!(deploy.conclusion, "success");
    assert!(deploy.url.ends_with("/runs/1"));

    // A repository nobody linked is acknowledged and ignored.
    let stranger = r#"{"ref":"refs/heads/main","after":"1","repository":{"full_name":"someone/else"}}"#;
    let (status, body, _) = send(&config, webhook("push", Some(&sign("hook-secret", stranger.as_bytes())), stranger)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.contains("ignored"));

    let (_, page, _) = send(&config, get_as("/admin/github", &session)).await;
    assert!(page.contains("success"));
}

#[tokio::test]
async fn rotating_replaces_the_secret_and_disconnecting_ends_the_token() {
    let (fake, api) = fake_github::start().await;
    fake.lock().unwrap().add_repo("acme", "dashboard", true);
    let (_dir, config) = github_server(&api);
    publish_app(&config, "dash");
    let session = admin(&config);
    install(&config, &session).await;
    let (_, page, _) = send(&config, get_as("/admin/apps/dash/repo", &session)).await;
    let token = form_token_from(&page);
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=import&app=dash&installation=1&repo=acme/dashboard&back=/admin/apps/dash/repo"))).await;
    let first = fake.lock().unwrap().repo("acme/dashboard").secrets["TOOLSITE_DEPLOY_TOKEN"].clone();

    // Sync dispatches again.
    send(&config, post_form("/admin/repo", &session, format!("token={token}&action=sync&app=dash&back=/admin/apps/dash/repo"))).await;
    assert_eq!(fake.lock().unwrap().repo("acme/dashboard").dispatches.len(), 2);

    // Rotate: a new token shown once, in the secret, old one dead.
    let (status, page, _) = send(&config, post_form("/admin/repo", &session, format!("token={token}&action=rotate&app=dash&back=/admin/apps/dash/repo"))).await;
    assert_eq!(status, StatusCode::OK, "rotate should render the tab with the new token");
    let shown = page
        .split("id=\"fresh-deploy-token\">")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("the new token is not shown")
        .to_string();
    let second = fake.lock().unwrap().repo("acme/dashboard").secrets["TOOLSITE_DEPLOY_TOKEN"].clone();
    assert_eq!(shown, second);
    assert_ne!(first, second);
    assert!(!toolsite::platform::deploy::authorize(&config, "dash", &first), "the old token survived rotation");
    assert!(toolsite::platform::deploy::authorize(&config, "dash", &second));

    // Disconnect: link gone from the live set, token dead, repository untouched.
    let (status, _, headers) = send(&config, post_form("/admin/repo", &session, format!("token={token}&action=disconnect&app=dash&back=/admin/apps/dash/repo"))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(flash(&headers).starts_with("ok:"));
    assert!(toolsite::platform::github::link(&config, "dash").is_none());
    assert!(!toolsite::platform::deploy::authorize(&config, "dash", &second));
    assert!(fake.lock().unwrap().repo("acme/dashboard").files.contains_key(".github/workflows/toolsite.yml"));
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
        "Metadata: read",
        "workflow_run",
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
}
