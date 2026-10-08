-- Schema `oauth`: the OAuth server that MCP clients sign in through, on
-- Postgres. The end state of the SQLite ladder (migrations/oauth/001..002),
-- not a replay of it: same tables, same columns, same meaning.
--
-- user_id points at accounts' users(id) across the boundary and is resolved
-- through the accounts API, never joined: nothing here has a foreign key
-- into the `accounts` schema. Times are Unix seconds, as everywhere in
-- toolsite.

create schema oauth;

-- A client is whatever registered itself. Nothing about it is trusted; it
-- is a name to show on the consent screen and the redirect URIs a code may
-- be sent to.
create table oauth.clients (
    id            text primary key,
    name          text,
    -- JSON array of strings, as in SQLite.
    redirect_uris text not null,
    created_at    bigint not null
);

-- One-time, short-lived, and only ever exchanged by the client that asked.
-- Redeemed with one `delete ... returning`, so two runners cannot both
-- exchange one code.
create table oauth.codes (
    code_hash      text primary key,
    client_id      text not null references oauth.clients(id),
    user_id        text not null,
    redirect_uri   text not null,
    code_challenge text not null,
    expires_at     bigint not null,
    -- The resource the code was asked for (RFC 8707), normalised; NULL when
    -- the client named none.
    resource       text
);

-- Access and refresh tokens, stored hashed: a dump of this table is not a
-- set of live credentials.
create table oauth.tokens (
    token_hash text primary key,
    kind       text not null check (kind in ('access', 'refresh')),
    client_id  text not null references oauth.clients(id),
    user_id    text not null,
    expires_at bigint not null,
    created_at bigint not null,
    resource   text
);

create index tokens_expiry on oauth.tokens(expires_at);
create index tokens_by_user on oauth.tokens(user_id);
create index tokens_by_client on oauth.tokens(client_id);
create index codes_expiry on oauth.codes(expires_at);
create index codes_by_user on oauth.codes(user_id);
create index codes_by_client on oauth.codes(client_id);
