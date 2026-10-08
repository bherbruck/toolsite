//! What every `OAuthStore` must do, written once and run against each
//! backend by `tests.rs`. Each function is one property; the name says
//! which.

use super::{Aged, Backdoor, Grant, OAuthStore, ACCESS_LIFETIME, CODE_LIFETIME, IDLE_CLIENT_LIFETIME, REFRESH_LIFETIME};
use std::{sync::Arc, time::Duration};

const CB: &str = "https://c.test/cb";

/// Requests presenting one value at the same moment: enough that a store
/// without a real single-use guard lets two through.
const RACERS: usize = 8;

async fn registered<S: OAuthStore>(store: &S) -> String {
    store.register_client(Some("t"), &[CB.to_string()]).await.unwrap().id
}

async fn code_for<S: OAuthStore>(store: &S, client_id: &str, user_id: &str, resource: Option<&str>) -> String {
    store
        .issue_code(&Grant {
            client_id,
            user_id,
            redirect_uri: CB,
            code_challenge: "ch",
            resource,
        })
        .await
        .unwrap()
}

pub(super) async fn a_code_is_redeemed_exactly_once<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let client = registered(&*store).await;
    let code = code_for(&*store, &client, "u1", None).await;
    let first = store.redeem_code(&code).await.expect("first redemption");
    assert_eq!(first.client_id, client);
    assert_eq!(first.user_id, "u1");
    assert_eq!(first.redirect_uri, CB);
    assert_eq!(first.code_challenge, "ch");
    assert_eq!(first.resource, None);
    assert!(store.redeem_code(&code).await.is_none(), "a code replayed");
    assert!(store.redeem_code("never-issued").await.is_none());
}

pub(super) async fn an_expired_code_is_refused_and_spent<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let client = registered(&*store).await;
    let code = code_for(&*store, &client, "u1", None).await;
    let by = (CODE_LIFETIME + Duration::from_secs(1)).as_secs() as i64;
    store.age(Aged::Codes, by).await;
    assert!(store.redeem_code(&code).await.is_none(), "an expired code was redeemed");
    // Turning the clock forward again would revive a code that was only
    // refused; a spent one stays gone.
    store.age(Aged::Codes, -by).await;
    assert!(store.redeem_code(&code).await.is_none(), "a refused code was not spent");
}

pub(super) async fn tokens_are_stored_hashed<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let client = registered(&*store).await;
    let issued = store.issue_tokens(&client, "u1", None).await.unwrap();
    let stored = store.stored_tokens().await;
    assert_eq!(stored.len(), 2);
    for (hash, _, _) in &stored {
        assert_ne!(hash, &issued.access_token);
        assert_ne!(hash, &issued.refresh_token);
    }
    assert_eq!(issued.expires_in, ACCESS_LIFETIME.as_secs());
    assert_eq!(
        store.access_token_grant(&issued.access_token).await,
        Some(("u1".to_string(), client.clone(), None))
    );
}

pub(super) async fn an_expired_access_token_stops_working<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let client = registered(&*store).await;
    let issued = store.issue_tokens(&client, "u1", None).await.unwrap();
    store.age(Aged::Tokens, ACCESS_LIFETIME.as_secs() as i64 + 1).await;
    assert!(store.access_token_grant(&issued.access_token).await.is_none());
}

pub(super) async fn an_expired_refresh_token_is_refused_and_spent<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let client = registered(&*store).await;
    let issued = store.issue_tokens(&client, "u1", None).await.unwrap();
    store.age(Aged::Tokens, REFRESH_LIFETIME.as_secs() as i64 + 1).await;
    assert!(store.rotate_refresh(&client, &issued.refresh_token).await.is_none());
    assert!(store.stored_tokens().await.iter().all(|(_, kind, _)| kind != "refresh"));
}

pub(super) async fn a_refresh_token_works_once_and_only_for_its_client<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let client = registered(&*store).await;
    let other = registered(&*store).await;
    let issued = store.issue_tokens(&client, "u1", None).await.unwrap();

    assert!(
        store.rotate_refresh(&other, &issued.refresh_token).await.is_none(),
        "another client used the refresh token"
    );
    let next = store.rotate_refresh(&client, &issued.refresh_token).await.expect("rotation");
    assert_ne!(next.refresh_token, issued.refresh_token);
    assert!(
        store.rotate_refresh(&client, &issued.refresh_token).await.is_none(),
        "a retired refresh token was accepted"
    );
    assert_eq!(
        store.access_token_grant(&next.access_token).await,
        Some(("u1".to_string(), client.clone(), None))
    );
    assert!(store.rotate_refresh(&client, &next.refresh_token).await.is_some());
}

