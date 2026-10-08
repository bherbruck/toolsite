//! Accounts on Postgres, through the same functions the routes call: the
//! session rules, two-step sign-in under concurrency, what is at rest, and
//! the `toolsite user` commands. Every test is ignored unless asked for:
//! `scripts/test-postgres.sh` starts a database and sets
//! TOOLSITE_TEST_DATABASE_URL. Each test works in a database of its own.
//!
//! Account functions block their thread, as they do in the server (where
//! they run in `spawn_blocking`), so these are plain tests that wait on one
//! runtime kept for the binary.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, LazyLock,
};
use toolsite::{
    accounts::{
        mfa::{self, Clock, Policy, Primary, Settings, Step},
        users,
    },
    state::{self, Backend, Stores},
    Config,
};

const NEEDS: &str = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one";
const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const PASSWORD: &str = "correct horse battery";
/// Mid-step, so one step either side is plainly 30 seconds away.
const T0: u64 = 1_800_000_015;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    // Sealing reads the key from the environment, as on a real Postgres site.
    // Set once, before any thread of this binary reads it.
    unsafe { std::env::set_var("TOOLSITE_SECRET_KEY", KEY) };
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    state::keep_runtime(runtime.handle().clone());
    runtime
});

struct Site {
    config: Arc<Config>,
    url: String,
    name: String,
    _dir: tempfile::TempDir,
}

impl Drop for Site {
    fn drop(&mut self) {
        let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").unwrap();
        if let Backend::Postgres(postgres) = &self.config.stores.backend {
            postgres.pool.close();
        }
        let name = self.name.clone();
        RUNTIME.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
            tokio::spawn(connection);
            client.batch_execute(&format!("drop database if exists {name} with (force)")).await.unwrap();
        });
    }
}

/// A site on a fresh database, booted the way `main` boots one.
fn site(policy: Policy) -> Site {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").expect(NEEDS);
    let name = format!("t_{}", toolsite::content::slug::random_token(12).to_lowercase());
    let dir = tempfile::tempdir().unwrap();
    let (backend, url) = RUNTIME.block_on(async {
        let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client.batch_execute(&format!("create database {name}")).await.unwrap();
        let mut url = url::Url::parse(&server).unwrap();
        url.set_path(&name);
        let settings = state::Settings {
            database_url: Some(url.to_string()),
            secret_key: Some(KEY.into()),
            bucket: true,
            pool_size: 16,
        };
        (state::open(&settings, dir.path()).await.unwrap(), url.to_string())
    });
    let config = Config {
        mfa: Settings { policy, for_providers: false, clock: Clock::fixed(T0) },
        stores: Stores { backend, runner: None },
        ..Config::local(dir.path().to_path_buf(), "test-token")
    };
    Site { config: Arc::new(config), url, name, _dir: dir }
}

/// Every row of every table in schema `accounts`, as text.
fn every_row(site: &Site) -> String {
    let Backend::Postgres(postgres) = &site.config.stores.backend else { unreachable!() };
    RUNTIME.block_on(async {
        let client = postgres.pool.get().await.unwrap();
        let tables = client
            .query("select table_name from information_schema.tables where table_schema = 'accounts'", &[])
            .await
            .unwrap();
        let mut all = String::new();
        for table in tables {
            let table: String = table.get(0);
            for row in client.query(&format!("select t::text from accounts.{table} t"), &[]).await.unwrap() {
                all.push_str(&row.get::<_, String>(0));
                all.push('\n');
            }
        }
        all
    })
}

