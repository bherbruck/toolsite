-- The OAuth server that MCP clients sign in through. Its own database under
-- .site/, beside the accounts it refers to: this is who may PUBLISH, which is
-- the platform's concern, so it does not share a file with who may visit.
-- user_id points at accounts' users(id) across that boundary and is resolved
-- through the accounts API, never joined.

-- A client is whatever registered itself: Claude, Claude Code, an IDE. Nothing
-- about it is trusted; it is a name to show on the consent screen and the
-- redirect URIs a code may be sent to.
create table clients (
    id            text primary key,
    name          text,
    -- JSON array of strings.
    redirect_uris text not null,
    created_at    integer not null
);

-- One-time, short-lived, and only ever exchanged by the client that asked.
create table codes (
    code_hash      text primary key,
    client_id      text not null references clients(id),
    user_id        text not null,
    redirect_uri   text not null,
    code_challenge text not null,
    expires_at     integer not null
);

-- Access and refresh tokens, stored hashed like sessions are: a leaked
-- database is not a set of live credentials.
create table tokens (
    token_hash text primary key,
    kind       text not null check (kind in ('access', 'refresh')),
    client_id  text not null references clients(id),
    user_id    text not null,
    expires_at integer not null,
    created_at integer not null
);

create index tokens_expiry on tokens(expires_at);
create index tokens_by_user on tokens(user_id);
create index codes_expiry on codes(expires_at);
