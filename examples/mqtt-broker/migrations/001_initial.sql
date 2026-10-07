-- What the resident broker last saved, for /api/status. The broker itself
-- lives in memory; a request runs in a fresh instance and cannot see it,
-- so on-tick writes this one row when something changed, and at least
-- every 30 seconds.
create table status (
    id integer primary key check (id = 1),
    started_ms integer not null,
    updated_ms integer not null,
    publishes integer not null,
    -- JSON arrays: the connected clients, and the most recent publishes.
    clients text not null,
    recent text not null
);
