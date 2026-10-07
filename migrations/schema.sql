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
CREATE TABLE invites (
    token_hash text primary key,
    user_id    text not null references users(id),
    expires_at integer not null
)
CREATE INDEX invites_by_user on invites(user_id)
CREATE TABLE mfa (
    user_id    text primary key references users(id),
    secret     text not null,
    enabled_at integer,
    last_step  integer not null default 0
)
CREATE TABLE mfa_failures (
    user_id text not null references users(id),
    at      integer not null
)
CREATE INDEX mfa_failures_by_user on mfa_failures(user_id, at)
CREATE TABLE mfa_pending (
    token_hash text primary key,
    user_id    text not null references users(id),
    stage      text not null check (stage in ('code', 'setup')),
    next       text not null,
    expires_at integer not null,
    failures   integer not null default 0
)
CREATE INDEX mfa_pending_by_user on mfa_pending(user_id)
CREATE TABLE pins (
    user_id    text not null references users(id),
    app        text not null,
    created_at integer not null,
    primary key (user_id, app)
)
CREATE TABLE recovery_codes (
    code_hash text primary key,
    user_id   text not null references users(id),
    used_at   integer
)
CREATE INDEX recovery_codes_by_user on recovery_codes(user_id)
CREATE TABLE scopes (
    user_id    text not null references users(id),
    prefix     text not null,
    scope      text not null check (scope in ('viewer', 'editor', 'admin')),
    granted_by text,
    created_at integer not null,
    primary key (user_id, prefix)
)
CREATE INDEX scopes_by_prefix on scopes(prefix)
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
, is_admin integer not null default 0, disabled_at integer)
