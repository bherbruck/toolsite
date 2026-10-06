# inventory-policies

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- No handler. The policies are the whole access model, and `/me/mcp` is the
  interface. An app with screens would add a handler that reads through the
  same views with `db::query-scoped`.
- `members` is keyed by email, so a person can be placed before they first
  sign in.
- `stock_totals` has no location column. Every declared view is readable by
  every person with access, so it holds only what all of them may know.
- A person with no `members` row sees nothing: `location = NULL` is not true.

## Unfinished

- The example rows name alice@example.com and bob@example.com. Replace them.
