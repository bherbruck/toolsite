//! The benchmark's workload, written once against a minimal database trait so
//! the guest and the native comparison run exactly the same logic. Shaped
//! like the report that prompted it: a forecast over 70k formulation rows,
//! paged 1,000 at a time, plus lookups, arithmetic and row-at-a-time writes.

use std::collections::BTreeMap;

pub const ROWS: i64 = 70_000;
pub const PAGE: i64 = 1_000;

pub enum V {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl V {
    fn f(&self) -> f64 {
        match self {
            V::Int(i) => *i as f64,
            V::Real(f) => *f,
            _ => 0.0,
        }
    }
}

pub trait Db {
    /// One statement, its rows.
    fn query(&mut self, sql: &str, params: &[V]) -> Vec<Vec<V>>;
}

/// Pages through every row with OFFSET, as the reported app did, and totals
/// cost per product in Rust.
pub fn page(db: &mut impl Db) -> String {
    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    let mut seen = 0;
    let mut offset = 0;
    loop {
        let rows = db.query(
            "select id, product, ingredient, qty, cost from formulation order by id limit ? offset ?",
            &[V::Int(PAGE), V::Int(offset)],
        );
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            if let V::Text(product) = &row[1] {
                *totals.entry(product.clone()).or_default() += row[3].f() * row[4].f();
            }
        }
        seen += rows.len();
        offset += PAGE;
    }
    let sum: f64 = totals.values().sum();
    format!("rows={seen} products={} sum={sum:.3}", totals.len())
}

/// Many small indexed reads.
pub fn lookup(db: &mut impl Db) -> String {
    let mut sum = 0.0;
    for i in 0..5_000i64 {
        let id = (i * 7_919) % ROWS + 1;
        let rows = db.query("select qty, cost from formulation where id = ?", &[V::Int(id)]);
        sum += rows.first().map_or(0.0, |r| r[0].f() * r[1].f());
    }
    format!("sum={sum:.3}")
}

/// Pure arithmetic, no host calls: a 300x300 matrix product, 54M flops.
pub fn cpu() -> String {
    const N: usize = 300;
    let a: Vec<f64> = (0..N * N).map(|i| (i % 17) as f64 * 0.5).collect();
    let b: Vec<f64> = (0..N * N).map(|i| (i % 13) as f64 * 0.25).collect();
    let mut c = vec![0.0f64; N * N];
    for i in 0..N {
        for k in 0..N {
            let x = a[i * N + k];
            for j in 0..N {
                c[i * N + j] += x * b[k * N + j];
            }
        }
    }
    format!("trace={:.3}", (0..N).map(|i| c[i * N + i]).sum::<f64>())
}

/// Row-at-a-time inserts, each its own statement and so its own commit.
pub fn write(db: &mut impl Db) -> String {
    db.query("create table if not exists written (id integer primary key, product text, qty real)", &[]);
    db.query("delete from written", &[]);
    for i in 0..10_000i64 {
        db.query(
            "insert into written (product, qty) values (?, ?)",
            &[V::Text(format!("p{}", i % 50)), V::Real(i as f64 * 0.5)],
        );
    }
    let rows = db.query("select count(*) from written", &[]);
    format!("written={}", rows.first().map_or(0.0, |r| r[0].f()))
}

