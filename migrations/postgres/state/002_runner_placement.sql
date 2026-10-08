-- What an edge needs to route to a runner (docs/design/postgres-scaleout.md,
-- 6.4): the roles it serves (`control`, `worker`, `edge`; `all` is control
-- and worker), the worker pool it belongs to, the port other runners reach
-- it on beside `address`, and whether it is draining. Recorded from step 1,
-- read once edges exist. Rows are process records, so the single `role`
-- column goes rather than being carried over.
alter table state.runners
    drop column role,
    add column roles text[] not null default '{control,worker}',
    add column pool text not null default 'default',
    add column internal_port integer not null default 8081,
    add column draining boolean not null default false;
