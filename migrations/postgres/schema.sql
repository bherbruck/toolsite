-- The Postgres platform schema as it should look right now: every column,
-- constraint and index the ladders in migrations/postgres/<store>/ build, as
-- the catalogue describes them. tests/postgres_schema.rs builds a database
-- from the ladders and compares, so the two cannot drift. Add a step, then
-- rewrite this file with TOOLSITE_WRITE_SCHEMA=1 and read the diff.

column accounts.grants.app text not null
column accounts.grants.role text not null
column accounts.grants.user_id text not null
column accounts.identities.provider text not null
column accounts.identities.provider_id text not null
column accounts.identities.user_id text not null
column accounts.invites.expires_at bigint not null
column accounts.invites.token_hash text not null
column accounts.invites.user_id text not null
column accounts.mfa.begun_by text
column accounts.mfa.enabled_at bigint
column accounts.mfa.last_step bigint not null default 0
column accounts.mfa.secret text not null
column accounts.mfa.user_id text not null
column accounts.mfa_failures.at bigint not null
column accounts.mfa_failures.user_id text not null
column accounts.mfa_pending.expires_at bigint not null
column accounts.mfa_pending.failures bigint not null default 0
column accounts.mfa_pending.next text not null
column accounts.mfa_pending.stage text not null
column accounts.mfa_pending.token_hash text not null
column accounts.mfa_pending.user_id text not null
column accounts.pins.app text not null
column accounts.pins.created_at bigint not null
column accounts.pins.user_id text not null
column accounts.recovery_codes.code_hash text not null
column accounts.recovery_codes.used_at bigint
column accounts.recovery_codes.user_id text not null
column accounts.scopes.created_at bigint not null
column accounts.scopes.granted_by text
column accounts.scopes.prefix text not null
column accounts.scopes.scope text not null
column accounts.scopes.user_id text not null
column accounts.sessions.expires_at bigint not null
column accounts.sessions.scope text
column accounts.sessions.token_hash text not null
column accounts.sessions.user_id text not null
column accounts.users.created_at bigint not null
column accounts.users.disabled_at bigint
column accounts.users.email text not null
column accounts.users.id text not null
column accounts.users.is_admin boolean not null default false
column accounts.users.password_hash text
column oauth.clients.created_at bigint not null
column oauth.clients.id text not null
column oauth.clients.name text
column oauth.clients.redirect_uris text not null
column oauth.codes.client_id text not null
column oauth.codes.code_challenge text not null
column oauth.codes.code_hash text not null
column oauth.codes.expires_at bigint not null
column oauth.codes.redirect_uri text not null
column oauth.codes.resource text
column oauth.codes.user_id text not null
column oauth.tokens.client_id text not null
column oauth.tokens.created_at bigint not null
column oauth.tokens.expires_at bigint not null
column oauth.tokens.kind text not null
column oauth.tokens.resource text
column oauth.tokens.token_hash text not null
column oauth.tokens.user_id text not null
column state.migrations.applied_at bigint not null
column state.migrations.store text not null
column state.migrations.version integer not null
column state.runners.address text
column state.runners.draining boolean not null default false
column state.runners.heartbeat_at bigint not null
column state.runners.id text not null
column state.runners.internal_port integer not null default 8081
column state.runners.pool text not null default 'default'::text
column state.runners.roles ARRAY not null default '{control,worker}'::text[]
column state.runners.started_at bigint not null
constraint accounts.grants grants_pkey: PRIMARY KEY (user_id, app)
constraint accounts.grants grants_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.identities identities_pkey: PRIMARY KEY (provider, provider_id)
constraint accounts.identities identities_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.invites invites_pkey: PRIMARY KEY (token_hash)
constraint accounts.invites invites_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.mfa mfa_pkey: PRIMARY KEY (user_id)
constraint accounts.mfa mfa_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.mfa_failures mfa_failures_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.mfa_pending mfa_pending_pkey: PRIMARY KEY (token_hash)
constraint accounts.mfa_pending mfa_pending_stage_check: CHECK ((stage = ANY (ARRAY['code'::text, 'setup'::text])))
constraint accounts.mfa_pending mfa_pending_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.pins pins_pkey: PRIMARY KEY (user_id, app)
constraint accounts.pins pins_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.recovery_codes recovery_codes_pkey: PRIMARY KEY (code_hash)
constraint accounts.recovery_codes recovery_codes_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.scopes scopes_pkey: PRIMARY KEY (user_id, prefix)
constraint accounts.scopes scopes_scope_check: CHECK ((scope = ANY (ARRAY['viewer'::text, 'editor'::text, 'admin'::text])))
constraint accounts.scopes scopes_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.sessions sessions_pkey: PRIMARY KEY (token_hash)
constraint accounts.sessions sessions_user_id_fkey: FOREIGN KEY (user_id) REFERENCES accounts.users(id)
constraint accounts.users users_email_key: UNIQUE (email)
constraint accounts.users users_pkey: PRIMARY KEY (id)
constraint oauth.clients clients_pkey: PRIMARY KEY (id)
constraint oauth.codes codes_client_id_fkey: FOREIGN KEY (client_id) REFERENCES oauth.clients(id)
constraint oauth.codes codes_pkey: PRIMARY KEY (code_hash)
constraint oauth.tokens tokens_client_id_fkey: FOREIGN KEY (client_id) REFERENCES oauth.clients(id)
constraint oauth.tokens tokens_kind_check: CHECK ((kind = ANY (ARRAY['access'::text, 'refresh'::text])))
constraint oauth.tokens tokens_pkey: PRIMARY KEY (token_hash)
constraint state.migrations migrations_pkey: PRIMARY KEY (store, version)
constraint state.runners runners_pkey: PRIMARY KEY (id)
index accounts CREATE INDEX grants_by_app ON accounts.grants USING btree (app)
index accounts CREATE INDEX invites_by_user ON accounts.invites USING btree (user_id)
index accounts CREATE INDEX mfa_failures_by_user ON accounts.mfa_failures USING btree (user_id, at)
index accounts CREATE INDEX mfa_pending_by_user ON accounts.mfa_pending USING btree (user_id)
index accounts CREATE INDEX recovery_codes_by_user ON accounts.recovery_codes USING btree (user_id)
index accounts CREATE INDEX scopes_by_prefix ON accounts.scopes USING btree (prefix)
index accounts CREATE INDEX sessions_by_scope ON accounts.sessions USING btree (user_id, scope)
index accounts CREATE INDEX sessions_expiry ON accounts.sessions USING btree (expires_at)
index accounts CREATE UNIQUE INDEX grants_pkey ON accounts.grants USING btree (user_id, app)
index accounts CREATE UNIQUE INDEX identities_pkey ON accounts.identities USING btree (provider, provider_id)
index accounts CREATE UNIQUE INDEX invites_pkey ON accounts.invites USING btree (token_hash)
index accounts CREATE UNIQUE INDEX mfa_pending_pkey ON accounts.mfa_pending USING btree (token_hash)
index accounts CREATE UNIQUE INDEX mfa_pkey ON accounts.mfa USING btree (user_id)
index accounts CREATE UNIQUE INDEX pins_pkey ON accounts.pins USING btree (user_id, app)
index accounts CREATE UNIQUE INDEX recovery_codes_pkey ON accounts.recovery_codes USING btree (code_hash)
index accounts CREATE UNIQUE INDEX scopes_pkey ON accounts.scopes USING btree (user_id, prefix)
index accounts CREATE UNIQUE INDEX sessions_pkey ON accounts.sessions USING btree (token_hash)
index accounts CREATE UNIQUE INDEX users_email_key ON accounts.users USING btree (email)
index accounts CREATE UNIQUE INDEX users_pkey ON accounts.users USING btree (id)
index oauth CREATE INDEX codes_by_client ON oauth.codes USING btree (client_id)
index oauth CREATE INDEX codes_by_user ON oauth.codes USING btree (user_id)
index oauth CREATE INDEX codes_expiry ON oauth.codes USING btree (expires_at)
index oauth CREATE INDEX tokens_by_client ON oauth.tokens USING btree (client_id)
index oauth CREATE INDEX tokens_by_user ON oauth.tokens USING btree (user_id)
index oauth CREATE INDEX tokens_expiry ON oauth.tokens USING btree (expires_at)
index oauth CREATE UNIQUE INDEX clients_pkey ON oauth.clients USING btree (id)
index oauth CREATE UNIQUE INDEX codes_pkey ON oauth.codes USING btree (code_hash)
index oauth CREATE UNIQUE INDEX tokens_pkey ON oauth.tokens USING btree (token_hash)
index state CREATE UNIQUE INDEX migrations_pkey ON state.migrations USING btree (store, version)
index state CREATE UNIQUE INDEX runners_pkey ON state.runners USING btree (id)
