-- The account database as it should look right now.
--
-- This is the file to read when you want to know the current shape; the
-- numbered migrations are only the route that gets there. A test applies
-- every migration to an empty database and asserts the result matches this
-- exactly, so the two cannot drift.
--
-- Changing the schema means: add a numbered migration, then update this file
-- to match. Editing only this file will fail the test rather than silently
-- doing nothing, which is what `create table if not exists` used to do.

CREATE TABLE grants (
    user_id text not null references users(id),
    app     text not null,
    role    text not null,
    primary key (user_id, app)
)
CREATE INDEX grants_by_app on grants(app)
CREATE TABLE identities (
    provider    text not null,
    provider_id text not null,
    user_id     text not null references users(id),
    primary key (provider, provider_id)
)
CREATE TABLE sessions (
    token_hash text primary key,
    user_id    text not null references users(id),
    expires_at integer not null
-- Null for the site session that proves identity; an app slug for a session
-- that may only speak for that one app.
, scope text)
CREATE INDEX sessions_by_scope on sessions(user_id, scope)
CREATE INDEX sessions_expiry on sessions(expires_at)
CREATE TABLE users (
    id            text primary key,
    email         text not null unique,
    password_hash text,
    created_at    integer not null
)
