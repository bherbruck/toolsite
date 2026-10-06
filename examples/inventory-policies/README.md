# inventory-policies

Stock at two warehouses, with row-level policies and nothing else: no
handler, no build, no screens. Each person sees the stock of their own
warehouse. People work with the data from their own AI assistant through
`/me/mcp`.

## What it shows

- `[[access.table]]` policies that scope rows by a membership table.
- `write = true`, so a person can change stock at their warehouse, and a
  write that would put a row somewhere else is refused.
- `owner = "by_user"`, which fills the column with the person's account id.
- A read-only policy (`members`): each person sees their own row only.
- A hand-written view under `[access] views` (`stock_totals`), which every
  person may read. It has no location column on purpose.

## Start it

```sh
toolsite init my-inventory --example inventory-policies
cd my-inventory
toolsite deploy
```

The app is restricted: grant the people who use it from the site admin.

## Prove the policies

Do this before you tell anyone the policies hold. `run_sql` with `as_user`
runs a statement exactly as that account would through `/me/mcp`.

1. Make two accounts, if they do not exist:

   ```
   create_user(email: "alice@example.com")
   create_user(email: "bob@example.com")
   ```

   The first migration places alice at `north` and bob at `south`.

2. Run the same query as each of them:

   ```
   run_sql(app: "my-inventory", sql: "select location, sku, quantity from my_stock order by sku", as_user: "alice@example.com")
   run_sql(app: "my-inventory", sql: "select location, sku, quantity from my_stock order by sku", as_user: "bob@example.com")
   ```

   Alice gets three `north` rows. Bob gets three `south` rows.

3. Try to reach past the policy:

   ```
   run_sql(app: "my-inventory", sql: "select * from stock", as_user: "alice@example.com")
   run_sql(app: "my-inventory", sql: "update my_stock set location = 'south' where sku = 'PAL-STD'", as_user: "alice@example.com")
   run_sql(app: "my-inventory", sql: "update members set location = 'south'", as_user: "alice@example.com")
   ```

   All three are refused: the base table is not reachable, a row cannot move
   to a warehouse she cannot see, and `members` has no write policy.

4. Record a movement and look at who it names:

   ```
   run_sql(app: "my-inventory", sql: "insert into my_movements (location, sku, delta, reason) values ('north', 'PAL-STD', -10, 'shipped')", as_user: "alice@example.com")
   run_sql(app: "my-inventory", sql: "select by_user, delta, reason from movements")
   ```

   `by_user` is alice's account id, filled by the policy.

5. Read the totals as either person:

   ```
   run_sql(app: "my-inventory", sql: "select * from stock_totals", as_user: "bob@example.com")
   ```

   Bob sees `PAL-STD` as 225 (140 plus 85), but not how it splits.

## Place real people

```
run_sql(app: "my-inventory", sql: "insert into members (email, location) values ('you@company.com', 'north')")
```

Without `as_user`, `run_sql` is the admin path and reaches every table.

## Files

- `migrations/001_initial.sql`: tables, the totals view, example rows.
- `toolsite.toml`: the gate and the policies.
- `public/index.html`: one page that says how to use the app.
