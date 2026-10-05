//! Row-level access, attacked. Every test here is an attempt to read or
//! change another person's rows, or to get out of the app database, made
//! as a person holding a `/me/mcp` token, as a handler calling
//! `query-scoped`, or as an admin's agent running `run_sql as_user`. A test
//! that passes is a refusal that holds. The boundary is the scoped
//! authorizer in `runtime::db` and the generated triggers in
//! `runtime::access`; the SQL text is never inspected, so every trick that
//! only changes the text should fail the same way.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;
use toolsite::{
    build_router,
    runtime::db::{self, Identity, Scope},
    runtime::wasm::Runtime,
    Config,
};
use tower::ServiceExt;

const TOKEN: &str = "test-token";
const BASE: &str = "https://site.test";

fn server() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config::local(dir.path().to_path_buf(), TOKEN));
    (dir, config)
}

fn public_server() -> (TempDir, Arc<Config>) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        base_url: Some(BASE.to_string()),
        valid_tokens: Vec::new(),
        ..Config::local(dir.path().to_path_buf(), "unused")
    });
    (dir, config)
}

fn write_page(config: &Config, slug: &str, html: &str) {
    let path = config.data_dir.join(format!("{slug}.html"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, html).unwrap();
}

const SCHEMA: &str = "\
create table orders (id integer primary key, owner_id text, total real, sku text unique);\n\
create table notes (id integer primary key, owner_id text, body text);\n\
create table secrets (id integer primary key, payload text);\n\
create table members (user_id text, location text);\n\
create table records (id integer primary key, location text, note text);\n\
create table parents (id integer primary key, owner_id text);\n\
create table children (id integer primary key, parent_id integer references parents(id) on delete cascade, body text);\n\
create view all_orders as select * from orders;\n\
create view shared_totals as select owner_id, total from orders where owner_id = current_user();\n\
create view via_all as select * from all_orders where owner_id = current_user();\n";

const MANIFEST: &str = r#"
[access]
views = ["shared_totals", "via_all"]

[[access.table]]
table = "orders"
where = "owner_id = current_user()"
owner = "owner_id"
write = true

[[access.table]]
table = "notes"
where = "owner_id = current_user()"
owner = "owner_id"

[[access.table]]
table = "records"
where = "location = (select location from members where user_id = current_user())"
write = true

[[access.table]]
table = "parents"
where = "owner_id = current_user()"
owner = "owner_id"
write = true
"#;

struct People {
    alice: Identity,
    bob: Identity,
    alice_user: toolsite::accounts::users::User,
}

fn identity(user: &toolsite::accounts::users::User) -> Identity {
    Identity {
        user_id: user.id.clone(),
        email: user.email.clone(),
        role: None,
    }
}

/// An app with everything an attacker could want: a table they own rows in,
/// a table they may only read, a table they may not see at all, an
/// undeclared view over the whole table, and two accounts.
async fn vault(config: &Config) -> People {
    write_page(config, "vault/index", "<title>Vault</title>");
    toolsite::runtime::migrate::store(config, "vault", vec![("001_initial.sql".to_string(), SCHEMA.to_string())]).unwrap();
    toolsite::runtime::migrate::apply(config, "vault").unwrap();
    toolsite::platform::manifest::apply(config, "vault", MANIFEST).await.unwrap();
    let alice_user = toolsite::accounts::users::sign_up(config, "alice@example.com", "correct horse battery").unwrap();
    let bob_user = toolsite::accounts::users::sign_up(config, "bob@example.com", "correct horse battery").unwrap();
    let a = &alice_user.id;
    let b = &bob_user.id;
    db::run(
        config,
        "vault",
        &format!(
            "insert into orders (id, owner_id, total, sku) values (1, '{a}', 10, 'A-1'), (2, '{b}', 20, 'B-1'), (3, '{b}', 30, 'B-2');\n\
             insert into notes (owner_id, body) values ('{a}', 'alice note'), ('{b}', 'bob note');\n\
             insert into secrets (payload) values ('the launch codes');\n\
             insert into members values ('{a}', 'north'), ('{b}', 'south');\n\
             insert into records (location, note) values ('north', 'north record'), ('south', 'south record');\n\
             insert into parents (id, owner_id) values (1, '{a}'), (2, '{b}');\n\
             insert into children (parent_id, body) values (1, 'alice child'), (2, 'bob child');"
        ),
        &[],
    )
    .unwrap();
    People {
        alice: identity(&alice_user),
        bob: identity(&bob_user),
        alice_user,
    }
}

fn scope(config: &Config) -> Scope {
    Scope::of(&toolsite::content::store::read_meta_blocking(config, "vault"))
}

fn scoped(config: &Config, who: Option<&Identity>, sql: &str) -> Result<db::SqlOutcome, String> {
    db::run_scoped(config, "vault", who, &scope(config), sql, &[])
}

fn scoped_with(config: &Config, who: &Identity, sql: &str, params: &[Value]) -> Result<db::SqlOutcome, String> {
    db::run_scoped(config, "vault", Some(who), &scope(config), sql, params)
}

/// The whole table as the admin sees it, to prove nothing moved.
fn admin(config: &Config, sql: &str) -> Vec<Vec<Value>> {
    db::run(config, "vault", sql, &[]).unwrap().rows
}

fn refused(result: &Result<db::SqlOutcome, String>, what: &str) {
    match result {
        Err(message) => assert!(
            message.contains("not authorized") || message.contains("prohibited") || message.contains("one statement") || message.contains("cannot") || message.contains("no such") || message.contains("not visible") || message.contains("syntax error") || message.contains("interrupted") || message.contains("too long") || message.contains("string or blob too big"),
            "{what}: refused for an unexpected reason: {message}"
        ),
        Ok(outcome) => panic!("{what}: went through with {} rows, {} affected", outcome.rows.len(), outcome.rows_affected),
    }
}

fn totals(outcome: &db::SqlOutcome) -> Vec<f64> {
    outcome.rows.iter().filter_map(|r| r.first().and_then(Value::as_f64)).collect()
}

// --- reads around the view ---------------------------------------------------

#[tokio::test]
async fn the_view_shows_a_person_only_their_rows_to_start_with() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let mine = scoped(&config, Some(&people.alice), "select total from my_orders order by total").unwrap();
    assert_eq!(totals(&mine), [10.0]);
    let theirs = scoped(&config, Some(&people.bob), "select total from my_orders order by total").unwrap();
    assert_eq!(totals(&theirs), [20.0, 30.0]);
}

