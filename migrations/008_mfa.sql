-- Two-step sign-in with an authenticator app (TOTP, RFC 6238).
--
-- One row per account that has set it up or is setting it up. The shared
-- secret is sealed with the site key (see seal.rs), like app settings, so
-- the database alone does not give anyone the codes.
create table mfa (
    user_id    text primary key references users(id),
    secret     text not null,
    -- Null while setup waits for the first code; set when it is confirmed.
    enabled_at integer,
    -- The last 30-second step a code was accepted for. A code for this step
    -- or an earlier one is refused, so a code works one time.
    last_step  integer not null default 0
);

-- Single-use codes for a lost phone. Stored as a digest keyed with the site
-- key, so a leaked database cannot be searched for them.
create table recovery_codes (
    code_hash text primary key,
    user_id   text not null references users(id),
    used_at   integer
);

create index recovery_codes_by_user on recovery_codes(user_id);

-- A sign-in that passed the password and waits for a code (stage 'code'),
-- or for setup that the site's policy requires (stage 'setup'). Not a
-- session: nothing accepts it except the two-step pages. Hashed like
-- sessions.
create table mfa_pending (
    token_hash text primary key,
    user_id    text not null references users(id),
    stage      text not null check (stage in ('code', 'setup')),
    next       text not null,
    expires_at integer not null,
    failures   integer not null default 0
);

create index mfa_pending_by_user on mfa_pending(user_id);

-- Wrong codes, per account, across sign-ins: what the per-account limit
-- counts. Old rows are removed as new ones arrive.
create table mfa_failures (
    user_id text not null references users(id),
    at      integer not null
);

create index mfa_failures_by_user on mfa_failures(user_id, at);
