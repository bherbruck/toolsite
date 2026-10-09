//! The OAuth store on Postgres: schema `oauth`, built by
//! `migrations/postgres/oauth/`. Every runner shares it, so single use
//! cannot rest on a process lock: a code is redeemed, and a refresh token
//! retired, by one `delete ... returning`, which only one transaction can
//! win however many runners present the same value at once.
//!
//! Every value is a bound parameter; the SQL text is constant.

use super::{
    hash, now, Client, Grant, Issued, OAuthStore, Redeemed, ACCESS_LIFETIME, CODE_LIFETIME, IDLE_CLIENT_LIFETIME,
    REFRESH_LIFETIME, TOKEN_LEN,
};
use crate::content::slug::random_token;
use deadpool_postgres::{Pool, Transaction};

#[derive(Clone)]
pub struct PostgresOAuth {
    pool: Pool,
}

/// An error and every cause under it: a driver error's `Display` names the
/// kind and drops the reason.
fn why(context: &str, error: &dyn std::error::Error) -> String {
    let mut text = format!("{context}: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        let next = cause.to_string();
        if !text.contains(&next) {
            text.push_str(": ");
            text.push_str(&next);
        }
        source = cause.source();
    }
    text
}

/// For the methods that answer "nothing" on any failure: the caller refuses,
/// and the reason is in the log rather than lost.
fn logged<T>(what: &str, result: Result<Option<T>, String>) -> Option<T> {
    result.unwrap_or_else(|error| {
        tracing::warn!(%error, "OAuth store: {what} failed");
        None
    })
}

impl PostgresOAuth {
    pub fn new(pool: Pool) -> PostgresOAuth {
        PostgresOAuth { pool }
    }

    async fn connection(&self) -> Result<deadpool_postgres::Object, String> {
        self.pool.get().await.map_err(|e| why("could not reach Postgres", &e))
    }

    async fn find_client(&self, id: &str) -> Result<Option<Client>, String> {
        let conn = self.connection().await?;
        let row = conn
            .query_opt("select id, name, redirect_uris from oauth.clients where id = $1", &[&id])
            .await
            .map_err(|e| why("could not read the client", &e))?;
        Ok(row.map(|row| {
            let uris: String = row.get(2);
            Client {
                id: row.get(0),
                name: row.get(1),
                redirect_uris: serde_json::from_str(&uris).unwrap_or_default(),
            }
        }))
    }

    async fn take_code(&self, code: &str) -> Result<Option<Redeemed>, String> {
        let conn = self.connection().await?;
        // One statement finds and spends the code: of two redemptions at
        // once, the second waits on the first's row lock and then finds no
        // row. An expired code is spent too, as in file mode.
        let row = conn
            .query_opt(
                "delete from oauth.codes where code_hash = $1
                 returning client_id, user_id, redirect_uri, code_challenge, expires_at, resource",
                &[&hash(code)],
            )
            .await
            .map_err(|e| why("could not redeem the code", &e))?;
        Ok(row.and_then(|row| {
            let expires_at: i64 = row.get(4);
            (expires_at >= now() as i64).then(|| Redeemed {
                client_id: row.get(0),
                user_id: row.get(1),
                redirect_uri: row.get(2),
                code_challenge: row.get(3),
                resource: row.get(5),
            })
        }))
    }

    async fn rotate(&self, client_id: &str, refresh_token: &str) -> Result<Option<Issued>, String> {
        let mut conn = self.connection().await?;
        let tx = conn.transaction().await.map_err(|e| why("could not begin a rotation", &e))?;
        // Whose token it is, to take that account's turn before touching it.
        // The delete below checks the row again, so a token rotated or
        // revoked in between is simply not found.
        let Some(holder) = tx
            .query_opt(
                "select user_id from oauth.tokens where token_hash = $1 and kind = 'refresh' and client_id = $2",
                &[&hash(refresh_token), &client_id],
            )
            .await
            .map_err(|e| why("could not read the refresh token", &e))?
        else {
            return Ok(None);
        };
        hold_account(&tx, &holder.get::<_, String>(0)).await?;
        let row = tx
            .query_opt(
                "delete from oauth.tokens
                  where token_hash = $1 and kind = 'refresh' and client_id = $2
                 returning user_id, expires_at, resource",
                &[&hash(refresh_token), &client_id],
            )
            .await
            .map_err(|e| why("could not retire the refresh token", &e))?;
        // Unknown, another client's, or already rotated by a request that
        // got there first: nothing changed, and dropping the transaction
        // rolls back nothing.
        let Some(row) = row else { return Ok(None) };
        let (user_id, expires_at, resource): (String, i64, Option<String>) = (row.get(0), row.get(1), row.get(2));
        if expires_at < now() as i64 {
            tx.commit().await.map_err(|e| why("could not retire the refresh token", &e))?;
            return Ok(None);
        }
        // A refreshed pair is for the same resource as the one it replaces.
        let issued = insert_pair(&tx, client_id, &user_id, resource.as_deref()).await?;
        tx.commit().await.map_err(|e| why("could not commit the rotation", &e))?;
        Ok(Some(issued))
    }