#[tokio::test]
async fn a_base_table_cannot_be_read_by_any_name_or_route() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "select * from orders",
        "select count(*) from orders",
        "select * from main.orders",
        "select * from \"orders\"",
        "select * from secrets",
        "select payload from secrets limit 1",
        "select * from ORDERS",
        "select * from notes where owner_id <> current_user()",
        "select * from children",
    ] {
        refused(&scoped(&config, a, sql), sql);
    }
}

#[tokio::test]
async fn the_schema_and_the_pragma_tables_are_out_of_reach() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "select sql from sqlite_master",
        "select name from sqlite_schema",
        "select * from sqlite_temp_master",
        "select * from pragma_table_info('orders')",
        "select * from pragma_table_list",
        "select * from pragma_database_list",
        "select * from pragma_function_list",
        "select * from dbstat",
        "pragma table_info(orders)",
        "pragma writable_schema = 1",
        "pragma schema_version",
        "explain select * from my_orders",
        "explain query plan select * from my_orders",
    ] {
        refused(&scoped(&config, a, sql), sql);
    }
}

#[tokio::test]
async fn a_cte_or_subquery_cannot_reach_a_base_table() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "with recursive r(n) as (select 1 union all select n + 1 from r where n < 3) select (select payload from secrets) from r",
        "with everything as (select * from orders) select * from everything",
        "with my_orders as (select * from orders) select * from my_orders",
        "with ts_access_my_orders_insert as (select * from orders) select * from ts_access_my_orders_insert",
        "with shared_totals as (select * from secrets) select * from shared_totals",
        "with my_orders as (select * from orders) select (select total from main.my_orders limit 1), * from my_orders",
        "with recursive my_orders(a) as (select payload from secrets) select * from my_orders",
        "select (select payload from secrets limit 1) from my_orders",
        "select * from my_orders where exists (select 1 from secrets)",
        "select total from my_orders union select total from orders",
        "select total from my_orders union all select payload from secrets",
        "select * from my_orders join orders on 1 = 1",
        "select * from my_orders, secrets",
        "select * from my_orders where total in (select total from orders)",
        "select json_each.value from my_orders, json_each((select json_group_array(payload) from secrets))",
        "select count(*) over () from orders",
        "select * from (select * from orders)",
    ] {
        refused(&scoped(&config, a, sql), sql);
    }
}

