use crate::{config::Config, slug::valid_slug};
use rusqlite::{
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::{ToSqlOutput, Value as SqlValue, ValueRef},
    Connection, OpenFlags,
};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Per-app ceiling. SQLite enforces it itself via max_page_count, so a runaway
/// insert fails its statement instead of filling the volume.
pub(crate) const MAX_DB_BYTES: u64 = 64 * 1024 * 1024;
const PAGE_SIZE: u64 = 4096;
/// Cap on rows returned in one call, so a `select *` can't blow up the caller.
pub(crate) const MAX_ROWS: usize = 1_000;
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// Every app gets its own file. The path comes from an already-validated slug
/// and never from anything a caller supplied verbatim.
pub(crate) fn db_path(config: &Config, app: &str) -> Option<PathBuf> {
    valid_slug(app).then(|| config.data_dir.join(app).join("data.db"))
}

/// Blocks any statement that could reach outside this one file. Done with
/// SQLite's authorizer rather than by inspecting the SQL, because the
/// authorizer sees the parsed action and can't be talked out of it by
/// creative formatting.
fn deny_escapes(context: AuthContext<'_>) -> Authorization {
    match context.action {
        // ATTACH is path traversal expressed in SQL: it would open another
        // app's database through a connection that looks correctly scoped.
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        // The host sets its own pragmas before installing this, so any pragma
        // reaching here came from caller SQL.
        AuthAction::Pragma { .. } => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

pub(crate) fn open(config: &Config, app: &str) -> Result<Connection, String> {
    let path = db_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|e| e.to_string())?;

    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "max_page_count", (MAX_DB_BYTES / PAGE_SIZE) as i64)
        .map_err(|e| e.to_string())?;
    // Belt and braces alongside the authorizer.
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)
        .map_err(|e| e.to_string())?;

    conn.authorizer(Some(deny_escapes))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn to_sql(value: &Value) -> Result<ToSqlOutput<'static>, String> {
    let owned = match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => SqlValue::Integer(i),
            (None, Some(f)) => SqlValue::Real(f),
            _ => return Err(format!("unsupported number: {n}")),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        other => return Err(format!("parameters must be scalars, got {other}")),
    };
    Ok(ToSqlOutput::Owned(owned))
}

fn from_sql(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        // Blobs are reported by size rather than dumped into a JSON response.
        ValueRef::Blob(b) => json!(format!("<{} byte blob>", b.len())),
    }
}

#[derive(Debug)]
pub(crate) struct SqlOutcome {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) truncated: bool,
    pub(crate) rows_affected: usize,
}

