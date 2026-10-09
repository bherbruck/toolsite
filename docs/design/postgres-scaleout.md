# Postgres scale-out: design note

Status: proposal for step 1, with the seams for steps 2 to 5. Written against
`main` at `e8bce6d`. Nothing here is built yet.

The approved plan has four steps:

1. Platform state in Postgres when `DATABASE_URL` is set. App bundles and
   handlers in the bucket. SQLite and files stay the zero-config default.
2. A `Bus` for cross-runner publish and send. Resident apps pinned to one
   runner by a lease. The scheduler fires from the runner that holds a lease.
3. Per-app `[database] engine = "postgres" | "sqlite"`: a schema and a role
   per app, native row-level security from `[[access.table]]`.
4. One binary, `TOOLSITE_ROLE=all|control|worker`.
5. An **edge** role (added by the owner after the first draft): toolsite's
   own reverse proxy in the same binary, the only public entry point.
   `TOOLSITE_ROLE` takes a comma list: `all` (default), `control,edge`,
   `edge`, `control`, `worker`.

This note covers step 1 in detail and says where steps 2 to 5 attach, so that
step 1 leaves the right hooks and does not need to be redone.

Terms used here:

- **Runner**: one toolsite process. Today there is exactly one.
- **File mode**: today's storage, everything under `DATA_DIR`.
- **Postgres mode**: `DATABASE_URL` is set.
- **Bucket**: the S3 bucket that `TOOLSITE_BLOB_S3_*` already configures.

---

## 1. Inventory of state

Every piece of state the server keeps today. "Breaks with two runners" means:
if two processes served the same site, each with its own copy of this state
or each writing the same files, would something go wrong.

### 1.1 Account database: `.site/auth.db`

Owner: `accounts/users.rs` (opens with `users::open`), `accounts/mfa.rs`.
Schema: `migrations/001..009`, declared shape in `migrations/schema.sql`,
ladder in `accounts/schema.rs`. Read on almost every request (session
lookup), written on sign-in, sign-out and admin changes.

| State | Table | Written by | Read by | Breaks with two runners |
|---|---|---|---|---|
| Accounts | `users` (`is_admin`, `disabled_at`) | `sign_up_as`, `invite`, `create_provider_account`, `set_active`, `change_password` | everything that resolves a person | Yes, unless both share one file on one volume |
| Provider identities | `identities` | `link_identity` | `user_by_identity` | Yes, same reason |
| Site sessions | `sessions`, `scope is null` | `log_in`, `start_session`, `log_out` | `site_session_user` | Yes |
| App sessions | `sessions`, `scope = <app>` | `create_app_session(_for)` | `app_session_user` | Yes |
| Invitations | `invites` | `invite`, `reinvite`, `accept_invite` | `invited_account` | Yes |
| Per-app grants (opaque role) | `grants` | `grant`, `revoke`, `forget_app` | `role_for`, `has_grant` | Yes |
| Platform scopes (permission rows) | `scopes` | `grant_scope`, `revoke_scope`, `move_scope_tree`, `remove_scope_tree` | `effective_scope`, `app_scope`, `explain_scope` | Yes |
| Pinned app tools | `pins` | `set_pin` | `pins_for` | Yes |
| MFA secret and replay step | `mfa` (`secret` sealed, `last_step`, `begun_by`) | `begin_setup`, `confirm_setup`, `accept` | `status`, `accept` | Yes |
| Recovery codes | `recovery_codes` (keyed digest) | `new_recovery_codes`, `accept` | `accept` | Yes |
| Pending two-step sign-ins | `mfa_pending` | `new_pending`, `wrong_code`, `finish` | `load_pending` | Yes |
| Wrong-code counter | `mfa_failures` | `record_failure` | `account_locked` | Yes; also a rate limiter |

SQLite runs this file in WAL mode with full sync. Single-use and replay rules
rely on conditional updates (`update mfa set last_step = ?1 where ... and
last_step < ?1`, `update recovery_codes ... where used_at is null`), not on a
process lock. That is good news: the same statements are correct in Postgres
under read committed.

### 1.2 OAuth database: `.site/oauth.db`

Owner: `platform/oauth_store.rs`. Schema: `migrations/oauth/001..002`.

| State | Table | Notes | Breaks with two runners |
|---|---|---|---|
| Registered clients | `clients` | `register_client`, `client` | Yes |
| Authorization codes | `codes` (hashed, PKCE challenge, `resource`) | `issue_code`, `redeem_code` (single use) | Yes |
| Access and refresh tokens | `tokens` (hashed, `kind`, `resource`) | `issue_tokens`, `rotate_refresh`, `access_token_grant`, `revoke_for_user`, `sweep` | Yes |

`user_id` points at `auth.db` across the boundary and is never joined. That
rule stays.

### 1.3 Site files under `.site/`

| File | Owner | What | Concurrency today | Breaks with two runners |
|---|---|---|---|---|
| `secret.key` | `seal.rs` `key()` | 32-byte key for sealed values (app settings, MFA secrets, recovery digests). Skipped when `TOOLSITE_SECRET_KEY` is set | Generated on first use, no lock | Yes: two runners on two volumes generate two keys and cannot open each other's values |
| `form.key` | `users::form_secret` | Key for `derive_form_token` (form CSRF tokens) | Generated once, `hard_link` race guard | Yes, same reason |
| `projects.json` | `content/store.rs` (`list_folders`, `write_folders`) | The project tree: path, name, `locked`, `renamed_from`, `gate` | `FOLDERS_WRITE` tokio mutex, write by rename | Yes: the mutex is per process, two writers lose an update, and a lost lock opens what it closed |
| `relocating.json` | `platform/projects.rs` (`begin_relocation`, `resume_pending`, `finish`) and `store::relocation_in_progress` | Journal of an unfinished project move | Resumed at boot in `main` and before each move | Yes: two runners could resume one move at once |
| `labels.json` | `content/origins.rs` (`label_for`, `app_for_label`) | Every DNS label ever issued in subdomain mode | `ASSIGNING` std mutex | Yes: two runners could issue one label to two apps |
| `github.json` | `platform/github.rs` (`installations`, `refresh_installations`) | GitHub App installations | None | Mild: a cache GitHub can rebuild, but stale across runners |
| `grants-adopted` | `permissions::adopt_grants` | One-time marker | Checked at boot | Mild: a second runner would redo idempotent work |

### 1.4 Per-app sidecars beside the app: `DATA_DIR/<slug>.<ext>`

The list in `platform/trash.rs` `SIDECARS` is the full set: `html meta icon
notes source secrets jobs migrations exports deploys devices repo tools`.
`meta`, `icon` and `notes` may also live inside the app as `index.meta`,
`index.icon`, `index.notes` (`store::meta_path`, `icon_path`, `notes_path`).

| Sidecar | Owner | What | Concurrency today | Breaks with two runners |
|---|---|---|---|---|
| `.meta` | `store::read_meta`, `write_meta`, `read_meta_blocking`, `write_meta_blocking` | `PageMeta`: listed, hidden, spa, gate, rules, allow_http, roles, project, created_by, queryable views, policies, generated objects, access salt, sockets, ports, resident, limits, label | None. Read-modify-write from about 9 modules, last writer wins | Yes. Also `write_meta` calls `close_if_hidden`, which reaches this process's `connections` and `residents` only |
| `.icon` | `store`, `manifest`, `mcp` | Icon bytes | None | Yes (files on one volume) |
| `.notes` | `store::read_notes`, `write_notes` | Notes for the next agent | None | Yes |
| `.html` | `upload`, `mcp`, `serve` | A loose page | None | Yes |
| `.source` | `upload`, `admin`, `github`, `manifest` | The app's source archive (tar.gz) | None | Yes |
| `.secrets` | `platform/secrets.rs` | Name to sealed value | None | Yes |
| `.jobs` | `platform/schedule.rs` (`read_jobs`, `set_job`, `remove_job`, `update`) | Job definitions and last-run status | `FILES` std mutex, write by rename (`e8bce6d`) | Yes: the mutex is per process |
| `.migrations` | `runtime/migrate.rs` `store` | The app's own SQL ladder | None | Yes |
| `.exports` | `platform/export.rs` | Hashed export tokens, `last_used` | None. `last_used` is a read-modify-write, so a token minted during a use can be lost today | Yes |
| `.deploys` | `platform/deploy.rs` | Hashed deploy tokens | Same as exports | Yes |
| `.devices` | `platform/devices.rs` | Hashed device tokens, `last_used` at 60 s resolution | `WRITING` std mutex, write by rename | Yes |
| `.repo` | `platform/github.rs` (`link`, `write_link`, `linked_apps`) | `RepoLink`: repository, branch, last push, deployed commit, disconnected_at | None | Yes |
| `.tools` | `platform/app_tools.rs` (`read`, `write`) | Tools the app declares | None | Yes |

Several modules list apps by scanning `DATA_DIR`: `store::collect_slugs`,
`origins::app_names`, `schedule::apps_with_jobs`, `github::linked_apps`,
`export::list_all`. With a database these become queries.

### 1.5 App content: `DATA_DIR/<app>/`