#[tokio::test]
async fn an_undeclared_view_is_as_closed_as_a_table() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    refused(&scoped(&config, a, "select * from all_orders"), "undeclared view over the table");
    // A declared view that itself goes through an undeclared view: the inner
    // view is the accessor SQLite reports, and it is not declared. Closed,
    // and the docs tell an app to select from tables directly.
    refused(&scoped(&config, a, "select * from via_all"), "declared view over an undeclared view");
    // The declared hand-written view works, and shows only the person's rows
    // because it was written that way.
    let shared = scoped(&config, a, "select total from shared_totals").unwrap();
    assert_eq!(totals(&shared), [10.0]);
}

#[tokio::test]
async fn nothing_may_be_created_altered_or_dropped() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "create temp view peek as select * from orders",
        "create temp table peek as select * from orders",
        "create view peek as select * from orders",
        "create table peek (x)",
        "create trigger t after insert on orders begin select 1; end",
        "create index i on orders (owner_id)",
        "create virtual table f using fts5(body)",
        "create virtual table r using rtree(id, x0, x1)",
        "alter table orders add column leak text",
        "alter table orders rename to orders2",
        "drop view my_orders",
        "drop table secrets",
        "drop trigger ts_access_my_orders_insert",
        "vacuum",
        "vacuum into '/tmp/stolen.sqlite'",
        "reindex",
        "analyze",
        "attach database ':memory:' as x",
        "attach database '../.site/auth.db' as site",
        "detach database main",
    ] {
        refused(&scoped(&config, a, sql), sql);
    }
    let still: i64 = admin(&config, "select count(*) from sqlite_master where name in ('orders','secrets','my_orders')")[0][0].as_i64().unwrap();
    assert_eq!(still, 3, "something was created, altered or dropped");
    // `if exists` on a name that is not there is a no-op SQLite never asks
    // the authorizer about; what matters is that nothing real went with it.
    let _ = scoped(&config, a, "drop trigger if exists ts_access_my_orders_insert");
    let _ = scoped(&config, a, "drop view if exists nothing_here");
    let triggers: i64 = admin(&config, "select count(*) from sqlite_master where type = 'trigger'")[0][0].as_i64().unwrap();
    assert_eq!(triggers, 9, "a trigger was dropped");
    assert!(!std::path::Path::new("/tmp/stolen.sqlite").exists());
}

#[tokio::test]
async fn one_statement_at_a_time_however_it_is_written() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "select 1; select payload from secrets",
        "select 1 /* ; */; drop table secrets",
        "select 1 --\n; select * from orders",
        "select 1;\n\nselect * from orders;",
    ] {
        refused(&scoped(&config, a, sql), sql);
    }
    assert_eq!(admin(&config, "select count(*) from secrets")[0][0], json!(1));
    // A comment or odd casing inside one statement is still one statement.
    let fine = scoped(&config, a, "SeLeCt /* hi */ ToTaL -- trailing\n from My_Orders").unwrap();
    assert_eq!(totals(&fine), [10.0]);
}

#[tokio::test]
async fn extensions_and_the_filesystem_stay_out() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "select load_extension('/tmp/evil.so')",
        "select load_extension('x', 'y')",
        "select readfile('/etc/passwd')",
        "select writefile('/tmp/pwned', 'x')",
        "select edit('x')",
        "select fts3_tokenizer('simple')",
    ] {
        refused(&scoped(&config, a, sql), sql);
    }
    assert!(!std::path::Path::new("/tmp/pwned").exists());
}

#[tokio::test]
async fn guessing_rowids_through_the_view_finds_nothing_that_is_not_yours() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "select * from my_orders where ts_rowid = 2",
        "select * from my_orders where rowid = 2",
        "select * from my_orders where id in (2, 3)",
        "select * from my_orders where ts_rowid between 1 and 100 and owner_id <> current_user()",
    ] {
        // A view has no rowid of its own, so that one is an error; the rest
        // answer with nothing.
        if let Ok(out) = scoped(&config, a, sql) {
            assert!(out.rows.is_empty(), "{sql}: {:?}", out.rows);
        }
    }
    let counted = scoped(&config, a, "select count(*) over (), sum(total) over () from my_orders").unwrap();
    assert_eq!(counted.rows, vec![vec![json!(1), json!(10.0)]]);
}

