//! An app's own schema, versioned the way the platform's is.
//!
//! `create table if not exists` in a handler cannot evolve anything: add a
//! column next month and that statement quietly does nothing on every
//! database that already exists. So an app ships numbered migrations with its
//! source, and they run at deploy — each once, in order, in a transaction,
//! tracked by SQLite's own `user_version`.
//!
//! The platform never reads what they create. Tables, columns and meaning are
//! entirely the app's business; only the ladder is ours.

use crate::{config::Config, content::slug::valid_slug, runtime::db};
use rusqlite_migration::{Migrations, M};

/// Numbered files, in the order their names sort — `001_initial.sql`,
/// `002_add_column.sql`. Kept in the records store (a sidecar on files)
/// rather than inside the app directory, so no spelling of a URL reaches an
/// app's DDL.
pub fn store(config: &Config, app: &str, files: Vec<(String, String)>) -> Result<(), String> {
    if !valid_slug(app) {
        return Err(format!("invalid app name '{app}'"));
    }
    let mut files = files;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // Rejected here rather than at the first request that needs a table.
    for (name, sql) in &files {
        if sql.trim().is_empty() {
            return Err(format!("{name} is empty"));
        }
    }
    let json = crate::platform::records::pretty(&files)?;
    crate::platform::records::of(config).set_migrations_blocking(app, &json)
}

/// The app's ladder; none, logged, when it cannot be read.
pub fn stored(config: &Config, app: &str) -> Vec<(String, String)> {
    if !valid_slug(app) {
        return Vec::new();
    }
    match crate::platform::records::of(config).migrations_blocking(app) {
        Ok(text) => text.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default(),
        Err(why) => {
            tracing::warn!(app, %why, "an app's migrations could not be read");
            Vec::new()
        }
    }
}

/// Brings an app's database up to its latest migration, returning the version
/// it reached, how many steps ran, and anything worth saying about the
/// access policies that were rebuilt on the new schema.
pub fn apply(config: &Config, app: &str) -> Result<(usize, usize, Vec<String>), String> {
    let files = stored(config, app);
    if files.is_empty() {
        return Ok((0, 0, Vec::new()));
    }
    let path = db::app_db(config, app)?;

    // Migrations read `pragma user_version`, which the authorizer refuses, so
    // the schema moves before the door closes — exactly as for the account
    // database.
    let mut conn = db::open_unguarded(&path, config.max_db_bytes)?;
    let before: usize = conn
        .query_row("pragma user_version", [], |row| row.get::<_, i64>(0))
        .map(|version| version as usize)
        .unwrap_or(0);

    let steps: Vec<M> = files.iter().map(|(_, sql)| M::up(sql)).collect();
    let migrations = Migrations::new(steps);
    migrations
        .to_latest(&mut conn)
        .map_err(|e| format!("migration failed: {e}"))?;

    let after: usize = conn
        .query_row("pragma user_version", [], |row| row.get::<_, i64>(0))
        .map(|version| version as usize)
        .unwrap_or(0);
    db::lock_down(&conn)?;
    drop(conn);

    // The schema moved, so the generated views are rebuilt on it: a column
    // added here reaches `select *` only through a fresh `create view`.
    let meta = crate::content::catalog::meta_blocking(config, app);
    let mut notes = Vec::new();
    if !meta.policies.is_empty() || !meta.queryable.is_empty() || !meta.generated.is_empty() {
        let regenerated = crate::runtime::access::regenerate(config, app, &meta)?;
        notes = regenerated.notes;
        let salt = Some(regenerated.salt);
        if meta.generated != regenerated.generated || meta.access_salt != salt {
            // Only what the views made is written back, so a change to the
            // rest of the meta meanwhile is kept.
            let generated = regenerated.generated;
            crate::content::catalog::update_meta_blocking(config, app, move |meta| {
                meta.generated = generated;
                meta.access_salt = salt;
                Ok(())
            })?;
        }
    }
    Ok((after, after.saturating_sub(before), notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        (
            tempfile::tempdir().unwrap(),
            Config::local(dir.keep(), "test-token"),
        )
    }

    fn tables(config: &Config, app: &str) -> Vec<String> {
        db::run(config, app, "select name from sqlite_master where type='table' order by name", &[])
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row[0].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn migrations_run_in_order_and_only_once() {
        let (_t, config) = config();
        store(
            &config,
            "app",
            vec![
                ("001_initial.sql".into(), "create table todos (id integer primary key)".into()),
                ("002_body.sql".into(), "alter table todos add column body text".into()),
            ],
        )
        .unwrap();

        let (version, ran, _) = apply(&config, "app").unwrap();
        assert_eq!((version, ran), (2, 2));
        // Running again is a no-op rather than an error, which is what makes
        // a redeploy safe.
        assert_eq!(apply(&config, "app").map(|(v, r, _)| (v, r)).unwrap(), (2, 0));

        db::run(&config, "app", "insert into todos (body) values (?)", &[serde_json::json!("x")])
            .unwrap();
    }

    #[test]
    fn a_column_added_later_reaches_a_database_that_already_existed() {
        let (_t, config) = config();
        store(
            &config,
            "app",
            vec![("001.sql".into(), "create table todos (id integer primary key)".into())],
        )
        .unwrap();
        apply(&config, "app").unwrap();
        db::run(&config, "app", "insert into todos default values", &[]).unwrap();

        // The case `create table if not exists` gets wrong: the table is
        // already there, so nothing would happen.
        store(
            &config,
            "app",
            vec![
                ("001.sql".into(), "create table todos (id integer primary key)".into()),
                ("002.sql".into(), "alter table todos add column body text".into()),
            ],
        )
        .unwrap();
        let (version, ran, _) = apply(&config, "app").unwrap();
        assert_eq!((version, ran), (2, 1), "the new step did not run");

        let rows = db::run(&config, "app", "select body from todos", &[]).unwrap();
        assert_eq!(rows.rows.len(), 1, "the existing row was lost");
    }

    #[test]
    fn a_broken_migration_leaves_the_database_as_it_was() {
        let (_t, config) = config();
        store(
            &config,
            "app",
            vec![
                ("001.sql".into(), "create table good (a)".into()),
                ("002.sql".into(), "this is not sql".into()),
            ],
        )
        .unwrap();

        let error = apply(&config, "app").unwrap_err();
        assert!(error.contains("migration failed"), "got {error}");
        // The first step is not left half-applied for the next deploy to trip
        // over.
        assert!(tables(&config, "app").is_empty());
    }

    #[test]
    fn each_app_has_its_own_schema() {
        let (_t, config) = config();
        store(&config, "mine", vec![("001.sql".into(), "create table mine (a)".into())]).unwrap();
        apply(&config, "mine").unwrap();
        assert!(stored(&config, "theirs").is_empty());
        assert_eq!(apply(&config, "theirs").map(|(v, r, _)| (v, r)).unwrap(), (0, 0));
    }
}
