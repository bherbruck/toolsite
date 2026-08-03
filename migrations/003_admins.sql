-- Someone has to be able to see the accounts. Admin is a property of the
-- account rather than a grant, because it is not about one app.

alter table users add column is_admin integer not null default 0;
