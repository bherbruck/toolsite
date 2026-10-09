//! One suite every token store must pass, run on files always and on
//! Postgres when `TOOLSITE_TEST_DATABASE_URL` names a server
//! (`scripts/test-postgres.sh` starts one). It pins what the policy in
//! `export`, `deploy` and `devices` rests on: a token answers for its own
//! app and kind and no other; a revocation is final, even beside a check
//! recording use; a token minted while others are used is never lost; and
//! use is recorded at the kind's resolution, no more often.

use super::{files::Files, hash, postgres::Postgres, Kind, Token, Tokens};
use crate::accounts::store::conformance::{drop_postgres_database, postgres_database};
use std::sync::Arc;

const T0: u64 = 1_700_000_000;

fn token(id: &str, label: &str, created_at: u64) -> Token {
    Token { id: id.into(), label: label.into(), last_used: None, created_at }
}

async fn mint(tokens: &dyn Tokens, app: &str, kind: Kind, id: &str, at: u64) -> String {
    let plain = format!("{}{}", kind.prefix(), crate::content::slug::random_token(40));
    tokens.insert(app, kind, &token(id, &format!("label {id}"), at), &hash(&plain)).await.unwrap();
    plain
}

async fn tokens_answer_for_their_app_and_kind(tokens: &dyn Tokens) {
    let sales = mint(tokens, "sales", Kind::Export, "e1", T0).await;
    mint(tokens, "sales", Kind::Export, "e2", T0 + 1).await;
    let device = mint(tokens, "sales", Kind::Device, "d1", T0).await;
    let deploy = mint(tokens, "hr", Kind::Deploy, "p1", T0).await;

    let found = tokens.check("sales", Kind::Export, &hash(&sales), T0 + 5).await.unwrap().unwrap();
    assert_eq!((found.id.as_str(), found.label.as_str()), ("e1", "label e1"));
    // Another app, an app whose name only starts the same, another kind.
    assert_eq!(tokens.check("hr", Kind::Export, &hash(&sales), T0).await.unwrap(), None);
    assert_eq!(tokens.check("sale", Kind::Export, &hash(&sales), T0).await.unwrap(), None);
    assert_eq!(tokens.check("sales", Kind::Device, &hash(&sales), T0).await.unwrap(), None);
    assert_eq!(tokens.check("sales", Kind::Export, &hash(&device), T0).await.unwrap(), None);
    assert_eq!(tokens.check("sales", Kind::Deploy, &hash(&deploy), T0).await.unwrap(), None);
    assert_eq!(tokens.check("sales", Kind::Export, &hash("tse_guess"), T0).await.unwrap(), None);

    let listed = tokens.list("sales", Kind::Export).await.unwrap();
    assert_eq!(listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["e1", "e2"]);
    assert_eq!(listed[0].last_used, Some(T0 + 5), "use was not recorded");
    assert_eq!(listed[1].last_used, None);
    assert!(tokens.list("hr", Kind::Export).await.unwrap().is_empty());
    let all = tokens.list_all(Kind::Export).await.unwrap();
    assert_eq!(all.iter().map(|(app, t)| (app.as_str(), t.id.as_str())).collect::<Vec<_>>(), [("sales", "e1"), ("sales", "e2")]);
    assert_eq!(tokens.list_all(Kind::Deploy).await.unwrap().len(), 1);
}

async fn labels_round_trip(tokens: &dyn Tokens) {
    let labels = ["'); drop table platform.app_tokens; --", "\u{2215}etc \u{0430}dmin \u{1F512}", &"x".repeat(60)];
    for (n, label) in labels.iter().enumerate() {
        tokens.insert("labels", Kind::Device, &token(&format!("l{n}"), label, T0 + n as u64), &format!("digest-{n}")).await.unwrap();
    }
    let listed = tokens.list("labels", Kind::Device).await.unwrap();
    assert_eq!(listed.iter().map(|t| t.label.as_str()).collect::<Vec<_>>(), labels);
}

async fn revocation_is_final(tokens: &dyn Tokens) {
    let a = mint(tokens, "shop", Kind::Deploy, "a", T0).await;
    let b = mint(tokens, "shop", Kind::Deploy, "b", T0).await;
    let other = mint(tokens, "other", Kind::Deploy, "a", T0).await;
    assert!(!tokens.revoke("other-app", Kind::Deploy, "a").await.unwrap());
    assert!(!tokens.revoke("shop", Kind::Export, "a").await.unwrap(), "a revocation crossed kinds");
    assert!(tokens.revoke("shop", Kind::Deploy, "a").await.unwrap());
    assert!(!tokens.revoke("shop", Kind::Deploy, "a").await.unwrap());
    assert_eq!(tokens.check("shop", Kind::Deploy, &hash(&a), T0).await.unwrap(), None);
    assert!(tokens.check("shop", Kind::Deploy, &hash(&b), T0).await.unwrap().is_some());
    assert!(tokens.check("other", Kind::Deploy, &hash(&other), T0).await.unwrap().is_some(), "a revocation crossed apps");
    tokens.revoke_all("shop", Kind::Deploy).await.unwrap();
    assert_eq!(tokens.check("shop", Kind::Deploy, &hash(&b), T0).await.unwrap(), None);
    assert!(tokens.list("shop", Kind::Deploy).await.unwrap().is_empty());
    assert!(tokens.check("other", Kind::Deploy, &hash(&other), T0).await.unwrap().is_some());
}

