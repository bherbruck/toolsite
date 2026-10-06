# kitchen-sink

Every capability toolsite gives an app, one screen each, on one small
orders domain. Use it to see how a feature looks from the inside before you
build with it, or copy the part you need.

| Screen | What it shows | Where to look |
| --- | --- | --- |
| Database | Per-app SQLite, numbered migrations | `migrations/`, `overview()` |
| Orders | Row-level policy by location, writes through the view | `[[access.table]]`, `create_order()` |
| SQL as you | `db.query-scoped`: a person's own SQL inside the declared views | `scoped_sql()` |
| Files | `blobs.upload-url`, list, delete, serve with `x-toolsite-blob` | `file_upload_url()`, `serve_file()` |
| Settings | `secrets.get` and `secrets.names`, never the value | `settings()` |
| Fetch | `fetch.send` to a host in `allow_http`, refusal for the rest | `outbound()` |
| Identity | `current-user`, `current-role`, the same in SQL, declared `roles` | `me()` |
| Jobs | `[[job]]` writing a heartbeat row | `heartbeat()` |
| Routes | `[[route]]`: public `/status`, restricted `/admin` | `status()`, `admin_page()` |
| Admin | A role check in the handler behind a route rule | `place_member()` |
| AI tools | `[[tool]]`: `create_order` writes, `list_my_orders` reads | `[[tool]]` |

The icon is `icon` in `toolsite.toml`. The menu (top right) has "Connect an
AI assistant", which shows `<site>/p/<app>/mcp` with a copy button.

## Start it

```sh
toolsite init my-sink --example kitchen-sink
cd my-sink
toolsite deploy
```

`deploy` builds the handler (`cargo`, target `wasm32-wasip2`) and the front
end (`npm`), applies the migrations and `toolsite.toml`, then publishes.

## Make every screen work

1. Grant yourself the `manager` role on the app in the site admin. The Admin
   screen and `/admin` need it.
2. On the Admin screen, place yourself at `north`. The Orders screen now
   shows north's orders, and Create works.
3. Set the `GREETING` setting from the app's Settings tab. The Settings
   screen then says "Set."
4. Run the `heartbeat` job from the app's Jobs tab, or wait five minutes.

## Prove the policy

Place a second account at `south`, create an order as each person, then:

```
run_sql(app: "my-sink", sql: "select location, customer from my_orders", as_user: "you@example.com")
run_sql(app: "my-sink", sql: "select location, customer from my_orders", as_user: "other@example.com")
```

Each gets their own location's rows. The same check runs in this
repository's tests (`tests/examples.rs`).

## Files

- `handler/src/lib.rs`: every route, in one file, grouped by screen.
- `src/screens.tsx`: one component per screen. `src/App.tsx`: the sidebar.
- `migrations/`: `001_initial.sql`, then `002_order_notes.sql` as the
  second step.
- `toolsite.toml`: gate, routes, job, policy, tools.
