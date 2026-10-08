//! A handler's cost against the same Rust run natively, per workload.
//!
//! `cargo bench --bench handler` (release). Not part of `cargo test`: it
//! takes minutes and its numbers only mean something optimised. The guest
//! is tests/fixtures/bench, whose work.rs is compiled here too, so both
//! sides run identical logic over the same database file.
//!
//! `BENCH_RUNS` sets the repetitions per route (default 3); the median is
//! reported, since a loaded machine makes the mean meaningless.
//! `BENCH_ROUTES=page,write` runs only those. The database carries a
//! production-sized schema besides the table the routes use.

#[path = "../tests/fixtures/bench/src/work.rs"]
mod work;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use toolsite::{
    config::Config,
    runtime::wasm::{Guards, Request, Runtime, User},
};
use work::V;

const APP: &str = "bench";

/// Generous enough that no workload is cut short: the point is what each
/// costs, not where the production ceilings sit.
/// Rows per query stay at the production default, so paging costs what it
/// does for a real app.
const GUARDS: Guards = Guards {
    fuel: Some(50_000_000_000),
    memory_bytes: 256 * 1024 * 1024,
    wall_clock: Duration::from_secs(600),
    query_rows: 1_000,
};

struct Native(rusqlite::Connection);

impl work::Db for Native {
    fn query(&mut self, sql: &str, params: &[V]) -> Vec<Vec<V>> {
        let mut statement = self.0.prepare_cached(sql).unwrap();
        let params: Vec<rusqlite::types::Value> = params
            .iter()
            .map(|v| match v {
                V::Null => rusqlite::types::Value::Null,
                V::Int(i) => rusqlite::types::Value::Integer(*i),
                V::Real(f) => rusqlite::types::Value::Real(*f),
                V::Text(s) => rusqlite::types::Value::Text(s.clone()),
            })
            .collect();
        let columns = statement.column_count();
        let mut rows = statement.query(rusqlite::params_from_iter(params)).unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            out.push(
                (0..columns)
                    .map(|i| match row.get_ref(i).unwrap() {
                        rusqlite::types::ValueRef::Integer(i) => V::Int(i),
                        rusqlite::types::ValueRef::Real(f) => V::Real(f),
                        rusqlite::types::ValueRef::Text(t) => V::Text(String::from_utf8_lossy(t).into_owned()),
                        _ => V::Null,
                    })
                    .collect(),
            );
        }
        out
    }
}

fn seed(config: &Config) {
    let path = config.data_dir.join(APP).join("data.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "create table formulation (id integer primary key, product text, ingredient text, qty real, cost real);
         insert into formulation (product, ingredient, qty, cost)
           with recursive c(i) as (select 1 union all select i + 1 from c where i < 70000)
           select 'product-' || (i % 400), 'ingredient-' || (i % 1300), (i % 97) * 0.25, (i % 31) * 1.5 from c;",
    )
    .unwrap();
    // The rest of a real app's schema, at the size of one in production:
    // SQLite parses all of it on a connection's first statement.
    let mut schema = String::new();
    for t in 0..SCHEMA_TABLES {
        schema += &format!(
            "create table t{t} (id integer primary key, name text, amount real, ref integer, updated_at text);
             create index t{t}_name on t{t} (name);
             create trigger t{t}_stamp after update on t{t} begin
               update t{t} set updated_at = datetime('now') where id = new.id;
             end;"
        );
        if t % 2 == 0 {
            schema += &format!(
                "create index t{t}_ref on t{t} (ref, amount);
                 create trigger t{t}_guard before insert on t{t} when new.amount < 0 begin
                   select raise(abort, 'negative amount');
                 end;"
            );
        }
    }
    conn.execute_batch(&schema).unwrap();
}

/// 100 tables, 150 indexes, 150 triggers: about the size of the schema
/// whose app prompted this.
const SCHEMA_TABLES: usize = 100;

