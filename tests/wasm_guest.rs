//! Runs a real component through the host, using the fixture built by
//! scripts/build-fixtures.sh. These are the tests that would catch a WIT
//! change breaking every already-published handler.

use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
use toolsite::{
    runtime::wasm::{Guards, Runtime},
    Config,
};

const HANDLER: &[u8] = include_bytes!("fixtures/handler.wasm");

fn site() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), "test-token"));
    (dir, config)
}

fn request(path: &str) -> toolsite::runtime::wasm::Request {
    toolsite::runtime::wasm::Request {
        method: "GET".to_string(),
        path: path.to_string(),
        query: String::new(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn call(
    runtime: &Runtime,
    site: &Arc<Config>,
    app: &str,
    request: toolsite::runtime::wasm::Request,
) -> (u16, String) {
    call_as(runtime, site, app, request, None, Guards::default())
}

fn call_as(
    runtime: &Runtime,
    site: &Arc<Config>,
    app: &str,
    request: toolsite::runtime::wasm::Request,
    user: Option<toolsite::runtime::wasm::User>,
    guards: Guards,
) -> (u16, String) {
    let response = runtime
        .handle(site.clone(), app, HANDLER, user, request, guards)
        .unwrap();
    (
        response.status,
        String::from_utf8_lossy(&response.body).to_string(),
    )
}

#[test]
fn a_component_handles_a_request() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    let mut req = request("/echo");
    req.query = "a=1".to_string();
    assert_eq!(call(&runtime, &site, "app", req), (200, "GET /echo?a=1".into()));

    assert_eq!(call(&runtime, &site, "app", request("/nope")).0, 404);
}

#[test]
fn a_guest_can_read_and_write_its_own_database() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    // Each call is a fresh store, so a rising count proves the data outlived
    // the instance that wrote it.
    assert_eq!(call(&runtime, &site, "app", request("/count")), (200, "1".into()));
    assert_eq!(call(&runtime, &site, "app", request("/count")), (200, "2".into()));
    assert_eq!(call(&runtime, &site, "app", request("/count")), (200, "3".into()));
}

#[test]
fn each_app_sees_only_its_own_data() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    call(&runtime, &site, "one", request("/count"));
    call(&runtime, &site, "one", request("/count"));
    // A different app starts from nothing despite running identical code.
    assert_eq!(call(&runtime, &site, "two", request("/count")), (200, "1".into()));
}

#[test]
fn a_guest_cannot_attach_a_sibling_apps_database() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();
    call(&runtime, &site, "victim", request("/count"));

    let (status, body) = call(&runtime, &site, "attacker", request("/escape"));
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("not authorized"), "{body}");
}

#[test]
fn parameters_from_a_guest_are_bound_not_interpolated() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    let payload = "'); drop table hits; --";
    let mut req = request("/echo-param");
    req.body = payload.as_bytes().to_vec();
    assert_eq!(call(&runtime, &site, "app", req), (200, payload.into()));
}

#[test]
fn identity_comes_from_the_host_not_the_guest() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    assert_eq!(call(&runtime, &site, "app", request("/whoami")).0, 401);

    let user = toolsite::runtime::wasm::User {
        id: "u1".to_string(),
        email: "someone@example.com".to_string(),
    };
    let (status, body) = call_as(
        &runtime,
        &site,
        "app",
        request("/whoami"),
        Some(user),
        Guards::default(),
    );
    assert_eq!((status, body), (200, "u1:someone@example.com".into()));
}

#[test]
fn a_runaway_handler_is_killed_by_its_guards() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    let guards = Guards {
        fuel: 5_000_000,
        ..Guards::default()
    };
    let error = runtime
        .handle(site.clone(), "app", HANDLER, None, request("/spin"), guards)
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::OutOfFuel),
        "expected fuel exhaustion, got {error:?}"
    );

    // The runtime survives a guest that died, and still serves the next call.
    assert_eq!(call(&runtime, &site, "app", request("/echo")).0, 200);
}

#[test]
fn a_wall_clock_deadline_stops_a_spinning_handler() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    let guards = Guards {
        fuel: u64::MAX,
        wall_clock: Duration::from_millis(200),
        ..Guards::default()
    };
    let error = runtime
        .handle(site.clone(), "app", HANDLER, None, request("/spin"), guards)
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::Interrupt),
        "expected an epoch deadline, got {error:?}"
    );
}

