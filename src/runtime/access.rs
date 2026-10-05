//! Row-level access an app declares instead of writing.
//!
//! A policy names a table and a `where` that says which rows the signed-in
//! person may see, written against the app's own tables and the identity
//! functions the host binds: `current_user()`, `current_email()`,
//! `current_role()`. From it the platform generates a view, and when the
//! policy says `write = true`, three `instead of` triggers that carry an
//! insert, update or delete through to the base table and abort when the
//! result would be a row the person cannot see.
//!
//! The generated objects belong to the platform. They are rebuilt wholesale
//! whenever the manifest or the schema changes, so a column added by a later
//! migration shows up in the view, and a policy that is removed takes its
//! view and triggers with it. An app never edits them; it edits the policy.

use crate::{
    config::Config,
    content::store::{Policy, PageMeta},
    runtime::db,
};
use rusqlite::Connection;
use std::collections::HashSet;

/// Every generated trigger carries this prefix, which is also how the scoped
/// authorizer recognises a base-table write as the platform's own.
pub const TRIGGER_PREFIX: &str = "ts_access_";

pub fn trigger_names(view: &str) -> [String; 3] {
    [
        format!("{TRIGGER_PREFIX}{view}_insert"),
        format!("{TRIGGER_PREFIX}{view}_update"),
        format!("{TRIGGER_PREFIX}{view}_delete"),
    ]
}

/// A SQL identifier the generator will quote and the authorizer will match:
/// letters, digits and underscores, not starting with a digit.
pub fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 64
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The columns of a table, in declaration order, from the database itself.
fn columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({})", quote(table)))
        .map_err(|e| e.to_string())?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(names)
}