// --- writes around the policy ----------------------------------------------

#[tokio::test]
async fn an_insert_for_someone_else_is_aborted_and_the_owner_is_filled_in() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let bob = people.bob.user_id.clone();
    let out = scoped_with(&config, &people.alice, "insert into my_orders (owner_id, total, sku) values (?, 99, 'X-1')", &[json!(bob)]);
    refused(&out, "insert owned by someone else");
    let out = scoped(&config, Some(&people.alice), "insert into my_orders (total, sku) values (5, 'A-2')").unwrap();
    assert_eq!(out.rows_affected, 1);
    let owners = admin(&config, "select owner_id from orders where sku = 'A-2'");
    assert_eq!(owners[0][0], json!(people.alice.user_id));
    // An explicit NULL owner also becomes the caller, not an orphan row.
    scoped(&config, Some(&people.alice), "insert into my_orders (owner_id, total, sku) values (null, 6, 'A-3')").unwrap();
    assert_eq!(admin(&config, "select owner_id from orders where sku = 'A-3'")[0][0], json!(people.alice.user_id));
}

#[tokio::test]
async fn insert_select_can_only_copy_what_the_person_already_sees() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    refused(&scoped(&config, a, "insert into my_orders (total, sku) select total, sku || '-copy' from orders"), "insert ... select from the base table");
    refused(&scoped(&config, a, "insert into my_orders (total, sku) select total, sku || '-copy' from all_orders"), "insert ... select from an undeclared view");
    let out = scoped(&config, a, "insert into my_orders (total, sku) select total, sku || '-copy' from my_orders").unwrap();
    assert_eq!(out.rows_affected, 1);
    assert_eq!(admin(&config, "select count(*) from orders")[0][0], json!(4));
}

#[tokio::test]
async fn an_update_cannot_move_a_row_out_of_the_persons_sight() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let bob = people.bob.user_id.clone();
    let out = scoped_with(&config, &people.alice, "update my_orders set owner_id = ? where id = 1", &[json!(bob)]);
    refused(&out, "hand my row to someone else");
    refused(&scoped(&config, Some(&people.alice), "update my_orders set owner_id = null where id = 1"), "orphan my row");
    assert_eq!(admin(&config, "select owner_id from orders where id = 1")[0][0], json!(people.alice.user_id));
    // Moving a record to a location the person does not belong to is the
    // same thing in membership terms.
    refused(&scoped(&config, Some(&people.alice), "update my_records set location = 'south'"), "move a record to another site");
    assert_eq!(admin(&config, "select location from records where note = 'north record'")[0][0], json!("north"));
}

#[tokio::test]
async fn updating_or_deleting_someone_elses_row_changes_nothing() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        "update my_orders set total = 0 where id = 2",
        "update my_orders set total = 0 where ts_rowid = 2",
        "update my_orders set total = 0 where ts_rowid + 0 = 2",
        "update my_orders set total = 0 where id = (select 2)",
        "update my_orders set total = 0",
        "delete from my_orders where id = 2",
        "delete from my_orders where ts_rowid in (2, 3)",
        "delete from my_orders where owner_id <> current_user()",
        "delete from my_orders",
        "delete from my_records where note = 'south record'",
    ] {
        let out = scoped(&config, a, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let bobs = admin(&config, "select total from orders where id in (2, 3) order by id");
        assert_eq!(bobs, vec![vec![json!(20.0)], vec![json!(30.0)]], "{sql} touched someone else's rows");
        assert!(out.rows_affected <= 1, "{sql}: {} rows affected", out.rows_affected);
        // Put alice's own row back for the next attempt.
        db::run(&config, "vault", &format!("insert or replace into orders (id, owner_id, total, sku) values (1, '{}', 10, 'A-1')", people.alice.user_id), &[]).unwrap();
    }
    assert_eq!(admin(&config, "select count(*) from records")[0][0], json!(2));
}

