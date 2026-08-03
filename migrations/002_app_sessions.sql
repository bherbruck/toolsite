-- A session is now either the site session that proves who someone is, or an
-- app session that may speak for exactly one app. Null scope means the former.
--
-- Existing rows are all site sessions, and adding a column leaves them null,
-- so there is nothing to backfill.

alter table sessions add column scope text;

create index sessions_by_scope on sessions(user_id, scope);