fn table_sql(conn: &Connection, table: &str) -> Option<String> {
    conn.query_row(
        "select sql from sqlite_master where type = 'table' and lower(name) = lower(?)",
        [table],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// What is wrong with a policy, or nothing. Checked against the app's own
/// database, because the `where` is only meaningful there: it must prepare
/// inside `select 1 from <table> where (<where>)`, may look at any table or
/// view in that database (a person's site or team usually lives in another
/// table), and may not carry a second statement.
pub fn validate(conn: &Connection, policy: &Policy) -> Result<(), String> {
    if !valid_identifier(&policy.table) {
        return Err(format!("access: {:?} is not a table name", policy.table));
    }
    if !valid_identifier(&policy.view) {
        return Err(format!("access: {:?} is not a view name", policy.view));
    }
    if policy.view.eq_ignore_ascii_case(&policy.table) {
        return Err(format!("access: the view for {} needs a name of its own", policy.table));
    }
    if policy.where_.trim().is_empty() {
        return Err(format!("access: the policy for {} has no where", policy.table));
    }
    if policy.where_.contains(';') {
        return Err(format!("access: the where for {} may not contain ';'", policy.table));
    }
    if let Some(owner) = &policy.owner
        && !valid_identifier(owner)
    {
        return Err(format!("access: {owner:?} is not a column name"));
    }
    let Some(create) = table_sql(conn, &policy.table) else {
        return Err(format!("access: there is no table named {}", policy.table));
    };
    let table_columns = columns(conn, &policy.table)?;
    if let Some(owner) = &policy.owner
        && !table_columns.iter().any(|c| c.eq_ignore_ascii_case(owner))
    {
        return Err(format!("access: {} has no column {owner}", policy.table));
    }
    if table_columns.iter().any(|c| c.eq_ignore_ascii_case(ROWID_COLUMN)) {
        return Err(format!(
            "access: {} has a column named {ROWID_COLUMN}, which the generated view needs for itself",
            policy.table
        ));
    }
    if policy.write && create.to_ascii_uppercase().contains("WITHOUT ROWID") {
        return Err(format!(
            "access: {} is a WITHOUT ROWID table, which cannot take write = true; \
             give it an ordinary rowid or keep the policy read only",
            policy.table
        ));
    }
    // Preparing proves the where is one expression over things that exist.
    // Nothing runs.
    conn.prepare(&format!(
        "select 1 from {} where ({})",
        quote(&policy.table),
        policy.where_
    ))
    .map(|_| ())
    .map_err(|e| format!("access: the where for {} does not parse: {e}", policy.table))
}

/// The column every generated view carries so an update or delete through
/// it can name the base row: a view has no rowid of its own.
pub const ROWID_COLUMN: &str = "ts_rowid";

/// The SQL that realises one policy. Idempotent: drops before it creates.
pub fn generate(policy: &Policy, table_columns: &[String]) -> String {
    let view = quote(&policy.view);
    let table = quote(&policy.table);
    let [insert, update, delete] = trigger_names(&policy.view);
    let mut sql = format!(
        "drop view if exists {view};\n\
         create view {view} as select rowid as {ROWID_COLUMN}, * from {table} where ({});\n",
        policy.where_
    );
    for trigger in [&insert, &update, &delete] {
        sql.push_str(&format!("drop trigger if exists {};\n", quote(trigger)));
    }
    if !policy.write {
        return sql;
    }
    let column_list = table_columns.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ");
    let values = table_columns
        .iter()
        .map(|c| {
            if policy.owner.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(c)) {
                format!("coalesce(new.{}, current_user())", quote(c))
            } else {
                format!("new.{}", quote(c))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let assignments = table_columns
        .iter()
        .map(|c| format!("{0} = new.{0}", quote(c)))
        .collect::<Vec<_>>()
        .join(", ");
    // The visibility check is the policy's own where, applied to the base
    // row the trigger just touched. A view has no rowid, so the base table
    // is asked directly.
    let check = |rowid: &str| {
        format!(
            "select raise(abort, 'row is not visible to you') where not exists \
             (select 1 from {table} where rowid = {rowid} and ({}));",
            policy.where_
        )
    };
    sql.push_str(&format!(
        "create trigger {t} instead of insert on {view} begin\n  \
           insert into {table} ({column_list}) values ({values});\n  {check}\nend;\n",
        t = quote(&insert),
        check = check("last_insert_rowid()"),
    ));
    sql.push_str(&format!(
        "create trigger {t} instead of update on {view} begin\n  \
           update {table} set {assignments} where rowid = old.{ROWID_COLUMN};\n  {check}\nend;\n",
        t = quote(&update),
        check = check(&format!("old.{ROWID_COLUMN}")),
    ));
    sql.push_str(&format!(
        "create trigger {t} instead of delete on {view} begin\n  \
           delete from {table} where rowid = old.{ROWID_COLUMN};\nend;\n",
        t = quote(&delete),
    ));
    sql
}

/// Rebuilds every generated object from the app's policies and drops the
/// ones no policy claims any more. Policies whose table does not exist yet
/// are left for the next schema change, and named in the returned notes.
/// Returns the names now generated, which the caller stores in the meta.
pub fn regenerate(config: &Config, app: &str, meta: &PageMeta) -> Result<(Vec<String>, Vec<String>), String> {
    let path = db::db_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if !path.is_file() && meta.policies.is_empty() && meta.generated.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let conn = db::open_unguarded(&path, config.max_db_bytes)?;
    db::bind_identity(&conn, None)?;

    let mut keep: HashSet<String> = HashSet::new();
    let mut script = String::new();
    let mut notes = Vec::new();
    for policy in &meta.policies {
        if table_sql(&conn, &policy.table).is_none() {
            notes.push(format!(
                "access for {}: waiting for the table to exist; apply the migrations",
                policy.table
            ));
            continue;
        }
        validate(&conn, policy)?;
        let table_columns = columns(&conn, &policy.table)?;
        script.push_str(&generate(policy, &table_columns));
        keep.insert(policy.view.to_lowercase());
        for trigger in trigger_names(&policy.view) {
            if policy.write {
                keep.insert(trigger.to_lowercase());
            }
        }
    }
    // Objects from a policy that is gone, or whose write flag was withdrawn.
    for name in &meta.generated {
        if keep.contains(&name.to_lowercase()) {
            continue;
        }
        if name.starts_with(TRIGGER_PREFIX) {
            script.push_str(&format!("drop trigger if exists {};\n", quote(name)));
        } else {
            script.push_str(&format!("drop view if exists {};\n", quote(name)));
        }
    }
    if !script.is_empty() {
        conn.execute_batch(&format!("begin; {script} commit;"))
            .map_err(|e| format!("access: could not generate the views: {e}"))?;
    }
    let mut generated: Vec<String> = Vec::new();
    for policy in &meta.policies {
        if !keep.contains(&policy.view.to_lowercase()) {
            continue;
        }
        generated.push(policy.view.clone());
        if policy.write {
            generated.extend(trigger_names(&policy.view));
        }
    }
    Ok((generated, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(table: &str, where_: &str, write: bool) -> Policy {
        Policy {
            table: table.to_string(),
            view: format!("my_{table}"),
            where_: where_.to_string(),
            owner: Some("owner_id".to_string()),
            write,
        }
    }

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::bind_identity(&conn, None).unwrap();
        conn.execute_batch(
            "create table orders (id integer primary key, owner_id text, total real);
             create table pinned (k text primary key, v text) without rowid;",
        )
        .unwrap();
        conn
    }

    #[test]
    fn a_policy_must_name_a_real_table_and_a_where_that_parses() {
        let conn = conn();
        assert!(validate(&conn, &policy("orders", "owner_id = current_user()", true)).is_ok());
        let missing = validate(&conn, &policy("nothing", "1", false)).unwrap_err();
        assert!(missing.contains("no table named nothing"), "{missing}");
        let broken = validate(&conn, &policy("orders", "owner_id = = 1", false)).unwrap_err();
        assert!(broken.contains("does not parse"), "{broken}");
        let bad_name = validate(&conn, &Policy { view: "my orders".into(), ..policy("orders", "1", false) }).unwrap_err();
        assert!(bad_name.contains("not a view name"), "{bad_name}");
    }

    #[test]
    fn a_where_may_not_carry_a_second_statement() {
        let conn = conn();
        let error = validate(&conn, &policy("orders", "1 = 1; drop table orders", false)).unwrap_err();
        assert!(error.contains("';'"), "{error}");
        let tables: i64 = conn
            .query_row("select count(*) from sqlite_master where name = 'orders'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn a_where_may_look_at_another_table_in_the_same_database() {
        let conn = conn();
        conn.execute_batch("create table members (user_id text, location text);").unwrap();
        let membership = Policy {
            owner: None,
            ..policy("orders", "total > (select count(*) from members where user_id = current_user())", false)
        };
        assert!(validate(&conn, &membership).is_ok());
    }

    #[test]
    fn a_table_without_rowid_cannot_take_writes() {
        let conn = conn();
        let error = validate(&conn, &Policy { owner: None, ..policy("pinned", "1", true) }).unwrap_err();
        assert!(error.contains("WITHOUT ROWID"), "{error}");
        assert!(validate(&conn, &Policy { owner: None, ..policy("pinned", "1", false) }).is_ok());
    }

    #[test]
    fn the_generated_sql_makes_a_view_and_three_triggers_that_sqlite_accepts() {
        let conn = conn();
        let p = policy("orders", "owner_id = current_user()", true);
        let sql = generate(&p, &columns(&conn, "orders").unwrap());
        conn.execute_batch(&sql).unwrap();
        let names: Vec<String> = conn
            .prepare("select name from sqlite_master where name like 'my_orders%' or name like 'ts_access_%' order by name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            names,
            ["my_orders", "ts_access_my_orders_delete", "ts_access_my_orders_insert", "ts_access_my_orders_update"]
        );
        // Running it again is fine: drop before create.
        conn.execute_batch(&sql).unwrap();
        // A read-only policy leaves no triggers behind.
        let read_only = Policy { write: false, ..p };
        conn.execute_batch(&generate(&read_only, &columns(&conn, "orders").unwrap())).unwrap();
        let triggers: i64 = conn
            .query_row("select count(*) from sqlite_master where type = 'trigger'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(triggers, 0);
    }
}
