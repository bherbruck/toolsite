-- The resource a token was issued for (RFC 8707). A token asked for one
-- app's connector works there and nowhere stronger; NULL is a token from
-- before this column, or from a client that named no resource, and keeps
-- working where it did.
alter table codes add column resource text;
alter table tokens add column resource text;
