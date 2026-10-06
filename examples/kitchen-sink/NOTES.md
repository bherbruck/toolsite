# kitchen-sink

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- Screens switch on the URL hash. With a handler, a path with no file goes
  to the handler, so client-side paths would need the handler to answer them.
- The Database screen shows totals only. The handler's own `db::query`
  reaches every row, so it must not hand rows past the policy to a browser.
- `members` is keyed by email, so a manager can place a person before their
  first sign-in.
- `/api/heartbeat` refuses any call without `x-toolsite-scheduled`. The host
  strips that header from visitors, so only the schedule can send it.
- The Settings screen says whether `GREETING` is set and never shows it.

## Unfinished