| State | Path | Owner | Breaks with two runners |
|---|---|---|---|
| Bundle files | `<app>/**` (overlay: a new bundle overwrites files and keeps the others, `bundle::unpack_bundle`) | `upload`, `mcp`, `serve` | Yes |
| Handler | `<app>/handler.wasm` | `upload` (then `runtime.forget`), read per request by `serve::handler_wasm`, `schedule::run_once`, `app_tools` | Yes |
| App database | `<app>/data.db` (WAL, `synchronous = NORMAL`) | `runtime/db.rs` `db_path`, `open_as`, `open_scoped`; `access.rs`; `migrate.rs`; `export.rs` snapshots with `VACUUM INTO` | Yes. SQLite over a network filesystem is not safe |
| App files, local backend | `<app>/.blobs/{data,meta,tmp}/` | `runtime/blobs.rs` | Yes |
| App files, S3 backend | bucket `<app>/<key>` | `runtime/blobs.rs` `S3` | No |

### 1.6 Other directories

| Path | What | Breaks with two runners |
|---|---|---|
| `.trash/<at>-<slug>/` | Removed apps and pages, with `permissions.json` (the rows `forget_app` took away) | Yes, if the volume is not shared |
| `.tmp/` | Scratch: export snapshots, blob receive spools, device-file partials | No. Per-process scratch, stays local |
| `.tmp/inline/<id>/` | Base64 chunks of an inline upload (`inline_upload.rs`) | Yes: chunk 1 on runner A and chunk 2 on runner B never meet |

### 1.7 In-memory state in `Config`

| Field | Type | What | Breaks with two runners |
|---|---|---|---|
| `uploads` | `Mutex<HashMap<String, UploadTicket>>` | Upload tickets (15 min, reusable until expiry), and settings-entry links under the key `settings:<token>` (`secrets.rs`) | Yes: minted on one runner, used on another |
| `inline_uploads` | `Mutex<HashMap<String, InlineUpload>>` | Chunked uploads in progress | Yes |
| `blob_uploads` | `Mutex<HashMap<String, blobs::UploadTicket>>` | Browser upload tickets a handler mints; `take_upload` spends them | Yes |
| `logins` | `Mutex<HashMap<String, PendingLogin>>` | Provider sign-ins in flight: nonce, PKCE verifier, next | Yes: the callback can land on another runner |
| `previews` | `Mutex<HashMap<String, PreviewTicket>>` | One-time screenshot sign-ins | Only if the renderer reaches another runner |
| `handoffs` | `Mutex<HashMap<String, HandoffTicket>>` | Subdomain handoff codes. Each carries a **plain app session token** | Yes: the main host and the app host can be different runners |
| `connections` | `Arc<runtime::connections::Hub>` | Every open socket by app: topics, per-connection state, queues, per-app send rate, per-person and per-IP counts, total | Yes: a publish reaches only local sockets |
| `residents` | `Arc<runtime::resident::Residents>` | One instance per resident app, backoff, memory accounting, `declaring` deploy lock | Yes: two instances of one broker |
| `jobs` | `Arc<platform::schedule::Jobs>` | `running` slots and queued-again flags, `starts` per-minute windows, `changed` notify | Yes: two runners run one job twice |
| `ports` | `PortMap` | `TOOLSITE_PORTS` mappings and listeners (`ports::listen`, `tcp.rs`, `udp.rs` peers) | Yes, if two runners bind one public port |
| `limits`, `mfa`, `providers`, `github`, `renderer` | settings | From the environment | No |

### 1.8 Caches and other process state

| State | Where | Kind |
|---|---|---|
| Compiled handlers | `Runtime.handlers: Mutex<HashMap<String, (AppPre, Instant)>>`, keyed by app name, dropped by `Runtime::forget` | Cache. Breaks with two runners: a deploy on runner A does not tell runner B to forget |
| Compiled core modules | `Runtime.modules` | Cache, same issue |
| Favicons | `content/favicon.rs` `CACHE` | Cache, keyed by content; safe |
| GitHub installation tokens | `github::App.tokens` | Cache; safe |
| Scheduler | `schedule::Scheduler::spawn`, one per process from `main` | Process task. Two runners fire every job twice |
| MCP sessions | none: `with_legacy_session_mode(false)` | Stateless; any runner can answer |
| Per-call wasm state | `StoreState` (db connection, writers, deadline) | Per call; safe |

### 1.9 Network entry points and client facts

This matters for the edge role (section 6.4): whatever a runner learns from
the socket today, a worker behind an edge must learn from the edge instead.

