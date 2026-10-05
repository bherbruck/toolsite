use crate::{config::Config, content::slug::valid_slug};
use rusqlite::{
    functions::FunctionFlags,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::{ToSqlOutput, Value as SqlValue, ValueRef},
    Connection, OpenFlags,
};
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf};

/// Who a statement runs as, established by the host from a verified session
/// or token and never from the SQL. Bound into the connection as the
/// functions `current_user()`, `current_email()` and `current_role()`, which
/// is what lets a view say whose rows are whose.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub user_id: String,
    pub email: String,
    /// The grant's role on this app, if the account has one.
    pub role: Option<String>,
}

/// Registers the identity functions on a connection. With no identity they
/// answer NULL, so a view written against them shows nobody anything, which
/// is the right answer for a scheduled job or an unscoped admin query.
pub fn bind_identity(conn: &Connection, identity: Option<&Identity>) -> Result<(), String> {
    // Deterministic within a connection, which is all SQLite asks: the value
    // cannot change between two calls in one statement.
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    let user = identity.map(|i| i.user_id.clone());
    let email = identity.map(|i| i.email.clone());
    let role = identity.and_then(|i| i.role.clone());
    conn.create_scalar_function("current_user", 0, flags, move |_| Ok(user.clone()))
        .map_err(|e| e.to_string())?;
    conn.create_scalar_function("current_email", 0, flags, move |_| Ok(email.clone()))
        .map_err(|e| e.to_string())?;
    conn.create_scalar_function("current_role", 0, flags, move |_| Ok(role.clone()))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// SQLite's own page size; the ceiling is expressed to it in pages.
const PAGE_SIZE: u64 = 4096;
/// Cap on rows returned in one call, so a `select *` can't blow up the caller.
pub(crate) const MAX_ROWS: usize = 1_000;
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// Every app gets its own file. The path comes from an already-validated slug
/// and never from anything a caller supplied verbatim.
pub fn db_path(config: &Config, app: &str) -> Option<PathBuf> {
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

/// The app's database with the identity bound and the usual guard in place.
pub(crate) fn open_as(config: &Config, app: &str, identity: Option<&Identity>) -> Result<Connection, String> {
    let path = db_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    let conn = open_unguarded(&path, config.max_db_bytes)?;
    bind_identity(&conn, identity)?;
    lock_down(&conn)?;
    Ok(conn)
}

/// Everything `open_at` does except installing the authorizer. Only the
/// account database uses this, and only long enough to run migrations, which
/// need `pragma user_version` — a pragma the authorizer refuses once it is
/// in place. Never hand a connection from here to a guest.
/// `max_bytes` is the file's ceiling, which SQLite enforces itself through
/// `max_page_count` so a runaway insert fails its own statement instead of
/// filling the volume. Zero leaves SQLite's default, which is no ceiling worth
/// the name.
pub fn open_unguarded(path: &std::path::Path, max_bytes: u64) -> Result<Connection, String> {
    {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|e| e.to_string())?;

    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| e.to_string())?;
    if max_bytes > 0 {
        conn.pragma_update(None, "max_page_count", (max_bytes / PAGE_SIZE).max(1) as i64)
            .map_err(|e| e.to_string())?;
    }
    // Belt and braces alongside the authorizer.
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)
        .map_err(|e| e.to_string())?;

    Ok(conn)
    }
}

