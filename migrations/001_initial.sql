-- Accounts for people who use published apps. Kept in its own database under
-- .site/, which no slug can name.

create table users (
    id            text primary key,
    email         text not null unique,
    -- Null for an account that only signs in through a provider.
    password_hash text,
    created_at    integer not null
);

-- One account, several ways to sign in. Empty until a provider is wired up,
-- but present so adding one later does not mean migrating live accounts.
create table identities (
    provider    text not null,
    provider_id text not null,
    user_id     text not null references users(id),
    primary key (provider, provider_id)
);

create table sessions (
    -- Stored hashed, so a leaked database is not a set of live sessions.
    token_hash text primary key,
    user_id    text not null references users(id),
    expires_at integer not null
);

create table grants (
    user_id text not null references users(id),
    app     text not null,
    role    text not null,
    primary key (user_id, app)
);

create index sessions_expiry on sessions(expires_at);
create index grants_by_app on grants(app);
