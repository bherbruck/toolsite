//! Row-level access an app declares instead of writing.
//!
//! A policy names a table and a `where` that says which rows the signed-in
//! person may see, written against the app's own tables and the identity
//! functions the host binds: `current_user()`, `current_email()`,
//! `current_role()`. From it the platform generates a view, and when the
//! policy says `write = true`, three `instead of` triggers that carry an
//! insert, update or delete through to the base table and abort when the
//! result would be a row the person cannot see, or would replace a row they
//! cannot see.
//!
//! Every readable view is two views: a public one with the declared name,
//! which is one line, `select * from <inner>`, and an inner one whose name
//! carries a random salt and whose body does the work. The scoped authorizer
//! trusts a base-table read only when SQLite reports it as made through an
//! inner view. SQLite reports a common table expression the same way it
//! reports a view, under the CTE's own name, so a person could otherwise
//! write `with my_orders as (select * from orders) ...` and read everything.
//! They cannot name a CTE after a view whose name they cannot see.
//!
//! A hand-written view the app declares is given the same shape: its body is
//! moved into an inner view and the declared name becomes the one-line
//! wrapper. Removing the declaration puts the body back.
//!
//! The generated objects belong to the platform. They are rebuilt wholesale
//! whenever the manifest or the schema changes, so a column added by a later
//! migration shows up in the view, and a policy that is removed takes its
//! view and triggers with it. An app never edits them; it edits the policy.

use crate::{
    config::Config,
    content::store::{PageMeta, Policy},
    runtime::db,
};
use rusqlite::Connection;
use std::collections::HashSet;

/// The column every generated view carries so an update or delete through
/// it can name the base row: a view has no rowid of its own.
pub const ROWID_COLUMN: &str = "ts_rowid";

/// The three triggers behind a writable view. Salted like the inner views,
/// because the authorizer trusts their reads of the base table (a collision
/// check has to see every row) and a name a person could guess is a name
/// they could give a CTE.
pub fn trigger_names(salt: &str, view: &str) -> [String; 3] {
    [
        format!("ts_{salt}_{view}_insert"),
        format!("ts_{salt}_{view}_update"),
        format!("ts_{salt}_{view}_delete"),
    ]
}

/// The inner view behind a declared name. The salt is per app, random, and
/// never shown to a person querying; see the module notes.
pub fn inner_name(salt: &str, view: &str) -> String {
    format!("ts_{salt}_{view}")
}

pub fn new_salt() -> String {
    crate::content::slug::random_token(12)
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

/// What a write must not collide with: the column that is the rowid in
/// disguise, if there is one, and every unique index. A conflicting insert or
/// update is aborted before it runs, so `or replace` has nothing to replace.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Keys {
    pub rowid_alias: Option<String>,
    pub unique: Vec<Vec<String>>,
}

fn keys(conn: &Connection, table: &str) -> Result<Keys, String> {
    // table_info: cid, name, type, notnull, dflt_value, pk
    let mut statement = conn
        .prepare(&format!("pragma table_info({})", quote(table)))
        .map_err(|e| e.to_string())?;
    let info: Vec<(String, String, i64)> = statement
        .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(5)?)))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    let pk: Vec<&(String, String, i64)> = info.iter().filter(|(_, _, pk)| *pk > 0).collect();
    let rowid_alias = match pk.as_slice() {
        [(name, kind, _)] if kind.trim().eq_ignore_ascii_case("integer") => Some(name.clone()),
        _ => None,
    };
    let mut unique = Vec::new();
    let mut list = conn
        .prepare(&format!("pragma index_list({})", quote(table)))
        .map_err(|e| e.to_string())?;
    // index_list: seq, name, unique, origin, partial
    let indexes: Vec<(String, i64, i64)> = list
        .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(4)?)))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    for (name, is_unique, partial) in indexes {
        if is_unique == 0 || partial != 0 {
            continue;
        }
        let mut cols = conn
            .prepare(&format!("pragma index_info({})", quote(&name)))
            .map_err(|e| e.to_string())?;
        let names: Vec<String> = cols
            .query_map([], |row| row.get::<_, Option<String>>(2))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .flatten()
            .collect();
        if !names.is_empty() {
            unique.push(names);
        }
    }
    Ok(Keys { rowid_alias, unique })
}