fn race(n: usize, attempt: impl Fn(usize) -> bool + Sync) -> usize {
    let wins = AtomicUsize::new(0);
    let gate = std::sync::Barrier::new(n);
    std::thread::scope(|scope| {
        for i in 0..n {
            let (wins, gate, attempt) = (&wins, &gate, &attempt);
            scope.spawn(move || {
                gate.wait();
                if attempt(i) {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    wins.into_inner()
}

/// Turns two-step sign-in on for an account from its site session, the way
/// the account page does, and returns the secret and the recovery codes.
fn enable_mfa(config: &Config, user: &users::User, session: &str) -> (String, Vec<String>) {
    let secret = mfa::begin_setup(config, &user.id, session).unwrap();
    let codes = mfa::confirm_setup(config, user, &mfa::code_at(&secret, T0).unwrap(), session).unwrap();
    (secret, codes)
}

#[test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
fn sessions_on_postgres_keep_every_rule_they_keep_on_sqlite() {
    let site = site(Policy::Off);
    let config = &*site.config;
    let ann = users::sign_up(config, "Ann@Example.com", PASSWORD).unwrap();
    let bo = users::sign_up(config, "bo@example.com", PASSWORD).unwrap();
    assert!(users::sign_up(config, " ann@example.com ", PASSWORD).is_err(), "a second account took a used email");
    assert_eq!(
        users::log_in(config, "ann@example.com", "wrong").unwrap_err(),
        users::log_in(config, "nobody@example.com", "wrong").unwrap_err(),
        "the refusal says which half was wrong"
    );

    // Two tiers, one account each: no token answers for anyone else.
    let (_, site_a) = users::log_in(config, "ann@example.com", PASSWORD).unwrap();
    let (_, site_b) = users::log_in(config, "bo@example.com", PASSWORD).unwrap();
    let (_, notes_a, _) = users::create_app_session(config, &site_a, "notes").unwrap();
    assert_eq!(users::site_session_user(config, &site_a), Some(ann.clone()));
    assert_eq!(users::site_session_user(config, &site_b), Some(bo.clone()));
    assert_eq!(users::app_session_user(config, &notes_a, "notes"), Some(ann.clone()));
    assert!(users::app_session_user(config, &notes_a, "ledger").is_none());
    assert!(users::site_session_user(config, &notes_a).is_none());
    assert!(users::app_session_user(config, &site_a, "notes").is_none());
    assert!(users::create_app_session(config, &notes_a, "ledger").is_err(), "an app session minted another");

    // A changed password ends every session but the one that changed it.
    let (_, second_a) = users::log_in(config, "ann@example.com", PASSWORD).unwrap();
    users::change_password(config, &ann.id, PASSWORD, "a new long password", &site_a).unwrap();
    assert_eq!(users::site_session_user(config, &site_a), Some(ann.clone()));
    assert!(users::site_session_user(config, &second_a).is_none());
    assert!(users::app_session_user(config, &notes_a, "notes").is_none());
    assert_eq!(users::site_session_user(config, &site_b), Some(bo.clone()), "another account's session ended");
    assert!(users::log_in(config, "ann@example.com", PASSWORD).is_err());

    // Signing out takes the app sessions; disabling takes everything.
    let (_, notes_a, _) = users::create_app_session(config, &site_a, "notes").unwrap();
    users::log_out(config, &site_a).unwrap();
    assert!(users::app_session_user(config, &notes_a, "notes").is_none());
    users::set_active(config, "bo@example.com", false).unwrap();
    assert!(users::site_session_user(config, &site_b).is_none());
    assert!(users::user_by_id(config, &bo.id).is_none());
    assert!(users::log_in(config, "bo@example.com", PASSWORD).is_err());

    // An invitation is a reset: it works once and ends every session.
    let (_, site_a) = users::log_in(config, "ann@example.com", "a new long password").unwrap();
    let invite = users::reinvite(config, "ann@example.com").unwrap();
    assert!(users::accept_invite(config, &invite, "short").is_err());
    let wins = race(6, |i| users::accept_invite(config, &invite, &format!("racing password {i}")).is_ok());
    assert_eq!(wins, 1, "one setup link set the password {wins} times");
    assert!(users::site_session_user(config, &site_a).is_none());

    // Grants and scopes, through the functions the gates use.
    users::grant(config, "ann@example.com", "ledger", "editor").unwrap();
    assert_eq!(users::role_for(config, &ann.id, "ledger").as_deref(), Some("editor"));
    assert!(users::role_for(config, &bo.id, "ledger").is_none());
    users::grant_scope(config, "ann@example.com", "ops", users::Scope::Editor, None).unwrap();
    assert_eq!(users::effective_scope(config, &ann, "ops/tool", &[]), Some(users::Scope::Editor));
    assert_eq!(users::effective_scope(config, &ann, "opsx", &[]), None);
    assert_eq!(users::move_scope_tree(config, "ops", "labs").unwrap(), 1);
    assert_eq!(users::effective_scope(config, &ann, "labs/tool", &[]), Some(users::Scope::Editor));
}

#[test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
fn two_step_on_postgres_ends_other_sessions_and_keeps_its_secrets_sealed() {
    let site = site(Policy::Off);
    let config = &*site.config;
    let cy = users::sign_up(config, "cy@example.com", PASSWORD).unwrap();
    let (_, here) = users::log_in(config, "cy@example.com", PASSWORD).unwrap();
    let (_, elsewhere) = users::log_in(config, "cy@example.com", PASSWORD).unwrap();
    let (_, app, _) = users::create_app_session(config, &elsewhere, "notes").unwrap();

    let (secret, codes) = enable_mfa(config, &cy, &here);
    assert_eq!(codes.len(), 10);
    assert!(mfa::is_enabled(config, &cy.id));
    assert_eq!(users::site_session_user(config, &here), Some(cy.clone()), "the session that turned it on ended");
    assert!(users::site_session_user(config, &elsewhere).is_none(), "a session from before two-step survived");
    assert!(users::app_session_user(config, &app, "notes").is_none());

    // Nothing at rest opens the account: no secret, no code, no token.
    let rows = every_row(&site);
    let raw = data_encoding::BASE32_NOPAD.decode(secret.as_bytes()).unwrap();
    assert!(!rows.contains(&secret), "the secret is stored in the clear");
    assert!(!rows.contains(&data_encoding::HEXLOWER.encode(&raw)), "the secret's bytes are stored in the clear");
    for code in &codes {
        assert!(!rows.contains(code.as_str()) && !rows.contains(&code.replace('-', "")), "a recovery code is stored in the clear");
    }
    assert!(!rows.contains(&here), "a session token is stored in the clear");
    assert!(!rows.contains(KEY), "the site key is in the database");

    // A password sign-in now owes a code.
    let Step::Code(pending) = mfa::after_primary(config, &cy, Primary::Password, "/").unwrap() else { panic!("no code asked") };
    let done = mfa::finish_with_code(config, &pending, &mfa::code_at(&secret, T0 + 30).unwrap()).unwrap();
    assert_eq!(done.user, cy);

    // Turning it off with a code removes the secret and the codes.
    mfa::turn_off(config, &cy, &codes[0]).unwrap();
    assert!(!mfa::is_enabled(config, &cy.id));
    assert!(matches!(mfa::after_primary(config, &cy, Primary::Password, "/").unwrap(), Step::Session(_)));

    // An admin's reset ends every session of the account.
    let (_, still) = users::log_in(config, "cy@example.com", PASSWORD).unwrap();
    enable_mfa(config, &cy, &still);
    let (id, was_on) = mfa::reset(config, "cy@example.com").unwrap();
    assert_eq!((id.as_str(), was_on), (cy.id.as_str(), true));
    assert!(users::site_session_user(config, &still).is_none());
    assert!(users::site_session_user(config, &done.session).is_none());
}

#[test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
fn one_totp_code_or_recovery_code_sent_at_once_signs_in_once_on_postgres() {
    const AT_ONCE: usize = 8;
    let site = site(Policy::Off);
    let config = &*site.config;
    let di = users::sign_up(config, "di@example.com", PASSWORD).unwrap();
    let (_, session) = users::log_in(config, "di@example.com", PASSWORD).unwrap();
    let (secret, codes) = enable_mfa(config, &di, &session);

    let pendings = |n: usize| -> Vec<String> {
        (0..n)
            .map(|_| match mfa::after_primary(config, &di, Primary::Password, "/").unwrap() {
                Step::Code(token) => token,
                other => panic!("no code asked: {other:?}"),
            })
            .collect()
    };

    // The step after the one setup spent, sent from eight sign-ins at once.
    let code = mfa::code_at(&secret, T0 + 30).unwrap();
    let waiting = pendings(AT_ONCE);
    let wins = race(AT_ONCE, |i| mfa::finish_with_code(config, &waiting[i], &code).is_ok());
    assert_eq!(wins, 1, "one TOTP code signed in {wins} times");

    let waiting = pendings(AT_ONCE);
    let wins = race(AT_ONCE, |i| mfa::finish_with_code(config, &waiting[i], &codes[3]).is_ok());
    assert_eq!(wins, 1, "one recovery code signed in {wins} times");
    assert_eq!(mfa::status(config, &di.id).recovery_left, 9);

    // And the same code again, later, from a fresh sign-in, is refused.
    let again = pendings(1);
    assert!(mfa::finish_with_code(config, &again[0], &codes[3]).is_err());
}

#[test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
fn the_user_commands_work_on_postgres() {
    let site = site(Policy::Off);
    let dir = tempfile::tempdir().unwrap();
    let toolsite = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_toolsite"))
            .args(args)
            .env("DATABASE_URL", &site.url)
            .env("TOOLSITE_SECRET_KEY", KEY)
            .env("TOOLSITE_BLOB_S3_ENDPOINT", "http://127.0.0.1:1")
            .env("TOOLSITE_BLOB_S3_BUCKET", "unused")
            .env("TOOLSITE_DATA_DIR", dir.path())
            .env_remove("TOOLSITE_DATABASE_URL")
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "toolsite {args:?} failed: {text}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    assert!(toolsite(&["user", "add", "eve@example.com", "--admin", "--password", PASSWORD]).contains("as an admin"));
    let link = toolsite(&["user", "add", "fay@example.com"]);
    assert!(link.contains("/auth/setup?token="), "{link}");
    assert!(toolsite(&["user", "invite", "fay@example.com"]).contains("/auth/setup?token="));
    toolsite(&["user", "disable", "fay@example.com"]);
    let listed = toolsite(&["user", "list"]);
    assert!(listed.contains("eve@example.com") && listed.contains("admin"), "{listed}");
    assert!(listed.lines().any(|l| l.contains("fay@example.com") && l.contains("disabled")), "{listed}");
    toolsite(&["user", "enable", "fay@example.com"]);
    assert!(toolsite(&["user", "reset-mfa", "eve@example.com"]).contains("two-step sign-in was off"));

    // The server's own functions see what the command wrote, and nothing
    // was written to an account file beside it.
    let eve = users::log_in(&site.config, "eve@example.com", PASSWORD).unwrap().0;
    assert!(eve.is_admin);
    assert!(users::user_by_email(&site.config, "fay@example.com").is_some());
    assert!(!dir.path().join(".site/auth.db").exists(), "the command wrote accounts to a file on Postgres");
}
