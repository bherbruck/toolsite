//! One suite every account store must pass, run on SQLite always and on
//! Postgres when `TOOLSITE_TEST_DATABASE_URL` names a server
//! (`scripts/test-postgres.sh` starts one). The rules above the store have
//! their own tests; these pin what the rules rely on: rows come back as
//! they went in, one account's rows never answer for another's, and every
//! single-use write lets exactly one of many concurrent callers through.

use super::{postgres::PostgresAccounts, sqlite::SqliteAccounts, AccountStore};
use crate::{accounts::users::User, config::Config, state};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    LazyLock,
};

const NOW: i64 = 1_800_000_000;

fn person(store: &dyn AccountStore, email: &str) -> User {
    let user = User { id: crate::content::slug::random_token(16), email: email.into(), is_admin: false };
    assert!(store.insert_user(&user, Some("$argon2id$stand-in"), NOW).unwrap(), "{email} was taken");
    user
}

/// Runs `attempt` on `n` threads at once and counts the ones answered
/// `true`.
fn race(n: usize, attempt: impl Fn() -> bool + Sync) -> usize {
    let wins = AtomicUsize::new(0);
    let gate = std::sync::Barrier::new(n);
    std::thread::scope(|scope| {
        for _ in 0..n {
            scope.spawn(|| {
                gate.wait();
                if attempt() {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    wins.into_inner()
}

pub(crate) fn run(store: &dyn AccountStore) {
    people_come_back_as_they_went_in(store);
    a_disabled_account_answers_nowhere(store);
    hostile_text_is_data(store);
    sessions_answer_only_for_their_own_scope_and_account(store);
    an_invitation_is_spent_once(store);
    grants_and_scopes_stay_with_their_account_and_path(store);
    two_step_rows_belong_to_one_account(store);
    single_use_writes_let_exactly_one_caller_through(store);
}

fn people_come_back_as_they_went_in(store: &dyn AccountStore) {
    let ann = person(store, "ann@example.com");
    let twin = User { id: crate::content::slug::random_token(16), email: "ann@example.com".into(), is_admin: true };
    assert!(!store.insert_user(&twin, None, NOW).unwrap(), "a second account took a used email");

    assert_eq!(store.user_by_id(&ann.id).unwrap(), Some(ann.clone()));
    assert_eq!(store.user_by_email("ann@example.com").unwrap(), Some(ann.clone()));
    assert_eq!(store.account_by_email("ann@example.com").unwrap(), Some((ann.clone(), None)));
    assert_eq!(store.credentials("ann@example.com").unwrap(), Some((ann.clone(), Some("$argon2id$stand-in".into()))));
    assert_eq!(store.password_hash(&ann.id).unwrap(), Some(Some("$argon2id$stand-in".into())));
    assert!(store.has_password(&ann.id).unwrap());
    assert_eq!(store.is_active(&ann.id).unwrap(), Some(true));
    assert_eq!(store.is_active("nobody").unwrap(), None);
    assert_eq!(store.user_by_email("ANN@example.com").unwrap(), None, "the store matched an email it was not given");

    let root = User { id: crate::content::slug::random_token(16), email: "root@example.com".into(), is_admin: true };
    assert!(store.insert_user(&root, None, NOW).unwrap());
    assert_eq!(store.user_by_id(&root.id).unwrap(), Some(root.clone()));
    assert!(!store.has_password(&root.id).unwrap());
    assert_eq!(store.password_hash(&root.id).unwrap(), Some(None));

    store.set_password_hash(&root.id, "$argon2id$new").unwrap();
    assert_eq!(store.password_hash(&root.id).unwrap(), Some(Some("$argon2id$new".into())));
    assert_eq!(store.password_hash(&ann.id).unwrap(), Some(Some("$argon2id$stand-in".into())), "a change reached another account");

    store.link_identity("github", "42", &ann.id).unwrap();
    store.link_identity("google", "x", &ann.id).unwrap();
    assert_eq!(store.user_by_identity("github", "42").unwrap(), Some(ann.clone()));
    assert_eq!(store.user_by_identity("github", "43").unwrap(), None);
    assert_eq!(store.identities_for(&ann.id).unwrap(), vec!["github".to_string(), "google".to_string()]);
    store.link_identity("github", "42", &root.id).unwrap();
    assert_eq!(store.user_by_identity("github", "42").unwrap(), Some(root.clone()), "a link was not replaced");

    let listed = store.list_users().unwrap();
    let emails: Vec<&str> = listed.iter().map(|row| row.email.as_str()).collect();
    assert!(emails.windows(2).all(|w| w[0] <= w[1]), "not ordered by email: {emails:?}");
    let ann_row = listed.iter().find(|row| row.email == "ann@example.com").unwrap();
    assert_eq!((ann_row.created_at, ann_row.is_admin, ann_row.disabled_at, ann_row.mfa), (NOW, false, None, false));
}

fn a_disabled_account_answers_nowhere(store: &dyn AccountStore) {
    let bo = person(store, "bo@example.com");
    store.insert_session("bo-site", &bo.id, NOW + 100, None).unwrap();
    store.insert_session("bo-app", &bo.id, NOW + 100, Some("notes")).unwrap();
    store.link_identity("oidc", "bo", &bo.id).unwrap();
    store.replace_invite(&bo.id, "bo-invite", NOW + 100).unwrap();
    store.insert_pending("bo-pending", &bo.id, "code", "/", NOW + 100, NOW).unwrap();

    assert_eq!(store.set_disabled("bo@example.com", Some(NOW)).unwrap(), Some(bo.id.clone()));
    assert_eq!(store.set_disabled("nobody@example.com", Some(NOW)).unwrap(), None);
    assert_eq!(store.user_by_id(&bo.id).unwrap(), None);
    assert_eq!(store.user_by_email("bo@example.com").unwrap(), None);
    assert_eq!(store.credentials("bo@example.com").unwrap(), None);
    assert_eq!(store.password_hash(&bo.id).unwrap(), None);
    assert_eq!(store.is_active(&bo.id).unwrap(), Some(false));
    assert_eq!(store.account_by_email("bo@example.com").unwrap(), Some((bo.clone(), Some(NOW))));
    assert_eq!(store.user_by_identity("oidc", "bo").unwrap(), None);
    assert_eq!(store.session_user("bo-site", None, NOW).unwrap(), None);
    assert_eq!(store.session_user("bo-app", Some("notes"), NOW).unwrap(), None);
    assert_eq!(store.site_session("bo-site", NOW).unwrap(), None);
    assert_eq!(store.invited("bo-invite", NOW).unwrap(), None);
    assert_eq!(store.take_invite("bo-invite", NOW).unwrap(), None);
    assert_eq!(store.pending("bo-pending", NOW).unwrap(), None);

    store.set_disabled("bo@example.com", None).unwrap();
    assert_eq!(store.user_by_id(&bo.id).unwrap(), Some(bo.clone()));
    assert_eq!(store.invited("bo-invite", NOW).unwrap(), Some(bo), "a refused take spent the invitation");
}

fn hostile_text_is_data(store: &dyn AccountStore) {
    let long = "a".repeat(64 * 1024);
    for email in [
        "'); drop table users; --@example.com".to_string(),
        "\u{0430}nn@example.com".to_string(), // a Cyrillic а
        "x\"y`z\\@example.com".to_string(),
        format!("{long}@example.com"),
    ] {
        let user = person(store, &email);
        assert_eq!(store.user_by_email(&email).unwrap(), Some(user.clone()), "{:.40}", email);
        store.set_grant(&user.id, "app", &email).unwrap();
        assert_eq!(store.grant_of(&user.id, "app").unwrap(), Some(email.clone()));
    }
    assert!(store.user_by_email("ann@example.com").unwrap().is_some(), "a lookalike replaced the real account");
    assert!(store.list_users().unwrap().iter().any(|row| row.email == "ann@example.com"), "the users table is gone");

    // A NUL is refused or kept whole: never cut short into another value.
    let nul = "nul\0@example.com";
    let user = User { id: crate::content::slug::random_token(16), email: nul.into(), is_admin: false };
    if store.insert_user(&user, None, NOW).unwrap_or(false) {
        assert_eq!(store.user_by_email(nul).unwrap(), Some(user));
    }
    assert_eq!(store.user_by_email("nul").unwrap(), None);
}

fn sessions_answer_only_for_their_own_scope_and_account(store: &dyn AccountStore) {
    let cy = person(store, "cy@example.com");
    let di = person(store, "di@example.com");
    store.insert_session("cy-site", &cy.id, NOW + 100, None).unwrap();
    store.insert_session("cy-notes", &cy.id, NOW + 50, Some("notes")).unwrap();
    store.insert_session("cy-old", &cy.id, NOW - 1, None).unwrap();
    store.insert_session("di-site", &di.id, NOW + 100, None).unwrap();
    store.insert_session("di-notes", &di.id, NOW + 100, Some("notes")).unwrap();

    assert_eq!(store.session_user("cy-site", None, NOW).unwrap(), Some(cy.clone()));
    assert_eq!(store.session_user("cy-site", Some("notes"), NOW).unwrap(), None, "a site session spoke for an app");
    assert_eq!(store.session_user("cy-notes", None, NOW).unwrap(), None, "an app session spoke for the site");
    assert_eq!(store.session_user("cy-notes", Some("notes"), NOW).unwrap(), Some(cy.clone()));
    assert_eq!(store.session_user("cy-notes", Some("ledger"), NOW).unwrap(), None);
    assert_eq!(store.session_user("cy-old", None, NOW).unwrap(), None, "an expired session worked");
    assert_eq!(store.session_user("di-site", None, NOW).unwrap(), Some(di.clone()));
    assert_eq!(store.site_session("cy-site", NOW).unwrap(), Some((cy.clone(), NOW + 100)));
    assert_eq!(store.site_session("cy-notes", NOW).unwrap(), None);
    assert_eq!(store.session_user("cy-notes", Some("notes"), NOW + 51).unwrap(), None, "expiry is not checked against now");

    // Ending a site session takes this account's app sessions, no one else's.
    store.insert_session("cy-notes2", &cy.id, NOW + 50, Some("notes")).unwrap();
    store.insert_session("cy-site2", &cy.id, NOW + 100, None).unwrap();
    store.end_session("cy-site").unwrap();
    assert_eq!(store.session_user("cy-site", None, NOW).unwrap(), None);
    assert_eq!(store.session_user("cy-notes2", Some("notes"), NOW).unwrap(), None);
    assert_eq!(store.session_user("cy-site2", None, NOW).unwrap(), Some(cy.clone()), "another site session ended");
    assert_eq!(store.session_user("di-notes", Some("notes"), NOW).unwrap(), Some(di.clone()), "another account's session ended");
    store.end_session("never-was").unwrap();

    store.insert_session("cy-keep", &cy.id, NOW + 100, None).unwrap();
    store.delete_sessions_for(&cy.id, Some("cy-keep")).unwrap();
    assert_eq!(store.session_user("cy-keep", None, NOW).unwrap(), Some(cy.clone()));
    assert_eq!(store.session_user("cy-site2", None, NOW).unwrap(), None);
    store.delete_sessions_for(&cy.id, None).unwrap();
    assert_eq!(store.session_user("cy-keep", None, NOW).unwrap(), None);
    assert_eq!(store.session_user("di-site", None, NOW).unwrap(), Some(di));
}

fn an_invitation_is_spent_once(store: &dyn AccountStore) {
    let ed = person(store, "ed@example.com");
    store.replace_invite(&ed.id, "ed-1", NOW + 100).unwrap();
    store.replace_invite(&ed.id, "ed-2", NOW + 100).unwrap();
    assert_eq!(store.invited("ed-1", NOW).unwrap(), None, "a replaced invitation still works");
    assert_eq!(store.invited("ed-2", NOW).unwrap(), Some(ed.clone()));
    assert_eq!(store.invited("ed-2", NOW + 101).unwrap(), None, "an expired invitation works");
    assert_eq!(store.take_invite("ed-2", NOW + 101).unwrap(), None);
    assert_eq!(store.take_invite("ed-2", NOW).unwrap(), Some(ed));
    assert_eq!(store.take_invite("ed-2", NOW).unwrap(), None, "an invitation was spent twice");
    assert_eq!(store.invited("ed-2", NOW).unwrap(), None);
}

fn grants_and_scopes_stay_with_their_account_and_path(store: &dyn AccountStore) {
    let fay = person(store, "fay@example.com");
    let gus = person(store, "gus@example.com");
    store.set_grant(&fay.id, "ledger", "editor").unwrap();
    store.set_grant(&fay.id, "ledger", "viewer").unwrap();
    store.set_grant(&gus.id, "ledger", "admin").unwrap();
    assert_eq!(store.grant_of(&fay.id, "ledger").unwrap(), Some("viewer".into()));
    assert_eq!(store.grant_of(&fay.id, "other").unwrap(), None);
    let ledger: Vec<_> = store.list_grants().unwrap().into_iter().filter(|g| g.0 == "ledger").collect();
    assert_eq!(
        ledger,
        vec![
            ("ledger".into(), "fay@example.com".into(), "viewer".into()),
            ("ledger".into(), "gus@example.com".into(), "admin".into())
        ]
    );
    store.delete_grant(&fay.id, "ledger").unwrap();
    assert_eq!(store.grant_of(&fay.id, "ledger").unwrap(), None);
    assert_eq!(store.grant_of(&gus.id, "ledger").unwrap(), Some("admin".into()), "a revoke reached another account");

    store.set_scope(&fay.id, "ops", "viewer", None, NOW).unwrap();
    store.set_scope(&fay.id, "ops", "admin", Some("root"), NOW).unwrap();
    store.set_scope(&fay.id, "ops/yard", "editor", None, NOW).unwrap();
    store.set_scope(&fay.id, "opsx", "viewer", None, NOW).unwrap();
    store.set_scope(&gus.id, "ops/yard/tool", "viewer", None, NOW).unwrap();
    store.set_scope(&gus.id, "", "viewer", None, NOW).unwrap();
    assert_eq!(
        store.scopes_for(&fay.id).unwrap(),
        vec![("ops".into(), "admin".into()), ("ops/yard".into(), "editor".into()), ("opsx".into(), "viewer".into())]
    );
    let all = store.list_scopes().unwrap();
    let order: Vec<(&str, &str)> = all.iter().map(|r| (r.prefix.as_str(), r.email.as_str())).collect();
    assert!(order.windows(2).all(|w| w[0] <= w[1]), "not ordered: {order:?}");
    assert!(all.iter().any(|r| r.user_id == gus.id && r.prefix.is_empty()));

    // A move takes the tree, not its text-alike neighbour, and a moved row
    // replaces the one it lands on.
    store.set_scope(&fay.id, "labs/yard", "viewer", None, NOW).unwrap();
    assert_eq!(store.move_scope_tree("ops", "labs").unwrap(), 3);
    assert_eq!(
        store.scopes_for(&fay.id).unwrap(),
        vec![("labs".into(), "admin".into()), ("labs/yard".into(), "editor".into()), ("opsx".into(), "viewer".into())]
    );
    assert_eq!(store.scopes_for(&gus.id).unwrap(), vec![("".into(), "viewer".into()), ("labs/yard/tool".into(), "viewer".into())]);
    assert_eq!(store.move_scope_tree("ops", "labs").unwrap(), 0, "a finished move moved again");

    store.set_scope(&gus.id, "tool", "editor", None, NOW).unwrap();
    store.set_scope(&gus.id, "ops/tool", "viewer", None, NOW).unwrap();
    store.rename_scope("tool", "ops/tool").unwrap();
    assert!(store.scopes_for(&gus.id).unwrap().contains(&("ops/tool".into(), "editor".into())));
    assert!(!store.scopes_for(&gus.id).unwrap().iter().any(|(p, _)| p == "tool"));

    assert_eq!(store.delete_scope(&fay.id, "opsx").unwrap(), 1);
    assert_eq!(store.delete_scope(&fay.id, "opsx").unwrap(), 0);

    store.set_grant(&fay.id, "yard", "editor").unwrap();
    store.set_grant(&gus.id, "yardx", "editor").unwrap();
    let kept = store.forget_app("yard", "labs/yard").unwrap();
    assert_eq!(kept["app"], "yard");
    assert_eq!(kept["path"], "labs/yard");
    let mut access: Vec<String> = kept["access"].as_array().unwrap().iter().map(|r| format!("{} {} {}", r["email"], r["path"], r["scope"])).collect();
    access.sort();
    assert_eq!(
        access,
        vec![
            "\"fay@example.com\" \"labs/yard\" \"editor\"".to_string(),
            "\"gus@example.com\" \"labs/yard/tool\" \"viewer\"".to_string()
        ]
    );
    assert_eq!(kept["grants"], serde_json::json!([{ "email": "fay@example.com", "role": "editor" }]));
    assert_eq!(store.grant_of(&gus.id, "yardx").unwrap(), Some("editor".into()));
    assert_eq!(store.scopes_for(&fay.id).unwrap(), vec![("labs".into(), "admin".into())]);
    assert_eq!(store.remove_scope_tree("labs").unwrap(), 1);
    assert!(store.scopes_for(&fay.id).unwrap().is_empty());

    store.set_pin(&fay.id, "ledger", true, NOW).unwrap();
    store.set_pin(&fay.id, "ledger", true, NOW).unwrap();
    store.set_pin(&fay.id, "board", true, NOW).unwrap();
    assert_eq!(store.pins_for(&fay.id).unwrap(), vec!["board".to_string(), "ledger".to_string()]);
    assert!(store.pins_for(&gus.id).unwrap().is_empty());
    store.set_pin(&fay.id, "ledger", false, NOW).unwrap();
    assert_eq!(store.pins_for(&fay.id).unwrap(), vec!["board".to_string()]);
}

fn two_step_rows_belong_to_one_account(store: &dyn AccountStore) {
    let hal = person(store, "hal@example.com");
    let ivy = person(store, "ivy@example.com");
    assert_eq!(store.mfa_enabled(&hal.id).unwrap(), None);

    store.begin_mfa(&hal.id, "sealed-1", "by-a").unwrap();
    assert_eq!(store.mfa_enabled(&hal.id).unwrap(), Some(false));
    assert_eq!(store.mfa_secret(&hal.id, Some("by-a")).unwrap(), Some(("sealed-1".into(), 0)));
    assert_eq!(store.mfa_secret(&hal.id, Some("by-b")).unwrap(), None, "someone else's setup was shown");
    assert_eq!(store.mfa_secret(&hal.id, None).unwrap(), None, "a setup counted as enabled");
    assert_eq!(store.mfa_secret(&ivy.id, Some("by-a")).unwrap(), None);
    store.begin_mfa(&hal.id, "sealed-2", "by-b").unwrap();
    assert_eq!(store.mfa_secret(&hal.id, Some("by-a")).unwrap(), None, "a replaced setup still answers");
    store.cancel_mfa_setup(&hal.id).unwrap();
    assert_eq!(store.mfa_enabled(&hal.id).unwrap(), None);

    store.begin_mfa(&hal.id, "sealed-3", "by-c").unwrap();
    assert!(store.enable_mfa(&hal.id, NOW).unwrap());
    assert!(!store.enable_mfa(&hal.id, NOW).unwrap(), "enabled twice");
    assert_eq!(store.mfa_secret(&hal.id, None).unwrap(), Some(("sealed-3".into(), 0)));
    assert_eq!(store.mfa_secret(&hal.id, Some("by-c")).unwrap(), None, "the setup secret is still shown once on");
    store.begin_mfa(&hal.id, "sealed-evil", "by-d").unwrap();
    assert_eq!(store.mfa_secret(&hal.id, None).unwrap(), Some(("sealed-3".into(), 0)), "a new setup replaced an enabled secret");
    store.cancel_mfa_setup(&hal.id).unwrap();
    assert_eq!(store.mfa_enabled(&hal.id).unwrap(), Some(true), "cancelling a setup turned two-step off");
    assert!(store.list_users().unwrap().iter().any(|row| row.email == "hal@example.com" && row.mfa));

    assert!(store.advance_step(&hal.id, 10).unwrap());
    assert!(!store.advance_step(&hal.id, 10).unwrap(), "a step was spent twice");
    assert!(!store.advance_step(&hal.id, 9).unwrap(), "an earlier step was accepted");
    assert!(!store.advance_step(&ivy.id, 11).unwrap(), "a step moved for an account with no secret");
    assert_eq!(store.mfa_secret(&hal.id, None).unwrap(), Some(("sealed-3".into(), 10)));

    store.replace_recovery_codes(&hal.id, &["h1".into(), "h2".into()]).unwrap();
    store.replace_recovery_codes(&ivy.id, &["i1".into()]).unwrap();
    assert_eq!(store.recovery_left(&hal.id).unwrap(), 2);
    assert!(!store.spend_recovery_code(&ivy.id, "h1", NOW).unwrap(), "one account spent another's code");
    assert!(store.spend_recovery_code(&hal.id, "h1", NOW).unwrap());
    assert!(!store.spend_recovery_code(&hal.id, "h1", NOW).unwrap(), "a code was spent twice");
    assert_eq!(store.recovery_left(&hal.id).unwrap(), 1);
    store.replace_recovery_codes(&hal.id, &["h3".into()]).unwrap();
    assert!(!store.spend_recovery_code(&hal.id, "h2", NOW).unwrap(), "a replaced code still works");
    assert_eq!(store.recovery_left(&ivy.id).unwrap(), 1);

    store.record_failure(&hal.id, NOW - 1000, NOW - 2000).unwrap();
    store.record_failure(&hal.id, NOW, NOW - 900).unwrap();
    store.record_failure(&hal.id, NOW, NOW - 900).unwrap();
    assert_eq!(store.failures_since(&hal.id, NOW - 900).unwrap(), 2);
    assert_eq!(store.failures_since(&hal.id, NOW - 5000).unwrap(), 2, "a count from before the window was kept");
    assert_eq!(store.failures_since(&ivy.id, NOW - 900).unwrap(), 0);

    store.insert_pending("p-old", &hal.id, "code", "/", NOW - 1, NOW - 10).unwrap();
    store.insert_pending("p-1", &hal.id, "code", "/after", NOW + 300, NOW).unwrap();
    assert_eq!(store.pending("p-old", NOW).unwrap(), None);
    let pending = store.pending("p-1", NOW).unwrap().unwrap();
    assert_eq!((pending.user, pending.stage.as_str(), pending.next.as_str(), pending.failures), (hal.clone(), "code", "/after", 0));
    assert_eq!(store.fail_pending("p-1").unwrap(), Some(1));
    assert_eq!(store.fail_pending("p-1").unwrap(), Some(2));
    assert_eq!(store.fail_pending("nope").unwrap(), None);
    assert_eq!(store.pending("p-1", NOW + 301).unwrap(), None, "an expired pending sign-in answered");
    assert!(store.insert_pending("p-bad", &hal.id, "session", "/", NOW + 300, NOW).is_err(), "a stage outside code and setup was stored");
    store.insert_pending("p-ivy", &ivy.id, "setup", "/", NOW + 300, NOW).unwrap();
    store.delete_pending_for(&hal.id).unwrap();
    assert_eq!(store.pending("p-1", NOW).unwrap(), None);
    assert!(store.pending("p-ivy", NOW).unwrap().is_some(), "another account's pending sign-in ended");
    store.delete_pending("p-ivy").unwrap();
    assert_eq!(store.pending("p-ivy", NOW).unwrap(), None);

    store.insert_pending("p-2", &hal.id, "code", "/", NOW + 300, NOW).unwrap();
    store.remove_mfa(&hal.id).unwrap();
    assert_eq!(store.mfa_enabled(&hal.id).unwrap(), None);
    assert_eq!(store.recovery_left(&hal.id).unwrap(), 0);
    assert_eq!(store.pending("p-2", NOW).unwrap(), None);
    assert_eq!(store.recovery_left(&ivy.id).unwrap(), 1, "a removal reached another account");
}

fn single_use_writes_let_exactly_one_caller_through(store: &dyn AccountStore) {
    const CALLERS: usize = 8;
    let jo = person(store, "jo@example.com");
    store.begin_mfa(&jo.id, "sealed", "by").unwrap();
    assert_eq!(race(CALLERS, || store.enable_mfa(&jo.id, NOW).unwrap()), 1, "enabled more than once");
    assert_eq!(race(CALLERS, || store.advance_step(&jo.id, 100).unwrap()), 1, "one TOTP step let several in");
    store.replace_recovery_codes(&jo.id, &["j1".into()]).unwrap();
    assert_eq!(race(CALLERS, || store.spend_recovery_code(&jo.id, "j1", NOW).unwrap()), 1, "one recovery code let several in");
    store.replace_invite(&jo.id, "jo-invite", NOW + 100).unwrap();
    assert_eq!(race(CALLERS, || store.take_invite("jo-invite", NOW).unwrap().is_some()), 1, "one invitation was spent several times");
    store.insert_pending("jo-pending", &jo.id, "code", "/", NOW + 300, NOW).unwrap();
    race(CALLERS, || store.fail_pending("jo-pending").unwrap().is_some());
    assert_eq!(store.pending("jo-pending", NOW).unwrap().unwrap().failures, CALLERS as i64, "concurrent wrong codes were lost");
}

#[test]
fn the_sqlite_store_conforms() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::local(dir.path().to_path_buf(), "t");
    run(&SqliteAccounts::new(&config));
}

/// A runtime for the Postgres store to wait on from plain test threads.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    state::keep_runtime(runtime.handle().clone());
    runtime
});

/// A pool on a database of its own, with every ladder applied, and its name
/// for dropping. `None` without a server to use.
pub(crate) fn postgres_database() -> (deadpool_postgres::Pool, String) {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL")
        .expect("needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one");
    let name = format!("t_{}", crate::content::slug::random_token(12).to_lowercase());
    RUNTIME.block_on(async {
        let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client.batch_execute(&format!("create database {name}")).await.unwrap();
        let mut url = url::Url::parse(&server).unwrap();
        url.set_path(&name);
        let postgres = state::pg::connect(url.as_str(), 16).await.unwrap();
        state::pg::migrate(&postgres.pool, state::pg::LADDERS).await.unwrap();
        (postgres.pool, name)
    })
}

pub(crate) fn drop_postgres_database(pool: deadpool_postgres::Pool, name: &str) {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL").unwrap();
    pool.close();
    RUNTIME.block_on(async {
        let (client, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client.batch_execute(&format!("drop database if exists {name} with (force)")).await.unwrap();
    });
}

#[test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
fn the_postgres_store_conforms() {
    let (pool, name) = postgres_database();
    run(&PostgresAccounts::new(pool.clone()));
    drop_postgres_database(pool, &name);
}

/// Every statement the Postgres store sends is a literal: a value can only
/// ever arrive as a bound parameter, never spliced into SQL text.
#[test]
fn every_postgres_statement_is_a_literal() {
    let source = include_str!("postgres.rs");
    let mut calls = 0;
    for call in [".query(", ".query_opt(", ".query_one(", ".execute(", ".batch_execute("] {
        for (at, _) in source.match_indices(call) {
            let argument = source[at + call.len()..].trim_start();
            assert!(
                argument.starts_with('"') || argument.starts_with("sql,"),
                "a statement that is not a literal: {}",
                &source[at..(at + 120).min(source.len())]
            );
            calls += 1;
        }
    }
    assert!(calls > 40, "the scan found only {calls} statements");
}
