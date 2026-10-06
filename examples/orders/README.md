# orders

A realistic order entry app: orders with lines, totals computed on the
server, status rules in the handler, an approval queue, and MCP tools so a
person can place orders from their AI assistant.

## What it shows

- Two row-level policies: a person's own orders, and the lines of those
  orders. Every read and write for a person goes through `my_orders` and
  `my_order_lines` with `db.query-scoped`.
- Status rules in the handler: lines change only on a draft, a draft needs a
  line before it is submitted, and only the `approver` role decides.
- Prices in integer cents, copied onto the line when it is added. A client
  never sends a price or a total.
- Four app tools: `create_order`, `add_line`, `submit_order` and
  `my_orders`, each called as the person who calls it.
- "Connect an AI assistant" in the menu, with the app's connector link.

## Start it

```sh
toolsite init my-orders --example orders
cd my-orders
toolsite deploy
```

Grant someone the `approver` role to see the Approval queue tab.

## Use it from an assistant

Add `<site>/p/my-orders/mcp` as a connector and sign in. Then ask, for
example, "Start an order for Acme with 10 standard pallets and submit it."

## Files

- `handler/src/lib.rs`: routes, status rules, totals.
- `src/App.tsx`: the order list, the order detail and the queue.
- `migrations/001_initial.sql`: products, orders, lines.
- `toolsite.toml`: policies and tools.