| Entry point | Where | What it reads from the connection |
|---|---|---|
| HTTP on `PORT` | `main` (`axum::serve` without `ConnectInfo`) | No client address. `Host` (`app_hosts::route_by_host`, `origins::classify`), `Sec-Fetch-*` (`shield.rs`), cookies, `Authorization` |
| WebSocket upgrade | `websocket::upgrade` | Same as HTTP; `ConnectInfo` for the guest has no remote address |
| TCP ports from `TOOLSITE_PORTS` | `ports::listen`, `tcp.rs` | Peer address: `connections.remote` (the guest's `remote()`), `per_ip` and `raw_per_app` limits in `Hub` |
| UDP ports | `udp.rs` (`Peers` map per address) | Peer address: flow identity, `udp_per_second`, `udp_queued_bytes` |
| Screenshot renderer | `screenshot.rs`, `preview_base` (this runner's own port by default) | Opens `/preview/{token}` on whatever address `preview_base` names |

---

## 2. Target placement in Postgres mode

Rules behind the choices:

- Anything that must be the same on every runner goes to Postgres.
- Anything that is bytes and can be big goes to the bucket, with its pointer
  or digest in Postgres when readers must agree on a version.
- Caches stay per process and are keyed so that a stale entry is never used
  (by digest or generation), not invalidated by a message.
- Live sockets and instances stay per process. Step 2 coordinates them.

### 2.1 Placement table

| Item | Postgres mode | Reason |
|---|---|---|
| `auth.db` tables | Postgres schema `accounts`, same tables and columns | Read on every request; needs one truth |
| `oauth.db` tables | Postgres schema `oauth` | Same |
| `projects.json` | `platform.projects` (one row per project; `renamed_from text[]`) | Row updates under a transaction replace the whole-file rewrite |
| `relocating.json` | `platform.relocations` (at most one row) | Resumed under an advisory lock, so two runners cannot resume one move. PR 7: the lock is held for a whole move, not only a resume, since two moves at once share the one row; moves in one process also queue in memory before taking a connection, or waiters would hold the whole pool while the mover needs one |
| `labels.json` | `platform.host_labels (label primary key, app, issued_at)` | Uniqueness by constraint, not by a process mutex |
| `github.json` | `platform.github_installations` | Small, shared. PR 8: one row (`one boolean primary key`) holding the list as `json` |
| `grants-adopted` | `platform.site_flags (name primary key, value, at)` | One-time markers |
| `.meta` | `platform.pages (slug primary key, meta json, generation bigint, notes, created_at, updated_at)` | One row per page or app. `meta` keeps the `PageMeta` serde shape so the struct does not change. `json`, not `jsonb` (PR 6): `jsonb` refuses `\u0000`, which a meta string may hold, and `json` returns the text exactly as stored |
| `.notes` | `platform.pages.notes text` | Small text |
| `.secrets` | `platform.app_settings (app, name, sealed, primary key (app, name))` | Values stay sealed with the site key |
| `.jobs` | `platform.jobs (app, name, schedule, path, last_* columns)` | Single-row updates; no file lock. PR 9: `platform.jobs (app, name, job json)`, a row per job in `Job`'s serde shape, as the other records are; a change is `select ... for update` on the row, a new job is counted under `LOCK_JOBS` for the app |
| `.migrations` | `platform.app_migrations (app primary key, files jsonb)` | Small. PR 8: `json`, as metas are, for the same reason |
| `.exports`, `.deploys`, `.devices` | `platform.app_tokens (app, kind, id, label, hash unique, created_at, last_used)` with `kind in ('export','deploy','device')` | One row per token fixes today's lost-update race. PR 8: primary key `(app, kind, id)`, digest unique per kind; a check reads the app's digests of that kind and compares each in constant time. On files each list now changes under a lock and a rename too, so the race is gone there as well. A removal moves an app's tokens and records into `platform.removed_records`, in their sidecar's shape, and the trash writes them as sidecars |
| `.repo` | `platform.repo_links (app primary key, link jsonb, disconnected_at)` | Small. PR 8: `link json` only; `disconnected_at` stays inside the link, which `github.rs` reads. A change holds `LOCK_RECORDS` for the app |
| `.tools` | `platform.app_tools (app primary key, tools jsonb)` | Small. PR 8: `json` |
| `.icon`, `.source`, `.html`, bundle files, `handler.wasm` | Bucket objects under `.toolsite/content/` (section 2.3), with the generation in `platform.pages` | Bytes, possibly large |
| `<app>/data.db` | Stays on the volume in step 1. Moves to a Postgres schema in step 3 for apps on the `postgres` engine | SQLite needs a local file. See question 1 |
| Blobs, local backend | Refused: Postgres mode requires the bucket | A volume-local blob store cannot be shared |
| Blobs, S3 backend | Unchanged: bucket `<app>/<key>` | Already shared |
| `.trash/` | A removal moves the slug's `platform.pages` rows into `platform.removed_pages` (PR 6, so the primary key stays the slug and a republished app starts empty), plus `platform.trash (id, slug, at, permissions jsonb)`; objects move to `.toolsite/trash/<id>/` | Nothing destroys data; the rows and objects stay restorable |
| `.tmp/` scratch | Stays per process (`TOOLSITE_SCRATCH_DIR`, default `DATA_DIR/.tmp`) | Never shared, never read after the call |
| Upload, settings-link, blob-upload, provider-login, preview, handoff tickets | `state.tickets (kind, id_hash primary key, payload, sealed bool, expires_at)` | Any runner can mint or redeem |
| Inline upload chunks | Bucket `.toolsite/tmp/inline/<id>/<n>`; the ticket row holds the chunk map | Chunks can arrive at any runner |
| Job slots (`Jobs.running`) | `state.leases` rows `job:<app>/<name>` with holder, epoch, expiry | One run per job across runners. PR 9: `grp` (the app) counts the per-app ceiling under `LOCK_LEASES`; `again` is the queued rerun, spent or released in one statement; expiry is in milliseconds by the database's clock; a slot lasts 30 s and a run renews it every 10 s; an expired lease still asked to go again is an orphan the next scheduler wake takes over |
| Job start rate (`Jobs.starts`) | `state.rate_windows (key, window_start, count)` | Site-wide limit. PR 9: sliding, not fixed: a row per sixtieth of the window, summed under `LOCK_RATES`, so a burst across a minute boundary is still held to the rate |
| MFA wrong-code counter | Stays in `accounts.mfa_failures` | Already a table |
| Socket limits (`Hub` counts) | Stay per runner in step 1 and 2 | See question 4 |
| Compiled handler cache | Per process, keyed by `(app, generation)` | No invalidation message needed |
| Bundle file cache | Per process, on local disk, keyed by `(app, generation, path)` | Bucket reads are slow; generation makes stale entries unreachable |
| Hub, Residents, scheduler task | Per process | Step 2 adds the bus and leases |
| Runner registry (new) | `state.runners (id, role, address, started_at, heartbeat_at)` | Needed by the guard in 2.4 and by step 2 forwarding |

Timestamps stay `bigint` Unix seconds, as in SQLite, so the code that reads
them and the migration command do not convert anything.

### 2.2 Keys: seal key and form key

Today `seal.rs` takes `TOOLSITE_SECRET_KEY` if set, else generates
`.site/secret.key`. `users::form_secret` generates `.site/form.key`.

Two ways to give many runners the same key:

| Option | Gain | Cost |
|---|---|---|
| A. Environment variable on every runner | The key is never in the database. A database dump or a read-only SQL injection does not open sealed values or forge recovery-code digests | One more variable to keep safe. Losing it loses every sealed value |
| B. A `site_keys` table | Zero configuration | A dump of the database is a dump of the secrets. Sealing then protects nothing that the dump does not also give away |

Decision: **A**. In Postgres mode the server refuses to start without
`TOOLSITE_SECRET_KEY`. The form key is derived from it with HKDF-SHA256
(label `toolsite form key v1`), so there is one variable, not two. Changing
the form key ends open admin forms once, at cutover; nothing else uses it.

The migration command (section 5) checks that `TOOLSITE_SECRET_KEY` equals
`.site/secret.key` when that file exists, and refuses otherwise, because
sealed settings, MFA secrets and recovery digests would all stop working.
`toolsite key show` prints the existing key as base64 for the owner to copy
into the variable (on the machine only; never logged).

### 2.3 Bucket layout

App blobs already use `<app>/<key>` at the bucket root. Platform objects must
not collide with those keys. Every platform object goes under `.toolsite/`.
No app slug and no blob key can start with `.` (`valid_slug`,
`valid_asset_path`, `blobs::valid_key`), so no app can name that prefix.

```
.toolsite/content/<slug>/page.html          a loose page
.toolsite/content/<slug>/icon
.toolsite/content/<app>/source.tar.gz
.toolsite/content/<app>/files/<path>        bundle files, handler.wasm
.toolsite/trash/<id>/...                    removed content
.toolsite/tmp/inline/<id>/<n>               inline upload chunks
```

`TOOLSITE_CONTENT_S3_BUCKET` may name a second bucket for content; by
default it is the blob bucket.

Bundles keep today's overlay meaning: a new bundle overwrites the paths it
carries and keeps the others. Each publish increments
`platform.pages.generation` in the same step that completes the upload. Every
reader reads the generation with the meta it already reads per request, and
caches by `(app, generation, path)`. A deploy on runner A is then visible on
runner B at its next request, with no message.

Snapshot deploys (each deploy a new prefix, flipped by one pointer) would
also remove the short window in which a reader sees half of an overlay. That
is a change in meaning (old files vanish), so it is not part of step 1.

### 2.4 Guards at boot

In Postgres mode the server refuses to start when:

- `TOOLSITE_SECRET_KEY` is missing;
- the bucket is not configured;
- `.site/migrated-to-postgres` is absent and `DATA_DIR` holds file-mode
  platform state (`.site/auth.db` exists). The message names
  `toolsite migrate-to-postgres`.

In file mode the server refuses to start when `.site/migrated-to-postgres`
exists, so nobody writes to the old files by accident after cutover. The
message names `DATABASE_URL` and `toolsite export-to-files`.

Each runner writes a `state.runners` row and refreshes `heartbeat_at` every
10 s. Until step 3 lands, app databases are SQLite files on one volume. A
runner that finds another live runner in the table (heartbeat younger than
30 s) and has any SQLite app logs an error and refuses app traffic for those
apps. This turns a silent split brain into a loud refusal.

---

## 3. Interfaces

### 3.1 Shape

A few cohesive interfaces, each owned by the module that owns the concern
today. The data layer moves behind the trait; the rules stay where they are.
For example, `users::log_in` still checks the password and decides what a
session is; `AccountStore::insert_session` only stores the row.

```
src/state/             shared plumbing, no HTTP, everything may depend on it
  mod.rs               Backend selection, Stores struct, wait() helper
  pg.rs                pool, TLS, migration ladder runner, advisory lock keys
  tickets.rs           Tickets trait + memory and Postgres impls
  leases.rs            Leases trait + memory and Postgres impls
  rate.rs              RateWindows trait + memory and Postgres impls
  runners.rs           runner registry and heartbeat
  transfer.rs          files <-> Postgres mapping, used by the migrate commands
accounts/store/        AccountStore: mod.rs (trait), sqlite.rs, postgres.rs
platform/oauth_store/  OAuthStore: mod.rs, sqlite.rs, postgres.rs
content/catalog/       Catalog: mod.rs, files.rs, postgres.rs
content/files/         Files: mod.rs, local.rs, bucket.rs
platform/records/      AppRecords: mod.rs, files.rs, postgres.rs
platform/tokens/       Tokens: mod.rs, files.rs, postgres.rs
```

`Config` gains one field, `stores: state::Stores`, a struct of
`Arc<dyn Trait>` handles chosen once in `main`. `clone_for_task` clones the
handles. The six ticket maps leave `Config`.

`Tickets`, `Leases` and `RateWindows` sit in `state/` because `runtime`
needs them (a handler mints blob-upload tickets; residents take leases in
step 2), and `runtime` must not depend on `platform`. `accounts` needs
`Tickets` too (handoffs, provider logins).

Note on existing coupling: `runtime/wasm.rs` and `runtime/limits.rs` already
read `content::store::read_meta`, and `wasm.rs` calls `platform::secrets`
and `platform::devices`. Step 1 does not add to that. Those calls move to
`Catalog`, `AppRecords` and `Tokens`; a later change can move the three
traits' definitions into `state/` if the owner wants `runtime` to depend
only on `state` and `config`.

### 3.2 Sync or async

The traits are `async` (`async_trait`, already a dependency). Reasons:
axum handlers are async, `tokio-postgres` is async, and step 2's LISTEN and
lease renewal are async.

Two kinds of caller are synchronous today: wasm host functions (they run in
`spawn_blocking`) and the account functions (called from `spawn_blocking`).
They call `state::wait(future)`, which uses the stored runtime `Handle` and
`block_on`. That is legal on a blocking thread and panics on an async worker
thread, so a misuse fails loudly in tests. A host call passes its remaining
deadline, so a slow database counts against the guest's wall clock.

The SQLite and file implementations run their existing synchronous code
inside `spawn_blocking`, which is what the callers do today. Behaviour in
file mode does not change.

### 3.3 The traits

Methods are listed with the functions they replace. Signatures are
indicative; errors are `Result<_, String>` as today.

**`accounts::store::AccountStore`** replaces the SQL in `users.rs` and
`mfa.rs`.

| Group | Methods | Replaces SQL in |
|---|---|---|
| People | `insert_user`, `user_by_id`, `user_by_email`, `set_password_hash`, `password_hash`, `set_disabled`, `set_admin`, `list_users` | `sign_up_as`, `user_by_*`, `change_password`, `set_active`, `list_accounts` |
| Identities | `link_identity`, `user_by_identity`, `identities_for` | same names |
| Sessions | `insert_session(hash, user, scope, expires)`, `session_user(hash, scope, now)`, `delete_session`, `delete_sessions_for(user)` | `log_in`, `start_session`, `create_app_session*`, `session_user`, `log_out`, `set_active`, `mfa::reset` |
| Invitations | `insert_invite`, `invite_user(hash, now)`, `take_invite(hash, now)` | `new_invite`, `invited_account`, `accept_invite` |
| Grants and scopes | `set_grant`, `delete_grant`, `grant_of`, `list_grants`, `set_scope`, `delete_scope`, `scopes_for`, `list_scopes`, `move_scope_tree(from, to)`, `remove_scope_tree`, `forget_app(app, path) -> json` | `grant`, `revoke`, `role_for`, `grant_scope`, `revoke_scope`, `move_scope_tree`, `remove_scope_tree`, `forget_app`, `move_scopes` |
| Pins | `pins_for`, `set_pin` | same names |
| Two-step | `mfa_row`, `begin_mfa`, `enable_mfa`, `remove_mfa`, `advance_step(user, step) -> bool`, `replace_recovery_codes`, `spend_recovery_code -> bool`, `insert_pending`, `pending`, `fail_pending`, `delete_pending`, `record_failure`, `failures_since` | the SQL in `mfa.rs` |

`advance_step` and `spend_recovery_code` keep their conditional-update form
and return whether the row moved. They are the replay guard.

**`platform::oauth_store::OAuthStore`**: `register_client`, `client`,
`issue_code`, `redeem_code` (single use: `delete ... returning`),
`issue_tokens`, `rotate_refresh` (single use), `access_token_grant`,
`revoke_for_user`, `sweep`. Same names as the current free functions.

**`content::catalog::Catalog`** owns everything that answers "what is
published, where, and how":

| Method | Replaces |
|---|---|
| `slugs() -> Vec<String>`, `apps() -> Vec<String>` | `store::collect_slugs`, `origins::app_names`, directory scans |
| `meta(slug) -> PageMeta` | `read_meta`, `read_meta_blocking` |
| `update_meta(slug, FnOnce(&mut PageMeta)) -> PageMeta` | every `read_meta` then `write_meta` pair, `write_meta_blocking` |
| `generation(app) -> u64`, `bump_generation(app)` | (new) |
| `notes(slug)`, `set_notes(slug, text)` | `read_notes`, `write_notes` |
| `folders()`, `update_folders(FnOnce(&mut Vec<Folder>))` | `list_folders`, `write_folders`, `FOLDERS_WRITE` |
| `relocation()`, `begin_relocation(from, to)`, `end_relocation()` | `relocating.json` |
| `label_for(app)`, `app_for_label(label)` | `origins::label_for`, `app_for_label`, `ASSIGNING` |
| `flag(name)`, `set_flag(name)` | `.site/grants-adopted` |
| `mark_removed(slug, at) -> TrashId`, `restore(id)` | the bookkeeping half of `trash::remove` |

`update_meta` is the important change. It replaces about a dozen unlocked
read-modify-write sites with one call that is atomic on both backends (a
process mutex per slug in file mode; `select ... for update` in Postgres).

`close_if_hidden` moves out of the store. `update_meta` returns the new meta,
and the caller (or a small `AppEvents` sink, section 6.1) closes sockets and
stops residents. That removes the store's reach into `connections` and
`residents`.

**`content::files::Files`** owns bytes that are published:

| Method | Replaces |
|---|---|
| `page(slug) -> Option<Bytes>`, `put_page(slug, html)` | `store::page_path`, direct `fs::read` in `serve`, `mcp`, `upload` |
| `asset(app, generation, path) -> Option<(Bytes or stream, len)>` | asset reads in `serve::serve_page` |
| `put_bundle(app, archive) -> Unpacked` | `bundle::unpack_bundle` into `DATA_DIR/<app>` (validation stays in `bundle.rs`) |
| `handler(app, generation) -> Option<Bytes>`, `put_handler(app, wasm)` | `serve::handler_wasm`, `handler_wasm_blocking`, `schedule::run_once`, `app_tools` reads |
| `icon(slug)`, `put_icon(slug, bytes)` | `store::icon_path` and writers |
| `source(app)`, `put_source(app, bytes)` | `.source` readers and writers |
| `move_to_trash(slug, id)`, `restore(id)` | the file half of `trash::remove` |
| `chunk_put(id, n, bytes)`, `chunks(id)` | `.tmp/inline/` in `inline_upload.rs` |

The local implementation keeps today's layout byte for byte. The bucket
implementation keeps a disk cache keyed by generation. `bundle.rs` keeps the
traversal defence; `Files` receives only validated relative paths.

**`platform::records::AppRecords`** owns small per-app records that are not
credentials: `setting(app, name)`, `setting_names(app)`, `set_setting`
(values sealed by the caller, as `secrets.rs` does now), `tools`,
`set_tools`, `migrations`, `set_migrations`, `repo_link`, `set_repo_link`,
`linked_apps`, `installations`, `set_installations`, and jobs:
`jobs(app)`, `apps_with_jobs()`, `set_job`, `remove_job`,
`update_job(app, name, FnOnce(&mut Job))`.

**`platform::tokens::Tokens`**: `create(app, kind, label) -> (record,
plain)`, `list(app, kind)`, `list_all(kind)`, `revoke(app, kind, id)`,
`check(app, kind, plain) -> Option<record>` (records use at the token's
resolution: `update ... where last_used is null or last_used < now - $res`).
It replaces the three near-identical files `export.rs`, `deploy.rs` and
`devices.rs` keep today; the HTTP and policy code in those modules stays.

**`state::Tickets`**: `put(kind, ttl, payload) -> plain id`,
`get(kind, id)` (reusable tickets: uploads, settings links),
`take(kind, id)` (single use: blob uploads, logins, previews, handoffs),
`update(kind, id, payload)` (inline upload progress). Ids are stored as
SHA-256 digests. Payloads that carry a credential (the handoff's app session
token, the PKCE verifier) are sealed. Memory implementation for file mode.

**`state::Leases`**: `try_acquire(name, holder, ttl) -> Option<Lease>`,
`renew(&Lease) -> bool`, `release(Lease)`, `holder(name)`. A `Lease` carries
an `epoch` that grows on every new holder (a fencing token). Memory
implementation for file mode.

**`state::RateWindows`**: `spend(key, per, window) -> Result<(), u32>`.

### 3.4 Choosing the backend

`main` builds `state::Backend::from_env()`: `Files { data_dir }` when
`DATABASE_URL` is unset, `Postgres { pool, bucket }` when set. It then builds
`Stores` from it. The `toolsite user` subcommands use the same builder, so
`toolsite user add` works against either backend.

### 3.5 Tests on both backends

The suite stays hermetic: `cargo test` touches no network and no Postgres.

- **Conformance suites.** Each trait has one generic test module, for
  example `accounts::store::tests::conformance(make: impl Fn() -> S)`. The
  SQLite instance always runs. The Postgres instance is a separate test,
  `#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh
  starts one"]`. Each Postgres test creates a fresh schema prefix
  (`t_<random>_accounts` and so on), sets `search_path`, and drops it at the
  end.
- **The whole router on Postgres.** `tests/http.rs` and the other
  integration files build their `Config` through one helper. When
  `TOOLSITE_TEST_BACKEND=postgres` is set, the helper builds Postgres stores
  and a MinIO bucket instead. The same tests then run on both backends.
- **`scripts/test-postgres.sh`** starts `postgres:17` and `minio` with
  docker on random ports, exports the variables, runs `cargo test --
  --ignored postgres` and then the full suite with
  `TOOLSITE_TEST_BACKEND=postgres`, and removes the containers.
- **Container trial.** Per CLAUDE.md, a change that reaches the outside
  world is tried in the container: `compose.yml` gets a `postgres` profile
  with `postgres` and `minio` services. TLS to a public Postgres uses rustls
  with bundled roots, so it is tried from `debian:bookworm-slim` too.
- The push rule in memory (suite passes, adversarial pass on access changes)
  extends to: `scripts/test-postgres.sh` passes for any change under
  `state/` or a `*/postgres.rs`.

---

## 4. Postgres specifics

### 4.1 Crate choice

| Crate | For | Against |
|---|---|---|
| `tokio-postgres` + `deadpool-postgres` | Async, light, fast to compile, no macros, LISTEN/NOTIFY and cancel tokens built in, rustls via `tokio-postgres-rustls` | Hand-written row mapping; migrations are ours (we already write our own ladder) |
| `sqlx` | Pool and migrations included | Compile-time checked queries need a database or an offline cache at build time, which complicates the Docker build. Its SQLite feature links `libsqlite3-sys`, which conflicts with `rusqlite`'s bundled copy, so only the Postgres feature could be enabled. Heavier macros, slower builds |
| `diesel` | Typed DSL | Synchronous core, heavy, a DSL for a few dozen simple queries |

Decision: **`tokio-postgres` + `deadpool-postgres` + `tokio-postgres-rustls`**
(with `webpki-roots`, matching `reqwest`). No OpenSSL in the image.

### 4.2 Schema and migrations

```
migrations/postgres/accounts/001_initial.sql ...   schema "accounts"
migrations/postgres/oauth/001_initial.sql ...      schema "oauth"
migrations/postgres/platform/001_initial.sql ...   schema "platform"
migrations/postgres/state/001_initial.sql ...      schema "state"
migrations/postgres/schema.sql                     declared shape, all four
```

- One Postgres schema per store keeps the boundaries visible in the
  database: nothing in `oauth` has a foreign key into `accounts`, as today.
- `state.migrations (store text, version int, applied_at bigint)` records
  each ladder. `state::pg::migrate` applies pending files, each in its own
  transaction, while holding `pg_advisory_lock(LOCK_MIGRATE)`, so several
  runners booting at once apply each step once.
- `accounts/001_initial.sql` is the end state of the SQLite ladder
  (`migrations/schema.sql`), not a replay of nine steps.
- A test (on Postgres, `#[ignore]`d like the others) applies every ladder to
  an empty database and compares `information_schema` and `pg_indexes` with
  `migrations/postgres/schema.sql`, like
  `the_ladder_produces_exactly_the_declared_schema` does for SQLite.
- A runner refuses to start when the database holds a version newer than it
  knows (an older binary must not write a newer schema).

### 4.3 Pooling

- One `deadpool-postgres` pool per process, size from
  `TOOLSITE_DATABASE_POOL` (default 16). Statements are prepared per
  connection and cached by `tokio-postgres`.
- Leases that use session-level advisory locks (step 2) and LISTEN take a
  **dedicated** connection outside the pool, so they cannot be lost when a
  pooled connection is recycled.
- PgBouncer in transaction mode breaks session advisory locks, LISTEN and
  per-connection prepared statements. The README will say: connect directly,
  or use session mode.
- `DATABASE_URL` is never logged. Boot logs print host, port and database
  name only. Errors walk `source()` (as CLAUDE.md asks for `reqwest`) and
  the connection string is removed from any text that is logged.

### 4.4 Where file code relied on a process lock

| Today | Postgres |
|---|---|
| `FOLDERS_WRITE` around read, edit, write of `projects.json` | `update_folders` runs in one transaction after `pg_advisory_xact_lock(LOCK_PROJECTS)`, reads all rows, applies the closure, writes the difference |
| `relocating.json` journal, resumed at boot and before each move | `platform.relocations` row. `resume_pending`, and every move from its first check to its last step (PR 7), holds `pg_advisory_xact_lock(LOCK_RELOCATION)` in a transaction on a connection kept for the move; a hold dropped without release closes that connection. App moves and project creations and removals take the same hold. Waiters queue in-process first, at most `catalog::MAX_WAITING_MOVES` per site; past that a request is refused. A tree or move record that cannot be read counts as the whole site locked The three steps of `projects::finish` stay as they are, in the same order; each is idempotent. They stay separate steps because the access rows belong to `AccountStore` and the tree to `Catalog`, and one transaction across both would couple them |
| `schedule::FILES` around job record updates | Single-row `update platform.jobs set ... where app = $1 and name = $2`. `set_job` checks `MAX_JOBS_PER_APP` under `pg_advisory_xact_lock(LOCK_JOBS, hash(app))` |
| `Jobs.running` map | A lease `job:<app>/<name>` per run. The per-app running ceiling is a count of live `job:<app>/*` leases, checked under `pg_advisory_xact_lock(LOCK_JOBS, hash(app))` |
| `Jobs.starts` minute windows | `RateWindows::spend("job-starts:<app>", per_minute, 60s)`: one upsert on `(key, window_start)` |
| `origins::ASSIGNING` | Insert into `host_labels` with the primary key as the guard, under `pg_advisory_xact_lock(LOCK_LABELS)` while the free label is chosen |
| `devices::WRITING` | Row per token; `last_used` written with a conditional update |
| `Residents.declaring` (handler and manifest checked against each other) | A lease `deploy:<app>` held for the duration of the check and the write |
| `.site/form.key` and `secret.key` first-use races | Gone: the keys come from the environment |
| `write_meta` unlocked | `update_meta`: `select meta from platform.pages where slug = $1 for update`, closure, `update` |
| Single-use tickets and OAuth codes | `delete ... where id_hash = $1 and expires_at > $2 returning ...` |

Advisory lock keys are two `int4` values: a fixed class per purpose
(`LOCK_MIGRATE`, `LOCK_PROJECTS`, ...) in `state::pg`, and an object id
(`hashtext(app)` or 0). Fixed classes avoid collisions between purposes.

---

## 5. Migrating an existing site

### 5.1 Command

`toolsite migrate-to-postgres [--dry-run] [--verify-only]`, a subcommand
beside `toolsite user`. It reads `TOOLSITE_DATA_DIR`, `DATABASE_URL`, the
bucket variables and `TOOLSITE_SECRET_KEY`.

Not a startup import: an import at boot would run on every runner at once,
would run while traffic arrives, and would hide a long copy inside a health
check timeout. A command is run once, by a person, with the server stopped.

### 5.2 Steps

1. **Preconditions.** `TOOLSITE_SECRET_KEY` matches `.site/secret.key` (if
   that file exists). `.site/relocating.json` is absent (else: "start the
   server once in file mode so the move finishes"). No live row in
   `state.runners`. The bucket answers a test write under `.toolsite/tmp/`.
2. **Schema.** Apply the Postgres ladders.
3. **Rows**, in this order: accounts (all eleven tables, hashes copied as
   they are, so sessions, invitations and recovery codes keep working),
   oauth (connectors stay connected), projects, labels, installations,
   flags, pages and meta (one row per `.meta`, with `notes`), settings
   (sealed values copied as they are, never opened), jobs, migrations,
   tokens, repo links, tools, trash records.
4. **Objects.** Pages, icons, sources, bundle files and handlers to
   `.toolsite/content/`. Trash to `.toolsite/trash/`. With the local blob
   backend, every `<app>/.blobs/data/<key>` and its meta to `<app>/<key>`.
   `data.db` files stay where they are (step 1 keeps SQLite apps on the
   volume).
5. **Marker.** Write `.site/migrated-to-postgres` with the time and the
   counts.

The walk never follows symlinks and reads only paths that pass the slug and
asset-path rules, the same defence `bundle.rs` and the CLI archive use.

In-memory tickets are not migrated. They live minutes, and the server is
stopped.

### 5.3 Idempotence

Every row is an upsert on its natural key that sets every column, so a second
run converges on the files' state. Every object is compared by size and
SHA-256 (kept as object metadata) and skipped when equal. Interrupting the
command and running it again is safe.

### 5.4 Verification

`--verify-only` (and the end of every run) compares both sides per category:
row counts, and a digest over the canonical JSON of each row sorted by key;
object counts and digests. It prints one table and exits non-zero on any
difference. The verification is the same code as the import's mapping
(`state::transfer`), read in both directions.

### 5.5 Rollback

- **Before any write in Postgres mode:** remove `DATABASE_URL` and the
  marker. The files were only read, never changed.
- **After running in Postgres mode:** `toolsite export-to-files` writes
  Postgres and bucket state back into the `DATA_DIR` layout, through the same
  `state::transfer` mapping, and removes the marker.
- A round-trip test (files to Postgres to files) compares the two `DATA_DIR`
  trees and must find them equal, apart from `.tmp/`.

---

## 6. Seams for steps 2 to 5

Step 1 leaves these hooks, each with a trivial in-process implementation.

### 6.1 Step 2: bus, resident leases, scheduler lease

**`state::bus::Bus`**: `publish(event)`, `subscribe() -> Stream<Event>`.
Implementations: in-process (`tokio::sync::broadcast`), Postgres
`LISTEN/NOTIFY`, Redis when `REDIS_URL` is set. Events:

| Event | Sent by | Effect on every runner |
|---|---|---|
| `AppChanged { app, generation, hidden, removed }` | `update_meta` callers, publish, remove | Close the app's sockets and stop its resident when hidden or removed (today's `close_if_hidden`, `trash::remove`) |
| `ConnectionOp { app, conn, op }` | `Hub::send`, `close`, `state_*` for a connection on another runner | The holder applies it |
| `TopicPublish { app, topic, message }` | `Hub::publish` | Each runner delivers to its own subscribers |
| `JobsChanged { app }` | `set_job`, `remove_job` | The scheduler plans again (today's `Jobs.changed`) |

Step 1 hook: an `AppEvents` sink in `Stores`. Its in-process implementation
calls `connections.close_app` and `residents.stop`, which is today's
behaviour. Step 2 swaps it for the bus.

Step 1 hook: connection ids get the runner id as a prefix
(`<runner>.<random>`), so a remote op is routed without a lookup. Ids are
opaque to guests, so this is not a WIT change.

NOTIFY payloads are at most 8000 bytes. A message over 6 KB is stored in
`state.bus_spill` and the notification carries its id. Every bus payload is
authenticated with an HMAC under a key derived from `TOOLSITE_SECRET_KEY`
(section 7).

Two open semantics for step 2: `connections.publish` returns a delivery
count, which across runners is the local count unless the publisher waits for
acknowledgements; and `state-get` on a connection held by another runner
needs a round trip. Step 2's note decides both.

**Resident apps.** `Leases::try_acquire("resident:<app>")` decides which
runner runs the instance. Other runners forward the app's WebSocket upgrades
and its TCP and UDP traffic to the holder's address from `state.runners`.
The holder renews every `ttl / 3` and stops the instance (closing its
connections, as a failure does today) when a renewal fails before expiry, so
two instances never run at once. Step 1 hook: `Leases`, `state.runners.address`
(from `TOOLSITE_RUNNER_ADDRESS`, for Railway the private address).

**Scheduler.** `Scheduler::spawn` gets a `Leadership` argument: always-leader
in step 1, a `Leases` lease `scheduler` in step 2. Correctness does not depend
on the lease alone: a fired turn is recorded in `platform.job_turns (app,
name, due_at, primary key (app, name))`, claimed before the run starts by
an upsert that only moves `due_at` forward. A second leader in a
split-brain moment finds the turn taken and skips. PR 9: every runner runs
a scheduler already, since a turn is claimed in `job_turns` and a run's
slot is a lease; `due_at` is the latest scheduled time at or before the
tick, so two schedulers that read the job at different moments still name
the same turn. A turn no later than the one claimed is refused, so a
runner whose clock runs behind cannot fire an old turn after a newer one,
and a job keeps one row however often it fires (`job_fires`, a row per
turn kept for a day, was replaced by platform/005 after an adversarial
pass). In step 4,
the leader (control) inserts into `platform.job_queue`, and workers claim
with `select ... for update skip locked`.

### 6.2 Step 3: per-app Postgres schemas

**`runtime::appdb::AppDb`** is the seam: `query`, `query_scoped`, `batch`,
`batch_scoped`, `apply_migrations(files)`, `apply_access(policies) ->
generated`, `describe_views`, `snapshot() -> path` (for `/export`),
`size_bytes`. The SQLite implementation is today's `db.rs`, `access.rs` and
`migrate.rs`. The WIT `db` interface does not change; the meta gains
`engine`, fixed once the app holds data.

Recommendation: app schemas live in a **separate database** on the same
server (`TOOLSITE_APPS_DATABASE_URL`, default the same server with database
`toolsite_apps`). Reasons: `NOTIFY` and advisory locks are per database, so
an app role cannot touch the platform's bus or locks; the catalog an app can
read (`pg_class`, `pg_namespace`) then shows other apps' table names but
never platform tables.

| SQLite rule today | Postgres mechanism |
|---|---|
| One file per app (`db_path`) | Schema `app_<slug>`, owned by login role `app_<slug>`. Separate login role per app; never `SET ROLE` from a shared role, because guest SQL could `RESET ROLE` |
| `deny_escapes`: no `ATTACH`, `DETACH`, `PRAGMA` | `revoke all on schema public from public`; no `usage` on other schemas; no `create` on the database; app roles are not superuser and are members of no role |
| `max_page_count` size ceiling | `size_bytes` measured after writes (cached) and on a timer; over the ceiling, the platform revokes `insert` and `update` on the schema until it shrinks. `lo_create` and `lo_import` revoked from `public` so large objects cannot bypass it |
| `interrupt_at` and `busy_until` deadline | `statement_timeout` as a role default, plus the host's real guard: the cancel token fires at the guest's deadline. `SET statement_timeout = 0` in guest SQL therefore does not help it |
| Row cap and `truncated` | Rows read from a portal and stopped at the cap |
| `current_user()`, `current_email()`, `current_role()` | `current_user` is a reserved word in Postgres. Functions `toolsite.user_id()`, `toolsite.email()`, `toolsite.role()`, `security definer`, reading the GUC `toolsite.identity`. A GUC can be set by any session with `SET`, so the value carries an HMAC (key in a table only the platform role can read), bound to `pg_backend_pid()` and an expiry. A forged or replayed value reads as NULL |
| Scoped authorizer: declared views, salted inner names, platform triggers | Native RLS: `enable` and `force row level security` on policy tables, policies written against `toolsite.user_id()`. A second login role `app_<slug>_scoped` gets only the declared views and tables. Views are created with `security_invoker = true` (Postgres 15 or newer is required), else a view would read base tables as its owner and pass RLS |
| `DENIED_FUNCTIONS` (`readfile`, `load_extension`, ...) | Not reachable without superuser or `pg_read_server_files`; `create extension` denied; `COPY ... PROGRAM` denied |
| Batch in one transaction, no `begin` inside | Host-held transaction; the host checks transaction status after each statement and fails the batch if guest SQL ended it |
| Read-only BI | Login role `app_<slug>_read` with `select` on the schema, issued from `/admin/exports` like an export token |

Pools: one small pool per app role (2 connections, idle timeout 60 s),
created on first use, so hundreds of apps do not hold hundreds of idle
connections.

### 6.3 Step 4: control and worker roles

`TOOLSITE_ROLE` decides which routes `build_router` mounts and which loops
`main` starts. The binary and the code paths are the same. A route mounted
on the wrong role answers `421 Misdirected Request` with the right host.

| Control | Worker | Both |
|---|---|---|
| `/mcp`, `/me/mcp` | `/p/{*slug}` (pages, assets, handlers) | `/healthz` (`app_hosts::HEALTH_PATH`) |
| `/.well-known/*`, `/register`, `/authorize`, `/token` | WebSocket upgrades (`websocket::upgrade`) | |
| `/auth/login*`, `/auth/callback/*`, `/auth/logout`, `/auth/me`, `/auth/setup`, `/auth/mfa*` | `/p/{app}/mcp` (app tools run handlers) | |
| `/auth/handoff` (main host) | `/auth/landing` (app host) | |
| `/account*`, `/settings*` | `/blob/{ticket}` | |
| `/admin*`, `/github/*` | `/export/{file}` | |
| `/upload/*`, `/deploy/*` | `/preview/{token}` | |
| `/`, `/browse/*`, `/icon/*`, `/favicon.*`, `/guide`, `/wit/*`, `/scaffold/*`, `/examples*` | TCP and UDP ports, resident instances, job execution | |
| Scheduler leader, `resume_pending`, `adopt_grants`, migrations at boot | Job queue claims | |

Control also runs handlers when an MCP tool needs one (`call_app_tool`,
`run_sql`); that is the same code, not a route.

Without an edge, the split needs routing by host, which Railway does per
service and not per path: subdomain mode only (`tools.*` to control,
`*.apps.*` to workers). With an edge (section 6.4) the edge routes by host
and path, so path mode can split roles too.

`TOOLSITE_ROLE` is a comma list of `control`, `worker` and `edge`. `all` is
`control,worker` in one process with no proxy at all: today's behaviour,
and the default. Any set that holds `edge` makes the edge the public
listener on `PORT`; any set without `edge` (for example `worker`) listens
only on the internal port and accepts only requests that an edge signed.

Step 1 hook: `build_router` already assembles every route in one place. Step
1 groups them into `control_routes()` and `worker_routes()` functions with no
change in behaviour, so step 4 only selects.

### 6.4 Edge: toolsite's own router (step 5)

**Why.** Railway's load balancer picks a replica at random, with no
affinity. A resident app's connections must reach the one worker that holds
its lease, and a device's TCP stream cannot be redirected. An edge that
knows the placement solves that, and also gives worker pools, health checks,
draining, and workers that are not on the internet at all.

**What it is.** A module `platform::edge` (one concern: deciding where a
connection goes and carrying its bytes there). It runs no handler, opens no
app database and keeps no state that another edge would need. Several edges
run side by side behind Railway's balancer.

#### Placement: the routing table and its source

For each incoming request or stream the edge decides one target:

| Traffic | How it is recognised | Target |
|---|---|---|
| Platform route | Main host (`origins::classify`), path in the control set of 6.3 | A live control runner (the local one in `control,edge`) |
| App traffic, ordinary app | App host, or `/p/<app>/...` in path mode | A live worker in the app's pool, chosen by rendezvous hashing on the app name, so one app's compiled handler and bundle cache stay warm on few workers. Next choice on failure |
| App traffic, resident app or SQLite home app (question 1) | `meta.resident` or the app's engine | The holder of lease `resident:<app>` (or `home:<app>`). With no holder, the hashed choice, which then takes the lease |
| TCP or UDP port | `TOOLSITE_PORTS` mapping, now bound by the edge | As for the mapped app (a port's app is usually resident) |
| Anything else | Unknown host | Refused at the edge, as `route_by_host` refuses it today |

Sources, all in Postgres, all cached in the edge:

| Data | Table | Cache refresh |
|---|---|---|
| Runners: id, roles, pool, internal address, internal port, `draining`, `heartbeat_at` | `state.runners` (extended from step 1) | Every 2 s, and on bus `RunnersChanged` |
| Leases: `resident:<app>`, `home:<app>` | `state.leases` | On bus `LeaseChanged`; entries older than 10 s are read again |
| App facts: pool, resident, sockets, label | `platform.pages.meta` | On bus `AppChanged` |
| Port map | `TOOLSITE_PORTS` (environment of the edge) | At boot |

A stale cache is never a correctness problem, because **the worker is the
authority**. A worker that receives traffic for a resident app whose lease it
does not hold answers `421` with `x-toolsite-holder: <runner id>` (HTTP and
WebSocket) or a refusal code in the stream preamble reply (TCP, UDP). The
edge drops that cache entry, reads the lease and retries once. Two instances
of one resident app therefore never run, whatever the edges believe.

A new app field `pool` (from `[runtime] pool = "..."` in toolsite.toml,
default `default`) names the worker pool. Workers set their pool with
`TOOLSITE_POOL`. That is the hook for worker pools; it opens nothing new.

#### Finding workers

- Each runner writes its `state.runners` row at boot and every 5 s:
  `roles`, `pool`, `address` (its private address, from
  `TOOLSITE_RUNNER_ADDRESS`; on Railway the replica's address on
  `*.railway.internal`, IPv6), `internal_port` (`TOOLSITE_INTERNAL_PORT`,
  default 8081), `draining`.
- The edge counts a runner live when its heartbeat is younger than 15 s
  **and** its own probe of `GET /healthz` on the internal port passed in the
  last 5 s. A failed connect marks a runner down at once, with backoff,
  before the next probe.
- Workers get no public domain on Railway. The internal port is reachable
  only on the private network.

#### Carrying the bytes

| Kind | How | Backpressure and timeouts |
|---|---|---|
| HTTP | `hyper-util` client, HTTP/1.1 keep-alive pool per worker. Bodies stream both ways, never buffered, so `/upload` and `/blob` keep their own ceilings | Connect timeout 2 s. Retry on another runner only when the connect failed (no byte sent), for any method; never after a byte was sent. Response-header timeout = the request wall-clock ceiling (`limits.request_seconds`) plus 5 s. Header read timeout on the client side 10 s (slow headers). A cap on in-flight requests per worker and per edge; past it, `503` with `Retry-After` |
| WebSocket | The edge forwards the upgrade request. Only when the worker answers `101` does the edge answer the client `101` and splice the two upgraded connections (`copy_bidirectional`). Frames are not parsed | Bounded copy buffers: a slow client stalls the worker's writes, which the worker's existing per-connection queue cut-off handles. Idle timeout follows `Limits.check_every`. A non-`101` answer is passed through as an ordinary response and the connection is never spliced (upgrade smuggling) |
| TCP | The edge accepts on the mapped port, opens a TCP connection to the worker's internal stream port, sends a signed preamble (below), waits for the worker's accept byte, then splices | `tcp_idle` and `tcp_send_timeout` apply at the edge too. Per-IP and per-app connection counts move to the edge, which is the only place that sees every client |
| UDP | One authenticated framed stream per (edge, worker, port). Each frame: flow id, client address, length, datagram. Replies come back on the same stream and leave from the edge's socket | Per-flow queue bounded by `udp_queued_bytes`; on overflow the datagram is dropped, as UDP allows. `udp_per_second` is enforced at the edge before the datagram crosses the network |

#### What passes from edge to worker, and how it is protected

**Identity does not pass.** A worker reads the session cookie or bearer and
looks it up in Postgres itself, exactly as today. The edge never says who a
person is, so a compromised or confused edge cannot sign anyone in.

**Transport facts pass**, because only the edge knows them: client address
and port, the `Host` the client sent, scheme, and the edge's id. They travel
in one header, `x-toolsite-edge`, signed with HMAC-SHA256 under a key derived
from `TOOLSITE_SECRET_KEY` (label `toolsite edge v1`). The MAC covers the
facts, a timestamp, and the request's method, host and path. TCP and UDP
streams carry the same fields in a signed preamble.

- The edge removes every incoming `x-toolsite-*`, `forwarded`,
  `x-forwarded-*` and `x-real-ip` header before it adds its own.
- A worker, and a control runner behind an edge, accept a request only with a
  valid header no older than 30 s; anything else is `403`, logged at `warn`
  with the arriving headers (never cookies or tokens, as CLAUDE.md asks). The
  check is one middleware on the internal listener, outermost, before
  `shield` and `route_by_host`.
- Workers then trust the facts: `ConnectInfo.remote` and the per-IP limits
  use the signed address.

Why a signed header rather than relying on the private network: every
service in the Railway project can reach a worker's internal port, and that
includes the screenshot browser sidecar, which runs untrusted page script.
Without a check, a published page could have the sidecar call a worker with
any client address it likes. mTLS on the private network would also work,
but needs a certificate authority and rotation for no gain over a MAC: the
private network is already WireGuard-encrypted, so the header is not visible
to an eavesdropper. mTLS stays an option for a deployment off Railway.

#### Draining

On SIGTERM a worker sets `draining = true`. Edges stop sending it new
requests and streams within one cache refresh (bus `RunnersChanged` makes it
immediate). Requests in flight finish, up to the platform's drain window. Its
WebSockets get close code `1012` (service restart) so browsers reconnect,
which lands them on another worker. A resident holder releases its lease
first, so the next connection starts the instance elsewhere. An edge that
drains stops accepting and lets its spliced streams end, up to the same
window.

#### `control,edge` in one process

One public listener on `PORT`. The edge layer classifies each request:
platform routes go to the in-process control `Router` by `oneshot`, with the
transport facts in a request extension rather than a header (the same way
`lib.rs` already hands `/p/{app}/mcp` to its own router); app traffic is
proxied to workers. The edge also binds `TOOLSITE_PORTS`. Control's loops
run (scheduler leader, `resume_pending`, migrations); no worker loops run,
and this process never instantiates a guest for app traffic.

In `all` the edge layer is a pass-through that only builds the transport
facts from the socket, so file mode and single-runner sites behave as today.

#### Step 1 hooks for the edge

- `state.runners` carries `roles`, `pool`, `address`, `internal_port` and
  `draining` from the start.
- Transport facts become one type, `TransportFacts { client, host, scheme
  }`, built by a layer at the listener and read from request extensions by
  `route_by_host`, `shield`, `websocket.rs`, `tcp.rs` and `udp.rs`. In step 1
  it is filled from the socket; in step 5 from the verified header.
- `ports::listen` takes its port map and target as arguments, so the same
  code can run in an edge.

---

## 7. Security

| Surface | Risk | Defence | Tests |
|---|---|---|---|
| SQL in platform code | Injection through a slug, email, label or JSON field | Every value is a bound parameter. Identifiers come from constants only. A unit test scans `*/postgres.rs` and `state/pg.rs` and fails on `format!` inside a `query`, `execute` or `prepare` argument | Conformance tests pass hostile values (`'); drop table x; --`, NUL, 64 KB strings, Unicode lookalikes) through every write and read them back unchanged |
| Tenant mixing in shared tables | A token or setting of app A used for app B | Every per-app method takes `app` and filters on it; tokens are unique by hash and checked with `app` | Existing token tests (export, deploy, device) run on both backends; a test mints on A and presents on B |
| Tickets | Replay, guessing, theft from a dump | Ids stored as SHA-256; single-use `take` is one `delete ... returning`; credentials in payloads sealed | Two tasks `take` one ticket at once: exactly one wins. A dump of `state.tickets` contains no plain id and no session token |
| Sealed values | Key next to the data | Key from the environment only, refused boot without it | A test scans every text column after a full run for the key's base64 and for any plain setting value |
| Session and MFA races | Two runners accept one TOTP code or one recovery code | Conditional updates (already) | Concurrent `accept` with one code on Postgres: one success |
| Leases | Two holders after a pause or a lost connection | Epoch fencing; holders stop before expiry when renewal fails; a forward-only claim in `job_turns` | A test pauses a holder past its TTL: the second holder takes over, the first's writes with the old epoch are refused; two schedulers on one database fire each turn once |
| Bus (`NOTIFY`) | Any role in the database can `NOTIFY` any channel | Apps in a separate database; every payload carries an HMAC; unknown or bad payloads are dropped and logged at `warn` | A forged `NOTIFY` with a valid shape and no HMAC changes nothing |
| Per-app roles (step 3) | Cross-schema reads, `RESET ROLE`, forged identity, `SET statement_timeout = 0`, `pg_sleep`, large objects, `security definer` functions | Section 6.2 | One adversarial file `tests/pg_app_adversarial.rs`: each escape tried as guest SQL and refused, in the style of `rls_adversarial.rs` |
| Catalog visibility | App A sees app B's table names in `pg_class` | Accepted for step 3, stated in the README; a database per app is the fix if names are sensitive (question 2) | A test records exactly what is visible, so a change is noticed |
| Connection exhaustion | One app holds every connection | Per-app pool of 2; the host ends every transaction at the end of a call | Many concurrent calls of one app leave other apps served |
| `DATABASE_URL` leaks | Password in logs or error pages | Never logged; errors redacted | A test fails a connect on purpose and checks the logged text for the password |
| Bucket prefix | An app names `.toolsite/...` through a blob key | `blobs::valid_key` refuses a leading `.` | A forged key `.toolsite/content/x/files/index.html` is refused by `put`, `get`, `list` and `upload-url` |
| Migration command | Symlinks or odd names in `DATA_DIR` pull in other files | No symlink following; slug and asset-path rules | A symlink to `/etc/passwd` placed in an app directory is skipped and reported |
| Edge: bypass | A request reaches a worker or control without passing an edge | Internal listener only; signed `x-toolsite-edge` required | Worker router driven by `oneshot` with no header, a bad MAC, a header older than 30 s, and a valid header for another path: each `403`, logged |
| Edge: forged client facts | A client sends `x-forwarded-for`, `forwarded`, `x-real-ip` or its own `x-toolsite-edge` | The edge strips them all before signing | A request through a real edge with each forged header: the guest's `remote()` and the per-IP count use the socket's address |
| Edge: private-network callers | The screenshot sidecar or another project service calls a worker | Same MAC check; the key is never in the database | A test plays the sidecar: a plain request to the internal port, with and without copied headers, is refused |
| Edge: stream preambles | A forged or replayed TCP or UDP preamble on the worker's stream port | MAC over facts, timestamp and port; 30 s window | Forged, stale and other-port preambles are refused before any byte reaches the app |
| Edge: misrouting | A stale cache sends a resident app's traffic to a runner without the lease | The worker answers `421` with the holder; the edge retries once | Two workers, lease moved between them: the edge follows; at no point do two instances run |
| Edge: request and upgrade smuggling | Conflicting `Content-Length` and `Transfer-Encoding`; a non-`101` answer to an upgrade followed by raw bytes | hyper on both sides refuses ambiguous framing; the edge splices only after a real `101` | A forged ambiguous request is refused at the edge; a worker that answers `200` to an upgrade gets no raw tunnel |
| Edge: host confusion | An app host with a platform path, or a main host with an app path | The edge classifies with `origins::classify`; the worker runs `route_by_host` again | The `subdomains.rs` cases run through an edge with the same results |
| Edge: exhaustion | Slow headers, many idle sockets, a UDP flood | Header read timeout, in-flight caps, per-IP limits and UDP rate at the edge | A slow-header client is cut off; one IP past its limit does not starve another |
| Edge: draining | New traffic to a draining worker | `draining` flag and bus event | A draining worker gets no new requests; its WebSockets close with `1012` and reconnect elsewhere |

Every step gets an adversarial pass before it is pushed, as agreed.

---

## 8. Work breakdown for step 1

Each piece is one PR on `main`, done in this order. Each leaves file mode
unchanged and the suite green. Estimates are for one agent, including tests
and the adversarial pass.

| # | PR | Scope | Tests | Estimate |
|---|---|---|---|---|
| 1 | State foundation | `state/` with `Backend`, `Stores` (empty), `wait`, `pg.rs` pool and ladder runner, `runners.rs`, the boot guards of 2.4, `DATABASE_URL` and `TOOLSITE_SECRET_KEY` handling, HKDF form key in Postgres mode, `scripts/test-postgres.sh`, compose profile | Ladder applies once with three runners booting together; boot refusals each have a test; log redaction | 1 day |
| 2 | `AccountStore`, SQLite | Move every SQL statement out of `users.rs` and `mfa.rs` into `accounts/store/sqlite.rs`; callers use the trait | Existing account and MFA tests unchanged and green; new conformance suite on SQLite | 2 days |
| 3 | `AccountStore`, Postgres | `accounts/store/postgres.rs`, `migrations/postgres/accounts/`, schema test | Conformance on Postgres; concurrent TOTP and recovery-code tests; `toolsite user` commands on Postgres | 1 day |
| 4 | `OAuthStore` | Trait, both impls, `migrations/postgres/oauth/` | Existing OAuth tests; concurrent `redeem_code` and `rotate_refresh` | 1 day |
| 5 | `Tickets` | `state/tickets.rs`; the six ticket maps leave `Config`; sealed payloads; inline-upload chunks through `Files` later (memory spool until PR 9) | Single use under concurrency; expiry; no plain ids or tokens at rest; handoff across two routers sharing one database | 1 day |
| 6 | `Catalog`, part 1 | `meta`, `update_meta`, `generation`, `notes`, `slugs`, `apps`; replace every `read_meta`/`write_meta` pair; `close_if_hidden` moves to the `AppEvents` sink | Full suite on both backends; a test that two concurrent `update_meta` calls both land | 2.5 days |
| 7 | `Catalog`, part 2 | Projects, relocation journal, labels, flags | Existing project-move tests (including the step-by-step ones) on both backends; two concurrent moves; two concurrent label assignments never share a label | 1.5 days |
| 8 | `AppRecords` and `Tokens` | Settings, tools, migrations ladder, repo links, installations; export, deploy and device tokens in one table | Existing tests for each sidecar on both backends; token for A refused on B; device `last_used` resolution | 1.5 days |
| 9 | Jobs | Job records through `AppRecords`; slots through `Leases`; start rate through `RateWindows`; `job_turns`; scheduler scans the table | `jobs_limits_batch.rs` on both backends; two schedulers on one database fire each turn once | 1.5 days |
| 10 | `Files` | Local and bucket impls; generation-keyed disk cache; handler cache keyed by `(app, generation)`; trash as records plus moved objects; inline-upload chunks in the bucket | Full suite on both backends with MinIO; traversal fixtures from `bundle.rs` against the bucket impl; deploy on router A is served by router B at once | 3 days |
| 11 | Migration commands | `migrate-to-postgres`, `--verify-only`, `export-to-files`, `key show`, the marker | Round trip on a fixture `DATA_DIR` built from the examples; interrupted run resumes; symlink skipped; wrong key refused | 1.5 days |
| 12 | Container trial and docs | Compose profile run end to end in the container; README section; Railway notes | Manual: migrate a copy of a real `DATA_DIR`, sign in, publish, run a job, export | 0.5 day |

About 18 agent-days. PRs 2 to 5 touch different files and could run in
parallel after PR 1 if needed; 6 to 10 should stay in order because each
touches call sites in `admin.rs`, `mcp.rs` and `upload.rs`.

After step 1 a site can run on Postgres with **one** runner (the volume still
holds SQLite app data). That is the intended first production use: move the
Herbrucks site's platform state, watch it, then go on to steps 2 and 3.

Step 1 also leaves the edge hooks of 6.4 (runner columns, `TransportFacts`,
`ports::listen` arguments). They belong in PR 1 and in PR 6's touch of
`websocket.rs`; they add about half a day.

### 8.1 Step 5: the edge, as its own later step

Depends on step 2 (bus, leases) and step 4 (role sets, route groups). Each
row is one PR, in order.

| # | PR | Scope | Tests | Estimate |
|---|---|---|---|---|
| E1 | Role sets and edge auth | `TOOLSITE_ROLE` comma list; internal listener; `x-toolsite-edge` signing and the verifying middleware; header stripping | The bypass, forged-facts and sidecar tests of section 7 | 1.5 days |
| E2 | Registry and health | Runner rows with roles, pool, address, draining; probes; edge caches with bus invalidation; `pool` in toolsite.toml | A runner that stops heartbeating leaves the edge's table in 15 s; a failed connect removes it at once | 1.5 days |
| E3 | HTTP proxy | `platform::edge` placement and proxy; retries only before the first byte; timeouts and in-flight caps; `control,edge` in-process dispatch | The full suite through an edge in front of two workers; smuggling and host-confusion tests; 503 when no worker is live | 2.5 days |
| E4 | WebSocket and resident routing | Upgrade proxy; `421` with holder and one retry | Misrouting and upgrade-smuggling tests; `tests/resident.rs` and `connections.rs` through an edge | 1.5 days |
| E5 | TCP | Ports bound by the edge; signed preamble; splice; per-IP limits at the edge | Preamble tests; `tcp-chat` and `mqtt-broker` examples through an edge | 1.5 days |
| E6 | UDP | Framed authenticated stream per edge, worker and port | `syslog` example through an edge; flood test | 2 days |
| E7 | Draining and container trial | SIGTERM handling on edge and worker; compose file with two edges, two workers, one control; Railway notes | Draining tests; the whole set tried in the container | 1.5 days |

About 12 agent-days.

---

## Questions for the owner

1. **SQLite apps on a multi-runner site.** Until step 3, app data is SQLite
   on one volume. Should existing SQLite apps (a) stay on a single "home"
   runner with a lease, other runners forwarding to it (the same mechanism as
   resident apps), or (b) be required to move to the `postgres` engine before
   a site adds runners?
2. **Apps database.** Is a second database on the same Postgres server
   acceptable for app schemas (it isolates NOTIFY, advisory locks and the
   platform catalog), and does the Railway Postgres let toolsite `create
   database` and `create role`? Is table-name visibility between apps
   acceptable, or is a database per app needed?
3. **Secret key.** Is it acceptable that a Postgres site refuses to start
   without `TOOLSITE_SECRET_KEY`, so the key is never in the database?
4. **Limits across runners.** Should socket limits (`per_app`, `per_person`,
   `total`) stay per runner, so the site total grows with the runner count,
   or be counted site-wide (a write per connect and disconnect)?
5. **Bucket required.** Is it acceptable that Postgres mode requires the
   bucket, and that the local blob backend is refused there?
6. **Edge trust.** Is a signed internal header enough between edge and
   workers on Railway's private network, or should mTLS be planned now?
7. **Ports at the edge.** With an edge, Railway's TCP proxy would point at
   the edge service and the edge binds `TOOLSITE_PORTS`. Is that the wanted
   shape for the MQTT and syslog ports? Per-IP limits then live at the edge,
   which also answers part of question 4.
