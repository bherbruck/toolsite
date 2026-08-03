-- This app's schema. Numbered files here are applied in order, each exactly
-- once, in a transaction, before the app answers anything.
--
-- To change the schema later, add 002_whatever.sql rather than editing this
-- file: a database that already ran this one will never run it again, so an
-- edit reaches new databases only.
--
--     alter table visits add column note text;
--
-- Do not write `create table if not exists` in the handler instead. Once the
-- table exists that statement does nothing, so a column added later never
-- arrives and the failure surfaces as "no such column" against real rows.

create table visits (
    at integer not null
);
