-- Which sign-in a setup that waits for its first code belongs to: the digest
-- of the site session or pending sign-in that began it. Only that one is
-- shown the secret or may confirm it, so a secret seen by someone else (an
-- unfinished setup begun with a stolen password) is never the one the
-- person later scans. Null once setup is confirmed.
alter table mfa add column begun_by text;