pub(super) async fn an_access_token_and_a_refresh_token_are_not_interchangeable<S: OAuthStore + Backdoor>(
    store: Arc<S>,
) {
    let client = registered(&*store).await;
    let issued = store.issue_tokens(&client, "u1", None).await.unwrap();
    assert!(store.access_token_grant(&issued.refresh_token).await.is_none(), "a refresh token opened a request");
    assert!(store.rotate_refresh(&client, &issued.access_token).await.is_none(), "an access token was rotated");
    assert!(store.access_token_grant(&issued.access_token).await.is_some());
}

pub(super) async fn an_idle_registration_is_swept_but_a_connected_one_stays<S: OAuthStore + Backdoor>(
    store: Arc<S>,
) {
    let idle = registered(&*store).await;
    let live = registered(&*store).await;
    let pending = registered(&*store).await;
    store.issue_tokens(&live, "u1", None).await.unwrap();
    code_for(&*store, &pending, "u1", None).await;
    store.age(Aged::Clients, IDLE_CLIENT_LIFETIME.as_secs() as i64 + 1).await;
    // Any registration runs the sweep.
    registered(&*store).await;
    assert!(store.client(&idle).await.is_none(), "an idle client stayed");
    assert!(store.client(&live).await.is_some(), "a client holding tokens was swept");
    assert!(store.client(&pending).await.is_some(), "a client with a code in flight was swept");
}

pub(super) async fn tokens_stay_bound_to_their_resource<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let resource = "https://site.test/p/notes/mcp";
    let client = registered(&*store).await;
    let code = code_for(&*store, &client, "u1", Some(resource)).await;
    let redeemed = store.redeem_code(&code).await.unwrap();
    assert_eq!(redeemed.resource.as_deref(), Some(resource));

    let issued = store.issue_tokens(&client, "u1", redeemed.resource.as_deref()).await.unwrap();
    let grant = store.access_token_grant(&issued.access_token).await.unwrap();
    assert_eq!(grant.2.as_deref(), Some(resource));

    // A rotation cannot widen the token: the new pair names the same app.
    let next = store.rotate_refresh(&client, &issued.refresh_token).await.unwrap();
    let grant = store.access_token_grant(&next.access_token).await.unwrap();
    assert_eq!(grant.2.as_deref(), Some(resource), "a refreshed token lost its resource");

    // And one issued with none stays with none.
    let open = store.issue_tokens(&client, "u1", None).await.unwrap();
    assert_eq!(store.access_token_grant(&open.access_token).await.unwrap().2, None);
}

pub(super) async fn revoking_an_account_ends_its_tokens_and_codes_and_no_one_elses<S: OAuthStore + Backdoor>(
    store: Arc<S>,
) {
    let client = registered(&*store).await;
    let mine = store.issue_tokens(&client, "u1", None).await.unwrap();
    let my_code = code_for(&*store, &client, "u1", None).await;
    let theirs = store.issue_tokens(&client, "u2", None).await.unwrap();
    let their_code = code_for(&*store, &client, "u2", None).await;

    assert_eq!(store.revoke_for_user("u1").await.unwrap(), 3, "two tokens and a code");
    assert!(store.access_token_grant(&mine.access_token).await.is_none(), "the access token still works");
    assert!(store.rotate_refresh(&client, &mine.refresh_token).await.is_none(), "the refresh token still works");
    assert!(store.redeem_code(&my_code).await.is_none(), "the code still works");

    assert!(store.access_token_grant(&theirs.access_token).await.is_some(), "another account's token was revoked");
    assert!(store.redeem_code(&their_code).await.is_some(), "another account's code was revoked");
    assert_eq!(store.revoke_for_user("nobody").await.unwrap(), 0);
}

pub(super) async fn an_unknown_client_is_nothing<S: OAuthStore + Backdoor>(store: Arc<S>) {
    assert!(store.client("never-registered").await.is_none());
    let named = store.register_client(None, &[CB.into(), "http://127.0.0.1:9/cb".into()]).await.unwrap();
    let found = store.client(&named.id).await.unwrap();
    assert_eq!(found, named);
    assert_eq!(found.name, None);
}