    async fn grant_of(&self, token: &str) -> Result<Option<(String, String, Option<String>)>, String> {
        self.sweep().await;
        let conn = self.connection().await?;
        let row = conn
            .query_opt(
                "select user_id, client_id, resource from oauth.tokens
                  where token_hash = $1 and kind = 'access' and expires_at >= $2",
                &[&hash(token), &(now() as i64)],
            )
            .await
            .map_err(|e| why("could not read the token", &e))?;
        Ok(row.map(|row| (row.get(0), row.get(1), row.get(2))))
    }

    async fn sweep_now(&self) -> Result<(), String> {
        let conn = self.connection().await?;
        let cutoff = now() as i64;
        let idle = cutoff - IDLE_CLIENT_LIFETIME.as_secs() as i64;
        conn.execute("delete from oauth.tokens where expires_at < $1", &[&cutoff])
            .await
            .map_err(|e| why("could not sweep tokens", &e))?;
        conn.execute("delete from oauth.codes where expires_at < $1", &[&cutoff])
            .await
            .map_err(|e| why("could not sweep codes", &e))?;
        // A token or code inserted for a client between this statement's
        // snapshot and its delete makes the foreign key refuse the delete:
        // the client stays, which is the safe way to lose that race.
        conn.execute(
            "delete from oauth.clients c
              where c.created_at < $1
                and not exists (select 1 from oauth.tokens t where t.client_id = c.id)
                and not exists (select 1 from oauth.codes k where k.client_id = c.id)",
            &[&idle],
        )
        .await
        .map_err(|e| why("could not sweep clients", &e))?;
        Ok(())
    }
}

/// Takes this account's turn until the transaction ends. See
/// `state::pg::LOCK_OAUTH_USER`.
async fn hold_account(tx: &Transaction<'_>, user_id: &str) -> Result<(), String> {
    tx.execute(
        "select pg_advisory_xact_lock($1::int4, hashtext($2))",
        &[&crate::state::pg::LOCK_OAUTH_USER, &user_id],
    )
    .await
    .map_err(|e| why("could not take the account's turn", &e))?;
    Ok(())
}

async fn insert_pair(
    tx: &Transaction<'_>,
    client_id: &str,
    user_id: &str,
    resource: Option<&str>,
) -> Result<Issued, String> {
    let access_token = random_token(TOKEN_LEN);
    let refresh_token = random_token(TOKEN_LEN);
    let issued_at = now();
    for (token, kind, lifetime) in [
        (&access_token, "access", ACCESS_LIFETIME),
        (&refresh_token, "refresh", REFRESH_LIFETIME),
    ] {
        tx.execute(
            "insert into oauth.tokens (token_hash, kind, client_id, user_id, expires_at, created_at, resource)
             values ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &hash(token),
                &kind,
                &client_id,
                &user_id,
                &((issued_at + lifetime.as_secs()) as i64),
                &(issued_at as i64),
                &resource,
            ],
        )
        .await
        .map_err(|e| why("could not store a token", &e))?;
    }
    Ok(Issued {
        access_token,
        refresh_token,
        expires_in: ACCESS_LIFETIME.as_secs(),
    })
}

