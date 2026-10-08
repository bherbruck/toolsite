//! Benchmark guest: the workload in work.rs, run through the host's db import.

wit_bindgen::generate!({
    path: "../../../wit",
    world: "app",
});

mod work;

use toolsite::app::db;
use work::V;

struct Handler;

struct Host;

impl work::Db for Host {
    fn query(&mut self, sql: &str, params: &[V]) -> Vec<Vec<V>> {
        let params: Vec<db::Value> = params
            .iter()
            .map(|v| match v {
                V::Null => db::Value::Null,
                V::Int(i) => db::Value::Integer(*i),
                V::Real(f) => db::Value::Real(*f),
                V::Text(s) => db::Value::Text(s.clone()),
            })
            .collect();
        let rows = db::query(sql, &params).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        rows.values
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| match v {
                        db::Value::Null => V::Null,
                        db::Value::Integer(i) => V::Int(i),
                        db::Value::Real(f) => V::Real(f),
                        db::Value::Text(s) => V::Text(s),
                    })
                    .collect()
            })
            .collect()
    }
}

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        let body = match req.path.as_str() {
            "/page" => work::page(&mut Host),
            "/lookup" => work::lookup(&mut Host),
            "/cpu" => work::cpu(),
            "/trivial" => work::trivial(&mut Host),
            "/write" => work::write(&mut Host),
            "/noop" => String::new(),
            _ => return Response { status: 404, headers: Vec::new(), body: Vec::new() },
        };
        Response { status: 200, headers: Vec::new(), body: body.into_bytes() }
    }
}

export!(Handler);
