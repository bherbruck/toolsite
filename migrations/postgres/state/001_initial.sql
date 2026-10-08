-- Schema `state`: plumbing every runner shares. `state.migrations`, the
-- record of every ladder, is created by the ladder runner itself before
-- this step, since it has to exist to say which steps have run.

-- One row per toolsite process on this database, refreshed by its
-- heartbeat. Times are Unix seconds, as everywhere in toolsite.
create table state.runners (
    id text primary key,
    role text not null,
    address text,
    started_at bigint not null,
    heartbeat_at bigint not null
);
