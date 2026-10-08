-- Schema `accounts`: people who use published apps, their sessions,
-- grants, scopes and two-step sign-in. The end state of the SQLite ladder
-- (migrations/001..009, declared in migrations/schema.sql), not a replay of
-- its steps. Times are Unix seconds, as in SQLite; flags SQLite keeps as
-- integers are booleans here.
--
-- Tokens, invitations, pending sign-ins and recovery codes are stored as
-- digests and the two-step secret sealed: the rules hand the store nothing
-- else, so a dump of this schema opens no session and no account.

create schema accounts;

create table accounts.users (
    id            text primary key,
    email         text not null unique,
    password_hash text,
    created_at    bigint not null,
    is_admin      boolean not null default false,
    disabled_at   bigint
);

create table accounts.identities (
    provider    text not null,
    provider_id text not null,
    user_id     text not null references accounts.users(id),
    primary key (provider, provider_id)
);

-- `scope` is null for the site session that proves identity, or an app
-- slug for a session that may only speak for that one app.
create table accounts.sessions (
    token_hash text primary key,
    user_id    text not null references accounts.users(id),
    expires_at bigint not null,
    scope      text
);
create index sessions_by_scope on accounts.sessions (user_id, scope);
create index sessions_expiry on accounts.sessions (expires_at);

create table accounts.invites (
    token_hash text primary key,
    user_id    text not null references accounts.users(id),
    expires_at bigint not null
);
create index invites_by_user on accounts.invites (user_id);

create table accounts.grants (
    user_id text not null references accounts.users(id),
    app     text not null,
    role    text not null,
    primary key (user_id, app)
);
create index grants_by_app on accounts.grants (app);

create table accounts.scopes (
    user_id    text not null references accounts.users(id),
    prefix     text not null,
    scope      text not null check (scope in ('viewer', 'editor', 'admin')),
    granted_by text,
    created_at bigint not null,
    primary key (user_id, prefix)
);
create index scopes_by_prefix on accounts.scopes (prefix);

create table accounts.pins (
    user_id    text not null references accounts.users(id),
    app        text not null,
    created_at bigint not null,
    primary key (user_id, app)
);

create table accounts.mfa (
    user_id    text primary key references accounts.users(id),
    secret     text not null,
    enabled_at bigint,
    last_step  bigint not null default 0,
    begun_by   text
);

create table accounts.recovery_codes (
    code_hash text primary key,
    user_id   text not null references accounts.users(id),
    used_at   bigint
);
create index recovery_codes_by_user on accounts.recovery_codes (user_id);

create table accounts.mfa_pending (
    token_hash text primary key,
    user_id    text not null references accounts.users(id),
    stage      text not null check (stage in ('code', 'setup')),
    next       text not null,
    expires_at bigint not null,
    failures   bigint not null default 0
);
create index mfa_pending_by_user on accounts.mfa_pending (user_id);

create table accounts.mfa_failures (
    user_id text not null references accounts.users(id),
    at      bigint not null
);
create index mfa_failures_by_user on accounts.mfa_failures (user_id, at);