/// A device's use is written once a minute at most; an export's on every use.
async fn use_is_recorded_at_the_kinds_resolution(tokens: &dyn Tokens) {
    let device = mint(tokens, "broker", Kind::Device, "d", T0).await;
    let digest = hash(&device);
    let used = |at| tokens.check("broker", Kind::Device, &digest, at);
    assert_eq!(used(T0 + 100).await.unwrap().unwrap().last_used, Some(T0 + 100));
    assert_eq!(used(T0 + 130).await.unwrap().unwrap().last_used, Some(T0 + 100));
    assert_eq!(tokens.list("broker", Kind::Device).await.unwrap()[0].last_used, Some(T0 + 100));
    assert_eq!(used(T0 + 159).await.unwrap().unwrap().last_used, Some(T0 + 100));
    assert_eq!(used(T0 + 160).await.unwrap().unwrap().last_used, Some(T0 + 160));
    assert_eq!(tokens.list("broker", Kind::Device).await.unwrap()[0].last_used, Some(T0 + 160));
    assert_eq!(tokens.check_blocking("broker", Kind::Device, &hash(&device), T0 + 300).unwrap().unwrap().last_used, Some(T0 + 300));

    let export = mint(tokens, "broker", Kind::Export, "e", T0).await;
    for at in [T0 + 1, T0 + 2, T0 + 3] {
        tokens.check("broker", Kind::Export, &hash(&export), at).await.unwrap().unwrap();
        assert_eq!(tokens.list("broker", Kind::Export).await.unwrap()[0].last_used, Some(at));
    }
}

/// The bug the store exists to fix: a use recorded by reading the whole
/// list and writing it back lost every token minted in between.
async fn a_token_minted_while_others_are_used_is_never_lost(tokens: Arc<dyn Tokens>) {
    for kind in [Kind::Export, Kind::Device] {
        let first = mint(&*tokens, "busy", kind, "first", T0).await;
        let minting: Vec<_> = (0..8)
            .map(|n| {
                let tokens = tokens.clone();
                tokio::spawn(async move {
                    let mut minted = Vec::new();
                    for m in 0..20 {
                        minted.push(mint(&*tokens, "busy", kind, &format!("m{n}-{m}"), T0 + 1).await);
                    }
                    minted
                })
            })
            .collect();
        let using: Vec<_> = (0..4)
            .map(|n| {
                let (tokens, first) = (tokens.clone(), first.clone());
                // Each use far enough apart that every one is written.
                tokio::spawn(async move {
                    for m in 0..100u64 {
                        let at = T0 + 1_000 + (m * 4 + n) * 61;
                        assert!(tokens.check("busy", kind, &hash(&first), at).await.unwrap().is_some(), "a check missed a live token");
                    }
                })
            })
            .collect();
        let mut minted = Vec::new();
        for task in minting {
            minted.extend(task.await.unwrap());
        }
        for task in using {
            task.await.unwrap();
        }
        for plain in &minted {
            assert!(tokens.check("busy", kind, &hash(plain), T0).await.unwrap().is_some(), "a minted {kind:?} token was lost");
        }
        assert_eq!(tokens.list("busy", kind).await.unwrap().len(), minted.len() + 1);
    }
}

/// A check recording use beside a revocation never writes the revoked
/// token back, and never answers for it once the revocation returned.
async fn a_use_never_brings_back_a_token_revoked_beside_it(tokens: Arc<dyn Tokens>) {
    for round in 0..50u64 {
        let keep = mint(&*tokens, "race", Kind::Export, &format!("keep{round}"), T0).await;
        let doomed = mint(&*tokens, "race", Kind::Export, &format!("doomed{round}"), T0).await;
        let checking = {
            let (tokens, keep, doomed) = (tokens.clone(), keep.clone(), doomed.clone());
            tokio::spawn(async move {
                assert!(tokens.check("race", Kind::Export, &hash(&keep), T0 + round).await.unwrap().is_some());
                tokens.check("race", Kind::Export, &hash(&doomed), T0 + round).await.unwrap();
            })
        };
        assert!(tokens.revoke("race", Kind::Export, &format!("doomed{round}")).await.unwrap());
        checking.await.unwrap();
        assert_eq!(tokens.check("race", Kind::Export, &hash(&doomed), T0 + round + 1).await.unwrap(), None, "a revoked token came back");
    }
    assert_eq!(tokens.list("race", Kind::Export).await.unwrap().len(), 50);
}

