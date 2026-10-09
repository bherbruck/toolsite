//! Two-step sign-in, end to end through the real router: setup on the
//! account page, the code page after a password, the setup the site's
//! policy forces, recovery codes, limits, and the doors a pending sign-in
//! must not open. The clock codes are checked against is fixed, so each
//! test computes what a phone would show.
//!
//! Alone in its own binary because one test installs the process's log
//! subscriber, to prove no code or secret is ever written to the log.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use toolsite::{
    accounts::{
        mfa::{self, Clock, Policy, Primary, Settings, Step},
        users,
    },
    build_router,
    runtime::wasm::Runtime,
    Config,
};
use tower::ServiceExt;

const TOKEN: &str = "test-token";
const PASSWORD: &str = "correct horse battery";
/// Mid-step, so one step either side is plainly 30 seconds away.
const T0: u64 = 1_800_000_015;

// --- the log, captured for the whole binary ------------------------------

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn log() -> &'static Captured {
    static LOG: OnceLock<Captured> = OnceLock::new();
    LOG.get_or_init(|| {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        captured
    })
}

// --- a site and a browser ---------------------------------------------------

fn site_with(policy: Policy, for_providers: bool, base_url: Option<&str>) -> (TempDir, Arc<Config>) {
    log();
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        base_url: base_url.map(str::to_string),
        mfa: Settings { policy, for_providers, clock: Clock::fixed(T0) },
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    (dir, config)
}

fn site(policy: Policy) -> (TempDir, Arc<Config>) {
    site_with(policy, false, None)
}

struct Reply {
    status: StatusCode,
    body: String,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == "location")
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no Location: {} {}", self.status, self.body))
    }
    /// The value a Set-Cookie of this name carries, if one was set to a value.
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .find_map(|(_, v)| v.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap_or("").to_string()))
            .filter(|value| !value.is_empty())
    }
}

async fn send(config: &Arc<Config>, request: Request<Body>) -> Reply {
    let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await.unwrap();
    Reply { status, body: String::from_utf8_lossy(&bytes).to_string(), headers }
}

async fn get(config: &Arc<Config>, uri: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    send(config, request.body(Body::empty()).unwrap()).await
}

async fn post(config: &Arc<Config>, uri: &str, body: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    send(config, request.body(Body::from(body.to_string())).unwrap()).await
}

fn person(config: &Config, email: &str, admin: bool) -> users::User {
    users::sign_up_as(config, email, PASSWORD, admin).unwrap()
}

fn session(config: &Config, email: &str) -> String {
    users::log_in(config, email, PASSWORD).unwrap().1
}

fn form_token(config: &Config, user: &users::User) -> String {
    users::derive_form_token(config, &user.id)
}

/// Moves the clock on one step and returns the code a phone shows then, so
/// each sign-in in a test spends a step of its own.
fn next_code(config: &Config, secret: &str) -> String {
    let now = config.mfa.clock.now() + mfa::STEP;
    config.mfa.clock.set(now);
    mfa::code_at(secret, now).unwrap()
}

fn code_now(config: &Config, secret: &str, offset: i64) -> String {
    mfa::code_at(secret, (config.mfa.clock.now() as i64 + offset) as u64).unwrap()
}

/// A six-digit code that is none of the ones accepted around now.
fn wrong_code(config: &Config, secret: &str) -> String {
    let accepted: Vec<String> = [-30, 0, 30].iter().map(|o| code_now(config, secret, *o)).collect();
    (0..).map(|n: u32| format!("{:06}", (n * 7919) % 1_000_000)).find(|c| !accepted.contains(c)).unwrap()
}

/// The text between the first `start` and the next `end`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(&text[from..to])
}

fn secret_on(page: &str) -> String {
    between(page, "<code id=\"mfa-key\">", "</code>")
        .unwrap_or_else(|| panic!("no key on the page: {page}"))
        .replace(' ', "")
}

fn recovery_codes_on(page: &str) -> Vec<String> {
    let list = between(page, "id=\"recovery-codes\">", "</ul>").unwrap_or_else(|| panic!("no recovery codes: {page}"));
    list.split("<li>").skip(1).map(|item| item.split("</li>").next().unwrap().to_string()).collect()
}

/// Turns two-step sign-in on from the account page, as the person would.
/// Returns the secret and the recovery codes.
async fn enable(config: &Arc<Config>, user: &users::User, site_session: &str) -> (String, Vec<String>) {
    let cookie = format!("ts_session={site_session}");
    let token = form_token(config, user);
    let started = post(config, "/account/mfa/start", &format!("token={token}"), Some(&cookie)).await;
    assert_eq!(started.status, StatusCode::SEE_OTHER, "{}", started.body);
    let page = get(config, "/account", Some(&cookie)).await;
    assert!(page.body.contains("<svg"), "no QR code on the account page");
    let secret = secret_on(&page.body);
    let code = next_code(config, &secret);
    let done = post(config, "/account/mfa/confirm", &format!("token={token}&code={code}&password={}", urlencoding::encode(PASSWORD)), Some(&cookie)).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    let codes = recovery_codes_on(&done.body);
    assert_eq!(codes.len(), 10);
    (secret, codes)
}

/// A password sign-in. Returns the reply and the pending cookie, if one.
async fn password(config: &Arc<Config>, email: &str, next: &str) -> (Reply, Option<String>) {
    let reply = post(
        config,
        "/auth/login",
        &format!("email={}&password={}&next={}", urlencoding::encode(email), urlencoding::encode(PASSWORD), urlencoding::encode(next)),
        None,
    )
    .await;
    let pending = reply.cookie("ts_mfa");
    (reply, pending)
}

async fn submit_code(config: &Arc<Config>, pending: &str, code: &str) -> Reply {
    post(config, "/auth/mfa", &format!("code={}", urlencoding::encode(code)), Some(&format!("ts_mfa={pending}"))).await
}

// --- setup ------------------------------------------------------------------

#[tokio::test]
async fn setup_needs_a_correct_code() {
    let (_dir, config) = site(Policy::Off);
    let ann = person(&config, "ann@example.com", false);
    let s = session(&config, "ann@example.com");
    let cookie = format!("ts_session={s}");
    let token = form_token(&config, &ann);
    post(&config, "/account/mfa/start", &format!("token={token}"), Some(&cookie)).await;
    let secret = secret_on(&get(&config, "/account", Some(&cookie)).await.body);

    let wrong = wrong_code(&config, &secret);
    let refused = post(&config, "/account/mfa/confirm", &format!("token={token}&code={wrong}"), Some(&cookie)).await;
    assert_eq!(refused.status, StatusCode::SEE_OTHER);
    assert!(!mfa::is_enabled(&config, &ann.id), "a wrong code turned it on");

    // Without the form token, even the right code does nothing.
    let code = code_now(&config, &secret, 0);
    let forged = post(&config, "/account/mfa/confirm", &format!("token=nope&code={code}"), Some(&cookie)).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert!(!mfa::is_enabled(&config, &ann.id));

    let done = post(&config, "/account/mfa/confirm", &format!("token={token}&code={code}&password={}", urlencoding::encode(PASSWORD)), Some(&cookie)).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert!(mfa::is_enabled(&config, &ann.id));
    assert_eq!(recovery_codes_on(&done.body).len(), 10);
    // The secret is shown while setup waits, never after.
    let after = get(&config, "/account", Some(&cookie)).await;
    assert!(!after.body.contains(&secret) && !after.body.contains("mfa-key"), "the secret was shown again");
    assert!(after.body.contains("10 of 10 left"), "{}", after.body);
}