#[async_trait::async_trait]
impl OAuthStore for PostgresOAuth {
    async fn register_client(&self, name: Option<&str>, redirect_uris: &[String]) -> Result<Client, String> {
        self.sweep().await;
        let id = random_token(24);
        let uris = serde_json::to_string(redirect_uris).map_err(|e| e.to_string())?;
        let conn = self.connection().await?;
        conn.execute(
            "insert into oauth.clients (id, name, redirect_uris, created_at) values ($1, $2, $3, $4)",
            &[&id, &name, &uris, &(now() as i64)],
        )
        .await
        .map_err(|e| why("could not register the client", &e))?;
        Ok(Client {
            id,
            name: name.map(str::to_string),
            redirect_uris: redirect_uris.to_vec(),
        })
    }

    async fn client(&self, id: &str) -> Option<Client> {
        logged("reading a client", self.find_client(id).await)
    }

    async fn issue_code(&self, grant: &Grant<'_>) -> Result<String, String> {
        let code = random_token(TOKEN_LEN);
        let mut conn = self.connection().await?;
        let tx = conn.transaction().await.map_err(|e| why("could not begin issuing a code", &e))?;
        hold_account(&tx, grant.user_id).await?;
        tx.execute(
            "insert into oauth.codes (code_hash, client_id, user_id, redirect_uri, code_challenge, expires_at, resource)
             values ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &hash(&code),
                &grant.client_id,
                &grant.user_id,
                &grant.redirect_uri,
                &grant.code_challenge,
                &((now() + CODE_LIFETIME.as_secs()) as i64),
                &grant.resource,
            ],
        )
        .await
        .map_err(|e| why("could not issue a code", &e))?;
        tx.commit().await.map_err(|e| why("could not commit the code", &e))?;
        Ok(code)
    }

    async fn redeem_code(&self, code: &str) -> Option<Redeemed> {
        logged("redeeming a code", self.take_code(code).await)
    }

    async fn issue_tokens(&self, client_id: &str, user_id: &str, resource: Option<&str>) -> Result<Issued, String> {
        let mut conn = self.connection().await?;
        let tx = conn.transaction().await.map_err(|e| why("could not begin issuing tokens", &e))?;
        hold_account(&tx, user_id).await?;
        let issued = insert_pair(&tx, client_id, user_id, resource).await?;
        tx.commit().await.map_err(|e| why("could not commit the tokens", &e))?;
        Ok(issued)
    }

    async fn rotate_refresh(&self, client_id: &str, refresh_token: &str) -> Option<Issued> {
        logged("rotating a refresh token", self.rotate(client_id, refresh_token).await)
    }

    async fn access_token_grant(&self, token: &str) -> Option<(String, String, Option<String>)> {
        logged("checking an access token", self.grant_of(token).await)
    }

    async fn revoke_for_user(&self, user_id: &str) -> Result<usize, String> {
        let mut conn = self.connection().await?;
        let tx = conn.transaction().await.map_err(|e| why("could not begin a revocation", &e))?;
        // Waits out any rotation or issue for this account in flight, and
        // holds the next one off until these deletes are committed.
        hold_account(&tx, user_id).await?;
        let tokens = tx
            .execute("delete from oauth.tokens where user_id = $1", &[&user_id])
            .await
            .map_err(|e| why("could not revoke tokens", &e))?;
        let codes = tx
            .execute("delete from oauth.codes where user_id = $1", &[&user_id])
            .await
            .map_err(|e| why("could not revoke codes", &e))?;
        tx.commit().await.map_err(|e| why("could not commit the revocation", &e))?;
        Ok((tokens + codes) as usize)
    }

    async fn sweep(&self) {
        if let Err(error) = self.sweep_now().await {
            tracing::warn!(%error, "OAuth store: sweep failed");
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl super::Backdoor for PostgresOAuth {
    async fn age(&self, rows: super::Aged, by: i64) {
        let sql = match rows {
            super::Aged::Codes => "update oauth.codes set expires_at = expires_at - $1",
            super::Aged::Tokens => "update oauth.tokens set expires_at = expires_at - $1",
            super::Aged::Clients => "update oauth.clients set created_at = created_at - $1",
        };
        let conn = self.connection().await.unwrap();
        conn.execute(sql, &[&by]).await.unwrap();
    }

    async fn stored_tokens(&self) -> Vec<(String, String, String)> {
        let conn = self.connection().await.unwrap();
        conn.query("select token_hash, kind, user_id from oauth.tokens", &[])
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect()
    }
}