#[tokio::test]
async fn replace_cannot_be_used_to_delete_a_row_the_person_cannot_see() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    // By primary key: bob's row 2 would be replaced by alice's new row.
    for sql in [
        "insert or replace into my_orders (id, total, sku) values (2, 1, 'A-9')",
        "replace into my_orders (id, total, sku) values (2, 1, 'A-9')",
        // By a unique column: bob's 'B-1' would go.
        "insert or replace into my_orders (total, sku) values (1, 'B-1')",
        "insert into my_orders (id, total, sku) values (2, 1, 'A-9') on conflict do update set total = 1",
        "insert into my_orders (total, sku) values (1, 'B-1') on conflict (sku) do update set total = 1",
        "insert or ignore into my_orders (id, total, sku) values (2, 1, 'A-9')",
        "insert or fail into my_orders (id, total, sku) values (2, 1, 'A-9')",
    ] {
        let out = scoped(&config, a, sql);
        let bobs = admin(&config, "select owner_id, total, sku from orders where id in (2, 3) order by id");
        assert_eq!(
            bobs,
            vec![vec![json!(people.bob.user_id), json!(20.0), json!("B-1")], vec![json!(people.bob.user_id), json!(30.0), json!("B-2")]],
            "{sql} changed or removed someone else's row ({out:?})"
        );
        assert_eq!(admin(&config, "select count(*) from orders")[0][0], json!(3), "{sql} left a different row count: {out:?}");
    }
    // And by update: alice may not move her row onto bob's key.
    for sql in [
        "update or replace my_orders set id = 2 where id = 1",
        "update or replace my_orders set sku = 'B-1' where id = 1",
    ] {
        let out = scoped(&config, a, sql);
        let bobs = admin(&config, "select count(*) from orders where owner_id = (select owner_id from orders where id = 3)");
        assert_eq!(bobs[0][0], json!(2), "{sql} removed one of bob's rows ({out:?})");
        assert_eq!(admin(&config, "select count(*) from orders")[0][0], json!(3), "{sql}: {out:?}");
    }
}

#[tokio::test]
async fn returning_gives_back_only_the_row_the_person_wrote() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    let out = scoped(&config, a, "insert into my_orders (total, sku) values (7, 'A-7') returning *");
    if let Ok(out) = &out {
        assert!(out.rows.len() <= 1, "returning leaked rows: {:?}", out.rows);
        for row in &out.rows {
            assert!(!row.iter().any(|v| v == &json!(people.bob.user_id)), "returning leaked someone else's row: {row:?}");
        }
    }
    let out = scoped(&config, a, "update my_orders set total = total returning *");
    if let Ok(out) = &out {
        assert!(out.rows.iter().all(|r| !r.iter().any(|v| v == &json!(people.bob.user_id))), "{:?}", out.rows);
    }
    let out = scoped(&config, a, "delete from my_orders where id = 2 returning *");
    if let Ok(out) = &out {
        assert!(out.rows.is_empty(), "{:?}", out.rows);
    }
    assert_eq!(admin(&config, "select count(*) from orders where id = 2")[0][0], json!(1));
}

#[tokio::test]
async fn read_only_views_take_no_writes_whoever_made_them() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in [
        // A policy without write = true.
        "insert into my_notes (body) values ('x')",
        "update my_notes set body = 'x'",
        "delete from my_notes",
        // A hand-written declared view.
        "insert into shared_totals (owner_id, total) values ('x', 1)",
        "update shared_totals set total = 0",
        "delete from shared_totals",
        // An undeclared view.
        "insert into all_orders (total) values (1)",
        "delete from all_orders",
        // The base tables themselves.
        "insert into orders (total) values (1)",
        "update orders set total = 0",
        "delete from secrets",
        "insert into members values ('x', 'north')",
    ] {
        refused(&scoped(&config, a, sql), sql);
    }
    assert_eq!(admin(&config, "select count(*) from notes")[0][0], json!(2));
    assert_eq!(admin(&config, "select count(*) from orders")[0][0], json!(3));
    assert_eq!(admin(&config, "select count(*) from members")[0][0], json!(2));
}