#[tokio::test]
async fn a_wrong_or_reused_code_is_refused() {
    let (_dir, config) = site(Policy::Off);
    let bo = person(&config, "bo@example.com", false);
    let (secret, _) = enable(&config, &bo, &session(&config, "bo@example.com")).await;

    // The code that confirmed setup is spent.
    let (_, pending) = password(&config, "bo@example.com", "/").await;
    let pending = pending.expect("no pending sign-in");
    let spent = code_now(&config, &secret, 0);
    let reply = submit_code(&config, &pending, &spent).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a spent code was accepted");
    assert!(reply.cookie("ts_session").is_none());

    let reply = submit_code(&config, &pending, &wrong_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.body.contains("You can try 3 more times"), "{}", reply.body);

    let fresh = next_code(&config, &secret);
    let reply = submit_code(&config, &pending, &fresh).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(reply.cookie("ts_session").is_some());

    // The same code again, on a new sign-in within its window.
    let (_, again) = password(&config, "bo@example.com", "/").await;
    let reply = submit_code(&config, &again.unwrap(), &fresh).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a code worked twice");
}

#[tokio::test]
async fn a_code_from_the_previous_and_next_step_works_but_not_two_steps_away() {
    let (_dir, config) = site(Policy::Off);
    let cy = person(&config, "cy@example.com", false);
    let (secret, _) = enable(&config, &cy, &session(&config, "cy@example.com")).await;
    config.mfa.clock.set(T0 + 600);

    let (_, pending) = password(&config, "cy@example.com", "/").await;
    let pending = pending.unwrap();
    for offset in [-60, 60] {
        let reply = submit_code(&config, &pending, &code_now(&config, &secret, offset)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a code {offset}s away was accepted");
    }
    let reply = submit_code(&config, &pending, &code_now(&config, &secret, -30)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "the previous step was refused: {}", reply.body);

    let (_, pending) = password(&config, "cy@example.com", "/").await;
    let reply = submit_code(&config, &pending.unwrap(), &code_now(&config, &secret, 30)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "the next step was refused: {}", reply.body);
}

#[tokio::test]
async fn recovery_codes_work_once_each() {
    let (_dir, config) = site(Policy::Off);
    let di = person(&config, "di@example.com", false);
    let (_, codes) = enable(&config, &di, &session(&config, "di@example.com")).await;

    let (_, pending) = password(&config, "di@example.com", "/account").await;
    let reply = submit_code(&config, &pending.unwrap(), &codes[0]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/account");
    let signed_in = reply.cookie("ts_session").unwrap();

    let (_, pending) = password(&config, "di@example.com", "/").await;
    let reply = submit_code(&config, &pending.clone().unwrap(), &codes[0]).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a recovery code worked twice");
    // Case, spaces and the dash do not matter.
    let typed = format!(" {} ", codes[1].to_uppercase().replace('-', " "));
    let reply = submit_code(&config, &pending.unwrap(), &typed).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);

    let page = get(&config, "/account", Some(&format!("ts_session={signed_in}"))).await;
    assert!(page.body.contains("8 of 10 left"), "{}", page.body);
}

// --- a pending sign-in is not a session --------------------------------------

#[tokio::test]
async fn password_alone_with_mfa_on_yields_no_session_and_no_access() {
    let (_dir, config) = site(Policy::Off);
    let root = person(&config, "root@example.com", true);
    enable(&config, &root, &session(&config, "root@example.com")).await;
    let dir = config.data_dir.join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<h1>notes home</h1>").unwrap();
    let mut meta = toolsite::content::store::read_meta_blocking(&config, "notes");
    meta.gate = Some("authenticated".into());
    toolsite::content::store::write_meta_blocking(&config, "notes", &meta).unwrap();

    let (reply, pending) = password(&config, "root@example.com", "/admin").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/auth/mfa");
    assert!(reply.cookie("ts_session").is_none(), "a session came with the password alone");
    let pending = pending.expect("no pending cookie");

    // Presented as anything, the pending token opens nothing.
    for cookie in [format!("ts_session={pending}"), format!("ts_mfa={pending}"), format!("ts_session={pending}; ts_mfa={pending}")] {
        let account = get(&config, "/account", Some(&cookie)).await;
        assert_eq!(account.status, StatusCode::SEE_OTHER, "/account opened for a pending sign-in");
        assert!(account.location().starts_with("/auth/login"));
        let admin = get(&config, "/admin", Some(&cookie)).await;
        assert_ne!(admin.status, StatusCode::OK, "/admin opened for a pending sign-in");
        let me = get(&config, "/auth/me", Some(&cookie)).await;
        assert_eq!(me.status, StatusCode::UNAUTHORIZED);
        let app = get(&config, "/p/notes/", Some(&cookie)).await;
        assert!(!app.body.contains("notes home"), "an app opened for a pending sign-in");
        let handoff = get(&config, "/auth/handoff?app=notes&next=/p/notes/", Some(&cookie)).await;
        assert!(handoff.location().starts_with("/auth/login"), "the handoff minted for a pending sign-in");
        assert!(handoff.cookie("ts_app_notes").is_none());
    }
}

#[tokio::test]
async fn a_pending_sign_in_expires_and_cannot_be_used_as_a_session() {
    let (_dir, config) = site(Policy::Off);
    let ed = person(&config, "ed@example.com", false);
    let (secret, _) = enable(&config, &ed, &session(&config, "ed@example.com")).await;
    let (_, pending) = password(&config, "ed@example.com", "/").await;
    let pending = pending.unwrap();
    assert!(users::site_session_user(&config, &pending).is_none());

    config.mfa.clock.set(config.mfa.clock.now() + mfa::PENDING_LIFETIME + 1);
    let code = code_now(&config, &secret, 0);
    let reply = submit_code(&config, &pending, &code).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "an expired pending sign-in finished");
    assert!(reply.cookie("ts_session").is_none());
    assert!(reply.body.contains("expired"), "{}", reply.body);
}

#[tokio::test]
async fn a_pending_sign_in_of_one_account_cannot_be_finished_with_anothers_code() {
    let (_dir, config) = site(Policy::Off);
    let a = person(&config, "a@example.com", false);
    let b = person(&config, "b@example.com", false);
    let (secret_a, _) = enable(&config, &a, &session(&config, "a@example.com")).await;
    let (secret_b, codes_b) = enable(&config, &b, &session(&config, "b@example.com")).await;

    let (_, pending_a) = password(&config, "a@example.com", "/").await;
    let pending_a = pending_a.unwrap();
    let code_b = next_code(&config, &secret_b);
    assert_ne!(code_b, code_now(&config, &secret_a, 0), "the two secrets agree at this step by chance");
    let reply = submit_code(&config, &pending_a, &code_b).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "B's code finished A's sign-in");
    let reply = submit_code(&config, &pending_a, &codes_b[0]).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "B's recovery code finished A's sign-in");
    // B's code still works for B: A's attempt did not spend it.
    let (_, pending_b) = password(&config, "b@example.com", "/").await;
    let reply = submit_code(&config, &pending_b.unwrap(), &code_b).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let session = reply.cookie("ts_session").unwrap();
    assert_eq!(users::site_session_user(&config, &session).unwrap().email, "b@example.com");
}

// --- every door that makes a session --------------------------------------

