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
