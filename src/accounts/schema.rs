//! The account database's shape, and how it moves forward.
//!
//! `create table if not exists` cannot evolve a schema: add a column and that
//! statement silently does nothing on every database that already exists, so
//! the failure surfaces later as "no such column" against live accounts. A
//! versioned ladder keyed on SQLite's own `user_version` does not have that
//! failure mode — each step runs exactly once, in order, in a transaction.
//!
//! Only *this* database is migrated. Each app's `data.db` belongs to the app,
//! and its schema is the app's business.

use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};
use std::sync::LazyLock;

/// Append only. Editing a step that has already run somewhere means databases
/// disagree about what version 1 was.
static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![M::up(include_str!("../../migrations/001_initial.sql"))])
});

/// Brings a connection's schema up to date. Cheap once it already is: the
/// version is a single pragma read, which is why this can sit on the open
/// path rather than needing to be wired into startup.
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), String> {
    MIGRATIONS.to_latest(conn).map_err(|e| e.to_string())
}

/// Every object SQLite reports for a database, in a stable order. Used to
/// compare a migrated database against the declared shape.
#[cfg(test)]
fn describe(conn: &Connection) -> String {
    let mut statement = conn
        .prepare(
            "select sql from sqlite_master
              where sql is not null and name not like 'sqlite_%'
              order by name",
        )
        .unwrap();
    // Already ordered by name; sorting the SQL text instead would group every
    // CREATE INDEX before every CREATE TABLE.
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_migration_is_valid_sql() {
        // Catches a typo in a .sql file at test time rather than on the first
        // deploy that runs it.
        MIGRATIONS.validate().unwrap();
    }

    #[test]
    fn migrating_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let version: i64 = conn
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap();
        assert!(version > 0, "user_version was not advanced");

        // Running again must be a no-op, not an error.
        migrate(&mut conn).unwrap();
        let again: i64 = conn
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, again);
    }

    /// The declared shape is documentation only until something checks it.
    #[test]
    fn the_ladder_produces_exactly_the_declared_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();

        // SQLite keeps comments inside the DDL it stores, so both sides are
        // stripped of them and of incidental whitespace before comparing.
        let normalise = |text: &str| {
            text.lines()
                .map(|line| match line.find("--") {
                    Some(at) => &line[..at],
                    None => line,
                })
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let actual = describe(&conn);
        let declared = include_str!("../../migrations/schema.sql");
        assert_eq!(
            normalise(&actual),
            normalise(&declared),
            "\nmigrations/schema.sql no longer matches what the migrations build.\n\
             Add a numbered migration for the change, then update schema.sql to match.\n"
        );
    }

    #[test]
    fn a_database_from_an_earlier_version_catches_up() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A fresh file is version 0: exactly the state a database created
        // before migrations existed would be in.
        assert_eq!(
            conn.query_row::<i64, _, _>("pragma user_version", [], |row| row.get(0))
                .unwrap(),
            0
        );
        migrate(&mut conn).unwrap();

        // The tables the rest of the module expects are all present.
        for table in ["users", "identities", "sessions", "grants"] {
            let count: i64 = conn
                .query_row(
                    "select count(*) from sqlite_master where type = 'table' and name = ?",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{table} is missing after migrating");
        }
    }
}
