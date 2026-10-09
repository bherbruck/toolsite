//! Tables copied whole between SQLite and Postgres: the accounts and OAuth
//! stores keep the same tables and columns on both backends, so one
//! description of each table is the whole mapping. `toolsite
//! migrate-to-postgres` reads every row through it on one side and writes
//! it on the other; `export-to-files` runs the same mapping back.
//!
//! A table's statements are built from its description, which is constant:
//! no name in them ever comes from a row or a request, and every value is a
//! bound parameter.

/// What a column holds, as both backends can read it: SQLite keeps a flag as
/// an integer, Postgres as a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Int,
    Bool,
}

/// One value of a row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum Cell {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
}

pub type Row = Vec<Cell>;

/// A table as both backends keep it.
#[derive(Debug)]
pub struct Table {
    pub name: &'static str,
    /// The primary key's columns; empty for a table without one, whose rows
    /// are matched on every column.
    pub key: &'static [&'static str],
    pub columns: &'static [(&'static str, Kind)],
}

impl Table {
    fn names(&self) -> String {
        self.columns.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ")
    }

    pub fn sqlite_select(&self) -> String {
        format!("select {} from {}", self.names(), self.name)
    }

    /// Inserts a row, or brings the row with its key up to it.
    pub fn sqlite_upsert(&self) -> String {
        let values = (1..=self.columns.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
        if self.key.is_empty() {
            let same = self
                .columns
                .iter()
                .enumerate()
                .map(|(n, (name, _))| format!("{name} is ?{}", n + 1))
                .collect::<Vec<_>>()
                .join(" and ");
            return format!(
                "insert into {table} ({names}) select {values} where not exists (select 1 from {table} where {same})",
                table = self.name,
                names = self.names(),
            );
        }
        let rest: Vec<&str> = self.columns.iter().map(|(name, _)| *name).filter(|name| !self.key.contains(name)).collect();
        let set = rest.iter().map(|name| format!("{name} = excluded.{name}")).collect::<Vec<_>>().join(", ");
        format!(
            "insert into {table} ({names}) values ({values}) on conflict ({key}) do update set {set}",
            table = self.name,
            names = self.names(),
            key = self.key.join(", "),
        )
    }

    pub fn postgres_select(&self, schema: &str) -> String {
        format!("select {} from {schema}.{}", self.names(), self.name)
    }

    pub fn postgres_upsert(&self, schema: &str) -> String {
        let cast = |n: usize, kind: Kind| match kind {
            Kind::Text => format!("${n}::text"),
            Kind::Int => format!("${n}::bigint"),
            Kind::Bool => format!("${n}::boolean"),
        };
        let values = self.columns.iter().enumerate().map(|(n, (_, kind))| cast(n + 1, *kind)).collect::<Vec<_>>().join(", ");
        if self.key.is_empty() {
            let same = self
                .columns
                .iter()
                .enumerate()
                .map(|(n, (name, kind))| format!("{name} is not distinct from {}", cast(n + 1, *kind)))
                .collect::<Vec<_>>()
                .join(" and ");
            return format!(
                "insert into {schema}.{table} ({names}) select {values} where not exists (select 1 from {schema}.{table} where {same})",
                table = self.name,
                names = self.names(),
            );
        }
        let rest: Vec<&str> = self.columns.iter().map(|(name, _)| *name).filter(|name| !self.key.contains(name)).collect();
        let set = rest.iter().map(|name| format!("{name} = excluded.{name}")).collect::<Vec<_>>().join(", ");
        format!(
            "insert into {schema}.{table} ({names}) values ({values}) on conflict ({key}) do update set {set}",
            table = self.name,
            names = self.names(),
            key = self.key.join(", "),
        )
    }

    /// A row read from SQLite.
    pub fn sqlite_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
        self.columns
            .iter()
            .enumerate()
            .map(|(n, (_, kind))| {
                Ok(match kind {
                    Kind::Text => row.get::<_, Option<String>>(n)?.map_or(Cell::Null, Cell::Text),
                    Kind::Int => row.get::<_, Option<i64>>(n)?.map_or(Cell::Null, Cell::Int),
                    Kind::Bool => row.get::<_, Option<i64>>(n)?.map_or(Cell::Null, |flag| Cell::Bool(flag != 0)),
                })
            })
            .collect()
    }