#[tokio::test]
async fn the_person_cannot_hold_a_transaction_or_touch_the_rowid_column() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let a = Some(&people.alice);
    for sql in ["begin", "begin immediate", "savepoint s", "commit", "rollback", "release s", "end transaction"] {
        refused(&scoped(&config, a, sql), sql);
    }
    // Writing the view's own rowid column is a no-op, never a way to
    // re-point the update at another row.
    let out = scoped(&config, a, "update my_orders set ts_rowid = 2, total = 11 where id = 1").unwrap();
    assert!(out.rows_affected <= 1);
    assert_eq!(admin(&config, "select total from orders where id = 1")[0][0], json!(11.0));
    assert_eq!(admin(&config, "select total from orders where id = 2")[0][0], json!(20.0));
    let out = scoped(&config, a, "insert into my_orders (ts_rowid, total, sku) values (2, 12, 'A-12')").unwrap();
    assert_eq!(out.rows_affected, 1);
    assert_eq!(admin(&config, "select owner_id from orders where id = 2")[0][0], json!(people.bob.user_id));
}

// --- identity ----------------------------------------------------------------

#[tokio::test]
async fn no_identity_sees_nothing_and_writes_nothing() {
    let (_dir, config) = server();
    let _people = vault(&config).await;
    let out = scoped(&config, None, "select * from my_orders").unwrap();
    assert!(out.rows.is_empty(), "NULL identity saw rows: {:?}", out.rows);
    let out = scoped(&config, None, "select * from my_records").unwrap();
    assert!(out.rows.is_empty());
    let out = scoped(&config, None, "insert into my_orders (total, sku) values (1, 'N-1')");
    refused(&out, "insert with no identity");
    assert_eq!(admin(&config, "select count(*) from orders")[0][0], json!(3));
    // A row with a NULL owner belongs to nobody, including someone with no
    // identity: NULL = NULL is not true.
    db::run(&config, "vault", "insert into orders (owner_id, total, sku) values (null, 1, 'orphan')", &[]).unwrap();
    let out = scoped(&config, None, "select * from my_orders").unwrap();
    assert!(out.rows.is_empty());
}

#[tokio::test]
async fn a_disabled_or_unknown_account_cannot_be_impersonated_and_a_live_one_can() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let router = build_router(config.clone(), Runtime::new().unwrap());
    let session = mcp_session(&router, "/mcp", TOKEN).await;
    toolsite::accounts::users::set_active(&config, "bob@example.com", false).unwrap();
    let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", json!({"app":"vault","sql":"select total from my_orders","as_user":"bob@example.com"})).await;
    assert!(err && out.contains("disabled"), "{out}");
    let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", json!({"app":"vault","sql":"select 1","as_user":"ghost@example.com"})).await;
    assert!(err && out.contains("no account"), "{out}");
    // The platform's own databases are not an app.
    for app in [".site", "../.site", ".site/auth", "vault/../.site"] {
        let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", json!({"app":app,"sql":"select * from users","as_user":"alice@example.com"})).await;
        assert!(err, "{app}: {out}");
        assert!(!out.contains("alice@example.com") || out.contains("invalid") || out.contains("not"), "{app}: {out}");
    }
    let (err, out) = mcp_tool(&router, "/mcp", TOKEN, &session, "run_sql", json!({"app":"vault","sql":"select total from my_orders","as_user":"alice@example.com"})).await;
    assert!(!err, "{out}");
    assert_eq!(rows_of(&out), json!([[10.0]]));
    let _ = people;
}

// --- boundaries --------------------------------------------------------------

#[tokio::test]
async fn an_app_with_no_declared_access_refuses_everything_rather_than_falling_open() {
    let (_dir, config) = server();
    write_page(&config, "plain/index", "<h1>plain</h1>");
    db::run(&config, "plain", "create table t (x); insert into t values (1)", &[]).unwrap();
    let who = Identity { user_id: "u".into(), email: "u@example.com".into(), role: None };
    let scope = Scope::of(&toolsite::content::store::read_meta_blocking(&config, "plain"));
    let out = db::run_scoped(&config, "plain", Some(&who), &scope, "select 1", &[]);
    refused_or_declares_nothing(&out);
    let out = db::run_scoped(&config, "plain", Some(&who), &scope, "select * from t", &[]);
    refused_or_declares_nothing(&out);
    // Policies declared but not yet generated (manifest before migrations)
    // are the same as none.
    write_page(&config, "early/index", "<h1>early</h1>");
    toolsite::platform::manifest::apply(&config, "early", "[[access.table]]\ntable = \"t\"\nwhere = \"1\"\nwrite = true\n").await.unwrap();
    let scope = Scope::of(&toolsite::content::store::read_meta_blocking(&config, "early"));
    assert!(scope.readable.is_empty() && scope.writable.is_empty());
}