/// Runs caller SQL against one app's database. A single statement may carry
/// bound parameters and return rows; a parameterless script may hold several
/// statements, which is what schema migrations look like.
pub(crate) fn run(
    config: &Config,
    app: &str,
    sql: &str,
    params: &[Value],
) -> Result<SqlOutcome, String> {
    let conn = open(config, app)?;

    let mut statement = match conn.prepare(sql) {
        Ok(statement) => statement,
        Err(rusqlite::Error::MultipleStatement) if !params.is_empty() => {
            return Err("pass one statement at a time when using parameters".to_string())
        }
        // Preparing a whole script fails as soon as one statement references
        // something an earlier one creates — the classic `create table` then
        // `create index` migration — so run it statement by statement instead.
        // Nothing has executed at this point, so there is nothing to undo.
        Err(prepare_error) if params.is_empty() => {
            let before = conn.total_changes();
            conn.execute_batch(sql)
                .map_err(|batch_error| match batch_error {
                    // Genuinely broken SQL: report what the batch said, which
                    // names the offending statement.
                    rusqlite::Error::SqliteFailure(..) => batch_error.to_string(),
                    _ => prepare_error.to_string(),
                })?;
            return Ok(SqlOutcome {
                columns: Vec::new(),
                rows: Vec::new(),
                truncated: false,
                rows_affected: (conn.total_changes() - before) as usize,
            });
        }
        Err(e) => return Err(e.to_string()),
    };

    let bound: Vec<ToSqlOutput<'static>> = params
        .iter()
        .map(to_sql)
        .collect::<Result<_, _>>()?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();

    let before = conn.total_changes();
    let mut cursor = statement
        .query(rusqlite::params_from_iter(bound.iter()))
        .map_err(|e| e.to_string())?;

    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = cursor.next().map_err(|e| e.to_string())? {
        if rows.len() >= MAX_ROWS {
            truncated = true;
            break;
        }
        let values = (0..columns.len())
            .map(|i| row.get_ref(i).map(from_sql).unwrap_or(Value::Null))
            .collect();
        rows.push(values);
    }
    drop(cursor);

    Ok(SqlOutcome {
        columns,
        rows,
        truncated,
        rows_affected: (conn.total_changes() - before) as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::local(dir.path().to_path_buf(), "test-token", true);
        (dir, config)
    }

    #[test]
    fn migrations_run_as_scripts_and_are_idempotent() {
        let (_dir, config) = config();
        let migration = "create table if not exists todos (id integer primary key, body text);\
                         create index if not exists todos_body on todos(body);";
        run(&config, "app", migration, &[]).unwrap();
        // The second run must not error, which is what makes redeploys safe.
        run(&config, "app", migration, &[]).unwrap();

        let out = run(&config, "app", "select name from sqlite_master order by name", &[]).unwrap();
        let names: Vec<_> = out.rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(names, ["todos", "todos_body"]);
    }

    #[test]
    fn parameters_are_bound_not_interpolated() {
        let (_dir, config) = config();
        run(&config, "app", "create table t (body text)", &[]).unwrap();
        // A classic injection payload must land as literal text.
        let payload = "'); drop table t; --";
        run(
            &config,
            "app",
            "insert into t (body) values (?)",
            &[json!(payload)],
        )
        .unwrap();

        let out = run(&config, "app", "select body from t", &[]).unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0].as_str().unwrap(), payload);
    }

    #[test]
    fn each_app_gets_its_own_file() {
        let (_dir, config) = config();
        run(&config, "one", "create table only_in_one (a)", &[]).unwrap();
        run(&config, "two", "create table only_in_two (a)", &[]).unwrap();

        let out = run(&config, "two", "select name from sqlite_master", &[]).unwrap();
        let names: Vec<_> = out.rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(names, ["only_in_two"]);
    }

    #[test]
    fn attach_is_refused_so_sql_cannot_reach_another_app() {
        let (_dir, config) = config();
        run(&config, "victim", "create table secrets (a)", &[]).unwrap();
        run(&config, "attacker", "create table t (a)", &[]).unwrap();

        for attempt in [
            "attach database '../victim/data.db' as v",
            "attach database '/etc/passwd' as p",
            "ATTACH DATABASE '../victim/data.db' AS v",
        ] {
            let error = run(&config, "attacker", attempt, &[]).unwrap_err();
            assert!(
                error.contains("not authorized"),
                "{attempt:?} gave {error:?}"
            );
        }
    }

    #[test]
    fn pragmas_are_refused() {
        let (_dir, config) = config();
        let error = run(&config, "app", "pragma journal_mode=delete", &[]).unwrap_err();
        assert!(error.contains("not authorized"), "got {error:?}");
    }

    #[test]
    fn an_invalid_app_name_never_reaches_the_filesystem() {
        let (_dir, config) = config();
        assert!(db_path(&config, "../etc").is_none());
        assert!(db_path(&config, "ok/name").is_some());
        assert!(run(&config, "../etc", "select 1", &[]).is_err());
    }

    #[test]
    fn reads_are_capped_and_say_so() {
        let (_dir, config) = config();
        run(&config, "app", "create table n (i integer)", &[]).unwrap();
        run(
            &config,
            "app",
            "insert into n with recursive c(i) as (select 1 union all select i+1 from c where i<1500) select i from c",
            &[],
        )
        .unwrap();

        let out = run(&config, "app", "select i from n", &[]).unwrap();
        assert_eq!(out.rows.len(), MAX_ROWS);
        assert!(out.truncated);
    }

    #[test]
    fn writes_stop_at_the_size_cap_instead_of_filling_the_volume() {
        let (dir, config) = config();
        run(&config, "app", "create table big (x blob)", &[]).unwrap();
        let error = run(
            &config,
            "app",
            "insert into big with recursive c(i) as (select 1 union all select i+1 from c where i<80) select randomblob(1000000) from c",
            &[],
        )
        .unwrap_err();
        assert!(error.contains("full"), "got {error:?}");

        let size = std::fs::metadata(dir.path().join("app/data.db")).unwrap().len();
        assert!(size < MAX_DB_BYTES, "database grew to {size}");
    }

    #[test]
    fn rows_affected_is_reported_for_writes() {
        let (_dir, config) = config();
        run(&config, "app", "create table t (a)", &[]).unwrap();
        let out = run(&config, "app", "insert into t values (1)", &[]).unwrap();
        assert_eq!(out.rows_affected, 1);
    }
}