/// Closes the door: from here on the connection refuses ATTACH, DETACH and
/// any pragma.
pub fn lock_down(conn: &Connection) -> Result<(), String> {
    conn.authorizer(Some(deny_escapes)).map_err(|e| e.to_string())
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
pub struct SqlOutcome {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
    pub rows_affected: usize,
}

/// Runs caller SQL against one app's database. A single statement may carry
/// bound parameters and return rows; a parameterless script may hold several
/// statements, which is what schema migrations look like.
pub fn run(
    config: &Config,
    app: &str,
    sql: &str,
    params: &[Value],
) -> Result<SqlOutcome, String> {
    run_as(config, app, None, sql, params)
}

/// `run`, with `current_user()` and friends answering for `identity`. This is
/// what a handler's own `db.query` gets: the full database, and a way to ask
/// who is calling from inside SQL.
pub fn run_as(
    config: &Config,
    app: &str,
    identity: Option<&Identity>,
    sql: &str,
    params: &[Value],
) -> Result<SqlOutcome, String> {
    let conn = open_as(config, app, identity)?;
    execute(&conn, sql, params, true)
}

/// What a scoped caller may reach, by name. Everything is matched case
/// insensitively, as SQLite does.
pub struct Scope {
    /// Views that may be read, by their declared names.
    pub readable: HashSet<String>,
    /// Views that may also be inserted into, updated and deleted through.
    pub writable: HashSet<String>,
    /// The platform's own triggers on the writable views, whose base-table
    /// writes are the only ones allowed. Trusted for writes alone: a person
    /// can name a CTE after a trigger, and a CTE can read but never write.
    pub triggers: HashSet<String>,
    /// The salted inner views behind the declared names. A base-table read
    /// is allowed only when SQLite reports it as made through one of these.
    /// The names are the secret: a CTE named after a declared view reads
    /// nothing, because its reads are reported under the declared name, and
    /// the declared name is not in this set.
    pub inner: HashSet<String>,
}

/// Functions a person's SQL may not call. Everything else SQLite ships is
/// pure computation over the row at hand.
const DENIED_FUNCTIONS: [&str; 7] = [
    "load_extension",
    "fts3_tokenizer",
    "readfile",
    "writefile",
    "edit",
    "fsdir",
    "zipfile",
];

impl Scope {
    /// Built from the app's meta: hand-written views are read only, policy
    /// views read and perhaps write, with their generated triggers. Nothing
    /// is reachable until it has been generated, so a declaration that has
    /// not met its table yet opens nothing.
    pub fn of(meta: &crate::content::store::PageMeta) -> Self {
        let mut readable = HashSet::new();
        let mut writable = HashSet::new();
        let mut triggers = HashSet::new();
        let mut inner = HashSet::new();
        let generated = |name: &str| meta.generated.iter().any(|g| g.eq_ignore_ascii_case(name));
        let salt = meta.access_salt.as_deref().unwrap_or("");
        for view in &meta.queryable {
            let behind = crate::runtime::access::inner_name(salt, view);
            if salt.is_empty() || !generated(&behind) {
                continue;
            }
            readable.insert(view.to_lowercase());
            inner.insert(behind.to_lowercase());
        }
        for policy in &meta.policies {
            let behind = crate::runtime::access::inner_name(salt, &policy.view);
            if salt.is_empty() || !generated(&policy.view) || !generated(&behind) {
                continue;
            }
            readable.insert(policy.view.to_lowercase());
            inner.insert(behind.to_lowercase());
            if policy.write {
                writable.insert(policy.view.to_lowercase());
                for trigger in crate::runtime::access::trigger_names(salt, &policy.view) {
                    triggers.insert(trigger.to_lowercase());
                }
            }
        }
        Self {
            readable,
            writable,
            triggers,
            inner,
        }
    }

    fn reads(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        self.readable.contains(&name) || self.writable.contains(&name) || self.inner.contains(&name)
    }

    /// Through an inner view, or through one of the platform's own triggers,
    /// whose collision check has to see every row. Both names are salted.
    fn through_inner(&self, accessor: Option<&str>) -> bool {
        accessor.is_some_and(|a| {
            let a = a.to_lowercase();
            self.inner.contains(&a) || self.triggers.contains(&a)
        })
    }

    /// The authorizer: the whole boundary, decided per parsed action.
    fn authorize(&self, context: AuthContext<'_>) -> Authorization {
        let accessor = context.accessor;
        match context.action {
            AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
            // A column read: of a declared view, of an inner view, or of
            // anything an inner view reaches on the person's behalf. A read
            // made through a declared name or a trigger name is not enough,
            // because both are names a CTE can borrow.
            AuthAction::Read { table_name, .. } => {
                if self.reads(table_name) || self.through_inner(accessor) {
                    Authorization::Allow
                } else {
                    Authorization::Deny
                }
            }
            // A write: by the person on a writable view, or by the view's own
            // trigger on the base table. Nothing else writes.
            AuthAction::Insert { table_name } | AuthAction::Update { table_name, .. } | AuthAction::Delete { table_name } => {
                let own = accessor.is_none() && self.writable.contains(&table_name.to_lowercase());
                let via_trigger = accessor.is_some_and(|a| self.triggers.contains(&a.to_lowercase()));
                if own || via_trigger {
                    Authorization::Allow
                } else {
                    Authorization::Deny
                }
            }
            AuthAction::Function { function_name, .. } => {
                if DENIED_FUNCTIONS.iter().any(|f| f.eq_ignore_ascii_case(function_name)) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            // A scoped call is one statement with the connection's own
            // atomicity; the person does not get to hold a transaction open.
            _ => Authorization::Deny,
        }
    }
}

/// How long one scoped statement may run before it is interrupted, and how
/// big one value or one statement may be. A person typing SQL gets an
/// answer or a refusal, never the server's afternoon.
pub(crate) const SCOPED_WALL_CLOCK: std::time::Duration = std::time::Duration::from_secs(10);
const SCOPED_MAX_VALUE_BYTES: i32 = 16 * 1024 * 1024;
const SCOPED_MAX_SQL_BYTES: i32 = 256 * 1024;
const SCOPED_MAX_COMPOUND: i32 = 50;

/// Runs one statement as `identity`, inside `scope`: the declared views, read
/// or written only as their policies allow, and nothing else in the file. For
/// a person typing SQL, whether through `/me/mcp` or an app that offers it.
pub fn run_scoped(
    config: &Config,
    app: &str,
    identity: Option<&Identity>,
    scope: &Scope,
    sql: &str,
    params: &[Value],
) -> Result<SqlOutcome, String> {
    if scope.readable.is_empty() && scope.writable.is_empty() {
        return Err(format!("{app} declares nothing a person may query"));
    }
    let path = db_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if !path.is_file() {
        return Err(format!("{app} has no database yet"));
    }
    let conn = open_unguarded(&path, config.max_db_bytes)?;
    bind_identity(&conn, identity)?;
    for (limit, value) in [
        (Limit::SQLITE_LIMIT_LENGTH, SCOPED_MAX_VALUE_BYTES),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, SCOPED_MAX_SQL_BYTES),
        (Limit::SQLITE_LIMIT_COMPOUND_SELECT, SCOPED_MAX_COMPOUND),
    ] {
        conn.set_limit(limit, value).map_err(|e| e.to_string())?;
    }
    let started = std::time::Instant::now();
    conn.progress_handler(1_000, Some(move || started.elapsed() > SCOPED_WALL_CLOCK))
        .map_err(|e| e.to_string())?;
    let names = Scope {
        readable: scope.readable.clone(),
        writable: scope.writable.clone(),
        triggers: scope.triggers.clone(),
        inner: scope.inner.clone(),
    };
    conn.authorizer(Some(move |context: AuthContext<'_>| names.authorize(context)))
        .map_err(|e| e.to_string())?;
    execute(&conn, sql, params, false)
}

/// The columns of each named view that exists, for a caller deciding what to
/// ask. Host-run: a person never gets to pragma.
pub fn describe_views(config: &Config, app: &str, views: &[String]) -> Vec<(String, Vec<String>)> {
    let Some(path) = db_path(config, app) else {
        return Vec::new();
    };
    let Ok(conn) = open_unguarded(&path, config.max_db_bytes) else {
        return Vec::new();
    };
    // A view's columns come from preparing its select, which needs the
    // identity functions to exist even though nobody is asking as anyone.
    if bind_identity(&conn, None).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for view in views {
        if !crate::runtime::access::valid_identifier(view) {
            continue;
        }
        let Ok(mut statement) = conn.prepare(&format!("pragma table_info(\"{view}\")")) else {
            continue;
        };
        let columns: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default();
        if !columns.is_empty() {
            out.push((view.clone(), columns));
        }
    }
    out
}

/// One statement with parameters, or a parameterless script when `scripts`
/// is allowed. The scoped path never allows a script: one statement, one
/// decision.
/// SQLite words a refusal differently per action ("access to t.c is
/// prohibited", "not authorized"); callers match on one phrase.
fn describe(error: rusqlite::Error) -> String {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::AuthorizationForStatementDenied =>
        {
            format!("not authorized: {error}")
        }
        _ => error.to_string(),
    }
}

fn execute(conn: &Connection, sql: &str, params: &[Value], scripts: bool) -> Result<SqlOutcome, String> {
    let mut statement = match conn.prepare(sql) {
        Ok(statement) => statement,
        Err(rusqlite::Error::MultipleStatement) if !params.is_empty() || !scripts => {
            return Err("pass one statement at a time".to_string())
        }
        // Preparing a whole script fails as soon as one statement references
        // something an earlier one creates — the classic `create table` then
        // `create index` migration — so run it statement by statement instead.
        // Nothing has executed at this point, so there is nothing to undo.
        Err(prepare_error) if params.is_empty() && scripts => {
            let before = conn.total_changes();
            conn.execute_batch(sql)
                .map_err(|batch_error| match batch_error {
                    // Genuinely broken SQL: report what the batch said, which
                    // names the offending statement.
                    rusqlite::Error::SqliteFailure(..) => describe(batch_error),
                    _ => describe(prepare_error),
                })?;
            return Ok(SqlOutcome {
                columns: Vec::new(),
                rows: Vec::new(),
                truncated: false,
                rows_affected: (conn.total_changes() - before) as usize,
            });
        }
        Err(e) => return Err(describe(e)),
    };
    // EXPLAIN prints the plan, and the plan names the inner views whose
    // names are the scoped boundary's secret. It is for the app's author,
    // through the admin's own path.
    if !scripts && statement.is_explain() != 0 {
        return Err("not authorized: explain is not available here".to_string());
    }

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
        .map_err(describe)?;

    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = cursor.next().map_err(describe)? {
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
        let config = Config::local(dir.path().to_path_buf(), "test-token");
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
        let cap = 8 * 1024 * 1024;
        let config = Config {
            max_db_bytes: cap,
            ..config
        };
        run(&config, "app", "create table big (x blob)", &[]).unwrap();
        let error = run(
            &config,
            "app",
            "insert into big with recursive c(i) as (select 1 union all select i+1 from c where i<20) select randomblob(1000000) from c",
            &[],
        )
        .unwrap_err();
        assert!(error.contains("full"), "got {error:?}");

        let size = std::fs::metadata(dir.path().join("app/data.db")).unwrap().len();
        assert!(size <= cap, "database grew to {size}");
    }

    #[test]
    fn a_cap_of_zero_means_none() {
        let (_dir, config) = config();
        let config = Config {
            max_db_bytes: 0,
            ..config
        };
        run(&config, "app", "create table big (x blob)", &[]).unwrap();
        // Well past the old 64 MB ceiling, in one statement.
        run(
            &config,
            "app",
            "insert into big with recursive c(i) as (select 1 union all select i+1 from c where i<80) select randomblob(1000000) from c",
            &[],
        )
        .unwrap();
    }

    #[test]
    fn an_app_database_is_never_migrated_by_the_platform() {
        let (_dir, config) = config();
        run(&config, "app", "create table t (a)", &[]).unwrap();

        // The account database has a schema the platform owns and versions.
        // An app's database does not: its shape is the app's business, and
        // run_sql and the guest's db.query are the only things that shape it.
        let conn = open_as(&config, "app", None).unwrap();
        let version: i64 = conn
            .query_row("select 1 from sqlite_master where name = 'users'", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        assert_eq!(version, 0, "platform tables appeared in an app database");
    }

    #[test]
    fn rows_affected_is_reported_for_writes() {
        let (_dir, config) = config();
        run(&config, "app", "create table t (a)", &[]).unwrap();
        let out = run(&config, "app", "insert into t values (1)", &[]).unwrap();
        assert_eq!(out.rows_affected, 1);
    }

    // --- identity and scope -------------------------------------------------

    fn alice() -> Identity {
        Identity { user_id: "u-alice".into(), email: "alice@example.com".into(), role: Some("viewer".into()) }
    }

    fn bob() -> Identity {
        Identity { user_id: "u-bob".into(), email: "bob@example.com".into(), role: None }
    }

    fn manager() -> Identity {
        Identity { user_id: "u-boss".into(), email: "boss@example.com".into(), role: Some("manager".into()) }
    }

    /// An orders table with a policy view generated the way the platform does
    /// it, plus a hand-written read-only view.
    fn shop(config: &Config, write: bool) -> Scope {
        run(config, "shop", "create table orders (id integer primary key, owner_id text, total real)", &[]).unwrap();
        run(config, "shop", "insert into orders (owner_id, total) values ('u-alice', 10), ('u-bob', 20), ('u-alice', 30)", &[]).unwrap();
        // The hand-written view, already moved behind its inner view the way
        // regenerate does it.
        run(config, "shop", "create view ts_abc_totals as select owner_id, sum(total) as total from orders group by owner_id; create view totals as select * from \"ts_abc_totals\"", &[]).unwrap();
        let policy = crate::content::store::Policy {
            table: "orders".into(),
            view: "my_orders".into(),
            where_: "owner_id = current_user() or current_role() = 'manager'".into(),
            owner: Some("owner_id".into()),
            write,
        };
        let columns = vec!["id".to_string(), "owner_id".to_string(), "total".to_string()];
        let keys = crate::runtime::access::Keys { rowid_alias: Some("id".into()), unique: Vec::new() };
        run(config, "shop", &crate::runtime::access::generate(&policy, "abc", &columns, &keys), &[]).unwrap();
        // The platform writes these with the authorizer off; the test did the
        // same through run, whose authorizer allows DDL.
        let mut generated = vec!["my_orders".to_string(), "ts_abc_my_orders".to_string(), "ts_abc_totals".to_string()];
        if write {
            generated.extend(crate::runtime::access::trigger_names("abc", "my_orders"));
        }
        let meta = crate::content::store::PageMeta {
            queryable: vec!["totals".into()],
            policies: vec![policy],
            generated,
            access_salt: Some("abc".into()),
            ..Default::default()
        };
        Scope::of(&meta)
    }

    fn scoped(config: &Config, who: &Identity, scope: &Scope, sql: &str) -> Result<SqlOutcome, String> {
        run_scoped(config, "shop", Some(who), scope, sql, &[])
    }

    #[test]
    fn the_identity_functions_answer_for_the_bound_account_and_null_for_nobody() {
        let (_dir, config) = config();
        let out = run_as(&config, "app", Some(&alice()), "select current_user(), current_email(), current_role()", &[]).unwrap();
        assert_eq!(out.rows[0], vec![json!("u-alice"), json!("alice@example.com"), json!("viewer")]);
        let out = run_as(&config, "app", Some(&bob()), "select current_role()", &[]).unwrap();
        assert_eq!(out.rows[0], vec![Value::Null]);
        let out = run(&config, "app", "select current_user(), current_email(), current_role()", &[]).unwrap();
        assert_eq!(out.rows[0], vec![Value::Null, Value::Null, Value::Null]);
    }

    #[test]
    fn a_scoped_read_sees_only_the_bound_accounts_rows() {
        let (_dir, config) = config();
        let scope = shop(&config, false);
        let mine = scoped(&config, &alice(), &scope, "select total from my_orders order by total").unwrap();
        assert_eq!(mine.rows, vec![vec![json!(10.0)], vec![json!(30.0)]]);
        let his = scoped(&config, &bob(), &scope, "select total from my_orders").unwrap();
        assert_eq!(his.rows, vec![vec![json!(20.0)]]);
        // A hand-written view is readable too, and a where on it is fine.
        let totals = scoped(&config, &alice(), &scope, "select total from totals where owner_id = 'u-bob'").unwrap();
        assert_eq!(totals.rows, vec![vec![json!(20.0)]]);
    }

    #[test]
    fn a_scoped_caller_cannot_reach_the_base_table_or_an_undeclared_view() {
        let (_dir, config) = config();
        let scope = shop(&config, true);
        run(&config, "shop", "create view everything as select * from orders", &[]).unwrap();
        for sql in [
            "select * from orders",
            "select count(*) from orders",
            "select * from everything",
            "select name from sqlite_master",
            "select * from my_orders join orders using (id)",
        ] {
            let error = scoped(&config, &alice(), &scope, sql).unwrap_err();
            assert!(error.contains("not authorized"), "{sql} was allowed: {error}");
        }
    }

    #[test]
    fn a_scoped_caller_cannot_write_without_a_writable_policy_or_hold_a_transaction() {
        let (_dir, config) = config();
        let scope = shop(&config, false);
        for sql in [
            "insert into my_orders (owner_id, total) values ('u-alice', 1)",
            "update my_orders set total = 0",
            "delete from my_orders",
            "insert into totals values ('x', 1)",
            "update orders set total = 0",
            "attach database ':memory:' as other",
            "pragma table_info(orders)",
            "begin",
            "savepoint s",
            "create table t (x)",
            "drop view my_orders",
        ] {
            let error = scoped(&config, &alice(), &scope, sql).unwrap_err();
            assert!(
                error.contains("not authorized") || error.contains("cannot modify"),
                "{sql} was allowed: {error}"
            );
        }
        let error = scoped(&config, &alice(), &scope, "select 1; select 2").unwrap_err();
        assert!(error.contains("one statement"), "{error}");
        // Nothing changed underneath.
        let all = run(&config, "shop", "select count(*), sum(total) from orders", &[]).unwrap();
        assert_eq!(all.rows[0], vec![json!(3), json!(60.0)]);
    }

    #[test]
    fn writes_through_a_writable_policy_stay_inside_the_accounts_rows() {
        let (_dir, config) = config();
        let scope = shop(&config, true);

        // An insert gets the account as owner when it leaves it out.
        let inserted = scoped(&config, &alice(), &scope, "insert into my_orders (total) values (5)").unwrap();
        assert_eq!(inserted.rows_affected, 1);
        let owner = run(&config, "shop", "select owner_id from orders where total = 5", &[]).unwrap();
        assert_eq!(owner.rows[0], vec![json!("u-alice")]);

        // An insert for someone else is aborted, and leaves nothing.
        let error = scoped(&config, &alice(), &scope, "insert into my_orders (owner_id, total) values ('u-bob', 99)").unwrap_err();
        assert!(error.contains("not visible to you"), "{error}");
        let count = run(&config, "shop", "select count(*) from orders where total = 99", &[]).unwrap();
        assert_eq!(count.rows[0], vec![json!(0)]);

        // Updating or deleting another account's row changes nothing, and
        // does not say the row exists.
        let update = scoped(&config, &alice(), &scope, "update my_orders set total = 0 where owner_id = 'u-bob'").unwrap();
        assert_eq!(update.rows_affected, 0);
        let delete = scoped(&config, &alice(), &scope, "delete from my_orders where owner_id = 'u-bob'").unwrap();
        assert_eq!(delete.rows_affected, 0);
        let bobs = run(&config, "shop", "select total from orders where owner_id = 'u-bob'", &[]).unwrap();
        assert_eq!(bobs.rows, vec![vec![json!(20.0)]]);

        // Moving a row to someone else is an update that lands outside the
        // view, so it is aborted too.
        let error = scoped(&config, &alice(), &scope, "update my_orders set owner_id = 'u-bob' where total = 10").unwrap_err();
        assert!(error.contains("not visible to you"), "{error}");
        let still = run(&config, "shop", "select owner_id from orders where total = 10", &[]).unwrap();
        assert_eq!(still.rows[0], vec![json!("u-alice")]);

        // Their own rows: fine.
        let update = scoped(&config, &alice(), &scope, "update my_orders set total = 11 where total = 10").unwrap();
        assert_eq!(update.rows_affected, 1);
        let delete = scoped(&config, &alice(), &scope, "delete from my_orders where total = 11").unwrap();
        assert_eq!(delete.rows_affected, 1);
    }

    #[test]
    fn a_role_named_in_the_policy_reaches_everyones_rows() {
        let (_dir, config) = config();
        let scope = shop(&config, true);
        let all = scoped(&config, &manager(), &scope, "select count(*) from my_orders").unwrap();
        assert_eq!(all.rows[0], vec![json!(3)]);
        let update = scoped(&config, &manager(), &scope, "update my_orders set total = total + 1").unwrap();
        assert_eq!(update.rows_affected, 3);
        let bobs = run(&config, "shop", "select total from orders where owner_id = 'u-bob'", &[]).unwrap();
        assert_eq!(bobs.rows, vec![vec![json!(21.0)]]);
    }

    #[test]
    fn a_scope_with_nothing_declared_refuses_everything() {
        let (_dir, config) = config();
        run(&config, "shop", "create table orders (id integer primary key)", &[]).unwrap();
        let scope = Scope::of(&crate::content::store::PageMeta::default());
        let error = scoped(&config, &alice(), &scope, "select 1").unwrap_err();
        assert!(error.contains("declares nothing"), "{error}");
    }
}