fn refused_or_declares_nothing(out: &Result<db::SqlOutcome, String>) {
    match out {
        Err(message) => assert!(message.contains("declares nothing") || message.contains("not authorized"), "{message}"),
        Ok(outcome) => panic!("fell open: {:?}", outcome.rows),
    }
}

#[tokio::test]
async fn a_person_cannot_query_an_app_they_may_not_open_or_one_that_is_not_there() {
    let (_dir, config) = public_server();
    let people = vault(&config).await;
    // vault is granted-only from now on; alice has no grant.
    toolsite::platform::manifest::apply(&config, "vault", &format!("gate = \"granted\"\n{MANIFEST}")).await.unwrap();
    let token = bearer_for(&config, &people.alice_user);
    let router = build_router(config.clone(), Runtime::new().unwrap());
    let session = mcp_session(&router, "/me/mcp", &token).await;
    let (err, out) = mcp_tool(&router, "/me/mcp", &token, &session, "query", json!({"app":"vault","sql":"select * from my_orders"})).await;
    assert!(err && out.contains("may not open"), "{out}");
    let (_, apps) = mcp_tool(&router, "/me/mcp", &token, &session, "my_apps", json!({})).await;
    assert!(!apps.contains("vault"), "{apps}");
    for app in ["nothing", "..", "../vault", ".site", "vault/../vault", ""] {
        let (err, out) = mcp_tool(&router, "/me/mcp", &token, &session, "query", json!({"app":app,"sql":"select 1"})).await;
        assert!(err, "{app:?} was accepted: {out}");
    }
    // Granted: in. Grant revoked: out again on the very next call.
    toolsite::accounts::users::grant(&config, "alice@example.com", "vault", "viewer").unwrap();
    let (err, out) = mcp_tool(&router, "/me/mcp", &token, &session, "query", json!({"app":"vault","sql":"select total from my_orders"})).await;
    assert!(!err, "{out}");
    assert_eq!(rows_of(&out), json!([[10.0]]));
    toolsite::accounts::users::revoke(&config, "alice@example.com", "vault").unwrap();
    let (err, ..) = mcp_tool(&router, "/me/mcp", &token, &session, "query", json!({"app":"vault","sql":"select total from my_orders"})).await;
    assert!(err, "a revoked grant still worked within the session");
    // Disabled mid-session: the token is dead on the next call.
    toolsite::accounts::users::grant(&config, "alice@example.com", "vault", "viewer").unwrap();
    toolsite::accounts::users::set_active(&config, "alice@example.com", false).unwrap();
    let (status, ..) = mcp_post(&router, "/me/mcp", &token, Some(&session), json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"query","arguments":{"app":"vault","sql":"select 1 from my_orders"}}})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_policy_naming_a_table_that_does_not_exist_is_refused_not_generated_broken() {
    let (_dir, config) = server();
    let _people = vault(&config).await;
    let error = toolsite::platform::manifest::apply(
        &config,
        "vault",
        "[[access.table]]\ntable = \"orders\"\nwhere = \"owner_id in (select user_id from nothing_here)\"\n",
    )
    .await
    .unwrap_err();
    assert!(error.contains("does not parse") || error.contains("no such table"), "{error}");
    // The earlier policies are untouched by the failed apply.
    let meta = toolsite::content::store::read_meta_blocking(&config, "vault");
    assert_eq!(meta.policies.len(), 4, "{:?}", meta.policies);
    // A where with a bound parameter cannot become a view either.
    let error = toolsite::platform::manifest::apply(&config, "vault", "[[access.table]]\ntable = \"orders\"\nwhere = \"owner_id = ?\"\n").await.unwrap_err();
    assert!(!error.is_empty());
    let left = admin(&config, "select count(*) from sqlite_master where name = 'my_orders'");
    assert_eq!(left[0][0], json!(1));
}

