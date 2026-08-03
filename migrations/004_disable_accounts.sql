-- Turning an account off, without destroying it. Null means active; a
-- timestamp records when it was disabled, which is more use than a boolean
-- when you are working out what happened.

alter table users add column disabled_at integer;
