//! Per-app records on Postgres, schema `platform`: `app_settings` (a row per
//! setting, its value sealed by the caller), `app_tools`, `app_migrations`
//! and `repo_links` (a row per app), and `github_installations` (one row).
//!
//! Records are `json`, not `jsonb`, as metas are: `json` keeps the text as
//! written, so a `\u0000` in a tool's description comes back as it went in.
//! A repository link changes under `LOCK_RECORDS` for its app, so the first
//! change to an app with no link yet queues like any other. A removal moves
//! an app's rows into `platform.removed_records`.

use super::{AppRecords, DocEdit};
use crate::state::pg::LOCK_RECORDS;
use async_trait::async_trait;
use deadpool_postgres::Pool;
use std::collections::BTreeMap;

pub struct Postgres {
    pool: Pool,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn failed(what: &str) -> impl Fn(tokio_postgres::Error) -> String + '_ {
    move |e| format!("could not {what}: {}", crate::state::pg::chain(&e))
}

impl Postgres {
    pub fn new(pool: Pool) -> Postgres {
        Postgres { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, String> {
        self.pool
            .get()
            .await
            .map_err(|e| format!("could not reach Postgres for app records: {}", crate::state::pg::chain(&e)))
    }

    async fn retire(&self, app: &str, at: u64) -> Result<Vec<(&'static str, String)>, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin a removal"))?;
        let at = at as i64;
        transaction
            .execute("select pg_advisory_xact_lock($1::int4, hashtext($2))", &[&LOCK_RECORDS, &app])
            .await
            .map_err(failed("hold an app's records"))?;
        let mut out = Vec::new();

        let settings: BTreeMap<String, String> = transaction
            .query("delete from platform.app_settings where app = $1 returning name, sealed", &[&app])
            .await
            .map_err(failed("take an app's settings"))?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        if !settings.is_empty() {
            out.push(("secrets", super::pretty(&settings)?));
        }
        for (extension, text) in [
            (
                "tools",
                transaction
                    .query_opt("delete from platform.app_tools where app = $1 returning tools::text", &[&app])
                    .await
                    .map_err(failed("take an app's tools"))?,
            ),
            (
                "migrations",
                transaction
                    .query_opt("delete from platform.app_migrations where app = $1 returning files::text", &[&app])
                    .await
                    .map_err(failed("take an app's migrations"))?,
            ),
            (
                "repo",
                transaction
                    .query_opt("delete from platform.repo_links where app = $1 returning link::text", &[&app])
                    .await
                    .map_err(failed("take an app's repository link"))?,
            ),
        ] {
            if let Some(row) = text {
                out.push((extension, row.get::<_, String>(0)));
            }
        }
        for (kind, text) in &out {
            transaction
                .execute(
                    "insert into platform.removed_records (app, kind, record, removed_at) values ($1, $2, $3::text::json, $4)",
                    &[&app, kind, text, &at],
                )
                .await
                .map_err(failed("keep an app's removed records"))?;
        }
        transaction.commit().await.map_err(failed("commit a removal"))?;
        Ok(out)
    }
}

#[async_trait]
impl AppRecords for Postgres {
    async fn settings(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        let rows = self
            .client()
            .await?
            .query("select name, sealed from platform.app_settings where app = $1", &[&app])
            .await
            .map_err(failed("read an app's settings"))?;
        Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    fn settings_blocking(&self, app: &str) -> Result<BTreeMap<String, String>, String> {
        crate::state::wait_in_place(self.settings(app))
    }

    async fn set_setting(&self, app: &str, name: &str, sealed: Option<&str>) -> Result<bool, String> {
        let client = self.client().await?;
        match sealed {
            Some(sealed) => {
                // `xmax` is not zero on a row an upsert updated, so this says
                // whether a value was replaced without a second statement.
                let row = client
                    .query_one(
                        "insert into platform.app_settings (app, name, sealed, updated_at) values ($1, $2, $3, $4)
                         on conflict (app, name) do update set sealed = excluded.sealed, updated_at = excluded.updated_at
                         returning xmax <> 0",
                        &[&app, &name, &sealed, &now()],
                    )
                    .await
                    .map_err(failed("store a setting"))?;
                Ok(row.get(0))
            }
            None => Ok(client
                .execute("delete from platform.app_settings where app = $1 and name = $2", &[&app, &name])
                .await
                .map_err(failed("remove a setting"))?
                > 0),
        }
    }

    async fn tools(&self, app: &str) -> Result<Option<String>, String> {
        let row = self
            .client()
            .await?
            .query_opt("select tools::text from platform.app_tools where app = $1", &[&app])
            .await
            .map_err(failed("read an app's tools"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn set_tools(&self, app: &str, tools: Option<&str>) -> Result<(), String> {
        let client = self.client().await?;
        match tools {
            Some(tools) => client
                .execute(
                    "insert into platform.app_tools (app, tools, updated_at) values ($1, $2::text::json, $3)
                     on conflict (app) do update set tools = excluded.tools, updated_at = excluded.updated_at",
                    &[&app, &tools, &now()],
                )
                .await
                .map_err(failed("store an app's tools"))?,
            None => client
                .execute("delete from platform.app_tools where app = $1", &[&app])
                .await
                .map_err(failed("remove an app's tools"))?,
        };
        Ok(())
    }

    async fn apps_with_tools(&self) -> Result<Vec<String>, String> {
        let rows = self
            .client()
            .await?
            .query("select app from platform.app_tools", &[])
            .await
            .map_err(failed("list the apps with tools"))?;
        let mut apps: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        // By the name's bytes, as a directory listing sorts, whatever
        // collation the database was made with.
        apps.sort();
        Ok(apps)
    }

    fn migrations_blocking(&self, app: &str) -> Result<Option<String>, String> {
        crate::state::wait_in_place(async {
            let row = self
                .client()
                .await?
                .query_opt("select files::text from platform.app_migrations where app = $1", &[&app])
                .await
                .map_err(failed("read an app's migrations"))?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn set_migrations_blocking(&self, app: &str, ladder: &str) -> Result<(), String> {
        crate::state::wait_in_place(async {
            self.client()
                .await?
                .execute(
                    "insert into platform.app_migrations (app, files, updated_at) values ($1, $2::text::json, $3)
                     on conflict (app) do update set files = excluded.files, updated_at = excluded.updated_at",
                    &[&app, &ladder, &now()],
                )
                .await
                .map_err(failed("store an app's migrations"))?;
            Ok(())
        })
    }

    async fn repo_link(&self, app: &str) -> Result<Option<String>, String> {
        let row = self
            .client()
            .await?
            .query_opt("select link::text from platform.repo_links where app = $1", &[&app])
            .await
            .map_err(failed("read an app's repository link"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn update_repo_link(&self, app: &str, edit: DocEdit<'_>) -> Result<String, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin a repository link change"))?;
        transaction
            .execute("select pg_advisory_xact_lock($1::int4, hashtext($2))", &[&LOCK_RECORDS, &app])
            .await
            .map_err(failed("hold an app's records"))?;
        let current: Option<String> = transaction
            .query_opt("select link::text from platform.repo_links where app = $1", &[&app])
            .await
            .map_err(failed("read an app's repository link"))?
            .map(|row| row.get(0));
        let next = edit(current.as_deref())?;
        transaction
            .execute(
                "insert into platform.repo_links (app, link, updated_at) values ($1, $2::text::json, $3)
                 on conflict (app) do update set link = excluded.link, updated_at = excluded.updated_at",
                &[&app, &next, &now()],
            )
            .await
            .map_err(failed("store an app's repository link"))?;
        transaction.commit().await.map_err(failed("commit a repository link change"))?;
        Ok(next)
    }

    async fn repo_links(&self) -> Result<Vec<(String, String)>, String> {
        let rows = self
            .client()
            .await?
            .query("select app, link::text from platform.repo_links", &[])
            .await
            .map_err(failed("list the repository links"))?;
        let mut links: Vec<(String, String)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        links.sort();
        Ok(links)
    }

    async fn installations(&self) -> Result<Option<String>, String> {
        let row = self
            .client()
            .await?
            .query_opt("select installations::text from platform.github_installations", &[])
            .await
            .map_err(failed("read the GitHub installations"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn set_installations(&self, installations: &str) -> Result<(), String> {
        self.client()
            .await?
            .execute(
                "insert into platform.github_installations (one, installations, fetched_at) values (true, $1::text::json, $2)
                 on conflict (one) do update set installations = excluded.installations, fetched_at = excluded.fetched_at",
                &[&installations, &now()],
            )
            .await
            .map_err(failed("store the GitHub installations"))?;
        Ok(())
    }

    fn retire_blocking(&self, app: &str, at: u64) -> Result<Vec<(&'static str, String)>, String> {
        crate::state::wait_in_place(self.retire(app, at))
    }
}
