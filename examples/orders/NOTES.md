# orders

## Schema

See migrations/. Add a numbered file for each change.

## Decisions

- Whose order it is lives in the policies. What may happen to it lives in
  the handler. Keep them apart: a policy cannot see a status transition, and
  a handler check is easy to forget on one route.
- The approver reads the queue and decides with `db::query`, past the
  policy, because an approver is not the owner. Both routes check the role
  first.
- Adding a product that is already on the order adds to that line.
- `submit_order` on an order already submitted returns it unchanged, so a
  retried tool call does not fail.

## Unfinished

- No way to withdraw a submitted order.
