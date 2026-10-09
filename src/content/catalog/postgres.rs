//! The catalog on Postgres: schema `platform`, one `platform.pages` row per
//! page or app that has a meta, notes or a generation.
//!
//! `meta` keeps the `PageMeta` serde shape as `json`, not `jsonb`: `json`
//! stores the text as written, so every string a meta can hold, `\u0000`
//! included, comes back exactly as it went in. A change holds the row with
//! `select ... for update` after making sure there is one to hold, so the
//! first two changes to a new slug queue like any others.
//!
//! A generation comes from `platform.generations`, one sequence for the
//! site, so no two publishes of any name ever share one.
//!
//! The project tree is `platform.projects`, a row per project. A change
//! takes `LOCK_PROJECTS` for its transaction, reads every row, runs the
//! edit and writes only the rows that differ. A move under way is the one
//! row `platform.relocations` may hold; moves take turns on
//! `LOCK_RELOCATION`, held by a connection kept for the whole move. Host
//! labels are `platform.host_labels`, chosen under `LOCK_LABELS`, with the
//! label as primary key so no label is ever issued to two apps.

use super::{Catalog, FoldersEdit, Held, LabelChoice, MetaEdit, Relocation, Retired};
use crate::{
    content::store::{current_words, Folder, PageMeta},
    state::pg::{LOCK_LABELS, LOCK_PROJECTS, LOCK_RELOCATION},
};
use async_trait::async_trait;
use deadpool_postgres::Pool;
use std::{collections::BTreeMap, path::PathBuf};

pub struct Postgres {
    pool: Pool,
    /// The site's data directory: this process's queue of moves is kept
    /// by it.
    data_dir: PathBuf,
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

fn folder_of(row: &tokio_postgres::Row) -> Folder {
    Folder {
        path: row.get(0),
        name: row.get(1),
        created_at: row.get::<_, i64>(2) as u64,
        locked: row.get(3),
        renamed_from: row.get(4),
        gate: row.get(5),
    }
}

/// Sorted as the file sorts it: by the path's bytes, whatever collation
/// the database was made with.
fn sorted(mut folders: Vec<Folder>) -> Vec<Folder> {
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    folders
}

/// Project moves held: this process's turn, and a pooled connection inside
/// a transaction that took `LOCK_RELOCATION` for every other runner.
/// Committing lets the next move in; a hold dropped without that closes
/// its connection, which ends the transaction and the lock with it, rather
/// than handing a held lock back to the pool.
struct MoveHold(Option<deadpool_postgres::Client>, #[allow(dead_code)] tokio::sync::OwnedMutexGuard<()>);

#[async_trait]
impl Held for MoveHold {
    async fn release(mut self: Box<Self>) {
        let Some(client) = self.0.take() else { return };
        if let Err(e) = client.batch_execute("commit").await {
            tracing::warn!(why = %crate::state::pg::chain(&e), "a hold on project moves did not end cleanly; its connection is closed instead");
            drop(deadpool_postgres::Object::take(client));
        }
    }
}

impl Drop for MoveHold {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            drop(deadpool_postgres::Object::take(client));
        }
    }
}