fn object_sql(conn: &Connection, kind: &str, name: &str) -> Option<String> {
    conn.query_row(
        "select sql from sqlite_master where type = ? and lower(name) = lower(?)",
        [kind, name],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn table_sql(conn: &Connection, table: &str) -> Option<String> {
    object_sql(conn, "table", table)
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
    if policy.view.to_lowercase().starts_with("ts_") {
        return Err(format!("access: view names starting with ts_ are the platform's; rename {}", policy.view));
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
    // Nothing runs. A bound parameter would prepare here and then fail to
    // become a view, so it is refused by name.
    let statement = conn
        .prepare(&format!("select 1 from {} where ({})", quote(&policy.table), policy.where_))
        .map_err(|e| format!("access: the where for {} does not parse: {e}", policy.table))?;
    if statement.parameter_count() > 0 {
        return Err(format!("access: the where for {} may not contain a parameter", policy.table));
    }
    Ok(())
}

/// What is wrong with declaring a hand-written view, or nothing: it has to be
/// a view that exists, with a definition the platform can move into an inner
/// view. Returns Ok(None) while the view does not exist yet, which a later
/// migration may fix.
pub fn validate_declared(conn: &Connection, salt: &str, view: &str) -> Result<Option<ViewBody>, String> {
    if !valid_identifier(view) {
        return Err(format!("access: {view:?} is not a view name"));
    }
    if view.to_lowercase().starts_with("ts_") {
        return Err(format!("access: view names starting with ts_ are the platform's; rename {view}"));
    }
    if table_sql(conn, view).is_some() {
        return Err(format!(
            "access: {view} is a table; declare a view over it, or a [[access.table]] policy"
        ));
    }
    let Some(sql) = object_sql(conn, "view", view) else {
        return Ok(None);
    };
    let body = view_body(&sql).ok_or_else(|| {
        format!("access: could not read how {view} is defined; write it as `create view {view} as select ...`")
    })?;
    // Already wrapped by an earlier regeneration: the body lives in the inner view.
    if body.is_wrapper_for(&inner_name(salt, view)) {
        let inner = object_sql(conn, "view", &inner_name(salt, view))
            .ok_or_else(|| format!("access: the inner view behind {view} is missing; recreate {view} in a migration"))?;
        return view_body(&inner)
            .map(Some)
            .ok_or_else(|| format!("access: could not read the inner view behind {view}"));
    }
    Ok(Some(body))
}

/// The pieces of a `create view` statement the platform needs to move.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewBody {
    /// The parenthesised column list, if the view named its columns.
    pub columns: Option<String>,
    /// Everything after `as`.
    pub select: String,
}

impl ViewBody {
    fn is_wrapper_for(&self, inner: &str) -> bool {
        let wanted = format!("select * from {}", quote(inner)).to_lowercase();
        self.select.trim().trim_end_matches(';').trim().to_lowercase() == wanted
    }
}

/// Takes a `create view` statement apart without executing anything: skips
/// comments, reads the optional modifiers, the name (quoted or not, with an
/// optional schema), an optional column list, then `as`. Anything it does
/// not understand is a `None`, and the declaration is refused rather than
/// guessed at.
pub fn view_body(sql: &str) -> Option<ViewBody> {
    let mut scanner = Scanner::new(sql);
    scanner.expect_word("create")?;
    let mut word = scanner.word()?;
    if word.eq_ignore_ascii_case("temp") || word.eq_ignore_ascii_case("temporary") {
        word = scanner.word()?;
    }
    if !word.eq_ignore_ascii_case("view") {
        return None;
    }
    let mut name = scanner.name()?;
    if name.eq_ignore_ascii_case("if") {
        scanner.expect_word("not")?;
        scanner.expect_word("exists")?;
        name = scanner.name()?;
    }
    let _ = name;
    // schema.name
    if scanner.peek_char() == Some('.') {
        scanner.pos += 1;
        scanner.name()?;
    }
    let columns = if scanner.peek_char() == Some('(') {
        Some(scanner.parenthesised()?)
    } else {
        None
    };
    scanner.expect_word("as")?;
    let select = sql[scanner.pos..].trim().trim_end_matches(';').trim().to_string();
    if select.is_empty() {
        return None;
    }
    Some(ViewBody { columns, select })
}

struct Scanner<'a> {
    sql: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(sql: &'a str) -> Self {
        Self { sql, pos: 0 }
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            let rest = &self.sql[self.pos..];
            if rest.starts_with("--") {
                self.pos += rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
            } else if rest.starts_with("/*") {
                self.pos += rest.find("*/").map(|i| i + 2).unwrap_or(rest.len());
            } else if let Some(c) = rest.chars().next()
                && c.is_whitespace()
            {
                self.pos += c.len_utf8();
            } else {
                return;
            }
        }
    }

    fn peek_char(&mut self) -> Option<char> {
        self.skip_space_and_comments();
        self.sql[self.pos..].chars().next()
    }

    fn word(&mut self) -> Option<&'a str> {
        self.skip_space_and_comments();
        let rest = &self.sql[self.pos..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        self.pos += end;
        Some(&rest[..end])
    }

    fn expect_word(&mut self, wanted: &str) -> Option<()> {
        self.word().filter(|w| w.eq_ignore_ascii_case(wanted)).map(|_| ())
    }

    /// A bare or quoted identifier; the text is returned as written.
    fn name(&mut self) -> Option<&'a str> {
        self.skip_space_and_comments();
        let rest = &self.sql[self.pos..];
        let (open, close) = match rest.chars().next()? {
            '"' => ('"', '"'),
            '`' => ('`', '`'),
            '[' => ('[', ']'),
            _ => return self.word(),
        };
        let mut end = open.len_utf8();
        loop {
            let tail = &rest[end..];
            let i = tail.find(close)?;
            end += i + close.len_utf8();
            // A doubled quote is an escaped quote inside the name.
            if close != ']' && rest[end..].starts_with(close) {
                end += close.len_utf8();
                continue;
            }
            break;
        }
        self.pos += end;
        Some(&rest[..end])
    }

    fn parenthesised(&mut self) -> Option<String> {
        self.skip_space_and_comments();
        let rest = &self.sql[self.pos..];
        let mut depth = 0usize;
        for (i, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += i + 1;
                        return Some(rest[..=i].to_string());
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// The SQL that realises one policy. Idempotent: drops before it creates.
pub fn generate(policy: &Policy, salt: &str, table_columns: &[String], keys: &Keys) -> String {
    let inner_id = inner_name(salt, &policy.view);
    let inner = quote(&inner_id);
    let view = quote(&policy.view);
    let table = quote(&policy.table);
    let [insert, update, delete] = trigger_names(salt, &policy.view);
    let mut sql = format!(
        "drop view if exists {view};\n\
         drop view if exists {inner};\n\
         create view {inner} as select rowid as {ROWID_COLUMN}, * from {table} where ({});\n\
         create view {view} as select * from {inner};\n",
        policy.where_
    );
    for trigger in [&insert, &update, &delete] {
        sql.push_str(&format!("drop trigger if exists {};\n", quote(trigger)));
    }
    if !policy.write {
        return sql;
    }
    let value_of = |c: &str| {
        if policy.owner.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(c)) {
            format!("coalesce(new.{}, current_user())", quote(c))
        } else {
            format!("new.{}", quote(c))
        }
    };
    let column_list = table_columns.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ");
    let values = table_columns.iter().map(|c| value_of(c)).collect::<Vec<_>>().join(", ");
    let assignments = table_columns
        .iter()
        .map(|c| format!("{0} = new.{0}", quote(c)))
        .collect::<Vec<_>>()
        .join(", ");

    // Collisions are refused before the write runs, which is what makes
    // `insert or replace` harmless: the row that would have been replaced is
    // never reached. `exclude` keeps an update from colliding with itself.
    let collision = |exclude: Option<&str>| -> String {
        let mut clauses = Vec::new();
        if let Some(alias) = &keys.rowid_alias {
            let other = exclude.map(|e| format!(" and new.{} <> {e}", quote(alias))).unwrap_or_default();
            clauses.push(format!(
                "(new.{a} is not null{other} and exists (select 1 from {table} where rowid = new.{a}))",
                a = quote(alias)
            ));
        }
        for index in &keys.unique {
            let same = index
                .iter()
                .map(|c| format!("{table}.{0} = {1}", quote(c), value_of(c)))
                .collect::<Vec<_>>()
                .join(" and ");
            let other = exclude.map(|e| format!("rowid <> {e} and ")).unwrap_or_default();
            clauses.push(format!("exists (select 1 from {table} where {other}{same})"));
        }
        if clauses.is_empty() {
            String::new()
        } else {
            format!(
                "select raise(abort, 'a row with that key already exists') where {};\n  ",
                clauses.join(" or ")
            )
        }
    };
    // The visibility check is the policy's own where, applied through the
    // inner view to the base row the trigger just touched.
    let visible = |rowid: &str| {
        format!(
            "select raise(abort, 'row is not visible to you') where not exists \
             (select 1 from {inner} where {ROWID_COLUMN} = {rowid});"
        )
    };
    sql.push_str(&format!(
        "create trigger {t} instead of insert on {view} begin\n  \
           {collide}insert into {table} ({column_list}) values ({values});\n  {check}\nend;\n",
        t = quote(&insert),
        collide = collision(None),
        check = visible("last_insert_rowid()"),
    ));
    sql.push_str(&format!(
        "create trigger {t} instead of update on {view} begin\n  \
           {collide}update {table} set {assignments} where rowid = old.{ROWID_COLUMN};\n  {check}\nend;\n",
        t = quote(&update),
        collide = collision(Some(&format!("old.{ROWID_COLUMN}"))),
        check = visible(&format!("old.{ROWID_COLUMN}")),
    ));
    sql.push_str(&format!(
        "create trigger {t} instead of delete on {view} begin\n  \
           delete from {table} where rowid = old.{ROWID_COLUMN};\nend;\n",
        t = quote(&delete),
    ));
    sql
}

/// The SQL that moves a hand-written view behind an inner view of the same
/// body, leaving the declared name as a one-line wrapper.
pub fn wrap(salt: &str, view: &str, body: &ViewBody) -> String {
    let inner = quote(&inner_name(salt, view));
    let public = quote(view);
    let columns = body.columns.as_deref().unwrap_or("");
    format!(
        "drop view if exists {public};\n\
         drop view if exists {inner};\n\
         create view {inner}{columns} as {};\n\
         create view {public} as select * from {inner};\n",
        body.select
    )
}

/// The SQL that puts a wrapped view back the way the app wrote it.
fn unwrap(salt: &str, view: &str, body: &ViewBody) -> String {
    let inner = quote(&inner_name(salt, view));
    let public = quote(view);
    let columns = body.columns.as_deref().unwrap_or("");
    format!(
        "drop view if exists {public};\n\
         create view {public}{columns} as {};\n\
         drop view if exists {inner};\n",
        body.select
    )
}

/// What a regeneration leaves behind.
pub struct Regenerated {
    /// Every generated object, for the meta: public and inner views, triggers.
    pub generated: Vec<String>,
    /// Things worth telling the person who applied the change.
    pub notes: Vec<String>,
    /// The app's salt, minted now if it had none.
    pub salt: String,
}

/// Rebuilds every generated object from the app's policies and declared
/// views, and drops or unwraps the ones no declaration claims any more.
/// Policies whose table does not exist yet are left for the next schema
/// change, and named in the notes.
pub fn regenerate(config: &Config, app: &str, meta: &PageMeta) -> Result<Regenerated, String> {
    let salt = meta.access_salt.clone().unwrap_or_else(new_salt);
    let path = db::db_path(config, app).ok_or_else(|| format!("invalid app name '{app}'"))?;
    if !path.is_file() && meta.policies.is_empty() && meta.queryable.is_empty() && meta.generated.is_empty() {
        return Ok(Regenerated { generated: Vec::new(), notes: Vec::new(), salt });
    }
    let conn = db::open_unguarded(&path, config.max_db_bytes)?;
    db::bind_identity(&conn, None)?;

    let mut keep: HashSet<String> = HashSet::new();
    let mut script = String::new();
    let mut notes = Vec::new();
    let mut generated: Vec<String> = Vec::new();

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
        let table_keys = keys(&conn, &policy.table)?;
        script.push_str(&generate(policy, &salt, &table_columns, &table_keys));
        let mut names = vec![policy.view.clone(), inner_name(&salt, &policy.view)];
        if policy.write {
            names.extend(trigger_names(&salt, &policy.view));
        }
        for name in names {
            keep.insert(name.to_lowercase());
            generated.push(name);
        }
    }

    for view in &meta.queryable {
        match validate_declared(&conn, &salt, view)? {
            None => notes.push(format!("access for {view}: waiting for the view to exist; apply the migrations")),
            Some(body) => {
                script.push_str(&wrap(&salt, view, &body));
                let inner = inner_name(&salt, view);
                keep.insert(inner.to_lowercase());
                generated.push(inner);
            }
        }
    }

    // Objects from a declaration that is gone, or whose write flag was
    // withdrawn. A wrapped hand-written view gets its body back; a policy's
    // objects are simply dropped.
    for name in &meta.generated {
        if keep.contains(&name.to_lowercase()) {
            continue;
        }
        let prefix = format!("ts_{salt}_");
        if let Some(public) = name.strip_prefix(&prefix) {
            // A trigger of a policy that is gone or went read only. Dropping
            // a trigger that does not exist is nothing, so every salted name
            // can be tried as one first.
            script.push_str(&format!("drop trigger if exists {};\n", quote(name)));
            // An inner view: if its public name was a hand-written view that
            // is no longer declared, restore the body; otherwise the public
            // view was a policy's and goes too.
            // A policy's public view was itself generated, so it is in the
            // list; a hand-written view's public name never is.
            let was_policy = meta.generated.iter().any(|g| g.eq_ignore_ascii_case(public));
            if !was_policy
                && let Some(sql) = object_sql(&conn, "view", name)
                && let Some(body) = view_body(&sql)
                && object_sql(&conn, "view", public)
                    .and_then(|s| view_body(&s))
                    .is_some_and(|b| b.is_wrapper_for(name))
            {
                script.push_str(&unwrap(&salt, public, &body));
                continue;
            }
            script.push_str(&format!("drop view if exists {};\n", quote(name)));
        } else if !meta.queryable.iter().any(|v| v.eq_ignore_ascii_case(name)) {
            script.push_str(&format!("drop view if exists {};\n", quote(name)));
        }
    }

    if !script.is_empty() {
        conn.execute_batch(&format!("begin; {script} commit;"))
            .map_err(|e| format!("access: could not generate the views: {e}"))?;
    }
    Ok(Regenerated { generated, notes, salt })
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
            "create table orders (id integer primary key, owner_id text, total real, sku text unique);
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
        let reserved = validate(&conn, &Policy { view: "ts_mine".into(), ..policy("orders", "1", false) }).unwrap_err();
        assert!(reserved.contains("ts_"), "{reserved}");
        let parameter = validate(&conn, &policy("orders", "owner_id = ?", false)).unwrap_err();
        assert!(parameter.contains("parameter"), "{parameter}");
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
    fn the_keys_of_a_table_are_its_rowid_alias_and_its_unique_indexes() {
        let conn = conn();
        let k = keys(&conn, "orders").unwrap();
        assert_eq!(k.rowid_alias.as_deref(), Some("id"));
        assert_eq!(k.unique, vec![vec!["sku".to_string()]]);
        let k = keys(&conn, "pinned").unwrap();
        assert_eq!(k.rowid_alias, None, "a text primary key is not the rowid");
        assert_eq!(k.unique, vec![vec!["k".to_string()]]);
    }

    #[test]
    fn the_generated_sql_makes_two_views_and_three_triggers_that_sqlite_accepts() {
        let conn = conn();
        let p = policy("orders", "owner_id = current_user()", true);
        let sql = generate(&p, "abc", &columns(&conn, "orders").unwrap(), &keys(&conn, "orders").unwrap());
        conn.execute_batch(&sql).unwrap();
        let names: Vec<String> = conn
            .prepare("select name from sqlite_master where name like 'my_orders%' or name like 'ts_%' order by name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            names,
            [
                "my_orders",
                "ts_abc_my_orders",
                "ts_abc_my_orders_delete",
                "ts_abc_my_orders_insert",
                "ts_abc_my_orders_update"
            ]
        );
        // Running it again is fine: drop before create.
        conn.execute_batch(&sql).unwrap();
        // A read-only policy leaves no triggers behind.
        let read_only = Policy { write: false, ..p };
        conn.execute_batch(&generate(&read_only, "abc", &columns(&conn, "orders").unwrap(), &keys(&conn, "orders").unwrap()))
            .unwrap();
        let triggers: i64 = conn
            .query_row("select count(*) from sqlite_master where type = 'trigger'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(triggers, 0);
    }

    #[test]
    fn a_create_view_statement_comes_apart_into_columns_and_select() {
        let plain = view_body("CREATE VIEW v AS select a, b from t").unwrap();
        assert_eq!(plain, ViewBody { columns: None, select: "select a, b from t".into() });
        let quoted = view_body("create temp view if not exists \"odd name\"(x, y) as\n  select 1, 2;").unwrap();
        assert_eq!(quoted, ViewBody { columns: Some("(x, y)".into()), select: "select 1, 2".into() });
        let commented = view_body("create /* c */ view -- hi\n main.[v] as with q as (select 1) select * from q").unwrap();
        assert_eq!(commented.select, "with q as (select 1) select * from q");
        assert!(view_body("create table t (x)").is_none());
        assert!(view_body("select 1").is_none());
        assert!(view_body("create view v").is_none());
    }

    #[test]
    fn wrapping_a_view_keeps_its_rows_and_unwrapping_restores_its_definition() {
        let conn = conn();
        conn.execute_batch("create view mine as select total from orders where owner_id = current_user()").unwrap();
        let before = object_sql(&conn, "view", "mine").unwrap();
        let body = validate_declared(&conn, "abc", "mine").unwrap().unwrap();
        conn.execute_batch(&wrap("abc", "mine", &body)).unwrap();
        assert!(object_sql(&conn, "view", "ts_abc_mine").is_some());
        let wrapped = view_body(&object_sql(&conn, "view", "mine").unwrap()).unwrap();
        assert!(wrapped.is_wrapper_for("ts_abc_mine"));
        // Validating again reads through the wrapper to the real body.
        let again = validate_declared(&conn, "abc", "mine").unwrap().unwrap();
        assert_eq!(again.select, body.select);
        conn.execute_batch(&unwrap("abc", "mine", &again)).unwrap();
        assert!(object_sql(&conn, "view", "ts_abc_mine").is_none());
        let after = object_sql(&conn, "view", "mine").unwrap();
        assert_eq!(view_body(&after).unwrap().select, view_body(&before).unwrap().select);
        // A table is not a view, and a reserved name is not for the app.
        assert!(validate_declared(&conn, "abc", "orders").unwrap_err().contains("is a table"));
        assert!(validate_declared(&conn, "abc", "ts_x").unwrap_err().contains("ts_"));
        assert!(validate_declared(&conn, "abc", "later").unwrap().is_none());
    }
}