    /// A row's values bound for SQLite.
    pub fn sqlite_values(&self, row: &Row) -> Result<Vec<rusqlite::types::Value>, String> {
        self.check(row)?;
        Ok(row
            .iter()
            .map(|cell| match cell {
                Cell::Null => rusqlite::types::Value::Null,
                Cell::Int(n) => rusqlite::types::Value::Integer(*n),
                Cell::Text(text) => rusqlite::types::Value::Text(text.clone()),
                Cell::Bool(flag) => rusqlite::types::Value::Integer(*flag as i64),
            })
            .collect())
    }

    /// A row read from Postgres.
    pub fn postgres_row(&self, row: &tokio_postgres::Row) -> Row {
        self.columns
            .iter()
            .enumerate()
            .map(|(n, (_, kind))| match kind {
                Kind::Text => row.get::<_, Option<String>>(n).map_or(Cell::Null, Cell::Text),
                Kind::Int => row.get::<_, Option<i64>>(n).map_or(Cell::Null, Cell::Int),
                Kind::Bool => row.get::<_, Option<bool>>(n).map_or(Cell::Null, Cell::Bool),
            })
            .collect()
    }

    /// A row's values bound for Postgres, each typed as its column is.
    pub fn postgres_values(&self, row: &Row) -> Result<Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>>, String> {
        self.check(row)?;
        Ok(row
            .iter()
            .zip(self.columns)
            .map(|(cell, (_, kind))| -> Box<dyn tokio_postgres::types::ToSql + Sync + Send> {
                match (kind, cell) {
                    (Kind::Text, Cell::Text(text)) => Box::new(Some(text.clone())),
                    (Kind::Text, _) => Box::new(None::<String>),
                    (Kind::Int, Cell::Int(n)) => Box::new(Some(*n)),
                    (Kind::Int, _) => Box::new(None::<i64>),
                    (Kind::Bool, Cell::Bool(flag)) => Box::new(Some(*flag)),
                    (Kind::Bool, _) => Box::new(None::<bool>),
                }
            })
            .collect())
    }

    /// Whether a row fits the table: as many values as columns, each of its
    /// column's kind or null.
    fn check(&self, row: &Row) -> Result<(), String> {
        if row.len() != self.columns.len() {
            return Err(format!("a row of {} has {} values, not {}", self.name, row.len(), self.columns.len()));
        }
        for (cell, (name, kind)) in row.iter().zip(self.columns) {
            let fits = matches!(
                (kind, cell),
                (_, Cell::Null) | (Kind::Text, Cell::Text(_)) | (Kind::Int, Cell::Int(_)) | (Kind::Bool, Cell::Bool(_))
            );
            if !fits {
                return Err(format!("{}.{name} cannot hold {cell:?}", self.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYED: Table = Table { name: "pins", key: &["user_id", "app"], columns: &[("user_id", Kind::Text), ("app", Kind::Text), ("created_at", Kind::Int)] };
    const LOOSE: Table = Table { name: "mfa_failures", key: &[], columns: &[("user_id", Kind::Text), ("at", Kind::Int)] };

    #[test]
    fn an_upsert_sets_every_column_but_the_key() {
        assert_eq!(
            KEYED.sqlite_upsert(),
            "insert into pins (user_id, app, created_at) values (?1, ?2, ?3) on conflict (user_id, app) do update set created_at = excluded.created_at"
        );
        assert_eq!(
            KEYED.postgres_upsert("accounts"),
            "insert into accounts.pins (user_id, app, created_at) values ($1::text, $2::text, $3::bigint) on conflict (user_id, app) do update set created_at = excluded.created_at"
        );
    }

    #[test]
    fn a_table_without_a_key_inserts_only_a_row_it_does_not_have() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("create table mfa_failures (user_id text not null, at integer not null)").unwrap();
        let row = vec![Cell::Text("u".into()), Cell::Int(5)];
        for _ in 0..3 {
            conn.execute(&LOOSE.sqlite_upsert(), rusqlite::params_from_iter(LOOSE.sqlite_values(&row).unwrap())).unwrap();
        }
        let count: i64 = conn.query_row("select count(*) from mfa_failures", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn a_row_that_does_not_fit_its_table_is_refused() {
        assert!(KEYED.sqlite_values(&vec![Cell::Text("u".into())]).is_err());
        assert!(KEYED.sqlite_values(&vec![Cell::Text("u".into()), Cell::Int(1), Cell::Int(1)]).is_err());
        assert!(KEYED.postgres_values(&vec![Cell::Text("u".into()), Cell::Text("a".into()), Cell::Null]).is_ok());
    }
}