pub(super) async fn hostile_values_are_kept_exactly_as_given<S: OAuthStore + Backdoor>(store: Arc<S>) {
    let long = "x".repeat(64 * 1024);
    let hostile = [
        "'); drop table clients; --",
        "\"; delete from tokens; --",
        "$1 $2 ? ?1 :name @p",
        "раураl.соm \u{202e}moc.evil\u{200b}",
        long.as_str(),
    ];
    for value in hostile {
        let client = store.register_client(Some(value), &[value.to_string(), CB.into()]).await.unwrap();
        let found = store.client(&client.id).await.unwrap();
        assert_eq!(found.name.as_deref(), Some(value));
        assert_eq!(found.redirect_uris, vec![value.to_string(), CB.to_string()]);

        let code = store
            .issue_code(&Grant {
                client_id: &client.id,
                user_id: value,
                redirect_uri: value,
                code_challenge: value,
                resource: Some(value),
            })
            .await
            .unwrap();
        let redeemed = store.redeem_code(&code).await.unwrap();
        assert_eq!(
            (redeemed.user_id.as_str(), redeemed.redirect_uri.as_str(), redeemed.code_challenge.as_str()),
            (value, value, value)
        );
        assert_eq!(redeemed.resource.as_deref(), Some(value));

        let issued = store.issue_tokens(&client.id, value, Some(value)).await.unwrap();
        assert_eq!(
            store.access_token_grant(&issued.access_token).await,
            Some((value.to_string(), client.id.clone(), Some(value.to_string())))
        );
        assert_eq!(store.revoke_for_user(value).await.unwrap(), 2);
    }
    // Every table still answers.
    assert!(store.client("x").await.is_none());
    registered(&*store).await;

    // Postgres text cannot hold NUL; SQLite can. Either refuses or keeps it
    // exactly, and neither stores something else.
    match store.register_client(Some("a\0b"), &[CB.into()]).await {
        Ok(client) => assert_eq!(store.client(&client.id).await.unwrap().name.as_deref(), Some("a\0b")),
        Err(error) => assert!(!error.is_empty()),
    }
}

pub(super) async fn one_code_redeemed_by_many_at_once_yields_one_token<S: OAuthStore + Backdoor + 'static>(
    store: Arc<S>,
) {
    let client = registered(&*store).await;
    let code = code_for(&*store, &client, "u1", None).await;
    let start = Arc::new(tokio::sync::Barrier::new(RACERS));
    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (store, start, code) = (store.clone(), start.clone(), code.clone());
            tokio::spawn(async move {
                start.wait().await;
                let redeemed = store.redeem_code(&code).await?;
                store.issue_tokens(&redeemed.client_id, &redeemed.user_id, None).await.ok()
            })
        })
        .collect();
    let mut won = 0;
    for racer in racers {
        if racer.await.unwrap().is_some() {
            won += 1;
        }
    }
    assert_eq!(won, 1, "one code was redeemed {won} times");
    assert_eq!(store.stored_tokens().await.len(), 2, "one pair, not more");
}

pub(super) async fn one_refresh_rotated_by_many_at_once_yields_one_new_refresh<
    S: OAuthStore + Backdoor + 'static,
>(
    store: Arc<S>,
) {
    let client = registered(&*store).await;
    let issued = store.issue_tokens(&client, "u1", None).await.unwrap();
    let start = Arc::new(tokio::sync::Barrier::new(RACERS));
    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (store, start, client, token) =
                (store.clone(), start.clone(), client.clone(), issued.refresh_token.clone());
            tokio::spawn(async move {
                start.wait().await;
                store.rotate_refresh(&client, &token).await
            })
        })
        .collect();
    let mut winners = Vec::new();
    for racer in racers {
        if let Some(next) = racer.await.unwrap() {
            winners.push(next);
        }
    }
    assert_eq!(winners.len(), 1, "one refresh token was rotated {} times", winners.len());
    let next = winners.pop().unwrap();

    let refresh: Vec<_> = store
        .stored_tokens()
        .await
        .into_iter()
        .filter(|(_, kind, _)| kind == "refresh")
        .collect();
    assert_eq!(refresh.len(), 1, "more than one live refresh token: {refresh:?}");
    assert!(store.rotate_refresh(&client, &issued.refresh_token).await.is_none(), "the old refresh token lives");
    assert!(store.access_token_grant(&next.access_token).await.is_some());
    assert!(store.rotate_refresh(&client, &next.refresh_token).await.is_some(), "the winner's refresh token is dead");
}
