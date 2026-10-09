-- Schema `platform`, step 3: per-app records and tokens. An app's settings,
-- the tools it declares, its own migration ladder, the repository it
-- mirrors to; the GitHub App's installations; and every export, deploy and
-- device token. Times are Unix seconds.

-- One row per setting. The value is sealed with the site key before it
-- arrives, so a dump of this table holds no setting in the clear.
create table platform.app_settings (
    app text not null,
    name text not null,
    sealed text not null,
    updated_at bigint not null,
    primary key (app, name)
);

-- The records below are `json`, not `jsonb`, as metas are: `json` keeps the
-- text as written, `\u0000` included, where `jsonb` would refuse it.

-- The tools an app declares in its manifest.
create table platform.app_tools (
    app text primary key,
    tools json not null,
    updated_at bigint not null
);

-- The app's own SQL ladder: file names and their SQL, in order.
create table platform.app_migrations (
    app text primary key,
    files json not null,
    updated_at bigint not null
);

-- The repository an app mirrors to, kept once disconnected: the history of
-- where an app came from is worth having.
create table platform.repo_links (
    app text primary key,
    link json not null,
    updated_at bigint not null
);

-- The accounts the GitHub App is installed on, as GitHub last listed them.
-- At most one row: `one` is the primary key and can only be true.
create table platform.github_installations (
    one boolean primary key default true check (one),
    installations json not null,
    fetched_at bigint not null
);

-- One row per token. Only the SHA-256 of the token handed out is kept. A
-- row per token, rather than a list per app, is what lets a use be recorded
-- without ever writing back a list that lost a token minted meanwhile.
create table platform.app_tokens (
    app text not null,
    kind text not null check (kind in ('export', 'deploy', 'device')),
    -- Short and public: names the token in a listing and a revocation.
    id text not null,
    label text not null,
    hash text not null,
    created_at bigint not null,
    last_used bigint,
    primary key (app, kind, id)
);
create unique index app_tokens_hash on platform.app_tokens (kind, hash);

-- What a removal took out of the tables above. Nothing destroys data: each
-- record stays here, in the shape its sidecar has on files (`kind` is that
-- sidecar's extension), beside the copy the trash keeps.
create table platform.removed_records (
    id bigserial primary key,
    app text not null,
    kind text not null,
    record json not null,
    removed_at bigint not null
);
create index removed_records_app on platform.removed_records (app);