#[tokio::test]
async fn a_migration_that_breaks_a_policy_fails_loudly_and_opens_nothing() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    toolsite::runtime::migrate::store(
        &config,
        "vault",
        vec![
            ("001_initial.sql".to_string(), SCHEMA.to_string()),
            ("002_drop_owner.sql".to_string(), "alter table orders drop column owner_id;".to_string()),
        ],
    )
    .unwrap();
    let error = toolsite::runtime::migrate::apply(&config, "vault").unwrap_err();
    assert!(error.contains("access") || error.contains("owner_id") || error.contains("migration failed"), "{error}");
    // Whatever state the view is in, alice does not now see bob's rows.
    if let Ok(out) = scoped(&config, Some(&people.alice), "select total from my_orders") {
        assert!(!totals(&out).contains(&20.0), "the broken view opened bob's rows: {:?}", out.rows);
    }
    refused(&scoped(&config, Some(&people.alice), "select * from orders"), "base table after a broken migration");
}

#[tokio::test]
async fn a_handler_calling_query_scoped_on_an_app_without_policies_is_refused() {
    let (_dir, config) = server();
    write_page(&config, "bare/index", "<h1>bare</h1>");
    let dir = config.data_dir.join("bare");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("handler.wasm"), include_bytes!("fixtures/handler.wasm")).unwrap();
    db::run(&config, "bare", "create table t (x); insert into t values (1)", &[]).unwrap();
    let router = build_router(config.clone(), Runtime::new().unwrap());
    let request = Request::builder().uri("/p/bare/api/scoped?q=select+*+from+t").body(Body::empty()).unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = String::from_utf8_lossy(&axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap()).to_string();
    assert_ne!(status, StatusCode::OK, "query-scoped fell open on an app with no policies: {body}");
    assert!(!body.contains("1"), "{body}");
}

// --- resource abuse ----------------------------------------------------------

#[tokio::test]
async fn a_statement_that_never_ends_is_stopped() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let started = std::time::Instant::now();
    let out = scoped(&config, Some(&people.alice), "with recursive r(n) as (select 1 union all select n + 1 from r) select count(*) from r");
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "the statement ran for {:?}", started.elapsed());
    refused(&out, "runaway recursive query");
    // Pulling rows out one by one is capped by the row limit as before.
    let out = scoped(&config, Some(&people.alice), "with recursive r(n) as (select 1 union all select n + 1 from r) select n from r").unwrap();
    assert!(out.truncated);
}

#[tokio::test]
async fn a_statement_cannot_ask_for_a_gigabyte() {
    let (_dir, config) = server();
    let people = vault(&config).await;
    let out = scoped(&config, Some(&people.alice), "select length(zeroblob(900000000))");
    refused(&out, "a near-gigabyte blob");
    let out = scoped(&config, Some(&people.alice), "select length(randomblob(900000000))");
    refused(&out, "a near-gigabyte random blob");
    let long = format!("select 1 where '{}' = 'x'", "a".repeat(2_000_000));
    refused(&scoped(&config, Some(&people.alice), &long), "a two megabyte statement");
}

// --- helpers shared with the main suite -------------------------------------

fn bearer_for(config: &Config, user: &toolsite::accounts::users::User) -> String {
    let client = toolsite::platform::oauth_store::register_client(config, Some("t"), &["https://c.test/cb".into()]).unwrap();
    toolsite::platform::oauth_store::issue_tokens(config, &client.id, &user.id).unwrap().access_token
}

async fn mcp_post(router: &axum::Router, path: &str, token: &str, session: Option<&str>, body: Value) -> (StatusCode, Option<String>, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let session = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .next_back()
        .or_else(|| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    (status, session, json)
}

async fn mcp_session(router: &axum::Router, path: &str, token: &str) -> String {
    let (status, session, json) = mcp_post(
        router,
        path,
        token,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    // The transport is stateless: no session id is issued, and none is sent.
    let session = session.unwrap_or_default();
    let (status, ..) = mcp_post(router, path, token, (!session.is_empty()).then_some(session.as_str()), json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
    assert!(status.is_success(), "initialized notification: {status}");
    session
}

async fn mcp_tool(router: &axum::Router, path: &str, token: &str, session: &str, name: &str, arguments: Value) -> (bool, String) {
    let (status, _, json) = mcp_post(
        router,
        path,
        token,
        (!session.is_empty()).then_some(session),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let result = &json["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
    (is_error, text)
}

fn rows_of(text: &str) -> Value {
    serde_json::from_str::<Value>(text).map(|v| v["rows"].clone()).unwrap_or(Value::Null)
}
