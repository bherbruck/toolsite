//! Ending an account's MCP clients: what a new password, two-step sign-in
//! turned on, an admin's reset, a setup link and turning the account off
//! each do, and what must hold when one of them meets a sign-in code being
//! exchanged at the same moment. Whichever lands first, the client must end
//! up with no live token: the revocation either finds the code and spends
//! it, or finds the tokens and deletes them. Issue #14 was the case between,
//! a code spent and its tokens not yet written, where a revocation found
//! neither and the tokens issued after it lived on.
//!
//! The race starts the revocation at a sweep of delays after the exchange,
//! so some land in each part of it. Files here; Postgres in the
//! `_on_postgres` twin, with the exchange and the revocation on different
//! runners.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::{sync::Arc, time::Duration};
use toolsite::{
    accounts::users::{self, User},
    build_router,
    platform::oauth_store,
    runtime::wasm::Runtime,
    Config,
};
use tower::ServiceExt;

mod common;

const BASE: &str = "https://site.test";
const CALLBACK: &str = "https://client.test/callback";
// RFC 7636's own test vector.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const ROUNDS: u64 = 120;

async fn send(config: &Arc<Config>, uri: &str, body: String, headers: &[(&str, String)]) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "localhost")
        .header("content-type", "application/x-www-form-urlencoded");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = build_router(config.clone(), Runtime::new().unwrap())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn exchange(config: &Arc<Config>, client: &str, code: &str) -> Option<String> {
    let body = format!(
        "grant_type=authorization_code&client_id={client}&code={code}&redirect_uri={}&code_verifier={VERIFIER}",
        urlencoding::encode(CALLBACK)
    );
    let (status, body) = send(config, "/token", body, &[]).await;
    (status == StatusCode::OK).then(|| {
        let tokens: serde_json::Value = serde_json::from_str(&body).unwrap();
        tokens["access_token"].as_str().unwrap().to_string()
    })
}