#[tokio::test]
async fn mcp_oauth_consent_requires_the_code() {
    let (_dir, config) = site_with(Policy::Off, false, Some("https://site.test"));
    let root = person(&config, "root@example.com", true);
    let (secret, _) = enable(&config, &root, &session(&config, "root@example.com")).await;
    let client = toolsite::platform::oauth_store::register_client(&config, Some("Test"), &["https://client.test/cb".to_string()]).unwrap();
    let authorize = format!(
        "/authorize?response_type=code&client_id={}&redirect_uri={}&state=xyz&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256",
        client.id,
        urlencoding::encode("https://client.test/cb")
    );

    let first = get(&config, &authorize, None).await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    let login = first.location().to_string();
    assert!(login.starts_with("/auth/login?next="), "{login}");
    let next = urlencoding::decode(login.trim_start_matches("/auth/login?next=")).unwrap().into_owned();

    let (reply, pending) = password(&config, "root@example.com", &next).await;
    assert_eq!(reply.location(), "/auth/mfa");
    let pending = pending.unwrap();
    let held = get(&config, &authorize, Some(&format!("ts_session={pending}; ts_mfa={pending}"))).await;
    assert_eq!(held.status, StatusCode::SEE_OTHER, "the consent screen opened without the code");
    assert!(held.location().starts_with("/auth/login"));

    let reply = submit_code(&config, &pending, &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), next, "the code page did not return to the consent screen");
    let session = reply.cookie("ts_session").unwrap();
    let consent = get(&config, &authorize, Some(&format!("ts_session={session}"))).await;
    assert_eq!(consent.status, StatusCode::OK, "{}", consent.body);
    assert!(consent.body.contains("client.test"));
}