/// wasi is linked because a wasm32-wasip2 guest imports it through std. The
/// sandbox is therefore the *context*, which grants nothing — so prove that
/// rather than trusting it.
#[test]
fn a_guest_has_no_filesystem_no_environment_and_no_sockets() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    let (status, body) = call(&runtime, &site, "app", request("/read-file"));
    assert_eq!(status, 403, "guest read a host file: {body}");

    let (status, body) = call(&runtime, &site, "app", request("/list-root"));
    assert_eq!(status, 403, "guest listed the host filesystem: {body}");

    let (status, body) = call(&runtime, &site, "app", request("/connect"));
    assert_eq!(status, 403, "guest opened a socket: {body}");

    // An inherited environment would hand the guest BEARER_TOKEN.
    let (status, body) = call(&runtime, &site, "app", request("/env"));
    assert_eq!(status, 200);
    assert!(
        body.starts_with("0:"),
        "guest saw host environment variables: {body}"
    );
}

/// The platform's account database sits under `.site/`, and app databases are
/// resolved from a validated slug. This asserts a guest cannot reach across
/// that line — by naming it, by attaching it, or by querying its tables.
#[test]
fn a_guest_cannot_reach_the_platforms_account_database() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    // Put a real account in the platform database.
    toolsite::accounts::users::sign_up(&site, "someone@example.com", "correct horse battery")
        .unwrap();
    assert!(site.data_dir.join(".site/auth.db").exists());

    // 1. An app cannot be named after the platform directory: a slug may not
    //    contain a dot, so `.site` never resolves to a path.
    assert!(toolsite::runtime::db::db_path(&site, ".site").is_none());

    // 2. Its own database has no idea those tables exist.
    let (status, body) = call(&runtime, &site, "app", request("/api/read-users"));
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("no such table"), "{body}");

    // 3. ATTACH is refused, so it cannot pull the file in sideways.
    let (status, body) = call(&runtime, &site, "app", request("/api/steal-auth"));
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("not authorized"), "{body}");
}

/// A handler is the only thing that can read an app's settings — that is the
/// point of storing them outside the bundle.
#[test]
fn a_handler_reads_its_own_settings_and_no_one_elses() {
    let runtime = Runtime::new().unwrap();
    let (_dir, site) = site();

    assert_eq!(call(&runtime, &site, "app", request("/api/secret")).0, 404);

    toolsite::platform::secrets::set(&site, "app", "API_KEY", Some("hunter2")).unwrap();
    assert_eq!(
        call(&runtime, &site, "app", request("/api/secret")),
        (200, "key=hunter2".into())
    );

    // Same code, different app: nothing.
    assert_eq!(call(&runtime, &site, "other", request("/api/secret")).0, 404);
    assert_eq!(
        call(&runtime, &site, "other", request("/api/secret-names")),
        (200, String::new())
    );
}

/// A scheduled run is the same call a request makes, so a job is just a route
/// the app already has. This runs one and checks it left a mark.
#[tokio::test]
async fn a_scheduled_job_runs_the_apps_handler_and_records_what_happened() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), "test-token"));
    let state = toolsite::AppState {
        config: config.clone(),
        runtime: Runtime::new().unwrap(),
    };
    std::fs::create_dir_all(config.data_dir.join("app")).unwrap();
    std::fs::write(config.data_dir.join("app/handler.wasm"), HANDLER).unwrap();

    // /api/count writes to the app's database and returns how many times.
    toolsite::platform::schedule::set_job(&config, "app", "tick", "0 * * * * *", "/api/count")
        .unwrap();

    let status = toolsite::platform::schedule::run_job(&state, "app", "tick")
        .await
        .unwrap();
    assert_eq!(status, "200");

    // The work actually happened: a second run sees the first one's row.
    toolsite::platform::schedule::run_job(&state, "app", "tick").await.unwrap();
    let jobs = toolsite::platform::schedule::read_jobs(&config, "app");
    let job = jobs.get("tick").unwrap();
    assert_eq!(job.last_status.as_deref(), Some("200"));
    assert!(job.last_run.is_some(), "the run was not recorded");

    let counted = toolsite::runtime::db::run(&config, "app", "select count(*) from hits", &[])
        .unwrap();
    assert_eq!(counted.rows[0][0], serde_json::json!(2));
}

#[tokio::test]
async fn a_job_for_an_app_with_no_handler_says_so_rather_than_failing_silently() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), "test-token"));
    let state = toolsite::AppState {
        config: config.clone(),
        runtime: Runtime::new().unwrap(),
    };
    toolsite::platform::schedule::set_job(&config, "static", "tick", "0 * * * * *", "/api/x")
        .unwrap();

    let error = toolsite::platform::schedule::run_job(&state, "static", "tick")
        .await
        .unwrap_err();
    assert!(error.contains("no handler"), "got {error}");
}
