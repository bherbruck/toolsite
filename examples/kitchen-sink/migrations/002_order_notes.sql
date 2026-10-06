-- The second step of the schema. A new file, because 001 already ran on
-- every database that exists.
alter table orders add column note text;