/// Races one exchange on `exchanger` against one revocation on `revoker`,
/// round after round, and fails on any round that leaves a live token.
async fn race(exchanger: Arc<Config>, revoker: Arc<Config>) {
    let config = exchanger.clone();
    let (user, client) = tokio::task::spawn_blocking(move || {
        let user = users::sign_up_as(&config, "ann@example.com", "correct horse battery", true).unwrap();
        let client = oauth_store::register_client(&config, Some("t"), &[CALLBACK.to_string()]).unwrap();
        (user.id, client.id)
    })
    .await
    .unwrap();
    let mut issued = 0;
    for round in 0..ROUNDS {
        let (config, user_id, client_id) = (exchanger.clone(), user.clone(), client.clone());
        let code = tokio::task::spawn_blocking(move || {
            let resource = format!("{BASE}/mcp");
            oauth_store::issue_code(
                &config,
                &oauth_store::Grant {
                    client_id: &client_id,
                    user_id: &user_id,
                    redirect_uri: CALLBACK,
                    code_challenge: CHALLENGE,
                    resource: Some(&resource),
                },
            )
            .unwrap()
        })
        .await
        .unwrap();
        let revocation = {
            let (config, user_id) = (revoker.clone(), user.clone());
            tokio::task::spawn_blocking(move || {
                // Spread across the exchange: from before it reaches the
                // store to after it has answered.
                std::thread::sleep(Duration::from_micros(round * 40));
                oauth_store::revoke_for_user(&config, &user_id).unwrap();
            })
        };
        let access = exchange(&exchanger, &client, &code).await;
        revocation.await.unwrap();
        if let Some(access) = access {
            issued += 1;
            let (config, held) = (exchanger.clone(), access.clone());
            let live = tokio::task::spawn_blocking(move || oauth_store::access_token_holder(&config, &held)).await.unwrap();
            assert!(live.is_none(), "round {round}: a token exchanged during a revocation outlived it");
        }
    }
    // Both orders were tried, or the sweep proved nothing.
    assert!(issued > 0 && issued < ROUNDS, "every round went one way ({issued} of {ROUNDS} issued)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_exchanged_during_a_revocation_never_leaves_a_live_token() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config { base_url: Some(BASE.to_string()), ..Config::local(dir.path().to_path_buf(), "t") });
    race(config.clone(), config).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_code_exchanged_during_a_revocation_never_leaves_a_live_token_on_postgres() {
    let dir = tempfile::tempdir().unwrap();
    let database = common::Database::new().await;
    let runner = || {
        Arc::new(Config {
            base_url: Some(BASE.to_string()),
            stores: database.stores(),
            ..Config::local(dir.path().to_path_buf(), "t")
        })
    };
    race(runner(), runner()).await;
    database.drop().await;
}

/// A site in file mode, with an admin and a person who has connected a
/// client: the person's access token.
fn connected() -> (tempfile::TempDir, Arc<Config>, User, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config { base_url: Some(BASE.to_string()), ..Config::local(dir.path().to_path_buf(), "t") });
    let admin = users::sign_up_as(&config, "boss@example.com", "correct horse battery", true).unwrap();
    let (_, boss) = users::log_in(&config, "boss@example.com", "correct horse battery").unwrap();
    let person = users::sign_up(&config, "bo@example.com", "correct horse battery").unwrap();
    let client = oauth_store::register_client(&config, Some("t"), &[CALLBACK.to_string()]).unwrap();
    let access = oauth_store::issue_tokens(&config, &client.id, &person.id, None).unwrap().access_token;
    assert!(oauth_store::access_token_holder(&config, &access).is_some());
    (dir, config, admin, boss, access)
}

fn live(config: &Config, access: &str) -> bool {
    oauth_store::access_token_holder(config, access).is_some()
}

/// The bearer check refuses a disabled account's token, so it stops working
/// at once; but if the token were only refused, turning the account back on
/// would bring every client connected before back with it. Sessions are
/// ended when an account is turned off, and so are its clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_an_account_off_ends_its_clients_so_turning_it_on_does_not_revive_them() {
    let (_dir, config, admin, boss, access) = tokio::task::spawn_blocking(connected).await.unwrap();
    let form = users::derive_form_token(&config, &admin.id);
    let cookie = [("cookie", format!("ts_session={boss}"))];
    for active in ["0", "1"] {
        let (status, body) =
            send(&config, "/admin/active", format!("token={form}&email=bo%40example.com&active={active}"), &cookie).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    }
    let (config2, held) = (config.clone(), access.clone());
    assert!(!tokio::task::spawn_blocking(move || live(&config2, &held)).await.unwrap(), "a client outlived its account being turned off");

    // The same through the publishing connector's tool.
    let (config2, person) = (config.clone(), users::user_by_email(&config, "bo@example.com").unwrap());
    let access = tokio::task::spawn_blocking(move || {
        let client = oauth_store::register_client(&config2, Some("t"), &[CALLBACK.to_string()]).unwrap();
        oauth_store::issue_tokens(&config2, &client.id, &person.id, None).unwrap().access_token
    })
    .await
    .unwrap();
    for active in [false, true] {
        let call = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"set_user_active","arguments":{"email":"bo@example.com","active":active}}});
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("authorization", "Bearer t")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(Body::from(call.to_string()))
            .unwrap();
        let response = build_router(config.clone(), Runtime::new().unwrap()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("\"isError\":true"), "{}", String::from_utf8_lossy(&bytes));
    }
    let (config2, held) = (config.clone(), access.clone());
    assert!(!tokio::task::spawn_blocking(move || live(&config2, &held)).await.unwrap(), "a client outlived set_user_active");
}

/// A setup link is the reset for a forgotten or leaked password, and it
/// ends every session; a client the old password connected is one of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_setup_link_ends_the_clients_the_old_password_connected() {
    let (_dir, config, _admin, _boss, access) = tokio::task::spawn_blocking(connected).await.unwrap();
    let config2 = config.clone();
    let link = tokio::task::spawn_blocking(move || users::reinvite(&config2, "bo@example.com").unwrap()).await.unwrap();
    let (status, body) = send(&config, "/auth/setup", format!("token={link}&password=a+new+long+password"), &[]).await;
    assert!(status.is_redirection() || status == StatusCode::OK, "{status} {body}");
    let (config2, held) = (config.clone(), access.clone());
    assert!(!tokio::task::spawn_blocking(move || live(&config2, &held)).await.unwrap(), "a client outlived a password reset by link");
}
