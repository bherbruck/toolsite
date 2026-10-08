# duckdb-report

Two years of daily sales by store and product, written as a Parquet file by
a nightly job and queried in the browser with DuckDB. SQLite keeps one row
per file, never the data.

## What it shows

- A `[[job]]` that writes 365,000 rows of Parquet with the streaming writer
  (`blobs.writer-open`, `writer-append`, `writer-finish`), a month of rows
  per row group, so the handler never holds the whole file. It uses the
  `parquet` crate with snappy, built for wasm32-wasip2.
- A `report_file` table in SQLite: key, rows, bytes, created_at. That is
  all SQLite holds.
- `GET /api/files/<key>`: the handler checks the caller has a grant and
  that the key is one the job recorded, then answers
  `x-toolsite-blob: <key>` with an empty body. The platform sends the whole
  file.
- A Vite + React + Tailwind page that fetches the file through that route,
  hands the bytes to DuckDB-wasm with `registerFileBuffer`, and runs
  grouped queries and a chart in the tab. It shows the file's size, how long
  the download took, and how long the queries took.
- DuckDB bundled with the app (`@duckdb/duckdb-wasm`, its wasm and worker
  imported with `?url`), so it loads from the app's own files, not a CDN.

DuckDB never reads a URL here: no httpfs, no `read_parquet('<url>')`, no
`ATTACH '<url>'`. Every byte comes through the handler, which is where
access is decided.

## Start it

```sh
toolsite init my-report --example duckdb-report
cd my-report
toolsite deploy
toolsite job my-report build-report --now
toolsite grant my-report someone@example.com --role analyst
```

The page opens for any signed-in account, but the file goes only to
someone with a grant on the app.

## How big a file one run can write

A job runs with two billion instructions and a minute. Encoding Parquet in
wasm costs about 3,600 instructions a row with these settings
(dictionaries on the text columns only, snappy, no statistics), so this job
uses about 1.4 billion for its 365,000 rows and makes a 2.3 MB file. For
more rows, make several files, one per month say, each in its own run, and
register them all in DuckDB (`registerFileBuffer` once per file, then
`read_parquet(['a.parquet', 'b.parquet'])`).

## Per person: one file each

A file has no row-level policy: whoever gets the file gets every row in
it. When people should see different rows, split the data when you write
it, and let the handler pick the key:

```rust
// The job: one file per region, written the same way.
for region in REGIONS {
    let key = format!("reports/{stamp}/{region}.parquet");
    // writer_open(&key, ...), the rows for that region, writer_finish(...)
}

// The route: the caller's role names their file; nobody can ask for
// another one.
fn serve_mine(stamp: i64) -> Answer {
    let role = identity::current_role().ok_or((403, "No access.".to_string()))?;
    let region = role.strip_prefix("region-").ok_or((403, "No region.".to_string()))?;
    let key = format!("reports/{stamp}/{region}.parquet");
    // check the key is one the job recorded, then answer x-toolsite-blob: key
}
```

The same works per person, with `reports/user-<id>.parquet` and
`identity::current_user()`.

## A .duckdb file instead

A DuckDB database file is delivered the same way. Build it on your machine,
or in the browser (DuckDB-wasm can write one), and upload it with
`blobs::upload_url` or from a shell:

```sh
curl -f -T report.duckdb '<upload-url>?blob=reports/report.duckdb'
```

Serve it through the handler like the Parquet file, then on the page:

```ts
await db.registerFileBuffer('report.duckdb', new Uint8Array(buffer))
await conn.query(`ATTACH 'report.duckdb' AS report (READ_ONLY)`)
```

## Files

- `handler/src/lib.rs`: the job, the file list and the route that serves a
  file.
- `migrations/001_initial.sql`: the `report_file` table.
- `src/duck.ts`: DuckDB from the app's own files, loading a file through the
  handler, and running a query.
- `src/App.tsx`, `src/charts.tsx`: the page.
