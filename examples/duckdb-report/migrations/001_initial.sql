-- SQLite keeps what the page needs to find the data, never the data. Each
-- row names one Parquet file in the app's file store, written by the
-- build-report job; the rows themselves live in that file.

create table report_file (
    key text primary key,
    rows integer not null,
    bytes integer not null,
    created_at integer not null default (cast(strftime('%s', 'now') as integer))
);
