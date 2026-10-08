//! Two years of daily sales, written as Parquet by a job and queried in the
//! browser by DuckDB.
//!
//! SQLite holds one row per file (`report_file`); the data is the file. The
//! job writes it with the streaming writer, one row group at a time, so the
//! handler never holds more than a month of rows. The page asks
//! `/api/files/<key>`, the handler decides whether this person may have it,
//! and answers `x-toolsite-blob: <key>` with an empty body. The platform
//! sends the whole file, and the page hands it to DuckDB-wasm as a buffer.

wit_bindgen::generate!({
    path: "wit",
    world: "app",
});

use parquet::{
    basic::Compression,
    data_type::{ByteArray, ByteArrayType, DoubleType, Int32Type},
    file::{
        properties::{EnabledStatistics, WriterProperties},
        writer::SerializedFileWriter,
    },
    schema::{parser::parse_message_type, types::ColumnPath},
};
use serde_json::{json, Value};
use std::{io::Write, sync::Arc};
use toolsite::app::{blobs, db, identity};

/// The day the data starts, as days since 1970-01-01: 2024-01-01.
const FIRST_DAY: i32 = 19_723;
const DAYS: i32 = 730;
const REGIONS: [&str; 4] = ["North", "South", "East", "West"];
const STORES: usize = 25;
/// Every store sells every product every day: 730 x 25 x 20 is 365,000
/// rows. A job runs under an instruction budget (two billion), and encoding
/// Parquet in wasm costs about 3,600 instructions a row, so one run makes
/// about half a million rows at most. More than that is several files, one
/// per run, or a larger budget for the job.
const PRODUCTS: usize = 20;
const ROWS_PER_DAY: usize = STORES * PRODUCTS;
/// A row group is a month of rows, 15,000. DuckDB works a row group at a
/// time, and the job never holds more than one.
const DAYS_PER_GROUP: i32 = 30;
/// Bytes kept before they go to the writer, so one append carries a
/// megabyte rather than every small write the encoder makes.
const FLUSH_BYTES: usize = 1 << 20;

const SCHEMA: &str = "
message sales {
    required int32 day (DATE);
    required binary region (STRING);
    required binary store (STRING);
    required binary product (STRING);
    required int32 units;
    required double revenue;
}";

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let answer = match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["api", "me"]) => me(),
            ("GET", ["api", "build"]) => build(&req),
            ("GET", ["api", "files"]) => files(),
            ("GET", ["api", "files", rest @ ..]) if !rest.is_empty() => serve(&rest.join("/")),
            _ => Err((404, "No such route.".to_string())),
        };
        match answer {
            Ok(response) => response,
            Err((status, message)) => json_response(status, &json!({ "error": message })),
        }
    }
}

export!(Handler);

type Answer = Result<Response, (u16, String)>;

fn me() -> Answer {
    let user = identity::current_user().map(|u| json!({ "id": u.id, "email": u.email }));
    Ok(json_response(200, &json!({ "user": user, "role": identity::current_role() })))
}

/// Anyone with a grant on the app may read its files. A file has no
/// row-level policy, so this check is the whole of who gets it.
fn may_read() -> Result<(), (u16, String)> {
    if identity::current_user().is_none() {
        return Err((401, "Sign in first.".to_string()));
    }
    if identity::current_role().is_none() {
        return Err((403, "Ask the owner for access to the report files.".to_string()));
    }
    Ok(())
}

fn files() -> Answer {
    may_read()?;
    let rows = db::query("select key, rows, bytes, created_at from report_file order by created_at desc, key desc", &[])
        .map_err(|e| (500, db_message(&e)))?;
    let files: Vec<Value> = rows
        .values
        .iter()
        .map(|row| {
            let mut file = serde_json::Map::new();
            for (column, value) in rows.columns.iter().zip(row) {
                file.insert(column.clone(), plain(value));
            }
            Value::Object(file)
        })
        .collect();
    Ok(json_response(200, &json!({ "files": files })))
}

