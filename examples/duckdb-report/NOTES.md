# duckdb-report

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- The data is a file, not rows. SQLite holds `report_file` only: which file
  is current and what is in it.
- Each build writes a new key (`reports/sales-<time>.parquet`) and then
  deletes the older files and rows. A key never changes content, so the
  route can say `immutable`.
- Only keys in `report_file` are served, so the route cannot hand out any
  other file the app keeps.
- Encoding settings were chosen for the job's instruction budget:
  dictionaries on region, store and product; plain day, units and revenue;
  snappy; no statistics. Dictionaries on every column and chunk statistics
  made the file smaller but cost about twice the instructions.
- The page ships only DuckDB's exception-handling build. Every current
  browser runs it, and it halves the bundle.

## Unfinished

- One file per run. More rows would mean one file per month, each its own
  run, registered together on the page.
