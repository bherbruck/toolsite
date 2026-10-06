-- The orders domain every screen of the kitchen sink works on.
--
-- Numbered files are applied in order, each exactly once, before the app
-- answers anything. Change the schema with a new file (see 002), never by
-- editing this one: a database that already ran it will not run it again.

create table locations (
    code text primary key,
    name text not null
);

insert into locations (code, name) values
    ('north', 'North yard'),
    ('south', 'South yard');

-- Who works where. Keyed by email so a manager can place a person before
-- they first sign in. The row-level policy on orders reads this table.
create table members (
    email text primary key,
    location text not null references locations(code)
);

create table orders (
    id integer primary key,
    owner_id text,
    location text not null references locations(code),
    customer text not null,
    item text not null,
    quantity integer not null check (quantity > 0),
    status text not null default 'open' check (status in ('open', 'shipped', 'cancelled')),
    created_at integer not null default (cast(strftime('%s', 'now') as integer))
);

-- Written by the scheduled job, so the Jobs screen can show it ran.
create table heartbeats (
    id integer primary key,
    at integer not null default (cast(strftime('%s', 'now') as integer))
);

-- Files the Files screen uploaded, with who uploaded them.
create table files (
    key text primary key,
    name text not null,
    uploaded_by text,
    at integer not null default (cast(strftime('%s', 'now') as integer))
);

-- A hand-written view of the person's own locations. toolsite.toml declares
-- it under [access] views, so query-scoped SQL can read it.
create view my_locations as
    select code, name from locations
    where code in (select location from members where email = current_email());