/// A header and no body: the platform sends the whole file in its place.
/// Only keys the job recorded are served, so this route cannot be used to
/// read anything else the app keeps.
fn serve(key: &str) -> Answer {
    may_read()?;
    let known = db::query("select 1 from report_file where key = ?", &[db::Value::Text(key.to_string())])
        .map_err(|e| (500, db_message(&e)))?;
    if known.values.is_empty() {
        return Err((404, "No such file.".to_string()));
    }
    Ok(Response {
        status: 200,
        headers: vec![
            ("x-toolsite-blob".to_string(), key.to_string()),
            ("content-type".to_string(), "application/vnd.apache.parquet".to_string()),
            // The file under a key never changes: a new build is a new key.
            ("cache-control".to_string(), "private, max-age=86400, immutable".to_string()),
        ],
        body: Vec::new(),
    })
}

/// The job. The host sets `x-toolsite-scheduled` and strips any
/// `x-toolsite-*` header a client sends, so a visitor cannot run it.
fn build(req: &Request) -> Answer {
    if !req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-toolsite-scheduled")) {
        return Err((403, "This path runs on the schedule only.".to_string()));
    }
    let stamp = db::query("select cast(strftime('%s', 'now') as integer)", &[])
        .ok()
        .and_then(|rows| match rows.values.first().and_then(|r| r.first()) {
            Some(db::Value::Integer(n)) => Some(*n),
            _ => None,
        })
        .unwrap_or(0);
    let key = format!("reports/sales-{stamp}.parquet");
    let handle = blobs::writer_open(&key, "application/vnd.apache.parquet").map_err(|e| (500, blob_message(&e)))?;
    let rows = match write_sales(handle) {
        Ok(rows) => rows,
        Err(why) => {
            blobs::writer_abort(handle);
            return Err((500, why));
        }
    };
    let entry = blobs::writer_finish(handle).map_err(|e| (500, blob_message(&e)))?;
    db::query(
        "insert or replace into report_file (key, rows, bytes, created_at) values (?, ?, ?, ?)",
        &[
            db::Value::Text(key.clone()),
            db::Value::Integer(rows as i64),
            db::Value::Integer(entry.size as i64),
            db::Value::Integer(stamp),
        ],
    )
    .map_err(|e| (500, db_message(&e)))?;
    // Older builds are superseded. Their files go, and so do their rows.
    if let Ok(old) = db::query("select key from report_file where key != ?", &[db::Value::Text(key.clone())]) {
        for row in &old.values {
            if let Some(db::Value::Text(old_key)) = row.first() {
                let _ = blobs::delete(old_key);
                let _ = db::query("delete from report_file where key = ?", &[db::Value::Text(old_key.clone())]);
            }
        }
    }
    Ok(json_response(200, &json!({ "key": key, "rows": rows, "bytes": entry.size })))
}

/// Bytes on their way to the blob writer, a megabyte at a time.
struct BlobSink {
    handle: u64,
    pending: Vec<u8>,
}

impl BlobSink {
    fn send(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        blobs::writer_append(self.handle, &self.pending).map_err(|e| std::io::Error::other(blob_message(&e)))?;
        self.pending.clear();
        Ok(())
    }
}

impl Write for BlobSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= FLUSH_BYTES {
            self.send()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
    }
}

