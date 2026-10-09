-- Leases and rate windows, so job slots and job start rates hold across
-- every runner on the database.

-- A name one holder has until `expires_at`, in Unix milliseconds by the
-- database's clock. `epoch` grows with every new holder, so a holder that
-- paused past its time cannot act on the name once it is taken over. A
-- released lease keeps its row (expired), so the next epoch still grows.
-- `grp` counts a lease against a ceiling (an app's running jobs); `again`
-- asks the holder for one more turn.
create table state.leases (
    name text primary key,
    grp text,
    holder text not null,
    epoch bigint not null,
    expires_at bigint not null,
    again boolean not null default false
);
create index leases_grp on state.leases (grp) where grp is not null;

-- How many times something happened under a key, per slice of its window
-- (a sixtieth). `window_start` is Unix milliseconds by the database's clock.
create table state.rate_windows (
    key text not null,
    window_start bigint not null,
    count integer not null,
    primary key (key, window_start)
);
