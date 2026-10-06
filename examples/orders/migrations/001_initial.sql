-- Orders with lines. Prices are kept in cents, as integers, so totals add
-- up exactly. A line copies the product's price when it is added, so a
-- later price change does not alter an order already placed.

create table products (
    sku text primary key,
    name text not null,
    unit_price_cents integer not null check (unit_price_cents >= 0)
);

insert into products (sku, name, unit_price_cents) values
    ('PAL-STD', 'Standard pallet', 1850),
    ('PAL-HD', 'Heavy-duty pallet', 2900),
    ('WRAP-18', 'Stretch wrap, 18 in roll', 2475),
    ('LBL-4X6', 'Shipping labels, 4x6, box of 500', 1999),
    ('TAPE-48', 'Packing tape, 48 mm, case of 36', 6420);

-- The price list as people may read it. Declared under [access] views, so
-- SQL run as a person (the handler's query-scoped, or /me/mcp) can join it.
create view catalog as select sku, name, unit_price_cents from products;

-- draft: lines may change. submitted: waiting for an approver.
-- approved and rejected: decided, no further change.
create table orders (
    id integer primary key,
    owner_id text,
    customer text not null,
    status text not null default 'draft'
        check (status in ('draft', 'submitted', 'approved', 'rejected')),
    created_at integer not null default (cast(strftime('%s', 'now') as integer)),
    submitted_at integer,
    decided_at integer,
    decided_by text
);

create table order_lines (
    id integer primary key,
    order_id integer not null references orders(id),
    sku text not null references products(sku),
    quantity integer not null check (quantity > 0),
    unit_price_cents integer not null
);

create index order_lines_by_order on order_lines(order_id);