/// What the pieces of one host round trip cost on their own, each averaged
/// over many repetitions.
fn attribute(site: &Config, user: &User) {
    let each = |what: &str, times: u32, mut f: Box<dyn FnMut() + '_>| {
        let started = Instant::now();
        for _ in 0..times {
            f();
        }
        println!("  {what:<44} {:>10.1?}", started.elapsed() / times);
    };
    let path = site.data_dir.join(APP).join("data.db");
    println!("one round trip, by piece:");
    each("look up the caller's role", 500, Box::new(|| {
        toolsite::accounts::users::role_for(site, &user.id, APP);
    }));
    each("open and close the app database", 500, Box::new(|| {
        toolsite::runtime::db::open_unguarded(&path, site.max_db_bytes).unwrap();
    }));
    each("open, run select 1, close (parses the schema)", 500, Box::new(|| {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.query_row("select 1", [], |_| Ok(())).unwrap();
    }));
    each("run_as select 1 (the old per-query path)", 500, Box::new(|| {
        let who = toolsite::runtime::db::Identity { user_id: user.id.clone(), email: user.email.clone(), role: None };
        toolsite::runtime::db::run_as(site, APP, Some(&who), "select 1", &[]).unwrap();
    }));
    let conn = toolsite::runtime::db::open_unguarded(&path, site.max_db_bytes).unwrap();
    let lookup = "select qty, cost from formulation where id = ?";
    each("prepare and run one lookup", 5_000, Box::new(|| {
        conn.prepare(lookup).unwrap().query_row([7], |_| Ok(())).unwrap();
    }));
    each("run one cached lookup", 5_000, Box::new(|| {
        conn.prepare_cached(lookup).unwrap().query_row([7], |_| Ok(())).unwrap();
    }));
    conn.execute_batch("create table if not exists scratch (x)").unwrap();
    each("commit one insert, synchronous=full", 300, Box::new(|| {
        conn.execute("insert into scratch values (1)", []).unwrap();
    }));
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
    each("commit one insert, synchronous=normal", 300, Box::new(|| {
        conn.execute("insert into scratch values (1)", []).unwrap();
    }));
    println!();
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn main() {
    let runs: usize = std::env::var("BENCH_RUNS").ok().and_then(|n| n.parse().ok()).unwrap_or(3);
    // On the target's own disk: /tmp is often tmpfs, where a commit's fsync
    // costs nothing and the write route would flatter the host.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let site = Arc::new(Config::local(dir.path().to_path_buf(), "bench-token"));
    seed(&site);

    // A signed-in visitor with a grant, so the identity a query carries is
    // looked up as it is for a real person.
    let account = toolsite::accounts::users::sign_up(&site, "bench@example.com", "correct horse battery").unwrap();
    toolsite::accounts::users::grant(&site, "bench@example.com", APP, "planner").unwrap();
    let user = User { id: account.id, email: account.email };

    let wasm = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bench.wasm")).unwrap();
    let runtime = Runtime::new().unwrap();
    let call = |path: &str| {
        let request = Request {
            method: "GET".into(),
            path: path.into(),
            query: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let started = Instant::now();
        let response = runtime.handle(site.clone(), APP, &wasm, Some(user.clone()), request, GUARDS).unwrap();
        assert_eq!(response.status, 200, "{path}: {}", String::from_utf8_lossy(&response.body));
        (started.elapsed(), String::from_utf8(response.body).unwrap())
    };

    attribute(&site, &user);

    let (first, _) = call("/noop");
    let noop = median((0..50).map(|_| call("/noop").0).collect());
    println!("first call (compile + run): {first:?}; a call doing nothing: {noop:?}");
    println!();
    println!("{:<8} {:>12} {:>12} {:>8}  result", "route", "toolsite", "native", "ratio");

    let path = site.data_dir.join(APP).join("data.db");
    let only = std::env::var("BENCH_ROUTES").unwrap_or_default();
    for route in ["trivial", "page", "lookup", "cpu", "write"] {
        if !only.is_empty() && !only.split(',').any(|r| r == route) {
            continue;
        }
        let mut answer = String::new();
        let guest = median(
            (0..runs)
                .map(|_| {
                    let (took, body) = call(&format!("/{route}"));
                    answer = body;
                    took
                })
                .collect(),
        );
        let mut native_answer = String::new();
        let native = median(
            (0..runs)
                .map(|_| {
                    // Committing as the host does, without waiting for the disk.
                    let conn = rusqlite::Connection::open(&path).unwrap();
                    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
                    let mut db = Native(conn);
                    let started = Instant::now();
                    native_answer = match route {
                        "page" => work::page(&mut db),
                        "lookup" => work::lookup(&mut db),
                        "cpu" => work::cpu(),
                        "trivial" => work::trivial(&mut db),
                        _ => work::write(&mut db),
                    };
                    started.elapsed()
                })
                .collect(),
        );
        assert_eq!(answer, native_answer, "{route}: the guest and native disagree");
        println!(
            "{route:<8} {:>12} {:>12} {:>7.1}x  {answer}",
            format!("{guest:.2?}"),
            format!("{native:.2?}"),
            guest.as_secs_f64() / native.as_secs_f64()
        );
    }
}