/// A small fast generator, seeded, so every build makes the same data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One row group per month of days, each column written whole before the
/// next. Returns how many rows went in.
fn write_sales(handle: u64) -> Result<usize, String> {
    let schema = Arc::new(parse_message_type(SCHEMA).map_err(|e| e.to_string())?);
    // What costs a job its instruction budget is encoding, not making rows:
    // dictionaries on the three text columns, where they shrink the file
    // most, and nowhere else; snappy on every page; and no statistics,
    // which only help a reader that skips row groups over the network, and
    // this file is always read whole.
    let plain = |column: &str| (ColumnPath::from(column), false);
    let mut props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_created_by("toolsite duckdb-report".to_string());
    for (column, dictionary) in [plain("day"), plain("units"), plain("revenue")] {
        props = props.set_column_dictionary_enabled(column, dictionary);
    }
    let props = Arc::new(props.build());
    let sink = BlobSink { handle, pending: Vec::with_capacity(FLUSH_BYTES * 2) };
    let mut writer = SerializedFileWriter::new(sink, schema, props).map_err(|e| e.to_string())?;

    let stores: Vec<ByteArray> = (1..=STORES).map(|n| ByteArray::from(format!("Store {n:02}").as_str())).collect();
    let store_region: Vec<usize> = (0..STORES).map(|n| n % REGIONS.len()).collect();
    let regions: Vec<ByteArray> = REGIONS.iter().map(|r| ByteArray::from(*r)).collect();
    let products: Vec<ByteArray> = (1..=PRODUCTS).map(|n| ByteArray::from(format!("Product {n:03}").as_str())).collect();
    // Each product has a price in cents and a usual daily volume.
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let price: Vec<i64> = (0..PRODUCTS).map(|_| 199 + rng.below(4_800) as i64).collect();
    let volume: Vec<u64> = (0..PRODUCTS).map(|_| 2 + rng.below(40)).collect();
    // Stores differ in size, from 60% to 140% of an average one.
    let size: Vec<u64> = (0..STORES).map(|_| 60 + rng.below(81)).collect();

    let mut total = 0;
    let mut start = 0;
    while start < DAYS {
        let end = (start + DAYS_PER_GROUP).min(DAYS);
        let n = (end - start) as usize * ROWS_PER_DAY;
        let mut day = Vec::with_capacity(n);
        let mut region = Vec::with_capacity(n);
        let mut store = Vec::with_capacity(n);
        let mut product = Vec::with_capacity(n);
        let mut units = Vec::with_capacity(n);
        let mut revenue = Vec::with_capacity(n);
        for d in start..end {
            let date = FIRST_DAY + d;
            // Weekends sell more, December most of all, and the business
            // grows by a fifth over the two years.
            let weekday = (date + 4).rem_euclid(7);
            let weekend = if weekday >= 5 { 3 } else { 2 };
            let month = ((d % 365) / 31) as u64;
            let season = if month == 11 { 3 } else { 2 };
            for s in 0..STORES {
                for p in 0..PRODUCTS {
                    let growth = 100 + d as u64 * 20 / DAYS as u64;
                    let base = volume[p] * weekend * season * size[s] * growth / 40_000;
                    let sold = base + rng.below(base + 1);
                    day.push(date);
                    region.push(regions[store_region[s]].clone());
                    store.push(stores[s].clone());
                    product.push(products[p].clone());
                    units.push(sold as i32);
                    revenue.push((sold as i64 * price[p]) as f64 / 100.0);
                }
            }
        }

        let mut group = writer.next_row_group().map_err(|e| e.to_string())?;
        let mut column = 0;
        while let Some(mut col) = group.next_column().map_err(|e| e.to_string())? {
            let written = match column {
                0 => col.typed::<Int32Type>().write_batch(&day, None, None),
                1 => col.typed::<ByteArrayType>().write_batch(&region, None, None),
                2 => col.typed::<ByteArrayType>().write_batch(&store, None, None),
                3 => col.typed::<ByteArrayType>().write_batch(&product, None, None),
                4 => col.typed::<Int32Type>().write_batch(&units, None, None),
                _ => col.typed::<DoubleType>().write_batch(&revenue, None, None),
            };
            written.map_err(|e| e.to_string())?;
            col.close().map_err(|e| e.to_string())?;
            column += 1;
        }
        group.close().map_err(|e| e.to_string())?;
        total += n;
        start = end;
    }
    let mut sink = writer.into_inner().map_err(|e| e.to_string())?;
    sink.flush().map_err(|e| e.to_string())?;
    Ok(total)
}

// --- helpers -------------------------------------------------------------------------

fn plain(value: &db::Value) -> Value {
    match value {
        db::Value::Null => Value::Null,
        db::Value::Integer(i) => json!(i),
        db::Value::Real(f) => json!(f),
        db::Value::Text(t) => json!(t),
    }
}

fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: value.to_string().into_bytes(),
    }
}

fn db_message(error: &db::Error) -> String {
    match error {
        db::Error::Failed(m) | db::Error::Denied(m) => m.clone(),
    }
}

fn blob_message(error: &blobs::Error) -> String {
    match error {
        blobs::Error::NotFound => "not found".to_string(),
        blobs::Error::InvalidKey(why) => format!("invalid key: {why}"),
        blobs::Error::TooLarge(size) => format!("too large: {size} bytes"),
        blobs::Error::Failed(why) => why.clone(),
    }
}
