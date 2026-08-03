-- A one-time link that lets someone set their own password, so an account can
-- be created without anyone else ever handling it.
--
-- Hashed like sessions are: a leaked database should not be a pile of usable
-- invitations.

create table invites (
    token_hash text primary key,
    user_id    text not null references users(id),
    expires_at integer not null
);

create index invites_by_user on invites(user_id);
