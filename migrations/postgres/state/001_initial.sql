-- Schema `state`: plumbing every runner shares. `state.migrations`, the
-- record of every ladder, is created by the ladder runner itself before
-- this step, since it has to exist to say which steps have run.

-- One row per toolsite process on this database, refreshed by its
-- heartbeat. Times are Unix seconds, as everywhere in toolsite.
-- `roles` is the set the runner serves (`control`, `worker`, `edge`; `all`
-- is control and worker). `address` and `internal_port` are where other
-- runners reach it on the private network, `pool` the worker pool it
-- belongs to, and `draining` says it takes no new work. Until edges exist
-- (docs/design/postgres-scaleout.md, 6.4) they are recorded and not read.
create table state.runners (
    id text primary key,
    roles text[] not null,
    pool text not null,
    address text,
    internal_port integer not null,
    draining boolean not null default false,
    started_at bigint not null,
    heartbeat_at bigint not null
);
