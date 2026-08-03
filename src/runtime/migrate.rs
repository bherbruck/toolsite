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
use std::path::PathBuf;

/// Kept as a sidecar rather than inside the app directory, so no spelling of
/// a URL reaches an app's DDL.
fn path(config: &Config, app: &str) -> Option<PathBuf> {
    valid_slug(app).then(|| config.data_dir.join(format!("{app}.migrations")))
}

/// Numbered files, in the order their names sort — `001_initial.sql`,
/// `002_add_column.sql`.
pub fn store(config: &Config, app: &str, files: Vec<(String, String)>) -> Result<(), String> {
    let path = path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    let mut files = files;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // Rejected here rather than at the first request that needs a table.
    for (name, sql) in &files {
        if sql.trim().is_empty() {
            return Err(format!("{name} is empty"));
        }
    }
    let json = serde_json::to_string_pretty(&files).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, json).map_err(|e| e.to_string())
}

pub fn stored(config: &Config, app: &str) -> Vec<(String, String)> {
    let Some(path) = path(config, app) else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Brings an app's database up to its latest migration, returning the version
/// it reached and how many steps ran.
pub fn apply(config: &Config, app: &str) -> Result<(usize, usize), String> {
    let files = stored(config, app);
    if files.is_empty() {
        return Ok((0, 0));
    }
    let path = db::db_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;

    // Migrations read `pragma user_version`, which the authorizer refuses, so
    // the schema moves before the door closes — exactly as for the account
    // database.
    let mut conn = db::open_unguarded(&path)?;
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
    Ok((after, after.saturating_sub(before)))
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

        let (version, ran) = apply(&config, "app").unwrap();
        assert_eq!((version, ran), (2, 2));
        // Running again is a no-op rather than an error, which is what makes
        // a redeploy safe.
        assert_eq!(apply(&config, "app").unwrap(), (2, 0));

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
        let (version, ran) = apply(&config, "app").unwrap();
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
        assert_eq!(apply(&config, "theirs").unwrap(), (0, 0));
    }
}
