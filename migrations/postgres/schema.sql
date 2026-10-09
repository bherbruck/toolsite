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
column platform.app_migrations.app text not null
column platform.app_migrations.files json not null
column platform.app_migrations.updated_at bigint not null
column platform.app_settings.app text not null
column platform.app_settings.name text not null
column platform.app_settings.sealed text not null
column platform.app_settings.updated_at bigint not null
column platform.app_tokens.app text not null
column platform.app_tokens.created_at bigint not null
column platform.app_tokens.hash text not null
column platform.app_tokens.id text not null
column platform.app_tokens.kind text not null
column platform.app_tokens.label text not null
column platform.app_tokens.last_used bigint
column platform.app_tools.app text not null
column platform.app_tools.tools json not null
column platform.app_tools.updated_at bigint not null
column platform.github_installations.fetched_at bigint not null
column platform.github_installations.installations json not null
column platform.github_installations.one boolean not null default true
column platform.host_labels.app text not null
column platform.host_labels.issued_at bigint not null
column platform.host_labels.label text not null
column platform.pages.created_at bigint not null
column platform.pages.generation bigint not null default 0
column platform.pages.meta json not null default '{}'::json
column platform.pages.notes text
column platform.pages.slug text not null
column platform.pages.updated_at bigint not null
column platform.projects.created_at bigint not null
column platform.projects.gate text
column platform.projects.locked boolean not null default false
column platform.projects.name text not null
column platform.projects.path text not null
column platform.projects.renamed_from ARRAY not null default '{}'::text[]
column platform.relocations.from_path text not null
column platform.relocations.one boolean not null default true
column platform.relocations.started_at bigint not null
column platform.relocations.to_path text not null
column platform.removed_pages.created_at bigint not null
column platform.removed_pages.generation bigint not null
column platform.removed_pages.id bigint not null default nextval('platform.removed_pages_id_seq'::regclass)
column platform.removed_pages.meta json not null
column platform.removed_pages.notes text
column platform.removed_pages.removed_at bigint not null
column platform.removed_pages.slug text not null
column platform.removed_records.app text not null
column platform.removed_records.id bigint not null default nextval('platform.removed_records_id_seq'::regclass)
column platform.removed_records.kind text not null
column platform.removed_records.record json not null
column platform.removed_records.removed_at bigint not null
column platform.repo_links.app text not null
column platform.repo_links.link json not null
column platform.repo_links.updated_at bigint not null
column platform.site_flags.at bigint not null
column platform.site_flags.name text not null
column platform.site_flags.value text not null
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
column state.tickets.expires_at bigint not null
column state.tickets.id_hash text not null
column state.tickets.kind text not null
column state.tickets.payload bytea not null
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
constraint platform.app_migrations app_migrations_pkey: PRIMARY KEY (app)
constraint platform.app_settings app_settings_pkey: PRIMARY KEY (app, name)
constraint platform.app_tokens app_tokens_kind_check: CHECK ((kind = ANY (ARRAY['export'::text, 'deploy'::text, 'device'::text])))
constraint platform.app_tokens app_tokens_pkey: PRIMARY KEY (app, kind, id)
constraint platform.app_tools app_tools_pkey: PRIMARY KEY (app)
constraint platform.github_installations github_installations_one_check: CHECK (one)
constraint platform.github_installations github_installations_pkey: PRIMARY KEY (one)
constraint platform.host_labels host_labels_pkey: PRIMARY KEY (label)
constraint platform.pages pages_pkey: PRIMARY KEY (slug)
constraint platform.projects projects_pkey: PRIMARY KEY (path)
constraint platform.relocations relocations_one_check: CHECK (one)
constraint platform.relocations relocations_pkey: PRIMARY KEY (one)
constraint platform.removed_pages removed_pages_pkey: PRIMARY KEY (id)
constraint platform.removed_records removed_records_pkey: PRIMARY KEY (id)
constraint platform.repo_links repo_links_pkey: PRIMARY KEY (app)
constraint platform.site_flags site_flags_pkey: PRIMARY KEY (name)
constraint state.migrations migrations_pkey: PRIMARY KEY (store, version)
constraint state.runners runners_pkey: PRIMARY KEY (id)
constraint state.tickets tickets_pkey: PRIMARY KEY (id_hash)
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
index platform CREATE INDEX host_labels_app ON platform.host_labels USING btree (app)
index platform CREATE INDEX removed_pages_slug ON platform.removed_pages USING btree (slug)
index platform CREATE INDEX removed_records_app ON platform.removed_records USING btree (app)
index platform CREATE UNIQUE INDEX app_migrations_pkey ON platform.app_migrations USING btree (app)
index platform CREATE UNIQUE INDEX app_settings_pkey ON platform.app_settings USING btree (app, name)
index platform CREATE UNIQUE INDEX app_tokens_hash ON platform.app_tokens USING btree (kind, hash)
index platform CREATE UNIQUE INDEX app_tokens_pkey ON platform.app_tokens USING btree (app, kind, id)
index platform CREATE UNIQUE INDEX app_tools_pkey ON platform.app_tools USING btree (app)
index platform CREATE UNIQUE INDEX github_installations_pkey ON platform.github_installations USING btree (one)
index platform CREATE UNIQUE INDEX host_labels_pkey ON platform.host_labels USING btree (label)
index platform CREATE UNIQUE INDEX pages_pkey ON platform.pages USING btree (slug)
index platform CREATE UNIQUE INDEX projects_pkey ON platform.projects USING btree (path)
index platform CREATE UNIQUE INDEX relocations_pkey ON platform.relocations USING btree (one)
index platform CREATE UNIQUE INDEX removed_pages_pkey ON platform.removed_pages USING btree (id)
index platform CREATE UNIQUE INDEX removed_records_pkey ON platform.removed_records USING btree (id)
index platform CREATE UNIQUE INDEX repo_links_pkey ON platform.repo_links USING btree (app)
index platform CREATE UNIQUE INDEX site_flags_pkey ON platform.site_flags USING btree (name)
index state CREATE INDEX tickets_expires_at ON state.tickets USING btree (expires_at)
index state CREATE UNIQUE INDEX migrations_pkey ON state.migrations USING btree (store, version)
index state CREATE UNIQUE INDEX runners_pkey ON state.runners USING btree (id)
index state CREATE UNIQUE INDEX tickets_pkey ON state.tickets USING btree (id_hash)
