//! The catalog on Postgres: schema `platform`, one `platform.pages` row per
//! page or app that has a meta, notes or a generation.
//!
//! `meta` keeps the `PageMeta` serde shape as `json`, not `jsonb`: `json`
//! stores the text as written, so every string a meta can hold, `\u0000`
//! included, comes back exactly as it went in. A change holds the row with
//! `select ... for update` after making sure there is one to hold, so the
//! first two changes to a new slug queue like any others.
//!
//! Which slugs exist is still read from the volume: pages and bundles stay
//! files until the `Files` store moves them, and a publish registers its
//! row there.

use super::{files, Catalog, MetaEdit, Retired};
use crate::content::store::{current_words, PageMeta};
use async_trait::async_trait;
use deadpool_postgres::Pool;
use std::path::PathBuf;

pub struct Postgres {
    pool: Pool,
    /// Where the published files still are, for listing them.
    files: files::Files,
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

/// A stored meta. Unlike a sidecar, a row that does not parse is an error,
/// not the default: the default is an open, listed page.
fn parse(slug: &str, text: &str) -> Result<PageMeta, String> {
    serde_json::from_str(text)
        .map(current_words)
        .map_err(|e| format!("the stored meta of {slug} is not a meta: {e}"))
}

impl Postgres {
    pub fn new(pool: Pool, data_dir: PathBuf) -> Postgres {
        Postgres { pool, files: files::Files::new(data_dir) }
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, String> {
        self.pool
            .get()
            .await
            .map_err(|e| format!("could not reach Postgres for the catalog: {}", crate::state::pg::chain(&e)))
    }

    async fn retire(&self, slug: &str, at: u64) -> Result<Retired, String> {
        let client = self.client().await?;
        // Rows under the slug are matched by prefix with `left`, not `like`:
        // `_` is a slug character and a `like` wildcard.
        let rows = client
            .query(
                "with gone as (
                     delete from platform.pages
                      where slug = $1 or left(slug, char_length($1) + 1) = $1 || '/'
                     returning slug, meta, notes, generation, created_at
                 ), kept as (
                     insert into platform.removed_pages (slug, meta, notes, generation, created_at, removed_at)
                     select slug, meta, notes, generation, created_at, $2 from gone
                     returning slug, meta, notes
                 )
                 select slug, meta::text, notes from kept order by slug",
                &[&slug, &(at as i64)],
            )
            .await
            .map_err(failed("take a removed app out of the catalog"))?;
        Ok(Retired { pages: rows.iter().map(|row| (row.get(0), row.get(1), row.get(2))).collect() })
    }
}

#[async_trait]
impl Catalog for Postgres {
    async fn meta(&self, slug: &str) -> Result<PageMeta, String> {
        let client = self.client().await?;
        let row = client
            .query_opt("select meta::text from platform.pages where slug = $1", &[&slug])
            .await
            .map_err(failed("read a meta"))?;
        match row {
            Some(row) => parse(slug, &row.get::<_, String>(0)),
            None => Ok(PageMeta::default()),
        }
    }

    fn meta_blocking(&self, slug: &str) -> Result<PageMeta, String> {
        crate::state::wait_in_place(self.meta(slug))
    }

    async fn update_meta(&self, slug: &str, edit: MetaEdit<'_>) -> Result<PageMeta, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin a meta change"))?;
        let at = now();
        transaction
            .execute(
                "insert into platform.pages (slug, created_at, updated_at) values ($1, $2, $2)
                 on conflict (slug) do nothing",
                &[&slug, &at],
            )
            .await
            .map_err(failed("make room for a meta"))?;
        let row = transaction
            .query_one("select meta::text from platform.pages where slug = $1 for update", &[&slug])
            .await
            .map_err(failed("hold a meta"))?;
        let mut meta = parse(slug, &row.get::<_, String>(0))?;
        edit(&mut meta)?;
        let meta = current_words(meta);
        let json = serde_json::to_string(&meta).map_err(|e| e.to_string())?;
        transaction
            .execute(
                "update platform.pages set meta = $2::text::json, updated_at = $3 where slug = $1",
                &[&slug, &json, &at],
            )
            .await
            .map_err(failed("store a meta"))?;
        transaction.commit().await.map_err(failed("commit a meta change"))?;
        Ok(meta)
    }

    fn update_meta_blocking(&self, slug: &str, edit: MetaEdit<'_>) -> Result<PageMeta, String> {
        crate::state::wait_in_place(self.update_meta(slug, edit))
    }

    async fn generation(&self, app: &str) -> Result<u64, String> {
        let client = self.client().await?;
        let row = client
            .query_opt("select generation from platform.pages where slug = $1", &[&app])
            .await
            .map_err(failed("read a generation"))?;
        Ok(row.map(|row| row.get::<_, i64>(0) as u64).unwrap_or(0))
    }

    async fn bump_generation(&self, app: &str) -> Result<u64, String> {
        let client = self.client().await?;
        let row = client
            .query_one(
                "insert into platform.pages (slug, generation, created_at, updated_at) values ($1, 1, $2, $2)
                 on conflict (slug) do update set generation = platform.pages.generation + 1, updated_at = $2
                 returning generation",
                &[&app, &now()],
            )
            .await
            .map_err(failed("count a publish"))?;
        Ok(row.get::<_, i64>(0) as u64)
    }

    async fn notes(&self, slug: &str) -> Result<Option<String>, String> {
        let client = self.client().await?;
        let row = client
            .query_opt("select notes from platform.pages where slug = $1", &[&slug])
            .await
            .map_err(failed("read notes"))?;
        Ok(row.and_then(|row| row.get::<_, Option<String>>(0)))
    }

    async fn set_notes(&self, slug: &str, notes: &str) -> Result<(), String> {
        let client = self.client().await?;
        client
            .execute(
                "insert into platform.pages (slug, notes, created_at, updated_at) values ($1, $2, $3, $3)
                 on conflict (slug) do update set notes = $2, updated_at = $3",
                &[&slug, &notes, &now()],
            )
            .await
            .map_err(failed("store notes"))?;
        Ok(())
    }

    async fn slugs(&self) -> Result<Vec<String>, String> {
        self.files.slugs().await
    }

    async fn apps(&self) -> Result<Vec<String>, String> {
        self.files.apps().await
    }

    fn retire_blocking(&self, slug: &str, at: u64) -> Result<Retired, String> {
        crate::state::wait_in_place(self.retire(slug, at))
    }
}
