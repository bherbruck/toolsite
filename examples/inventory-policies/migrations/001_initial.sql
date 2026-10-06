-- Stock at two locations, and who works where. There is no handler: people
-- read and write these tables from their own AI assistant through /me/mcp,
-- inside the row-level policies in toolsite.toml.

create table locations (
    code text primary key,
    name text not null
);

-- One row per person. The policies read this table to decide which
-- location's rows a person sees.
create table members (
    email text primary key,
    location text not null references locations(code)
);

create table stock (
    id integer primary key,
    location text not null references locations(code),
    sku text not null,
    description text not null,
    quantity integer not null check (quantity >= 0),
    unique (location, sku)
);

-- Every change to stock, written by the person who made it. by_user is
-- filled with current_user() by the policy, so nobody records a movement
-- under someone else's name.
create table movements (
    id integer primary key,
    location text not null references locations(code),
    sku text not null,
    delta integer not null,
    reason text not null,
    by_user text,
    at integer not null default (cast(strftime('%s', 'now') as integer))
);

-- Company-wide totals: every location added together, with no location
-- column. Declared under [access] views, so anyone may read it. A declared
-- view is read by everyone, so it holds only what everyone may know.
create view stock_totals as
    select sku, min(description) as description, sum(quantity) as quantity
    from stock group by sku;

insert into locations (code, name) values
    ('north', 'North warehouse'),
    ('south', 'South warehouse');

-- Example people. Replace them with real accounts:
--     run_sql(app, "insert into members values ('you@company.com', 'north')")
insert into members (email, location) values
    ('alice@example.com', 'north'),
    ('bob@example.com', 'south');

insert into stock (location, sku, description, quantity) values
    ('north', 'PAL-STD', 'Standard pallet', 140),
    ('north', 'WRAP-18', 'Stretch wrap, 18 in roll', 36),
    ('north', 'LBL-4X6', 'Shipping labels, 4x6, box of 500', 12),
    ('south', 'PAL-STD', 'Standard pallet', 85),
    ('south', 'PAL-HD', 'Heavy-duty pallet', 40),
    ('south', 'TAPE-48', 'Packing tape, 48 mm, case of 36', 9);