#[tokio::test]
async fn the_subdomain_handoff_refuses_to_mint_for_a_pending_sign_in() {
    log();
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        base_url: Some("https://site.test".into()),
        apps: Some(toolsite::content::origins::AppsDomain::parse("apps.test", Some("https://site.test"), None).unwrap()),
        mfa: Settings { policy: Policy::Off, for_providers: false, clock: Clock::fixed(T0) },
        ..Config::local(dir.path().to_path_buf(), TOKEN)
    });
    let app = config.data_dir.join("orders");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("index.html"), "<h1>orders home</h1>").unwrap();
    let mut meta = toolsite::content::store::read_meta_blocking(&config, "orders");
    meta.gate = Some("authenticated".into());
    toolsite::content::store::write_meta_blocking(&config, "orders", &meta).unwrap();

    let fy = person(&config, "fy@example.com", false);
    let site_session = session(&config, "fy@example.com");
    let cookie = format!("__Host-ts_session={site_session}");
    let token = form_token(&config, &fy);
    let host_post = |uri: &str, body: String| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("host", "site.test")
            .header("cookie", cookie.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    };
    send(&config, host_post("/account/mfa/start", format!("token={token}"))).await;
    let page = send(&config, Request::builder().uri("/account").header("host", "site.test").header("cookie", cookie.clone()).body(Body::empty()).unwrap()).await;
    let secret = secret_on(&page.body);
    let code = next_code(&config, &secret);
    let done = send(&config, host_post("/account/mfa/confirm", format!("token={token}&code={code}&password={}", urlencoding::encode(PASSWORD)))).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);

    let login = send(
        &config,
        Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("host", "site.test")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!("email=fy%40example.com&password={}&next=%2F", urlencoding::encode(PASSWORD))))
            .unwrap(),
    )
    .await;
    let pending = login.cookie("__Host-ts_mfa").expect("no pending cookie on the main host");
    assert!(login.cookie("__Host-ts_session").is_none());

    let state = "s".repeat(32);
    let handoff = send(
        &config,
        Request::builder()
            .uri(format!("/auth/handoff?app=orders&next=%2Fp%2Forders%2F&state={state}"))
            .header("host", "site.test")
            .header("cookie", format!("__Host-ts_session={pending}; __Host-ts_mfa={pending}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(handoff.status, StatusCode::SEE_OTHER);
    assert!(handoff.location().starts_with("/auth/login"), "the handoff went on: {}", handoff.location());
    assert!(config.stores.tickets.live(toolsite::state::tickets::Kind::Handoff).await.unwrap() == 0, "a handoff code was minted for a pending sign-in");
}

#[tokio::test]
async fn a_setup_link_still_asks_for_the_code() {
    let (_dir, config) = site(Policy::Off);
    let gu = person(&config, "gu@example.com", false);
    enable(&config, &gu, &session(&config, "gu@example.com")).await;
    let link = users::reinvite(&config, "gu@example.com").unwrap();
    let reply = post(&config, "/auth/setup", &format!("token={link}&password=another%20long%20password"), None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/auth/mfa", "a setup link signed in past two-step sign-in");
    assert!(reply.cookie("ts_session").is_none());
}

// --- limits -----------------------------------------------------------------

#[tokio::test]
async fn five_wrong_codes_end_a_pending_sign_in() {
    let (_dir, config) = site(Policy::Off);
    let hu = person(&config, "hu@example.com", false);
    let (secret, _) = enable(&config, &hu, &session(&config, "hu@example.com")).await;
    let (_, pending) = password(&config, "hu@example.com", "/").await;
    let pending = pending.unwrap();
    let wrong = wrong_code(&config, &secret);
    for attempt in 1..=5 {
        let reply = submit_code(&config, &pending, &wrong).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
        if attempt == 5 {
            assert!(reply.body.contains("Too many wrong codes"), "{}", reply.body);
        }
    }
    let reply = submit_code(&config, &pending, &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a right code finished a sign-in that had ended");
    assert!(reply.cookie("ts_session").is_none());
}

#[tokio::test]
async fn ten_wrong_codes_across_sign_ins_stop_the_account_for_fifteen_minutes() {
    let (_dir, config) = site(Policy::Off);
    let iv = person(&config, "iv@example.com", false);
    let (secret, _) = enable(&config, &iv, &session(&config, "iv@example.com")).await;
    let wrong = wrong_code(&config, &secret);
    for tries in [4, 4, 2] {
        let (_, pending) = password(&config, "iv@example.com", "/").await;
        let pending = pending.unwrap();
        for _ in 0..tries {
            assert_eq!(submit_code(&config, &pending, &wrong).await.status, StatusCode::UNAUTHORIZED);
        }
    }
    let (_, pending) = password(&config, "iv@example.com", "/").await;
    let reply = submit_code(&config, &pending.unwrap(), &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS, "{}", reply.body);
    assert!(reply.cookie("ts_session").is_none());

    // The window passes, and the right code works again.
    config.mfa.clock.set(config.mfa.clock.now() + 15 * 60 + 1);
    let (_, pending) = password(&config, "iv@example.com", "/").await;
    let reply = submit_code(&config, &pending.unwrap(), &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
}

/// Wrong codes sent all at once, each from its own request, are checked no
/// more often than one at a time would be. A limit read before the code is
/// checked and counted after lets a burst through whole: every request
/// reads "under the limit" before any of them has counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn wrong_codes_sent_at_once_are_checked_no_more_often_than_the_limits_allow() {
    let (_dir, config) = site(Policy::Off);
    let jo = person(&config, "jo@example.com", false);
    let (secret, _) = enable(&config, &jo, &session(&config, "jo@example.com")).await;
    let wrong = wrong_code(&config, &secret);
    let burst = |pending: String| {
        let (config, wrong) = (config.clone(), wrong.clone());
        async move {
            let sent: Vec<_> = (0..40)
                .map(|_| {
                    let (config, pending, wrong) = (config.clone(), pending.clone(), wrong.clone());
                    tokio::spawn(async move { submit_code(&config, &pending, &wrong).await.status })
                })
                .collect();
            for request in sent {
                assert_ne!(request.await.unwrap(), StatusCode::SEE_OTHER);
            }
        }
    };
    let checked = || toolsite::accounts::store::of(&config).failures_since(&jo.id, 0).unwrap();

    // One sign-in: five codes checked, however many arrive together.
    let (_, pending) = password(&config, "jo@example.com", "/").await;
    burst(pending.unwrap()).await;
    assert!(checked() <= 5, "one sign-in had {} codes checked", checked());

    // Three more sign-ins at once: the account's ten, and no more.
    let mut pendings = Vec::new();
    for _ in 0..3 {
        pendings.push(password(&config, "jo@example.com", "/").await.1.unwrap());
    }
    let bursts: Vec<_> = pendings.into_iter().map(|pending| tokio::spawn(burst(pending))).collect();
    for b in bursts {
        b.await.unwrap();
    }
    assert!(checked() <= 10, "the account had {} codes checked", checked());
    let (_, pending) = password(&config, "jo@example.com", "/").await;
    let reply = submit_code(&config, &pending.unwrap(), &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS, "{}", reply.body);
}

// --- policy -------------------------------------------------------------------

#[tokio::test]
async fn policy_admins_forces_setup_for_an_admin_and_not_for_a_regular_user() {
    let (_dir, config) = site(Policy::Admins);
    person(&config, "root@example.com", true);
    person(&config, "jo@example.com", false);

    let (reply, pending) = password(&config, "root@example.com", "/admin").await;
    assert_eq!(reply.location(), "/auth/mfa/setup");
    assert!(reply.cookie("ts_session").is_none(), "an admin got a session without setting up");
    let pending = pending.unwrap();
    let page = get(&config, "/auth/mfa/setup", Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("<svg"));
    let secret = secret_on(&page.body);
    // A reload shows the same secret, which the phone may already hold.
    let again = get(&config, "/auth/mfa/setup", Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(secret_on(&again.body), secret);

    let wrong = post(&config, "/auth/mfa/setup", &format!("code={}", wrong_code(&config, &secret)), Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.cookie("ts_session").is_none());

    let code = next_code(&config, &secret);
    let done = post(&config, "/auth/mfa/setup", &format!("code={code}"), Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert!(done.cookie("ts_session").is_some());
    assert_eq!(recovery_codes_on(&done.body).len(), 10);
    assert!(done.body.contains("href=\"/admin\""), "no way on to where the sign-in was going");

    let (reply, _) = password(&config, "jo@example.com", "/").await;
    assert!(reply.cookie("ts_session").is_some(), "a regular user was held for setup under admins");
}

#[tokio::test]
async fn policy_everyone_forces_setup_for_everyone() {
    let (_dir, config) = site(Policy::Everyone);
    person(&config, "ky@example.com", false);
    let (reply, pending) = password(&config, "ky@example.com", "/").await;
    assert_eq!(reply.location(), "/auth/mfa/setup");
    assert!(reply.cookie("ts_session").is_none());
    assert!(pending.is_some());
}

#[tokio::test]
async fn policy_off_asks_nothing_of_an_account_without_it() {
    let (_dir, config) = site(Policy::Off);
    person(&config, "root@example.com", true);
    let (reply, _) = password(&config, "root@example.com", "/").await;
    assert!(reply.cookie("ts_session").is_some());
}

#[tokio::test]
async fn a_provider_sign_in_skips_the_code_unless_mfa_for_providers() {
    for for_providers in [false, true] {
        let (_dir, config) = site_with(Policy::Everyone, for_providers, None);
        let lu = person(&config, "lu@example.com", false);
        enable(&config, &lu, &session(&config, "lu@example.com")).await;
        let step = mfa::after_primary(&config, &lu, Primary::Provider, "/").unwrap();
        match (for_providers, step) {
            (false, Step::Session(token)) => assert!(users::site_session_user(&config, &token).is_some()),
            (true, Step::Code(token)) => assert!(users::site_session_user(&config, &token).is_none()),
            (wanted, other) => panic!("for_providers={wanted}: {other:?}"),
        }
        // And a provider-only account the policy requires it of.
        let mo = users::create_provider_account(&config, "mo@example.com").unwrap();
        let step = mfa::after_primary(&config, &mo, Primary::Provider, "/").unwrap();
        assert_eq!(matches!(step, Step::Setup(_)), for_providers, "for_providers={for_providers}: {step:?}");
        // A password always owes it.
        assert!(matches!(mfa::after_primary(&config, &lu, Primary::Password, "/").unwrap(), Step::Code(_)));
    }
}

#[tokio::test]
async fn turning_it_off_is_refused_when_the_policy_requires_it() {
    let (_dir, config) = site(Policy::Admins);
    let root = person(&config, "root@example.com", true);
    let s = session(&config, "root@example.com");
    let (secret, _) = enable(&config, &root, &s).await;
    let code = next_code(&config, &secret);
    let reply = post(&config, "/account/mfa/off", &format!("token={}&code={code}", form_token(&config, &root)), Some(&format!("ts_session={s}"))).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(mfa::is_enabled(&config, &root.id), "turned off against the policy");

    // A regular account under the same policy may.
    let no = person(&config, "no@example.com", false);
    let s = session(&config, "no@example.com");
    let (_, codes) = enable(&config, &no, &s).await;
    let reply = post(&config, "/account/mfa/off", &format!("token={}&code={}", form_token(&config, &no), codes[3]), Some(&format!("ts_session={s}"))).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(!mfa::is_enabled(&config, &no.id));
}

#[tokio::test]
async fn new_recovery_codes_need_a_current_code_and_replace_the_old_ones() {
    let (_dir, config) = site(Policy::Off);
    let pi = person(&config, "pi@example.com", false);
    let s = session(&config, "pi@example.com");
    let (secret, old) = enable(&config, &pi, &s).await;
    let token = form_token(&config, &pi);
    let cookie = format!("ts_session={s}");
    // A recovery code is not enough to mint ten more.
    let refused = post(&config, "/account/mfa/recovery", &format!("token={token}&code={}", old[0]), Some(&cookie)).await;
    assert_eq!(refused.status, StatusCode::SEE_OTHER);
    let fresh = post(&config, "/account/mfa/recovery", &format!("token={token}&code={}", next_code(&config, &secret)), Some(&cookie)).await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    let new = recovery_codes_on(&fresh.body);
    assert_eq!(new.len(), 10);
    let (_, pending) = password(&config, "pi@example.com", "/").await;
    let pending = pending.unwrap();
    assert_eq!(submit_code(&config, &pending, &old[1]).await.status, StatusCode::UNAUTHORIZED, "an old recovery code still worked");
    assert_eq!(submit_code(&config, &pending, &new[0]).await.status, StatusCode::SEE_OTHER);
}

// --- sessions and tokens ------------------------------------------------------

#[tokio::test]
async fn an_admin_reset_removes_mfa_and_ends_every_session() {
    let (_dir, config) = site(Policy::Off);
    let root = person(&config, "root@example.com", true);
    let qu = person(&config, "qu@example.com", false);
    let qu_session = session(&config, "qu@example.com");
    enable(&config, &qu, &qu_session).await;
    let other = session(&config, "qu@example.com");
    let app = users::create_app_session(&config, &qu_session, "notes").unwrap().1;

    let admin_session = session(&config, "root@example.com");
    let page = get(&config, "/admin/accounts", Some(&format!("ts_session={admin_session}"))).await;
    assert!(page.body.contains("Two-step"), "no two-step column on the account list");
    let page = get(&config, "/admin/accounts/qu%40example.com", Some(&format!("ts_session={admin_session}"))).await;
    assert!(page.body.contains("two-step on") && page.body.contains("Reset two-step sign-in"), "{}", page.body);

    // The form token is required.
    let forged = post(&config, "/admin/mfa-reset", "token=nope&email=qu%40example.com", Some(&format!("ts_session={admin_session}"))).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert!(mfa::is_enabled(&config, &qu.id));

    let reply = post(
        &config,
        "/admin/mfa-reset",
        &format!("token={}&email=qu%40example.com&back=%2Fadmin%2Faccounts", form_token(&config, &root)),
        Some(&format!("ts_session={admin_session}")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(!mfa::is_enabled(&config, &qu.id));
    assert!(users::site_session_user(&config, &qu_session).is_none(), "a session survived the reset");
    assert!(users::site_session_user(&config, &other).is_none());
    assert!(users::app_session_user(&config, &app, "notes").is_none());
    let accounts = users::list_accounts(&config).unwrap();
    assert!(!accounts.iter().find(|a| a.email == "qu@example.com").unwrap().mfa);
    let (reply, _) = password(&config, "qu@example.com", "/").await;
    assert!(reply.cookie("ts_session").is_some(), "the password alone did not sign in after the reset");
}

#[tokio::test]
async fn enabling_mfa_ends_other_sessions_and_revokes_oauth_tokens() {
    let (_dir, config) = site(Policy::Off);
    let ra = person(&config, "ra@example.com", true);
    let mine = session(&config, "ra@example.com");
    let elsewhere = session(&config, "ra@example.com");
    let app = users::create_app_session(&config, &elsewhere, "notes").unwrap().1;
    let client = toolsite::platform::oauth_store::register_client(&config, Some("Test"), &["https://client.test/cb".to_string()]).unwrap();
    let issued = toolsite::platform::oauth_store::issue_tokens(&config, &client.id, &ra.id, None).unwrap();
    // Someone else's connection is not touched.
    let sy = person(&config, "sy@example.com", true);
    let theirs = toolsite::platform::oauth_store::issue_tokens(&config, &client.id, &sy.id, None).unwrap();

    enable(&config, &ra, &mine).await;
    assert!(users::site_session_user(&config, &mine).is_some(), "the session that turned it on was ended");
    assert!(users::site_session_user(&config, &elsewhere).is_none(), "another session survived");
    assert!(users::app_session_user(&config, &app, "notes").is_none(), "an app session survived");
    assert!(toolsite::platform::oauth_store::rotate_refresh(&config, &client.id, &issued.refresh_token).is_none(), "the refresh token still works");
    assert!(toolsite::platform::oauth_store::access_token_holder(&config, &issued.access_token).is_none(), "the access token still works");
    assert!(toolsite::platform::oauth_store::access_token_holder(&config, &theirs.access_token).is_some(), "another account's token was revoked");
}

#[tokio::test]
async fn forced_setup_at_sign_in_also_revokes_oauth_tokens() {
    let (_dir, config) = site(Policy::Admins);
    let ta = person(&config, "ta@example.com", true);
    let client = toolsite::platform::oauth_store::register_client(&config, Some("Test"), &["https://client.test/cb".to_string()]).unwrap();
    let issued = toolsite::platform::oauth_store::issue_tokens(&config, &client.id, &ta.id, None).unwrap();
    // A token from before the policy keeps working until two-step sign-in is on.
    assert!(toolsite::platform::oauth_store::access_token_holder(&config, &issued.access_token).is_some());
    let (_, pending) = password(&config, "ta@example.com", "/").await;
    let pending = pending.unwrap();
    let secret = secret_on(&get(&config, "/auth/mfa/setup", Some(&format!("ts_mfa={pending}"))).await.body);
    let code = next_code(&config, &secret);
    let done = post(&config, "/auth/mfa/setup", &format!("code={code}"), Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(toolsite::platform::oauth_store::rotate_refresh(&config, &client.id, &issued.refresh_token).is_none());
}

// --- at rest and in the log -------------------------------------------------

fn every_file(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            every_file(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[tokio::test]
async fn the_secret_and_recovery_codes_are_not_on_disk_in_the_clear() {
    let (dir, config) = site(Policy::Off);
    let uz = person(&config, "uz@example.com", false);
    let (secret, codes) = enable(&config, &uz, &session(&config, "uz@example.com")).await;
    let raw = data_encoding::BASE32_NOPAD.decode(secret.as_bytes()).unwrap();
    let mut files = Vec::new();
    every_file(dir.path(), &mut files);
    assert!(files.iter().any(|f| f.ends_with("auth.db")), "{files:?}");
    for file in files {
        let bytes = std::fs::read(&file).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(&secret), "the secret is in {file:?}");
        assert!(!bytes.windows(raw.len()).any(|w| w == raw.as_slice()), "the raw secret bytes are in {file:?}");
        for code in &codes {
            assert!(!text.contains(code.as_str()) && !text.contains(&code.replace('-', "")), "a recovery code is in {file:?}");
        }
    }
}

/// Whether `code` appears in `text` as a number of its own, not inside a
/// longer one (a timestamp, a port).
fn contains_number(text: &str, code: &str) -> bool {
    text.match_indices(code).any(|(at, _)| {
        let before = text[..at].chars().next_back().is_some_and(|c| c.is_ascii_digit());
        let after = text[at + code.len()..].chars().next().is_some_and(|c| c.is_ascii_digit());
        !before && !after
    })
}

#[tokio::test]
async fn nothing_logs_a_code_or_a_secret() {
    let (_dir, config) = site(Policy::Admins);
    person(&config, "vi@example.com", true);
    let (_, pending) = password(&config, "vi@example.com", "/").await;
    let pending = pending.unwrap();
    let secret = secret_on(&get(&config, "/auth/mfa/setup", Some(&format!("ts_mfa={pending}"))).await.body);
    let wrong = wrong_code(&config, &secret);
    post(&config, "/auth/mfa/setup", &format!("code={wrong}"), Some(&format!("ts_mfa={pending}"))).await;
    let setup_code = next_code(&config, &secret);
    let done = post(&config, "/auth/mfa/setup", &format!("code={setup_code}"), Some(&format!("ts_mfa={pending}"))).await;
    let codes = recovery_codes_on(&done.body);

    let mut used = vec![wrong.clone(), setup_code];
    let (_, pending) = password(&config, "vi@example.com", "/").await;
    let pending = pending.unwrap();
    submit_code(&config, &pending, &wrong).await;
    let code = next_code(&config, &secret);
    submit_code(&config, &pending, &code).await;
    used.push(code);
    let (_, last) = password(&config, "vi@example.com", "/").await;
    submit_code(&config, &last.unwrap(), &codes[0]).await;

    let text = String::from_utf8_lossy(&log().0.lock().unwrap()).to_string();
    assert!(text.contains("two-step"), "the flow logged nothing, so this proves nothing");
    assert!(!text.contains(&secret), "the secret was logged");
    for code in &used {
        assert!(!contains_number(&text, code), "a code was logged: {code}");
    }
    for code in &codes {
        assert!(!text.contains(code.as_str()) && !text.contains(&code.replace('-', "")), "a recovery code was logged");
    }
    assert!(!text.contains(&pending), "a pending token was logged");
}

// --- the adversarial pass -----------------------------------------------------

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

async fn post_as(config: &Arc<Config>, uri: &str, body: &str, cookie: &str, extra: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", cookie);
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    send(config, request.body(Body::from(body.to_string())).unwrap()).await
}

/// The pending setup page's secret, for a pending sign-in.
async fn forced_secret(config: &Arc<Config>, pending: &str) -> String {
    secret_on(&get(config, "/auth/mfa/setup", Some(&format!("ts_mfa={pending}"))).await.body)
}

/// Someone who learned the password opens the forced setup page and stops
/// there. When the owner then signs in and sets it up, the secret they scan
/// must be a new one: a shared one would leave the code in the attacker's
/// phone for good, and the owner believing their account was safe.
#[tokio::test]
async fn a_setup_secret_seen_by_another_sign_in_is_never_the_one_the_owner_confirms() {
    let (_dir, config) = site(Policy::Admins);
    person(&config, "root@example.com", true);

    let (_, theirs) = password(&config, "root@example.com", "/").await;
    let theirs = theirs.unwrap();
    let seen = forced_secret(&config, &theirs).await;

    let (_, mine) = password(&config, "root@example.com", "/").await;
    let mine = mine.unwrap();
    let scanned = forced_secret(&config, &mine).await;
    assert_ne!(seen, scanned, "the owner was shown the secret another sign-in had seen");
    // A reload still shows the owner the same one.
    assert_eq!(forced_secret(&config, &mine).await, scanned);

    // The other sign-in can no longer finish with what it saw.
    let code = next_code(&config, &seen);
    let reply = post(&config, "/auth/mfa/setup", &format!("code={code}"), Some(&format!("ts_mfa={theirs}"))).await;
    assert_ne!(reply.status, StatusCode::OK, "a replaced setup was confirmed");
    assert!(reply.cookie("ts_session").is_none());

    let code = next_code(&config, &scanned);
    let done = post(&config, "/auth/mfa/setup", &format!("code={code}"), Some(&format!("ts_mfa={mine}"))).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);

    // The secret seen first gives no codes that work.
    let (_, pending) = password(&config, "root@example.com", "/").await;
    let reply = submit_code(&config, &pending.unwrap(), &next_code(&config, &seen)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "the first secret signs in");
}

/// The same on the account page: setup begun in one session is not shown to,
/// nor confirmed by, another.
#[tokio::test]
async fn a_setup_begun_in_one_session_is_not_shown_to_or_confirmed_by_another() {
    let (_dir, config) = site(Policy::Off);
    let wu = person(&config, "wu@example.com", false);
    let token = form_token(&config, &wu);
    let (first, second) = (session(&config, "wu@example.com"), session(&config, "wu@example.com"));
    let (first, second) = (format!("ts_session={first}"), format!("ts_session={second}"));
    post(&config, "/account/mfa/start", &format!("token={token}"), Some(&first)).await;
    let seen = secret_on(&get(&config, "/account", Some(&first)).await.body);

    let page = get(&config, "/account", Some(&second)).await;
    assert!(!page.body.contains(&seen) && !page.body.contains("mfa-key"), "another session was shown the secret");
    let code = next_code(&config, &seen);
    let pass = urlencoding::encode(PASSWORD);
    post(&config, "/account/mfa/confirm", &format!("token={token}&code={code}&password={pass}"), Some(&second)).await;
    assert!(!mfa::is_enabled(&config, &wu.id), "another session confirmed a setup it never saw");

    // Starting again there replaces the secret, and the first session's is dead.
    post(&config, "/account/mfa/start", &format!("token={token}"), Some(&second)).await;
    let scanned = secret_on(&get(&config, "/account", Some(&second)).await.body);
    assert_ne!(seen, scanned);
    let code = next_code(&config, &seen);
    post(&config, "/account/mfa/confirm", &format!("token={token}&code={code}&password={pass}"), Some(&first)).await;
    assert!(!mfa::is_enabled(&config, &wu.id), "a replaced setup was confirmed");
}

/// A stolen session cookie alone must not put the thief's phone on the
/// account: that would sign the owner out and keep them out, since their
/// password would no longer be enough.
#[tokio::test]
async fn turning_it_on_needs_the_password_as_well_as_the_session() {
    let (_dir, config) = site(Policy::Off);
    let xi = person(&config, "xi@example.com", false);
    let owners = session(&config, "xi@example.com");
    let stolen = format!("ts_session={}", session(&config, "xi@example.com"));
    let token = form_token(&config, &xi);
    post(&config, "/account/mfa/start", &format!("token={token}"), Some(&stolen)).await;
    let secret = secret_on(&get(&config, "/account", Some(&stolen)).await.body);
    for password in ["", "&password=", "&password=guess%20one%20two"] {
        let code = next_code(&config, &secret);
        let reply = post(&config, "/account/mfa/confirm", &format!("token={token}&code={code}{password}"), Some(&stolen)).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        assert!(!mfa::is_enabled(&config, &xi.id), "turned on without the password ({password:?})");
    }
    assert!(users::site_session_user(&config, &owners).is_some(), "the owner was signed out");
}

#[tokio::test]
async fn one_code_sent_on_two_requests_at_once_signs_in_once() {
    let (_dir, config) = site(Policy::Off);
    let ya = person(&config, "ya@example.com", false);
    let (secret, codes) = enable(&config, &ya, &session(&config, "ya@example.com")).await;
    for round in 0..3 {
        let (_, a) = password(&config, "ya@example.com", "/").await;
        let (_, b) = password(&config, "ya@example.com", "/").await;
        let (a, b) = (a.unwrap(), b.unwrap());
        let code = if round == 2 { codes[round].clone() } else { next_code(&config, &secret) };
        let (one, two) = tokio::join!(submit_code(&config, &a, &code), submit_code(&config, &b, &code));
        let signed_in = [&one, &two].iter().filter(|r| r.cookie("ts_session").is_some()).count();
        assert_eq!(signed_in, 1, "round {round}: {} and {}", one.status, two.status);
    }
}

#[tokio::test]
async fn a_pending_sign_in_finishes_once() {
    let (_dir, config) = site(Policy::Off);
    let za = person(&config, "za@example.com", false);
    let (secret, _) = enable(&config, &za, &session(&config, "za@example.com")).await;
    let (_, pending) = password(&config, "za@example.com", "/").await;
    let pending = pending.unwrap();
    assert_eq!(submit_code(&config, &pending, &next_code(&config, &secret)).await.status, StatusCode::SEE_OTHER);
    let again = submit_code(&config, &pending, &next_code(&config, &secret)).await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED, "a finished sign-in finished again");
    assert!(again.cookie("ts_session").is_none());
}

/// What the old password started ends with it: a new password, a setup link
/// spent, the account disabled.
#[tokio::test]
async fn a_pending_sign_in_ends_with_the_password_or_the_account() {
    let (_dir, config) = site(Policy::Off);
    let ab = person(&config, "ab@example.com", false);
    let mine = session(&config, "ab@example.com");
    let (secret, _) = enable(&config, &ab, &mine).await;

    // A new password from the account page.
    let (_, pending) = password(&config, "ab@example.com", "/").await;
    let pending = pending.unwrap();
    users::change_password(&config, &ab.id, PASSWORD, "a brand new password", &mine).unwrap();
    let reply = submit_code(&config, &pending, &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a sign-in begun with the old password finished");
    users::change_password(&config, &ab.id, "a brand new password", PASSWORD, &mine).unwrap();

    // A setup link spent: also the end of every session.
    let (_, pending) = password(&config, "ab@example.com", "/").await;
    let pending = pending.unwrap();
    let link = users::reinvite(&config, "ab@example.com").unwrap();
    users::accept_invite(&config, &link, PASSWORD).unwrap();
    let reply = submit_code(&config, &pending, &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a sign-in begun before the reset finished");
    assert!(users::site_session_user(&config, &mine).is_none(), "a session outlived a password reset by link");

    // The account disabled.
    let (_, pending) = password(&config, "ab@example.com", "/").await;
    let pending = pending.unwrap();
    users::set_active(&config, "ab@example.com", false).unwrap();
    let reply = submit_code(&config, &pending, &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "a disabled account finished signing in");
    assert!(reply.cookie("ts_session").is_none());
}

/// Someone who knows only the email cannot start a pending sign-in, so
/// cannot spend the account's wrong-code allowance and lock its owner out.
#[tokio::test]
async fn a_wrong_password_starts_nothing_and_cannot_lock_the_account() {
    let (_dir, config) = site(Policy::Off);
    let bc = person(&config, "bc@example.com", false);
    let (secret, _) = enable(&config, &bc, &session(&config, "bc@example.com")).await;
    for _ in 0..12 {
        let reply = post(&config, "/auth/login", "email=bc%40example.com&password=not-it&next=%2F", None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert!(reply.cookie("ts_mfa").is_none() && reply.cookie("ts_session").is_none());
        // No pending cookie to send, so the code page has nothing to count against.
        let code = post(&config, "/auth/mfa", "code=000000", None).await;
        assert_eq!(code.status, StatusCode::UNAUTHORIZED);
    }
    let (_, pending) = password(&config, "bc@example.com", "/").await;
    let reply = submit_code(&config, &pending.unwrap(), &next_code(&config, &secret)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "the owner was locked out: {}", reply.body);
}

#[tokio::test]
async fn the_forced_setup_page_opens_only_for_a_pending_setup() {
    let (_dir, config) = site(Policy::Admins);
    person(&config, "root@example.com", true);
    let cd = person(&config, "cd@example.com", false);
    let (secret, _) = enable(&config, &cd, &session(&config, "cd@example.com")).await;

    let none = get(&config, "/auth/mfa/setup", None).await;
    assert!(none.location().starts_with("/auth/login"));
    let as_session = get(&config, "/auth/mfa/setup", Some(&format!("ts_session={}", session(&config, "cd@example.com")))).await;
    assert!(as_session.location().starts_with("/auth/login"), "a session opened the forced setup page");
    // A sign-in that owes a code, not setup, is sent to the code page and
    // cannot replace the secret it owes a code for.
    let (_, pending) = password(&config, "cd@example.com", "/").await;
    let pending = pending.unwrap();
    let held = get(&config, "/auth/mfa/setup", Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(held.location(), "/auth/mfa");
    let posted = post(&config, "/auth/mfa/setup", "code=123456", Some(&format!("ts_mfa={pending}"))).await;
    assert!(posted.cookie("ts_session").is_none());
    assert_eq!(submit_code(&config, &pending, &next_code(&config, &secret)).await.status, StatusCode::SEE_OTHER);

    // Walking away from forced setup leaves nothing signed in.
    let (_, pending) = password(&config, "root@example.com", "/admin").await;
    let pending = pending.unwrap();
    forced_secret(&config, &pending).await;
    for path in ["/admin", "/account", "/auth/me", "/"] {
        let reply = get(&config, path, Some(&format!("ts_mfa={pending}"))).await;
        assert!(!reply.body.contains("root@example.com"), "{path} knew who was there during forced setup");
    }
}

#[tokio::test]
async fn pages_with_a_secret_or_codes_are_never_cached() {
    let (_dir, config) = site(Policy::Admins);
    let root = person(&config, "root@example.com", true);
    let (_, pending) = password(&config, "root@example.com", "/").await;
    let pending = pending.unwrap();
    let setup = get(&config, "/auth/mfa/setup", Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(setup.header("cache-control"), Some("no-store"));
    let secret = secret_on(&setup.body);
    let done = post(&config, "/auth/mfa/setup", &format!("code={}", next_code(&config, &secret)), Some(&format!("ts_mfa={pending}"))).await;
    assert_eq!(done.header("cache-control"), Some("no-store"), "the recovery codes page may be cached");

    let (_, pending) = password(&config, "root@example.com", "/").await;
    let pending = pending.unwrap();
    assert_eq!(get(&config, "/auth/mfa", Some(&format!("ts_mfa={pending}"))).await.header("cache-control"), Some("no-store"));
    let signed_in = submit_code(&config, &pending, &next_code(&config, &secret)).await.cookie("ts_session").unwrap();
    let cookie = format!("ts_session={signed_in}");
    assert_eq!(get(&config, "/account", Some(&cookie)).await.header("cache-control"), Some("no-store"));
    let fresh = post(&config, "/account/mfa/recovery", &format!("token={}&code={}", form_token(&config, &root), next_code(&config, &secret)), Some(&cookie)).await;
    assert_eq!(fresh.status, StatusCode::OK);
    assert_eq!(fresh.header("cache-control"), Some("no-store"));
}

/// Turning it off takes the page's form token and a code, and the page that
/// holds the token cannot be read by a script.
#[tokio::test]
async fn turning_it_off_needs_the_form_token_and_a_code() {
    let (_dir, config) = site(Policy::Off);
    let de = person(&config, "de@example.com", false);
    let s = session(&config, "de@example.com");
    let (secret, _) = enable(&config, &de, &s).await;
    let cookie = format!("ts_session={s}");
    let token = form_token(&config, &de);

    let code = next_code(&config, &secret);
    let forged = post(&config, "/account/mfa/off", &format!("code={code}"), Some(&cookie)).await;
    assert_ne!(forged.status, StatusCode::SEE_OTHER);
    let forged = post(&config, "/account/mfa/off", &format!("token=nope&code={code}"), Some(&cookie)).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let no_code = post(&config, "/account/mfa/off", &format!("token={token}&code="), Some(&cookie)).await;
    assert_eq!(no_code.status, StatusCode::SEE_OTHER);
    assert!(mfa::is_enabled(&config, &de.id), "turned off without a code");

    let read = send(
        &config,
        Request::builder()
            .uri("/account")
            .header("cookie", &cookie)
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(read.status, StatusCode::FORBIDDEN, "a script read the account page and its form token");
    assert!(!read.body.contains(&token));
}

/// Only a site admin resets someone's two-step sign-in. Manage on an app or
/// a project is not enough, and one admin resetting another is allowed and
/// logged.
#[tokio::test]
async fn only_a_site_admin_may_reset_it_and_a_reset_is_logged_and_revokes_clients() {
    let (_dir, config) = site(Policy::Off);
    let target = person(&config, "ef@example.com", true);
    enable(&config, &target, &session(&config, "ef@example.com")).await;
    let client = toolsite::platform::oauth_store::register_client(&config, Some("Test"), &["https://client.test/cb".to_string()]).unwrap();
    let issued = toolsite::platform::oauth_store::issue_tokens(&config, &client.id, &target.id, None).unwrap();

    // Manage over the whole tree is the most a non-admin can hold.
    let manager = person(&config, "gh@example.com", false);
    users::grant_scope(&config, "gh@example.com", "", users::Scope::Admin, None).unwrap();
    let cookie = format!("ts_session={}", session(&config, "gh@example.com"));
    let reply = post(
        &config,
        "/admin/mfa-reset",
        &format!("token={}&email=ef%40example.com", form_token(&config, &manager)),
        Some(&cookie),
    )
    .await;
    assert_ne!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(mfa::is_enabled(&config, &target.id), "a non-admin reset an admin's two-step sign-in");

    let root = person(&config, "root@example.com", true);
    let cookie = format!("ts_session={}", session(&config, "root@example.com"));
    let reply = post(
        &config,
        "/admin/mfa-reset",
        &format!("token={}&email=ef%40example.com", form_token(&config, &root)),
        Some(&cookie),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(!mfa::is_enabled(&config, &target.id));
    assert!(toolsite::platform::oauth_store::access_token_holder(&config, &issued.access_token).is_none(), "a client outlived the reset");
    assert!(toolsite::platform::oauth_store::rotate_refresh(&config, &client.id, &issued.refresh_token).is_none());
    let text = String::from_utf8_lossy(&log().0.lock().unwrap()).to_string();
    assert!(
        text.lines().any(|l| l.contains("WARN") && l.contains("reset by an admin") && l.contains("root@example.com") && l.contains("ef@example.com")),
        "the reset was not logged with both accounts"
    );
}

#[tokio::test]
async fn a_new_password_revokes_connected_clients() {
    let (_dir, config) = site(Policy::Off);
    let ij = person(&config, "ij@example.com", false);
    let s = session(&config, "ij@example.com");
    let client = toolsite::platform::oauth_store::register_client(&config, Some("Test"), &["https://client.test/cb".to_string()]).unwrap();
    let issued = toolsite::platform::oauth_store::issue_tokens(&config, &client.id, &ij.id, None).unwrap();
    let new = urlencoding::encode("a brand new password");
    let reply = post(
        &config,
        "/account/password",
        &format!("token={}&current={}&new={new}&confirm={new}", form_token(&config, &ij), urlencoding::encode(PASSWORD)),
        Some(&format!("ts_session={s}")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(toolsite::platform::oauth_store::access_token_holder(&config, &issued.access_token).is_none(), "a client outlived the new password");
}

/// An admin's session from before the policy (or before the deployment set
/// it) keeps its own account page, where it can set two-step sign-in up,
/// and nothing an admin page or a new MCP client could do with it.
#[tokio::test]
async fn an_admin_session_that_owes_setup_reaches_only_its_account_page() {
    let (_dir, config) = site_with(Policy::Admins, false, Some("https://site.test"));
    let root = person(&config, "root@example.com", true);
    let s = session(&config, "root@example.com");
    let cookie = format!("ts_session={s}");
    for path in ["/admin", "/admin/accounts", "/admin/accounts/root%40example.com"] {
        let reply = get(&config, path, Some(&cookie)).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{path} opened");
        assert_eq!(reply.location(), "/account#two-step", "{path}");
    }
    let made = post(&config, "/admin/mfa-reset", &format!("token={}&email=root%40example.com", form_token(&config, &root)), Some(&cookie)).await;
    assert_eq!(made.location(), "/account#two-step");
    assert_eq!(get(&config, "/account", Some(&cookie)).await.status, StatusCode::OK);

    let client = toolsite::platform::oauth_store::register_client(&config, Some("Test"), &["https://client.test/cb".to_string()]).unwrap();
    let authorize = format!(
        "/authorize?response_type=code&client_id={}&redirect_uri={}&state=xyz&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256",
        client.id,
        urlencoding::encode("https://client.test/cb")
    );
    let consent = get(&config, &authorize, Some(&cookie)).await;
    assert_eq!(consent.status, StatusCode::FORBIDDEN, "a new client was offered to a session that owes setup");

    enable(&config, &root, &s).await;
    assert_eq!(get(&config, "/admin", Some(&cookie)).await.status, StatusCode::OK);
    assert_eq!(get(&config, &authorize, Some(&cookie)).await.status, StatusCode::OK);

    // A regular account under `admins` owes nothing.
    person(&config, "kl@example.com", false);
    let page = get(&config, "/account", Some(&format!("ts_session={}", session(&config, "kl@example.com")))).await;
    assert_eq!(page.status, StatusCode::OK);
}

/// By design a provider's own second step stands in for toolsite's unless
/// `TOOLSITE_MFA_FOR_PROVIDERS` is set, so under `admins` a provider-only
/// admin is let in without one, and with the setting is held for setup.
#[tokio::test]
async fn a_provider_admin_is_held_for_setup_only_with_mfa_for_providers() {
    for for_providers in [false, true] {
        let (_dir, config) = site_with(Policy::Admins, for_providers, None);
        // No route promotes an account, so the admin flag is set on the
        // value the sign-in would carry.
        let op = users::User { is_admin: true, ..users::create_provider_account(&config, "op@example.com").unwrap() };
        let step = mfa::after_primary(&config, &op, Primary::Provider, "/").unwrap();
        assert_eq!(matches!(step, Step::Setup(_)), for_providers, "for_providers={for_providers}: {step:?}");
        assert_eq!(mfa::owes_setup(&config, &op), for_providers);
    }
}

#[tokio::test]
async fn nothing_another_admin_sees_carries_the_secret() {
    let (_dir, config) = site(Policy::Off);
    person(&config, "root@example.com", true);
    let mn = person(&config, "mn@example.com", false);
    let theirs = format!("ts_session={}", session(&config, "mn@example.com"));
    post(&config, "/account/mfa/start", &format!("token={}", form_token(&config, &mn)), Some(&theirs)).await;
    let secret = secret_on(&get(&config, "/account", Some(&theirs)).await.body);
    let admin = format!("ts_session={}", session(&config, "root@example.com"));
    for path in ["/admin/accounts", "/admin/accounts/mn%40example.com", "/account", "/admin"] {
        let page = get(&config, path, Some(&admin)).await;
        assert!(!page.body.contains(&secret) && !page.body.contains(&mfa::grouped(&secret)), "{path} shows the secret");
    }
}

/// The account database lives under `.site/`, which no app name, export or
/// served path can name.
#[tokio::test]
async fn no_serving_route_reaches_the_account_database() {
    let (_dir, config) = site(Policy::Off);
    let op = person(&config, "op@example.com", false);
    enable(&config, &op, &session(&config, "op@example.com")).await;
    for path in [
        "/export/.site.sqlite",
        "/export/..%2F.site%2Fauth.sqlite",
        "/p/.site/auth.db",
        "/.site/auth.db",
        "/p/%2E%2E/.site/auth.db",
        "/p/x/..%2F..%2F.site%2Fauth.db",
    ] {
        let reply = get(&config, path, None).await;
        assert!(!reply.status.is_success(), "{path} answered {}", reply.status);
        assert!(!reply.body.contains("SQLite format"), "{path} served a database");
    }
}

#[test]
fn a_deployment_checks_codes_against_the_system_clock() {
    let settings = Settings::from_env(Some("admins"), None).unwrap();
    let system = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(settings.clock.now().abs_diff(system) < 5);
    // And a fixed clock's setter does nothing to it.
    settings.clock.set(1);
    assert!(settings.clock.now().abs_diff(system) < 5);
}