impl Postgres {
    pub fn new(pool: Pool, data_dir: PathBuf) -> Postgres {
        Postgres { pool, data_dir }
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

    async fn change_folders(&self, edit: FoldersEdit<'_>) -> Result<Vec<Folder>, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin a change to the project tree"))?;
        transaction
            .execute("select pg_advisory_xact_lock($1::int4, 0)", &[&LOCK_PROJECTS])
            .await
            .map_err(failed("hold the project tree"))?;
        let rows = transaction
            .query("select path, name, created_at, locked, renamed_from, gate from platform.projects", &[])
            .await
            .map_err(failed("read the project tree"))?;
        let before = sorted(rows.iter().map(folder_of).collect());
        let mut after = before.clone();
        edit(&mut after)?;
        let kept: Vec<&str> = after.iter().map(|folder| folder.path.as_str()).collect();
        for gone in before.iter().filter(|folder| !kept.contains(&folder.path.as_str())) {
            transaction
                .execute("delete from platform.projects where path = $1", &[&gone.path])
                .await
                .map_err(failed("remove a project"))?;
        }
        for folder in after.iter().filter(|folder| !before.contains(folder)) {
            transaction
                .execute(
                    "insert into platform.projects (path, name, created_at, locked, renamed_from, gate)
                     values ($1, $2, $3, $4, $5, $6)
                     on conflict (path) do update set name = $2, created_at = $3, locked = $4, renamed_from = $5, gate = $6",
                    &[&folder.path, &folder.name, &(folder.created_at as i64), &folder.locked, &folder.renamed_from, &folder.gate],
                )
                .await
                .map_err(failed("store a project"))?;
        }
        transaction.commit().await.map_err(failed("commit a change to the project tree"))?;
        Ok(after)
    }

    async fn read_relocation(&self) -> Result<Option<Relocation>, String> {
        let client = self.client().await?;
        let row = client
            .query_opt("select from_path, to_path from platform.relocations", &[])
            .await
            .map_err(failed("read the record of a project move"))?;
        Ok(row.map(|row| Relocation { from: row.get(0), to: row.get(1) }))
    }

    async fn label_owner(&self, label: &str) -> Result<Option<String>, String> {
        let client = self.client().await?;
        let row = client
            .query_opt("select app from platform.host_labels where label = $1", &[&label])
            .await
            .map_err(failed("read a host label"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn assign_label(&self, app: &str, record: bool, choose: LabelChoice<'_>) -> Result<String, String> {
        let mut client = self.client().await?;
        let transaction = client.transaction().await.map_err(failed("begin a host label"))?;
        transaction
            .execute("select pg_advisory_xact_lock($1::int4, 0)", &[&LOCK_LABELS])
            .await
            .map_err(failed("hold the host labels"))?;
        let rows = transaction
            .query("select label, app from platform.host_labels", &[])
            .await
            .map_err(failed("read the host labels"))?;
        let labels: BTreeMap<String, String> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        let label = choose(&labels);
        if record && labels.get(&label).map(String::as_str) != Some(app) {
            // A plain insert: a label issued to anyone, this app included,
            // is never issued again, so a conflict here is a refusal.
            transaction
                .execute(
                    "insert into platform.host_labels (label, app, issued_at) values ($1, $2, $3)",
                    &[&label, &app, &now()],
                )
                .await
                .map_err(failed("issue a host label"))?;
        }
        transaction.commit().await.map_err(failed("commit a host label"))?;
        Ok(label)
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
                "insert into platform.pages (slug, generation, created_at, updated_at)
                 values ($1, nextval('platform.generations'), $2, $2)
                 on conflict (slug) do update set generation = nextval('platform.generations'), updated_at = $2
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

    fn retire_blocking(&self, slug: &str, at: u64) -> Result<Retired, String> {
        crate::state::wait_in_place(self.retire(slug, at))
    }

    async fn folders(&self) -> Result<Vec<Folder>, String> {
        let client = self.client().await?;
        let rows = client
            .query("select path, name, created_at, locked, renamed_from, gate from platform.projects", &[])
            .await
            .map_err(failed("read the project tree"))?;
        Ok(sorted(rows.iter().map(folder_of).collect()))
    }

    fn folders_blocking(&self) -> Result<Vec<Folder>, String> {
        crate::state::wait_in_place(self.folders())
    }

    async fn update_folders(&self, edit: FoldersEdit<'_>) -> Result<Vec<Folder>, String> {
        self.change_folders(edit).await
    }

    async fn relocation(&self) -> Result<Option<Relocation>, String> {
        self.read_relocation().await
    }

    fn relocation_blocking(&self) -> Result<Option<Relocation>, String> {
        crate::state::wait_in_place(self.read_relocation())
    }

    async fn begin_relocation(&self, from: &str, to: &str) -> Result<(), String> {
        let client = self.client().await?;
        client
            .execute(
                "insert into platform.relocations (one, from_path, to_path, started_at) values (true, $1, $2, $3)
                 on conflict (one) do update set from_path = $1, to_path = $2, started_at = $3",
                &[&from, &to, &now()],
            )
            .await
            .map_err(failed("record a project move"))?;
        Ok(())
    }

    async fn end_relocation(&self) -> Result<(), String> {
        let client = self.client().await?;
        client
            .execute("delete from platform.relocations", &[])
            .await
            .map_err(failed("clear the record of a project move"))?;
        Ok(())
    }

    async fn hold_relocations(&self) -> Result<Box<dyn Held>, String> {
        // Moves in this process queue for their turn before they take a
        // connection. Without it, every move waiting on `LOCK_RELOCATION`
        // would hold a pooled connection while it waited, and enough of them
        // would leave none for the move that holds the lock to do its steps
        // with: a deadlock across the pool.
        let turn = super::take_turn(&self.data_dir).await?;
        let client = self.client().await?;
        // Held in the struct from here, so an error below closes the
        // connection rather than pooling one inside a transaction.
        let hold = MoveHold(Some(client), turn);
        let client = hold.0.as_ref().expect("just set");
        client.batch_execute("begin").await.map_err(failed("begin a hold on project moves"))?;
        client
            .execute("select pg_advisory_xact_lock($1::int4, 0)", &[&LOCK_RELOCATION])
            .await
            .map_err(failed("hold project moves"))?;
        Ok(Box::new(hold))
    }

    fn label_owner_blocking(&self, label: &str) -> Result<Option<String>, String> {
        crate::state::wait_in_place(self.label_owner(label))
    }

    fn assign_label_blocking(&self, app: &str, record: bool, choose: LabelChoice<'_>) -> Result<String, String> {
        crate::state::wait_in_place(self.assign_label(app, record, choose))
    }

    async fn flag(&self, name: &str) -> Result<Option<String>, String> {
        let client = self.client().await?;
        let row = client
            .query_opt("select value from platform.site_flags where name = $1", &[&name])
            .await
            .map_err(failed("read a marker"))?;
        Ok(row.map(|row| row.get(0)))
    }

    async fn set_flag(&self, name: &str, value: &str) -> Result<(), String> {
        let client = self.client().await?;
        client
            .execute(
                "insert into platform.site_flags (name, value, at) values ($1, $2, $3)
                 on conflict (name) do update set value = $2, at = $3",
                &[&name, &value, &now()],
            )
            .await
            .map_err(failed("store a marker"))?;
        Ok(())
    }
}