async fn run(tokens: Arc<dyn Tokens>) {
    tokens_answer_for_their_app_and_kind(&*tokens).await;
    labels_round_trip(&*tokens).await;
    revocation_is_final(&*tokens).await;
    use_is_recorded_at_the_kinds_resolution(&*tokens).await;
    a_token_minted_while_others_are_used_is_never_lost(tokens.clone()).await;
    a_use_never_brings_back_a_token_revoked_beside_it(tokens).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_files_tokens_conform() {
    let dir = tempfile::tempdir().unwrap();
    run(Arc::new(Files::new(dir.path().to_path_buf()))).await;
}

/// The lists are where they always were, field for field, and an empty one
/// leaves no file.
#[tokio::test]
async fn the_files_tokens_write_the_same_sidecars_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let tokens = Files::new(dir.path().to_path_buf());
    tokens.insert("sales", Kind::Export, &token("abc", "reporting", T0), "digest").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sales.exports")).unwrap(),
        "[\n  {\n    \"id\": \"abc\",\n    \"label\": \"reporting\",\n    \"created_at\": 1700000000,\n    \"hash\": \"digest\"\n  }\n]"
    );
    tokens.check("sales", Kind::Export, "digest", T0 + 9).await.unwrap().unwrap();
    assert!(std::fs::read_to_string(dir.path().join("sales.exports")).unwrap().contains("\"last_used\": 1700000009,\n    \"created_at\""));
    tokens.insert("shop", Kind::Deploy, &token("d", "ci", T0), "x").await.unwrap();
    tokens.insert("broker", Kind::Device, &token("v", "pump", T0), "y").await.unwrap();
    assert!(dir.path().join("shop.deploys").is_file() && dir.path().join("broker.devices").is_file());
    tokens.revoke("sales", Kind::Export, "abc").await.unwrap();
    assert!(!dir.path().join("sales.exports").exists());

    // A list that does not parse is an error, and nothing is written over it.
    std::fs::write(dir.path().join("torn.exports"), "[{").unwrap();
    assert!(tokens.check("torn", Kind::Export, "x", T0).await.is_err());
    assert!(tokens.insert("torn", Kind::Export, &token("n", "n", T0), "z").await.is_err());
    assert_eq!(std::fs::read_to_string(dir.path().join("torn.exports")).unwrap(), "[{");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_postgres_tokens_conform() {
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    let tokens = Arc::new(Postgres::new(pool.clone()));
    run(tokens.clone()).await;

    // Rows hold digests, never what was handed out.
    let plain = mint(&*tokens, "vault", Kind::Export, "v", T0).await;
    let client = pool.get().await.unwrap();
    let rows = client.query("select hash from platform.app_tokens where app = 'vault'", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), hash(&plain));
    assert!(!rows[0].get::<_, String>(0).contains(&plain[4..]));
    drop(client);
    drop(tokens);
    std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
}

/// A removal takes every token of the app out, keeps each kind's in
/// `removed_records` in the shape its sidecar has on files, and leaves
/// another app's alone: tokens of a removed app never open the next app
/// published at its name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn a_removal_retires_the_tokens_and_keeps_them_on_postgres() {
    let (pool, name) = std::thread::spawn(postgres_database).join().unwrap();
    let tokens: Arc<dyn Tokens> = Arc::new(Postgres::new(pool.clone()));
    let export = mint(&*tokens, "shop", Kind::Export, "e", T0).await;
    mint(&*tokens, "shop", Kind::Device, "d", T0).await;
    let neighbour = mint(&*tokens, "shop_x", Kind::Export, "e", T0).await;
    let retiring = tokens.clone();
    let retired = tokio::task::spawn_blocking(move || retiring.retire_blocking("shop", 77)).await.unwrap().unwrap();
    assert_eq!(retired.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(), ["exports", "devices"]);
    let files = tempfile::tempdir().unwrap();
    let on_files = Files::new(files.path().to_path_buf());
    on_files.insert("shop", Kind::Export, &token("e", "label e", T0), &hash(&export)).await.unwrap();
    assert_eq!(retired[0].1, std::fs::read_to_string(files.path().join("shop.exports")).unwrap(), "not the sidecar's shape");
    assert_eq!(tokens.check("shop", Kind::Export, &hash(&export), T0).await.unwrap(), None);
    assert!(tokens.check("shop_x", Kind::Export, &hash(&neighbour), T0).await.unwrap().is_some());
    let kept: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one("select count(*) from platform.removed_records where app = 'shop' and removed_at = 77", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, 2);
    drop(tokens);
    std::thread::spawn(move || drop_postgres_database(pool, &name)).join().unwrap();
}
