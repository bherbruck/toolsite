//! The sandbox published apps run their server-side code in.
//!
//! Three limits matter, and none of them are advisory:
//!
//! * **fuel** — a hard ceiling on executed instructions, so an infinite loop
//!   dies deterministically rather than pinning a core.
//! * **epochs** — wall-clock deadline, which catches guests that block without
//!   burning fuel. Each tick checks the deadline against the clock rather
//!   than counting ticks, which run slow on a loaded host. Epochs only
//!   interrupt wasm, so every host import that can wait (a wasi sleep, a
//!   query, a fetch) is cut off at the same deadline.
//! * **memory** — a cap enforced when the guest asks to grow, counting its
//!   tables as well as its linear memory.
//!
//! A store is built fresh per request. Reusing one would leak state between
//! requests, and app state belongs in that app's database instead. The one
//! exception is an app that asks to run resident: `Resident` is one store
//! kept for all of its connection events, under the same guards per call.

use crate::{config::Config as SiteConfig, runtime::db};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use wasmtime::{
    component::{Component, Linker, TypedFunc},
    Config, Engine, Module, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder, Trap,
    UpdateDeadline,
};
use wasmtime_wasi::{
    clocks::{WasiClocksCtxView, WasiClocksView},
    p2::{bindings::clocks::monotonic_clock, DynPollable},
    ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

// Generates the host side of wit/toolsite.wit: the `App` world's exported
// `handle`, and traits for every import we grant.
//
// `app-with-connections` is the same world plus an `on-connection` export,
// and `app-resident` adds `on-tick` to that. A component either has such an
// export or it does not, and the component model has no optional exports,
// so the host looks for each on the compiled component and calls it by
// name. Binding only `app` is what keeps every handler built before
// connections existed linking unchanged.
wasmtime::component::bindgen!({
    path: "wit",
    world: "app",
});

use self::toolsite::app::blobs::{
    Blob as WitBlob, Entry as WitEntry, Error as WitBlobError,
};
use self::toolsite::app::connections::Message as WitMessage;
pub use self::toolsite::app::connections::{ConnectInfo, Event as ConnectionEvent, Message as ConnectionMessage};
use self::toolsite::app::db::{Error as WitDbError, Rows as WitRows, Statement as WitStatement, Value as WitValue};
// Request and Response already land at module scope from bindgen; User sits
// under its interface, so re-export it rather than making callers spell out
// the generated path.
pub use self::toolsite::app::identity::User;

/// How often the epoch ticker advances. A deadline is noticed at the first
/// tick after it, so this trades timeout precision against wakeups.
const EPOCH_TICK: Duration = Duration::from_millis(50);

/// Compiled modules kept in memory. Eviction only costs a recompile, because
/// the `.wasm` itself stays on disk.
const MAX_CACHED_MODULES: usize = 32;

/// The export a component built for `app-with-connections` adds.
const ON_CONNECTION: &str = "on-connection";
/// The export a component built for `app-resident` adds.
const ON_TICK: &str = "on-tick";

/// What one call may use. An app's own come from `runtime::limits`, which
/// applies its `[limits]` under the site's ceilings; the default is what a
/// request gets when the app asks for nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guards {
    /// Instructions the guest may execute before it is killed. `None`
    /// meters nothing, leaving the wall clock as the only time limit.
    pub fuel: Option<u64>,
    /// Ceiling on the guest's linear memory.
    pub memory_bytes: usize,
    /// Wall-clock ceiling, enforced even if the guest never burns fuel.
    pub wall_clock: Duration,
    /// Rows one `query` or `query-scoped` may return before it says
    /// `truncated`.
    pub query_rows: usize,
}

impl Default for Guards {
    fn default() -> Self {
        use crate::runtime::limits::*;
        Self {
            fuel: Some(DEFAULT_REQUEST_FUEL),
            memory_bytes: (DEFAULT_REQUEST_MEMORY_MB * 1024 * 1024) as usize,
            wall_clock: Duration::from_secs(DEFAULT_REQUEST_SECONDS),
            query_rows: DEFAULT_QUERY_ROWS as usize,
        }
    }
}

/// What one table element is counted as against the memory cap: a pointer.
const TABLE_ELEMENT_BYTES: usize = 8;

/// The memory ceiling for one store, counted across every linear memory
/// and every table the guest has, and how much it holds now. A table is
/// host memory as much as a linear memory is, and with no element limit a
/// guest could `table.grow` its way to gigabytes the cap never saw.
struct Limits {
    /// Instances, tables and memories by count; their sizes are counted
    /// here instead.
    others: StoreLimits,
    memory_cap: usize,
    memory_used: usize,
    /// The bytes the last allowed growth added, taken back if it failed.
    last_growth: usize,
    /// A refused growth traps rather than returning -1 to the guest. A
    /// resident instance that cannot grow is better restarted than left to
    /// carry on after its allocator failed.
    trap_on_refusal: bool,
}

impl ResourceLimiter for Limits {
    fn memory_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        let after = self.memory_used.saturating_sub(current).saturating_add(desired);
        if after > self.memory_cap || maximum.is_some_and(|max| desired > max) {
            if self.trap_on_refusal {
                wasmtime::bail!("memory limit: the guest asked for {after} bytes, past its cap of {}", self.memory_cap);
            }
            return Ok(false);
        }
        self.last_growth = desired.saturating_sub(current);
        self.memory_used = after;
        Ok(true)
    }

    /// A growth the limit allowed but the system could not make.
    fn memory_grow_failed(&mut self, _error: wasmtime::Error) -> wasmtime::Result<()> {
        self.memory_used = self.memory_used.saturating_sub(self.last_growth);
        Ok(())
    }

    fn table_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        let added = desired.saturating_sub(current).saturating_mul(TABLE_ELEMENT_BYTES);
        let after = self.memory_used.saturating_add(added);
        if after > self.memory_cap || !self.others.table_growing(current, desired, maximum)? {
            return Ok(false);
        }
        self.last_growth = added;
        self.memory_used = after;
        Ok(true)
    }

    fn table_grow_failed(&mut self, _error: wasmtime::Error) -> wasmtime::Result<()> {
        self.memory_used = self.memory_used.saturating_sub(self.last_growth);
        Ok(())
    }
}

/// What every guest store carries. Capabilities get added here as they are
/// granted; anything absent is something the guest simply cannot do.
pub struct StoreState {
    limits: Limits,
    /// A guest compiled for wasm32-wasip2 imports wasi through Rust's std
    /// whether it uses it or not, so wasi has to be linked. What matters is
    /// that this context grants nothing: no preopened directory, no
    /// environment, no sockets, no inherited stdio. The capability list is
    /// the sandbox, not the presence of the interface.
    wasi: WasiCtx,
    table: ResourceTable,
    /// Which app is running. The guest never supplies this, which is what
    /// keeps one app's SQL off another app's database.
    app: String,
    site: Arc<SiteConfig>,
    /// Established by the host from a verified session, never from anything
    /// the guest or its client claimed.
    user: Option<User>,
    /// The connection whose `connect` event this sandbox runs, if any. What
    /// it sends goes straight to that connection; what anyone else sends
    /// there waits until `connect` returns.
    connecting: Option<String>,
    /// When the current call's wall clock runs out. Epochs stop wasm at this
    /// point; host imports that wait read it to stop there too.
    deadline: Instant,
    /// Rows one query may return, from the call's guards.
    query_rows: usize,
    /// The current call's connection to its database, opened at its first
    /// query or batch: opening one, and looking up the caller's role to
    /// bind into it, cost a millisecond, which per statement was most of a
    /// query's time. The caller's identity and the call's deadline are
    /// bound in, so it lives exactly as long as the call and `arm` drops it
    /// before the next, which in a resident instance may be someone else's.
    db: Option<rusqlite::Connection>,
    /// The same for `query-scoped` and `batch-scoped`, behind the app's
    /// declared access as it stood when the call first used it, with that
    /// scope kept so a batch puts the same authorizer back.
    scoped: Option<(rusqlite::Connection, db::Scope)>,
    /// Files this call is writing in pieces. Dropped with the store, which
    /// abandons any not finished; a resident instance empties it after
    /// each call.
    writers: crate::runtime::blobs::Writers,
}

impl StoreState {
    /// What is left of the current call's wall clock.
    fn time_left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// wasi's monotonic clock, with every sleep cut off at the call's deadline.
/// A sleep runs on the host, where epochs cannot reach it: without this a
/// guest calling `std::thread::sleep` for an hour holds its thread for an
/// hour. Cut short, the guest wakes at the deadline, and the epoch, which
/// ticked while it slept, stops it at the first instruction it runs.
struct DeadlineClock;

impl wasmtime::component::HasData for DeadlineClock {
    type Data<'a> = DeadlineClockView<'a>;
}

struct DeadlineClockView<'a> {
    clocks: WasiClocksCtxView<'a>,
    left: Duration,
}

impl monotonic_clock::Host for DeadlineClockView<'_> {
    fn now(&mut self) -> wasmtime::Result<monotonic_clock::Instant> {
        self.clocks.now()
    }

    fn resolution(&mut self) -> wasmtime::Result<monotonic_clock::Duration> {
        self.clocks.resolution()
    }

    fn subscribe_duration(
        &mut self,
        duration: monotonic_clock::Duration,
    ) -> wasmtime::Result<wasmtime::component::Resource<DynPollable>> {
        let left = self.left.as_nanos().min(u64::MAX as u128) as u64;
        self.clocks.subscribe_duration(duration.min(left))
    }

    fn subscribe_instant(
        &mut self,
        when: monotonic_clock::Instant,
    ) -> wasmtime::Result<wasmtime::component::Resource<DynPollable>> {
        let left = self.left.as_nanos().min(u64::MAX as u128) as u64;
        let latest = self.clocks.now()?.saturating_add(left);
        self.clocks.subscribe_instant(when.min(latest))
    }
}

impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl StoreState {
    /// Who the SQL runs as: the visitor the host established, with their
    /// grant on this app. Looked up per call so a grant changed mid-session
    /// is seen by the next call.
    fn identity(&self) -> Option<db::Identity> {
        let user = self.user.as_ref()?;
        Some(db::Identity {
            user_id: user.id.clone(),
            email: user.email.clone(),
            role: crate::accounts::users::role_for(&self.site, &user.id, &self.app),
        })
    }
}

fn wit_rows(outcome: Result<db::SqlOutcome, String>) -> Result<WitRows, WitDbError> {
    match outcome {
        Ok(outcome) => Ok(WitRows {
            columns: outcome.columns,
            values: outcome
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(wit_of).collect())
                .collect(),
            truncated: outcome.truncated,
            rows_affected: outcome.rows_affected as u64,
        }),
        // The authorizer's refusals are reported as their own case so a
        // guest can tell "you may not" from "that query was wrong".
        Err(message) if message.contains("not authorized") => Err(WitDbError::Denied(message)),
        Err(message) => Err(WitDbError::Failed(message)),
    }
}

impl self::toolsite::app::db::Host for StoreState {
    fn query(
        &mut self,
        sql: String,
        params: Vec<WitValue>,
    ) -> Result<WitRows, WitDbError> {
        let params: Vec<serde_json::Value> = params.into_iter().map(json_of).collect();
        let max_rows = self.query_rows;
        match self.kept() {
            Ok(conn) => wit_rows(db::execute_on(conn, &sql, &params, max_rows)),
            Err(error) => wit_rows(Err(error)),
        }
    }

    fn query_scoped(
        &mut self,
        sql: String,
        params: Vec<WitValue>,
    ) -> Result<WitRows, WitDbError> {
        let params: Vec<serde_json::Value> = params.into_iter().map(json_of).collect();
        let (max_rows, deadline) = (self.query_rows, self.deadline);
        match self.kept_scoped() {
            Ok((conn, _)) => wit_rows(db::execute_scoped(conn, &sql, &params, Some(deadline), max_rows)),
            Err(error) => wit_rows(Err(error)),
        }
    }

    fn batch(&mut self, statements: Vec<WitStatement>) -> Result<Vec<u64>, WitDbError> {
        let statements = batch_of(statements);
        let conn = match self.kept() {
            Ok(conn) => conn,
            Err(error) => return wit_counts(Err(error)),
        };
        let began = !conn.is_autocommit();
        let outcome = db::batch_on(conn, &statements);
        // A failed batch has rolled back; the connection goes with it, so
        // nothing it might have left behind, such as an authorizer not put
        // back, reaches the call's next statement. One the call holds a
        // transaction on was refused untouched and stays.
        if outcome.is_err() && !began {
            self.db = None;
        }
        wit_counts(outcome)
    }

    fn batch_scoped(&mut self, statements: Vec<WitStatement>) -> Result<Vec<u64>, WitDbError> {
        let statements = batch_of(statements);
        let deadline = self.deadline;
        let outcome = match self.kept_scoped() {
            Ok((conn, scope)) => db::batch_scoped_on(conn, scope, &statements, Some(deadline)),
            Err(error) => return wit_counts(Err(error)),
        };
        // The scope refuses `begin`, so the call never holds a transaction
        // here: a failed batch takes the connection with it, as in `batch`.
        if outcome.is_err() {
            self.scoped = None;
        }
        wit_counts(outcome)
    }
}

impl StoreState {
    /// The call's connection for `query` and `batch`, opened at the first.
    fn kept(&mut self) -> Result<&rusqlite::Connection, String> {
        if self.db.is_none() {
            let identity = self.identity();
            self.db = Some(db::open_until(&self.site, &self.app, identity.as_ref(), Some(self.deadline))?);
        }
        let conn = self.db.as_ref().expect("opened above");
        db::busy_until(conn, self.deadline)?;
        Ok(conn)
    }

    /// The call's connection for `query-scoped` and `batch-scoped`, and the
    /// scope it is behind.
    fn kept_scoped(&mut self) -> Result<(&rusqlite::Connection, &db::Scope), String> {
        if self.scoped.is_none() {
            let identity = self.identity();
            // Read per call rather than cached, like allow_http: a policy
            // added by a manifest upload applies to the next request.
            let meta = crate::content::catalog::meta_blocking(&self.site, &self.app);
            let scope = db::Scope::of(&meta);
            let conn = db::open_scoped(&self.site, &self.app, identity.as_ref(), &scope)?;
            self.scoped = Some((conn, scope));
        }
        let (conn, scope) = self.scoped.as_ref().expect("opened above");
        db::busy_until(conn, self.deadline)?;
        Ok((conn, scope))
    }
}
fn batch_of(statements: Vec<WitStatement>) -> Vec<db::Statement> {
    statements
        .into_iter()
        .map(|statement| db::Statement {
            sql: statement.sql,
            params: statement.params.into_iter().map(json_of).collect(),
        })
        .collect()
}

fn wit_counts(outcome: Result<Vec<u64>, String>) -> Result<Vec<u64>, WitDbError> {
    match outcome {
        Ok(counts) => Ok(counts),
        Err(message) if message.contains("not authorized") => Err(WitDbError::Denied(message)),
        Err(message) => Err(WitDbError::Failed(message)),
    }
}

/// Starts one of this app's own jobs: `self.app` comes from the host, so a
/// name can only ever mean a job this app declared.
impl self::toolsite::app::jobs::Host for StoreState {
    fn run(&mut self, name: String) -> Result<String, String> {
        crate::platform::schedule::start_from_app(&self.site, &self.app, &name)
    }
}

fn wit_blob_error(error: crate::runtime::blobs::Error) -> WitBlobError {
    use crate::runtime::blobs::Error;
    match error {
        Error::NotFound => WitBlobError::NotFound,
        Error::InvalidKey(why) => WitBlobError::InvalidKey(why),
        Error::TooLarge(size) => WitBlobError::TooLarge(size),
        Error::Failed(why) => WitBlobError::Failed(why),
    }
}

fn wit_entry(entry: crate::runtime::blobs::Entry) -> WitEntry {
    WitEntry {
        key: entry.key,
        size: entry.size,
        content_type: entry.content_type,
    }
}

/// Every call is scoped by `self.app`, which the guest never supplied — the
/// same arrangement that keeps its SQL on its own database.
impl self::toolsite::app::blobs::Host for StoreState {
    fn put(&mut self, key: String, content_type: String, body: Vec<u8>) -> Result<(), WitBlobError> {
        crate::runtime::blobs::put(&self.site, &self.app, &key, &content_type, &body)
            .map_err(wit_blob_error)
    }

    fn get(&mut self, key: String) -> Result<WitBlob, WitBlobError> {
        crate::runtime::blobs::get(&self.site, &self.app, &key)
            .map(|blob| WitBlob {
                content_type: blob.content_type,
                body: blob.body,
            })
            .map_err(wit_blob_error)
    }

    fn stat(&mut self, key: String) -> Result<Option<WitEntry>, WitBlobError> {
        crate::runtime::blobs::stat(&self.site, &self.app, &key)
            .map(|entry| entry.map(wit_entry))
            .map_err(wit_blob_error)
    }

    fn delete(&mut self, key: String) -> Result<(), WitBlobError> {
        crate::runtime::blobs::delete(&self.site, &self.app, &key).map_err(wit_blob_error)
    }

    fn list(&mut self, prefix: String) -> Result<Vec<WitEntry>, WitBlobError> {
        crate::runtime::blobs::list(&self.site, &self.app, &prefix)
            .map(|entries| entries.into_iter().map(wit_entry).collect())
            .map_err(wit_blob_error)
    }

    fn upload_url(&mut self, key: String, max_bytes: u64) -> Result<String, WitBlobError> {
        // A host call runs on a blocking thread, where a store call waits.
        crate::state::wait(crate::runtime::blobs::issue_upload(&self.site, &self.app, &key, max_bytes))
            .map_err(wit_blob_error)
    }

    fn writer_open(&mut self, key: String, content_type: String) -> Result<u64, WitBlobError> {
        self.writers.open(&self.site, &self.app, &key, &content_type).map_err(wit_blob_error)
    }

    fn writer_append(&mut self, handle: u64, bytes: Vec<u8>) -> Result<(), WitBlobError> {
        self.writers.append(handle, &bytes).map_err(wit_blob_error)
    }

    fn writer_finish(&mut self, handle: u64) -> Result<WitEntry, WitBlobError> {
        self.writers.finish(handle).map(wit_entry).map_err(wit_blob_error)
    }

    fn writer_abort(&mut self, handle: u64) {
        self.writers.abort(handle);
    }
}

/// No functions, only shared records — but the world's `use` still requires
/// the trait to be present.
impl self::toolsite::app::http::Host for StoreState {}

impl self::toolsite::app::fetch::Host for StoreState {
    fn send(
        &mut self,
        req: self::toolsite::app::fetch::Request,
    ) -> Result<self::toolsite::app::fetch::Response, String> {
        // Read per call rather than cached: changing an app's allowlist takes
        // effect on the next request, not the next restart.
        let allow = crate::content::catalog::meta_blocking(&self.site, &self.app).allow_http;
        // A slow host must not hold the call past its wall clock.
        let left = self.time_left();
        if left.is_zero() {
            return Err("the call ran out of time before the request could be sent".to_string());
        }

        crate::runtime::outbound::send(&req.method, &req.url, &req.headers, req.body, &allow, left).map(
            |fetched| self::toolsite::app::fetch::Response {
                status: fetched.status,
                headers: fetched.headers,
                body: fetched.body,
            },
        )
    }
}

fn hub_message(message: WitMessage) -> crate::runtime::connections::Message {
    match message {
        WitMessage::Text(text) => crate::runtime::connections::Message::Text(text),
        WitMessage::Binary(bytes) => crate::runtime::connections::Message::Binary(bytes),
    }
}

/// Acts on the running app's connections only: `self.app` comes from the
/// host, so an id from another app names nothing here.
impl self::toolsite::app::connections::Host for StoreState {
    fn send(&mut self, conn: String, message: WitMessage) -> Result<(), String> {
        self.site.connections.send(&self.app, &conn, hub_message(message), self.connecting.as_deref())
    }

    fn close(&mut self, conn: String) -> Result<(), String> {
        self.site.connections.close(&self.app, &conn, self.connecting.as_deref())
    }

    fn subscribe(&mut self, conn: String, topic: String) -> Result<(), String> {
        self.site.connections.subscribe(&self.app, &conn, &topic)
    }

    fn unsubscribe(&mut self, conn: String, topic: String) -> Result<(), String> {
        self.site.connections.unsubscribe(&self.app, &conn, &topic)
    }

    fn publish(&mut self, topic: String, message: WitMessage) -> Result<u32, String> {
        self.site.connections.publish(&self.app, &topic, hub_message(message), self.connecting.as_deref())
    }

    fn state_get(&mut self, conn: String, key: String) -> Option<String> {
        self.site.connections.state_get(&self.app, &conn, &key)
    }

    fn state_set(&mut self, conn: String, key: String, value: Option<String>) -> Result<(), String> {
        self.site.connections.state_set(&self.app, &conn, &key, value)
    }

    fn remote(&mut self, conn: String) -> Option<String> {
        self.site.connections.remote(&self.app, &conn)
    }
}

/// Checks a token against this app's device tokens only: `self.app` comes
/// from the host, so another app's token is no token here.
impl self::toolsite::app::auth::Host for StoreState {
    fn check_token(&mut self, token: String) -> Option<String> {
        crate::platform::devices::check(&self.site, &self.app, &token)
    }
}

impl self::toolsite::app::secrets::Host for StoreState {
    fn get(&mut self, name: String) -> Option<String> {
        crate::platform::secrets::get(&self.site, &self.app, &name)
    }

    fn names(&mut self) -> Vec<String> {
        crate::platform::secrets::names(&self.site, &self.app)
    }
}

impl self::toolsite::app::identity::Host for StoreState {
    fn current_user(&mut self) -> Option<User> {
        self.user.clone()
    }

    fn current_role(&mut self) -> Option<String> {
        let user = self.user.as_ref()?;
        crate::accounts::users::role_for(&self.site, &user.id, &self.app)
    }
}

fn json_of(value: WitValue) -> serde_json::Value {
    match value {
        WitValue::Null => serde_json::Value::Null,
        WitValue::Integer(i) => serde_json::json!(i),
        WitValue::Real(f) => serde_json::json!(f),
        WitValue::Text(s) => serde_json::Value::String(s),
    }
}

fn wit_of(value: serde_json::Value) -> WitValue {
    match value {
        serde_json::Value::Null => WitValue::Null,
        serde_json::Value::Bool(b) => WitValue::Integer(b as i64),
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => WitValue::Integer(i),
            (None, Some(f)) => WitValue::Real(f),
            _ => WitValue::Null,
        },
        serde_json::Value::String(s) => WitValue::Text(s),
        other => WitValue::Text(other.to_string()),
    }
}

pub struct Runtime {
    engine: Engine,
    modules: Mutex<HashMap<String, (Module, Instant)>>,
    /// Linking a component is the expensive part after compilation, so the
    /// pre-instantiated form is what gets cached and reused.
    handlers: Mutex<HashMap<String, (AppPre<StoreState>, Instant)>>,
    linker: Linker<StoreState>,
}

impl Runtime {
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.wasm_component_model(true);

        let engine = Engine::new(&config)?;

        // Wasmtime only notices a deadline when something advances the epoch,
        // so a ticker has to exist for wall-clock limits to mean anything.
        let ticker = engine.weak();
        std::thread::Builder::new()
            .name("wasm-epoch".into())
            .spawn(move || {
                while let Some(engine) = ticker.upgrade() {
                    std::thread::sleep(EPOCH_TICK);
                    engine.increment_epoch();
                }
            })?;

        // Only what this adds is reachable from a guest. There is no wasi
        // filesystem, socket or environment import here, and that omission is
        // the sandbox — not something checked at call time.
        let mut linker: Linker<StoreState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        App::add_to_linker::<_, wasmtime::component::HasSelf<StoreState>>(&mut linker, |state| {
            state
        })?;
        // The monotonic clock again, over the one wasi linked, so that no
        // sleep outlasts the call.
        linker.allow_shadowing(true);
        monotonic_clock::add_to_linker::<StoreState, DeadlineClock>(&mut linker, |state| {
            let left = state.time_left();
            DeadlineClockView {
                clocks: state.clocks(),
                left,
            }
        })?;
        linker.allow_shadowing(false);

        Ok(Arc::new(Self {
            engine,
            modules: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            linker,
        }))
    }

    /// Checks that bytes really are a component satisfying our world, so a
    /// broken handler is rejected at upload rather than on a visitor's first
    /// request. Says whether it exports `on-connection`.
    pub fn validate(&self, wasm: &[u8]) -> anyhow::Result<bool> {
        let component = Component::new(&self.engine, wasm)?;
        AppPre::new(self.linker.instantiate_pre(&component)?)?;
        Ok(component.get_export_index(None, ON_CONNECTION).is_some())
    }

    /// Forgets an app's compiled handler, so the next request picks up what
    /// was just uploaded. Without this the cache is keyed by app name and a
    /// redeploy keeps serving the previous component until eviction — code
    /// that is on disk but not running is a hard thing to debug.
    pub fn forget(&self, app: &str) {
        self.handlers.lock().unwrap().remove(app);
    }

    /// Compiles and links an app's handler, reusing the result while cached.
    fn handler(&self, key: &str, wasm: &[u8]) -> anyhow::Result<AppPre<StoreState>> {
        if let Some((handler, last_used)) = self.handlers.lock().unwrap().get_mut(key) {
            *last_used = Instant::now();
            return Ok(handler.clone());
        }

        let component = Component::new(&self.engine, wasm)?;
        let handler = AppPre::new(self.linker.instantiate_pre(&component)?)?;

        let mut handlers = self.handlers.lock().unwrap();
        if handlers.len() >= MAX_CACHED_MODULES {
            if let Some(coldest) = handlers
                .iter()
                .min_by_key(|(_, (_, last_used))| *last_used)
                .map(|(key, _)| key.clone())
            {
                handlers.remove(&coldest);
            }
        }
        handlers.insert(key.to_string(), (handler.clone(), Instant::now()));
        Ok(handler)
    }

    /// Runs one request through an app's handler. Blocking and CPU-bound, so
    /// callers on an async runtime must hand this to a blocking task.
    pub fn handle(
        &self,
        site: Arc<SiteConfig>,
        app: &str,
        wasm: &[u8],
        user: Option<User>,
        request: Request,
        guards: Guards,
    ) -> anyhow::Result<Response> {
        let handler = self.handler(app, wasm)?;
        let mut store = self.store(site, app, user, guards);
        let instance = handler.instantiate(&mut store)?;
        Ok(instance.call_handle(&mut store, &request)?)
    }

    /// Whether an app's handler exports `on-connection`, that is, was built
    /// for `app-with-connections`.
    pub fn takes_connections(&self, app: &str, wasm: &[u8]) -> anyhow::Result<bool> {
        let handler = self.handler(app, wasm)?;
        Ok(handler.instance_pre().component().get_export_index(None, ON_CONNECTION).is_some())
    }

    /// Runs one connection event through an app's handler. `Ok(None)` means
    /// the handler takes no connections; otherwise the handler's answer,
    /// which for `connect` decides whether the browser is let in. Blocking,
    /// like `handle`.
    #[allow(clippy::too_many_arguments)]
    pub fn connection_event(
        &self,
        site: Arc<SiteConfig>,
        app: &str,
        wasm: &[u8],
        user: Option<User>,
        conn: &str,
        event: ConnectionEvent,
        guards: Guards,
    ) -> anyhow::Result<Option<Result<(), String>>> {
        let handler = self.handler(app, wasm)?;
        let Some(export) = handler.instance_pre().component().get_export_index(None, ON_CONNECTION) else {
            return Ok(None);
        };
        let mut store = self.store(site, app, user, guards);
        if matches!(event, ConnectionEvent::Connect(_)) {
            store.data_mut().connecting = Some(conn.to_string());
        }
        let instance = handler.instance_pre().instantiate(&mut store)?;
        let call = instance
            .get_typed_func::<(String, ConnectionEvent), (Result<(), String>,)>(&mut store, &export)?;
        let (answer,) = call.call(&mut store, (conn.to_string(), event))?;
        Ok(Some(answer))
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compiles `wasm`, reusing the result while it stays in the cache.
    pub fn module(&self, key: &str, wasm: &[u8]) -> anyhow::Result<Module> {
        if let Some((module, last_used)) = self.modules.lock().unwrap().get_mut(key) {
            *last_used = Instant::now();
            return Ok(module.clone());
        }

        let module = Module::new(&self.engine, wasm)?;

        let mut modules = self.modules.lock().unwrap();
        if modules.len() >= MAX_CACHED_MODULES {
            if let Some(coldest) = modules
                .iter()
                .min_by_key(|(_, (_, last_used))| *last_used)
                .map(|(key, _)| key.clone())
            {
                modules.remove(&coldest);
            }
        }
        modules.insert(key.to_string(), (module.clone(), Instant::now()));
        Ok(module)
    }

    /// A fresh store with every guard applied. One per request, never reused.
    pub fn store(
        &self,
        site: Arc<SiteConfig>,
        app: &str,
        user: Option<User>,
        guards: Guards,
    ) -> Store<StoreState> {
        self.store_with(site, app, user, guards, false)
    }

    fn store_with(
        &self,
        site: Arc<SiteConfig>,
        app: &str,
        user: Option<User>,
        guards: Guards,
        trap_on_refusal: bool,
    ) -> Store<StoreState> {
        let state = StoreState {
            limits: Limits {
                others: StoreLimitsBuilder::new().build(),
                memory_cap: guards.memory_bytes,
                memory_used: 0,
                last_growth: 0,
                trap_on_refusal,
            },
            // Deliberately empty: no dirs, no env, no network, no stdio.
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            app: app.to_string(),
            site,
            user,
            connecting: None,
            deadline: Instant::now(),
            query_rows: guards.query_rows,
            db: None,
            scoped: None,
            writers: Default::default(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        // Every tick asks the clock. Counting ticks instead would let a call
        // run past its deadline by however far the ticker fell behind.
        store.epoch_deadline_callback(|store| {
            if Instant::now() >= store.data().deadline {
                Err(Trap::Interrupt.into())
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });
        arm(&mut store, guards);
        store
    }

    /// One long-lived instance of an app's handler, for an app that runs
    /// resident. Fails if the handler does not export `on-connection`.
    /// `memory_bytes` is its ceiling for the whole of its life; fuel and
    /// time are `Guards`, given again for each call.
    pub fn resident(
        &self,
        site: Arc<SiteConfig>,
        app: &str,
        wasm: &[u8],
        memory_bytes: usize,
        guards: Guards,
    ) -> anyhow::Result<Resident> {
        let handler = self.handler(app, wasm)?;
        let component = handler.instance_pre().component();
        let Some(on_connection) = component.get_export_index(None, ON_CONNECTION) else {
            anyhow::bail!("the handler does not export on-connection, which an app that runs resident needs");
        };
        let on_tick = component.get_export_index(None, ON_TICK);
        let guards = Guards { memory_bytes, ..guards };
        let mut store = self.store_with(site, app, None, guards, true);
        let instance = handler.instance_pre().instantiate(&mut store)?;
        let on_connection = instance
            .get_typed_func::<(String, ConnectionEvent), (Result<(), String>,)>(&mut store, &on_connection)?;
        let on_tick = match on_tick {
            Some(export) => Some(instance.get_typed_func::<(u64,), ()>(&mut store, &export)?),
            None => None,
        };
        Ok(Resident {
            store,
            on_connection,
            on_tick,
        })
    }
}

/// Fuel and a wall-clock deadline for the next call on `store`, counted
/// from now, with nothing left from the last: no database connection, no
/// open writer.
fn arm(store: &mut Store<StoreState>, guards: Guards) {
    end_call(store);
    // A wall clock no Instant can hold, from a ceiling set absurdly high,
    // is as good as none rather than a panic on every call.
    let now = Instant::now();
    store.data_mut().deadline = now.checked_add(guards.wall_clock).unwrap_or(now + Duration::from_secs(365 * 24 * 3600));
    store.data_mut().query_rows = guards.query_rows;
    store.set_fuel(guards.fuel.unwrap_or(u64::MAX)).expect("fuel is enabled");
    store.set_epoch_deadline(1);
}

/// Drops the connections a call opened, rolling back whatever transaction
/// it left open, and abandons the files it began writing and never
/// finished. Every call ends here or in its store's drop, so no statement
/// ever runs on a connection opened for another call, and no handle
/// reaches one either.
fn end_call(store: &mut Store<StoreState>) {
    let state = store.data_mut();
    state.db = None;
    state.scoped = None;
    state.writers.abort_all();
}

/// An app's handler kept alive between events: its memory, and so whatever
/// it keeps in statics, lasts from one call to the next. It has exactly the
/// imports a fresh instance has. After any error from a call the instance
/// is in an unknown state and must be dropped.
pub struct Resident {
    store: Store<StoreState>,
    on_connection: TypedFunc<(String, ConnectionEvent), (Result<(), String>,)>,
    on_tick: Option<TypedFunc<(u64,), ()>>,
}

impl Resident {
    /// Runs one connection event, as `user`, under fresh fuel and time.
    pub fn connection_event(
        &mut self,
        user: Option<User>,
        conn: &str,
        event: ConnectionEvent,
        guards: Guards,
    ) -> anyhow::Result<Result<(), String>> {
        let state = self.store.data_mut();
        state.user = user;
        state.connecting = matches!(event, ConnectionEvent::Connect(_)).then(|| conn.to_string());
        arm(&mut self.store, guards);
        let outcome = self.on_connection.call(&mut self.store, (conn.to_string(), event));
        // Not left open until the next event: the write lock of a
        // transaction it began would be held between calls, and a file it
        // never finished would wait to be finished by someone else's.
        end_call(&mut self.store);
        let (answer,) = outcome?;
        Ok(answer)
    }

    /// Whether the handler exports `on-tick`.
    pub fn ticks(&self) -> bool {
        self.on_tick.is_some()
    }

    /// Calls `on-tick`, as nobody, under fresh fuel and time. Does nothing
    /// for a handler without it.
    pub fn tick(&mut self, now_ms: u64, guards: Guards) -> anyhow::Result<()> {
        let Some(on_tick) = self.on_tick else {
            return Ok(());
        };
        let state = self.store.data_mut();
        state.user = None;
        state.connecting = None;
        arm(&mut self.store, guards);
        let outcome = on_tick.call(&mut self.store, (now_ms,));
        end_call(&mut self.store);
        Ok(outcome?)
    }

    /// Bytes of memory the instance holds now, linear memory and tables.
    pub fn memory_bytes(&self) -> usize {
        self.store.data().limits.memory_used
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::Instance;

    fn runtime() -> Arc<Runtime> {
        Runtime::new().unwrap()
    }

    fn test_site() -> Arc<SiteConfig> {
        Arc::new(SiteConfig::local(
            std::env::temp_dir().join("toolsite-wasm-tests"),
            "test-token",
        ))
    }

    /// Runs `wat`'s exported `run` function under `guards`.
    fn run(runtime: &Runtime, wat: &str, guards: Guards) -> anyhow::Result<i32> {
        let wasm = wat::parse_str(wat)?;
        let module = runtime.module(wat, &wasm)?;
        let mut store = runtime.store(test_site(), "test", None, guards);
        let instance = Instance::new(&mut store, &module, &[])?;
        let run = instance.get_typed_func::<(), i32>(&mut store, "run")?;
        Ok(run.call(&mut store, ())?)
    }

    const ADDER: &str = r#"
        (module (func (export "run") (result i32)
          i32.const 2 i32.const 40 i32.add))
    "#;

    const INFINITE_LOOP: &str = r#"
        (module (func (export "run") (result i32)
          (loop $forever (br $forever))
          i32.const 0))
    "#;

    const MEMORY_HOG: &str = r#"
        (module
          (memory 1)
          (func (export "run") (result i32)
            (local $grown i32)
            (loop $again
              (local.set $grown (memory.grow (i32.const 16)))
              (br_if $again (i32.ne (local.get $grown) (i32.const -1))))
            (local.get $grown)))
    "#;

    #[test]
    fn ordinary_code_runs_and_returns() {
        let runtime = runtime();
        assert_eq!(run(&runtime, ADDER, Guards::default()).unwrap(), 42);
    }

    #[test]
    fn an_infinite_loop_dies_on_fuel_rather_than_running_forever() {
        let runtime = runtime();
        let guards = Guards {
            fuel: Some(100_000),
            ..Guards::default()
        };
        let error = run(&runtime, INFINITE_LOOP, guards).unwrap_err();
        assert_eq!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(&wasmtime::Trap::OutOfFuel),
            "expected fuel exhaustion, got {error:?}"
        );
    }

    #[test]
    fn a_wall_clock_deadline_applies_even_with_fuel_to_spare() {
        let runtime = runtime();
        let guards = Guards {
            fuel: None,
            wall_clock: Duration::from_millis(100),
            ..Guards::default()
        };
        let started = Instant::now();
        let error = run(&runtime, INFINITE_LOOP, guards).unwrap_err();
        // A blown epoch deadline surfaces as an interrupt trap. Match on the
        // trap itself rather than the message, which is not a stable contract.
        assert_eq!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(&wasmtime::Trap::Interrupt),
            "expected an epoch deadline, got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "deadline did not fire promptly"
        );
    }

    #[test]
    fn a_call_stops_at_its_deadline_however_few_ticks_have_passed() {
        // A ticker that fell behind under load, as far as the store can
        // tell: its deadline is here while barely a tick has gone by. Were
        // the deadline a count of ticks, this would run for the full five
        // seconds.
        let runtime = runtime();
        let wasm = wat::parse_str(INFINITE_LOOP).unwrap();
        let module = runtime.module(INFINITE_LOOP, &wasm).unwrap();
        let guards = Guards {
            fuel: None,
            wall_clock: Duration::from_secs(5),
            ..Guards::default()
        };
        let mut store = runtime.store(test_site(), "test", None, guards);
        store.data_mut().deadline = Instant::now();
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let run = instance.get_typed_func::<(), i32>(&mut store, "run").unwrap();
        let started = Instant::now();
        let error = run.call(&mut store, ()).unwrap_err();
        assert_eq!(error.downcast_ref::<wasmtime::Trap>(), Some(&wasmtime::Trap::Interrupt), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(1), "ran on for {:?}", started.elapsed());
    }

    #[test]
    fn memory_growth_stops_at_the_cap() {
        let runtime = runtime();
        let guards = Guards {
            memory_bytes: 2 * 1024 * 1024,
            ..Guards::default()
        };
        // memory.grow reports -1 when refused, so the guest sees a failure
        // instead of the host being asked for unbounded memory.
        assert_eq!(run(&runtime, MEMORY_HOG, guards).unwrap(), -1);
    }

    /// Grows a table 4096 elements at a time until refused, then answers
    /// its size.
    const TABLE_HOG: &str = r#"
        (module
          (table 1 funcref)
          (func (export "run") (result i32)
            (loop $again
              (br_if $again (i32.ne (table.grow (ref.null func) (i32.const 4096)) (i32.const -1))))
            (table.size)))
    "#;

    #[test]
    fn a_table_cannot_grow_past_the_memory_cap() {
        let runtime = runtime();
        let cap = 2 * 1024 * 1024;
        let guards = Guards {
            memory_bytes: cap,
            ..Guards::default()
        };
        // Without a count of its elements a table is host memory the cap
        // never sees: 4 billion of them is 32 GB.
        let elements = run(&runtime, TABLE_HOG, guards).unwrap() as usize;
        assert!(elements > 1, "the table could not grow at all");
        assert!(elements * TABLE_ELEMENT_BYTES <= cap, "the table grew to {elements} elements under a {cap} byte cap");
    }

    #[test]
    fn each_request_gets_a_store_with_its_own_fuel() {
        let runtime = runtime();
        let guards = Guards {
            fuel: Some(100_000),
            ..Guards::default()
        };
        // A store that ran to exhaustion must not affect the next one.
        let _ = run(&runtime, INFINITE_LOOP, guards);
        assert_eq!(run(&runtime, ADDER, guards).unwrap(), 42);
    }

    #[test]
    fn uploading_a_handler_forgets_the_one_that_was_running() {
        // The cache is keyed by app name, so without this a redeploy leaves
        // the previous component serving: the new code sits on disk and never
        // runs, which reads from outside as "uploads land but don't activate".
        let runtime = runtime();
        let component = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/handler.wasm"
        ))
        .unwrap();

        let mut store = runtime.store(test_site(), "app", None, Guards::default());
        let handler = runtime.handler("app", &component).unwrap();
        let instance = handler.instantiate(&mut store).unwrap();
        let _ = instance;
        assert_eq!(runtime.handlers.lock().unwrap().len(), 1);

        runtime.forget("app");
        assert!(
            runtime.handlers.lock().unwrap().is_empty(),
            "the previous component survived an upload"
        );
    }

    // --- one connection per call --------------------------------------------

    fn person(id: &str) -> User {
        User { id: id.into(), email: format!("{id}@example.com") }
    }

    fn sql(store: &mut Store<StoreState>, sql: &str) -> Result<WitRows, WitDbError> {
        super::toolsite::app::db::Host::query(store.data_mut(), sql.to_string(), Vec::new())
    }

    fn text(rows: &WitRows) -> String {
        match &rows.values[0][0] {
            WitValue::Text(t) => t.clone(),
            WitValue::Integer(i) => i.to_string(),
            WitValue::Null => "null".into(),
            WitValue::Real(f) => f.to_string(),
        }
    }

    fn site_in(dir: &tempfile::TempDir) -> Arc<SiteConfig> {
        Arc::new(SiteConfig::local(dir.path().to_path_buf(), "test-token"))
    }

    #[test]
    fn a_connection_kept_for_a_call_never_answers_for_another_persons_call() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut alice = runtime.store(site_in(&dir), "app", Some(person("u-alice")), Guards::default());
        let mut bob = runtime.store(site_in(&dir), "app", Some(person("u-bob")), Guards::default());
        // Two people's calls on one app, statement by statement in turn.
        for _ in 0..20 {
            assert_eq!(text(&sql(&mut alice, "select current_user()").unwrap()), "u-alice");
            assert_eq!(text(&sql(&mut bob, "select current_user()").unwrap()), "u-bob");
        }

        // One store serving two people's calls, as a resident instance does:
        // the connection opened for the first is gone by the second.
        let mut resident = runtime.store(site_in(&dir), "app", Some(person("u-alice")), Guards::default());
        assert_eq!(text(&sql(&mut resident, "select current_email()").unwrap()), "u-alice@example.com");
        resident.data_mut().user = Some(person("u-bob"));
        arm(&mut resident, Guards::default());
        assert_eq!(text(&sql(&mut resident, "select current_email()").unwrap()), "u-bob@example.com");
        resident.data_mut().user = None;
        arm(&mut resident, Guards::default());
        assert_eq!(text(&sql(&mut resident, "select current_user()").unwrap()), "null");
    }

    #[test]
    fn a_transaction_a_call_leaves_open_is_rolled_back_and_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut first = runtime.store(site_in(&dir), "app", None, Guards::default());
        sql(&mut first, "create table t (x)").unwrap();
        sql(&mut first, "begin").unwrap();
        sql(&mut first, "insert into t values (1)").unwrap();
        // The call ends without committing: the next call on the same store
        // sees nothing of it.
        arm(&mut first, Guards::default());
        assert_eq!(text(&sql(&mut first, "select count(*) from t").unwrap()), "0");

        sql(&mut first, "begin").unwrap();
        sql(&mut first, "insert into t values (2)").unwrap();
        drop(first);
        // Nor does another call, which can also write: no lock outlived it.
        let mut second = runtime.store(site_in(&dir), "app", None, Guards::default());
        assert_eq!(text(&sql(&mut second, "select count(*) from t").unwrap()), "0");
        sql(&mut second, "insert into t values (3)").unwrap();
    }

    #[test]
    fn a_kept_connection_still_refuses_attach_and_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut store = runtime.store(site_in(&dir), "app", None, Guards::default());
        sql(&mut store, "select 1").unwrap();
        for refused in [
            "attach database '../victim/data.db' as v",
            "attach database ':memory:' as m",
            "pragma journal_mode = delete",
            "pragma query_only = 0",
        ] {
            assert!(matches!(sql(&mut store, refused), Err(WitDbError::Denied(_))), "{refused} was allowed");
            // And not once allowed: the second asking is refused too.
            assert!(matches!(sql(&mut store, refused), Err(WitDbError::Denied(_))), "{refused} was allowed");
        }
        sql(&mut store, "select 1").unwrap();
    }

    #[test]
    fn a_query_on_a_kept_connection_still_stops_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let guards = Guards { wall_clock: Duration::from_millis(300), ..Guards::default() };
        let mut store = runtime.store(site_in(&dir), "app", None, guards);
        sql(&mut store, "select 1").unwrap();
        let started = Instant::now();
        let error = sql(&mut store, "with recursive c(i) as (select 1 union all select i + 1 from c) select count(*) from c")
            .unwrap_err();
        assert!(matches!(&error, WitDbError::Failed(m) if m.contains("interrupt")), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "ran on for {:?}", started.elapsed());

        // The next call gets its own deadline, not the spent one.
        arm(&mut store, Guards::default());
        sql(&mut store, "select 1").unwrap();
    }

    // --- batches on the kept connection --------------------------------------

    fn batch(store: &mut Store<StoreState>, statements: &[&str]) -> Result<Vec<u64>, WitDbError> {
        let statements = statements
            .iter()
            .map(|sql| WitStatement { sql: sql.to_string(), params: Vec::new() })
            .collect();
        super::toolsite::app::db::Host::batch(store.data_mut(), statements)
    }

    fn batch_scoped(store: &mut Store<StoreState>, statements: &[&str]) -> Result<Vec<u64>, WitDbError> {
        let statements = statements
            .iter()
            .map(|sql| WitStatement { sql: sql.to_string(), params: Vec::new() })
            .collect();
        super::toolsite::app::db::Host::batch_scoped(store.data_mut(), statements)
    }

    fn scoped_sql(store: &mut Store<StoreState>, sql: &str) -> Result<WitRows, WitDbError> {
        super::toolsite::app::db::Host::query_scoped(store.data_mut(), sql.to_string(), Vec::new())
    }

    #[test]
    fn a_batch_on_a_call_that_already_queried_commits_and_leaves_the_connection_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut store = runtime.store(site_in(&dir), "app", None, Guards::default());
        sql(&mut store, "create table t (x)").unwrap();
        sql(&mut store, "insert into t values (1)").unwrap();
        assert_eq!(batch(&mut store, &["insert into t values (2)", "insert into t values (3)"]).unwrap(), vec![1, 1]);
        // The same call sees it, and so does the next, on a new connection.
        assert_eq!(text(&sql(&mut store, "select count(*) from t").unwrap()), "3");
        arm(&mut store, Guards::default());
        assert_eq!(text(&sql(&mut store, "select count(*) from t").unwrap()), "3");

        // The batch swapped the authorizer and put it back: the call's later
        // statements are refused exactly as before it.
        batch(&mut store, &["insert into t values (4)"]).unwrap();
        for refused in ["attach database ':memory:' as m", "pragma query_only = 0"] {
            assert!(matches!(sql(&mut store, refused), Err(WitDbError::Denied(_))), "{refused} was allowed");
        }
        // And a failed batch undoes all of itself and leaves the call able
        // to go on.
        assert!(batch(&mut store, &["insert into t values (5)", "insert into nowhere values (1)"]).is_err());
        assert_eq!(text(&sql(&mut store, "select count(*) from t").unwrap()), "4");
    }

    #[test]
    fn a_batch_is_refused_while_the_call_holds_a_transaction_and_that_transaction_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut store = runtime.store(site_in(&dir), "app", None, Guards::default());
        sql(&mut store, "create table t (x)").unwrap();
        sql(&mut store, "begin").unwrap();
        sql(&mut store, "insert into t values (1)").unwrap();
        let error = batch(&mut store, &["insert into t values (2)"]).unwrap_err();
        assert!(matches!(&error, WitDbError::Failed(m) if m.contains("its own transaction")), "{error:?}");
        // The guest's transaction is still its own to finish.
        sql(&mut store, "commit").unwrap();
        assert_eq!(text(&sql(&mut store, "select group_concat(x) from t").unwrap()), "1");
        // Once it has, a batch runs.
        batch(&mut store, &["insert into t values (2)"]).unwrap();
        assert_eq!(text(&sql(&mut store, "select count(*) from t").unwrap()), "2");
    }

    #[test]
    fn a_transaction_statement_the_call_prepared_earlier_cannot_split_a_batch() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut store = runtime.store(site_in(&dir), "app", None, Guards::default());
        sql(&mut store, "create table t (x)").unwrap();
        // Prepared, and so cached, under the call's own authorizer, which
        // lets a guest end its own transaction.
        for statement in ["begin", "commit", "savepoint s", "release s"] {
            sql(&mut store, statement).unwrap();
        }
        for split in ["commit", "savepoint s", "release s", "begin"] {
            let error = batch(&mut store, &["insert into t values (1)", split, "insert into nowhere values (1)"]).unwrap_err();
            assert!(matches!(&error, WitDbError::Denied(_)), "{split}: {error:?}");
            assert_eq!(text(&sql(&mut store, "select count(*) from t").unwrap()), "0", "{split} let half a batch commit");
        }
    }

    #[test]
    fn query_rows_applies_on_the_kept_connection_and_each_call_gets_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let three = Guards { query_rows: 3, ..Guards::default() };
        let mut store = runtime.store(site_in(&dir), "app", None, three);
        arm(&mut store, three);
        sql(&mut store, "create table t (x)").unwrap();
        sql(&mut store, "with recursive c(i) as (select 1 union all select i + 1 from c where i < 10) insert into t select i from c")
            .unwrap();
        for _ in 0..2 {
            let rows = sql(&mut store, "select x from t").unwrap();
            assert_eq!((rows.values.len(), rows.truncated), (3, true));
        }
        // The next call's guards, not the connection's first.
        arm(&mut store, Guards { query_rows: 7, ..Guards::default() });
        let rows = sql(&mut store, "select x from t").unwrap();
        assert_eq!((rows.values.len(), rows.truncated), (7, true));
        arm(&mut store, Guards { query_rows: 50, ..Guards::default() });
        let rows = sql(&mut store, "select x from t").unwrap();
        assert_eq!((rows.values.len(), rows.truncated), (10, false));
    }

    /// An app named `shop` whose `my_orders` view shows each person their
    /// own orders and lets them write in their own name.
    fn shop(site: &SiteConfig) {
        let policy = crate::content::store::Policy {
            table: "orders".into(),
            view: "my_orders".into(),
            where_: "owner_id = current_user()".into(),
            owner: Some("owner_id".into()),
            write: true,
        };
        db::run(site, "shop", "create table orders (id integer primary key, owner_id text, total real)", &[]).unwrap();
        let columns = vec!["id".to_string(), "owner_id".to_string(), "total".to_string()];
        let keys = crate::runtime::access::Keys { rowid_alias: Some("id".into()), unique: Vec::new(), defaults: Vec::new() };
        db::run(site, "shop", &crate::runtime::access::generate(&policy, "abc", &columns, &keys), &[]).unwrap();
        let mut generated = vec!["my_orders".to_string(), "ts_abc_my_orders".to_string()];
        generated.extend(crate::runtime::access::trigger_names("abc", "my_orders"));
        let meta = crate::content::store::PageMeta {
            policies: vec![policy],
            generated,
            access_salt: Some("abc".into()),
            ..Default::default()
        };
        crate::content::catalog::update_meta_blocking(site, "shop", move |stored| {
            *stored = meta;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_resident_batch_scoped_of_one_person_never_lends_its_identity_to_the_next_persons_query() {
        let dir = tempfile::tempdir().unwrap();
        let site = site_in(&dir);
        shop(&site);
        let runtime = runtime();
        let mut resident = runtime.store(site.clone(), "shop", Some(person("u-alice")), Guards::default());
        for round in 0..5 {
            resident.data_mut().user = Some(person("u-alice"));
            arm(&mut resident, Guards::default());
            assert_eq!(scoped_sql(&mut resident, "select count(*) from my_orders").map(|r| text(&r)).unwrap(), round.to_string());
            assert_eq!(batch_scoped(&mut resident, &["insert into my_orders (total) values (1)"]).unwrap(), vec![1]);

            // Bob's event on the same instance: none of Alice's rows, and his
            // writes land in his own name.
            resident.data_mut().user = Some(person("u-bob"));
            arm(&mut resident, Guards::default());
            assert_eq!(text(&scoped_sql(&mut resident, "select count(*) from my_orders").unwrap()), round.to_string());
            assert_eq!(text(&scoped_sql(&mut resident, "select current_user()").unwrap()), "u-bob");
            batch_scoped(&mut resident, &["insert into my_orders (total) values (2)"]).unwrap();
            // A write in Alice's name is refused, batch and all.
            assert!(batch_scoped(&mut resident, &["insert into my_orders (owner_id, total) values ('u-alice', 9)"]).is_err());
            // Nor does the plain connection answer for Alice.
            assert_eq!(text(&sql(&mut resident, "select current_user()").unwrap()), "u-bob");
        }
        let owners = db::run(&site, "shop", "select owner_id, count(*), sum(total) from orders group by owner_id order by owner_id", &[]).unwrap();
        assert_eq!(owners.rows, vec![
            vec![serde_json::json!("u-alice"), serde_json::json!(5), serde_json::json!(5.0)],
            vec![serde_json::json!("u-bob"), serde_json::json!(5), serde_json::json!(10.0)],
        ]);

        // The batch put the scope back: the call's next scoped statement is
        // held to it as the first was.
        for refused in ["select * from orders", "begin", "attach database ':memory:' as m"] {
            assert!(matches!(scoped_sql(&mut resident, refused), Err(WitDbError::Denied(_))), "{refused} was allowed");
        }
    }

    #[test]
    fn a_writer_a_call_left_open_is_gone_by_the_next_call_on_the_same_store() {
        use super::toolsite::app::blobs::Host as _;
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut resident = runtime.store(site_in(&dir), "app", Some(person("u-alice")), Guards::default());
        let handle = resident.data_mut().writer_open("report.bin".into(), "application/octet-stream".into()).unwrap();
        resident.data_mut().writer_append(handle, b"alice's half".to_vec()).unwrap();
        sql(&mut resident, "select 1").unwrap();

        // The next event, someone else's: the handle names nothing, and
        // nothing of the first call's file was stored.
        resident.data_mut().user = Some(person("u-bob"));
        arm(&mut resident, Guards::default());
        assert!(resident.data_mut().writer_append(handle, b"bob's".to_vec()).is_err());
        assert!(resident.data_mut().writer_finish(handle).is_err());
        assert!(resident.data().writers.is_empty());
        assert!(resident.data().db.is_none());
        assert!(resident.data_mut().get("report.bin".into()).is_err());
    }

    #[test]
    fn temp_tables_triggers_and_savepoints_of_one_call_are_gone_by_the_next_on_the_same_store() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut resident = runtime.store(site_in(&dir), "app", Some(person("u-alice")), Guards::default());
        sql(&mut resident, "create table t (x, who)").unwrap();
        // Alice's call leaves a temp table and a temp trigger that would
        // stamp every later insert as hers, and a savepoint open.
        sql(&mut resident, "create temp table scratch (x)").unwrap();
        sql(&mut resident, "insert into scratch values ('alice was here')").unwrap();
        sql(
            &mut resident,
            "create temp trigger stamp after insert on main.t begin update t set who = 'u-alice' where rowid = new.rowid; end",
        )
        .unwrap();
        sql(&mut resident, "savepoint held").unwrap();
        sql(&mut resident, "insert into t values (1, current_user())").unwrap();

        // Bob's event on the same instance.
        resident.data_mut().user = Some(person("u-bob"));
        arm(&mut resident, Guards::default());
        assert!(sql(&mut resident, "select * from scratch").is_err(), "a temp table outlived its call");
        assert_eq!(text(&sql(&mut resident, "select count(*) from temp.sqlite_master").unwrap()), "0");
        sql(&mut resident, "insert into t values (2, current_user())").unwrap();
        assert_eq!(text(&sql(&mut resident, "select group_concat(who) from t").unwrap()), "u-bob", "the savepoint's row stayed or the trigger fired");

        // A tick, as nobody: none of either person.
        resident.data_mut().user = None;
        arm(&mut resident, Guards::default());
        assert_eq!(text(&sql(&mut resident, "select current_user()").unwrap()), "null");
    }

    #[test]
    fn a_scoped_caller_cannot_create_temp_objects_or_triggers_even_in_a_batch() {
        let dir = tempfile::tempdir().unwrap();
        let site = site_in(&dir);
        shop(&site);
        let runtime = runtime();
        // An index for `reindex` to have something to rebuild: with none it
        // asks nothing and does nothing.
        db::run(&site, "shop", "create index orders_owner on orders (owner_id)", &[]).unwrap();
        let mut store = runtime.store(site.clone(), "shop", Some(person("u-alice")), Guards::default());
        for statement in [
            "create temp table scratch (x)",
            "create temp view v as select 1",
            "create temp trigger t after insert on orders begin delete from orders; end",
            "create trigger t after insert on orders begin delete from orders; end",
            "create trigger t instead of insert on my_orders begin delete from orders; end",
            "drop trigger if exists ts_abc_my_orders_insert",
            "vacuum",
            "reindex",
            "analyze",
        ] {
            assert!(scoped_sql(&mut store, statement).is_err(), "{statement} was allowed");
            assert!(
                batch_scoped(&mut store, &["insert into my_orders (total) values (1)", statement]).is_err(),
                "{statement} was allowed in a batch"
            );
        }
        let rows = db::run(&site, "shop", "select count(*) from orders", &[]).unwrap();
        assert_eq!(rows.rows[0][0], serde_json::json!(0), "a refused batch left its insert");
        let triggers = db::run(&site, "shop", "select count(*) from sqlite_master where type = 'trigger'", &[]).unwrap();
        assert_eq!(triggers.rows[0][0], serde_json::json!(crate::runtime::access::trigger_names("abc", "my_orders").len()));
    }

    #[test]
    fn a_statement_cached_on_the_plain_connection_is_never_run_on_the_scoped_one() {
        let dir = tempfile::tempdir().unwrap();
        let site = site_in(&dir);
        shop(&site);
        let runtime = runtime();
        let mut store = runtime.store(site, "shop", Some(person("u-alice")), Guards::default());
        // Prepared and cached under the plain authorizer, which allows it.
        for _ in 0..3 {
            sql(&mut store, "select * from orders").unwrap();
        }
        assert!(matches!(scoped_sql(&mut store, "select * from orders"), Err(WitDbError::Denied(_))));
        assert!(matches!(batch_scoped(&mut store, &["select * from orders"]), Err(WitDbError::Denied(_))));
        // And the reverse: the scoped connection's view does not make the
        // plain one's base table any less reachable for the author.
        scoped_sql(&mut store, "select * from my_orders").unwrap();
        sql(&mut store, "select * from orders").unwrap();
    }

    #[test]
    fn uncommitted_plain_writes_never_show_through_the_scoped_connection_and_roll_back_with_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let site = site_in(&dir);
        shop(&site);
        let runtime = runtime();
        let mut store = runtime.store(site.clone(), "shop", Some(person("u-alice")), Guards::default());
        sql(&mut store, "begin").unwrap();
        sql(&mut store, "insert into orders (owner_id, total) values ('u-alice', 1)").unwrap();
        assert_eq!(text(&scoped_sql(&mut store, "select count(*) from my_orders").unwrap()), "0");
        arm(&mut store, Guards::default());
        assert_eq!(text(&scoped_sql(&mut store, "select count(*) from my_orders").unwrap()), "0");
        assert_eq!(text(&sql(&mut store, "select count(*) from orders").unwrap()), "0");
    }

    #[test]
    fn waiting_on_a_lock_the_call_itself_holds_stops_at_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let site = site_in(&dir);
        shop(&site);
        let runtime = runtime();
        let guards = Guards { wall_clock: Duration::from_millis(600), ..Guards::default() };
        let mut store = runtime.store(site, "shop", Some(person("u-alice")), guards);
        // The plain connection takes the write lock; the scoped one then
        // asks for it. SQLite's busy handler would sleep its full five
        // seconds there, where no progress handler runs.
        sql(&mut store, "begin immediate").unwrap();
        let started = Instant::now();
        assert!(scoped_sql(&mut store, "insert into my_orders (total) values (1)").is_err());
        assert!(batch_scoped(&mut store, &["insert into my_orders (total) values (1)"]).is_err());
        assert!(started.elapsed() < Duration::from_millis(2500), "waited {:?} past a 600 ms call", started.elapsed());
    }

    #[test]
    fn vacuum_into_cannot_copy_the_database_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let mut store = runtime.store(site_in(&dir), "app", None, Guards::default());
        sql(&mut store, "create table t (x); insert into t values ('secret')").unwrap();
        let out = dir.path().join("copied.db");
        let other = dir.path().join("victim");
        std::fs::create_dir_all(&other).unwrap();
        for statement in [
            format!("vacuum into '{}'", out.display()),
            "vacuum into '../victim/data.db'".to_string(),
            "vacuum main into '../copied.db'".to_string(),
        ] {
            assert!(sql(&mut store, &statement).is_err(), "{statement} ran");
            assert!(batch(&mut store, &[&statement]).is_err(), "{statement} ran in a batch");
        }
        assert!(!out.exists());
        assert!(!other.join("data.db").exists());
        assert!(!dir.path().join("copied.db").exists());
        for refused in ["select load_extension('/lib/x86_64-linux-gnu/libc.so.6')", "select writefile('/tmp/x', 'y')"] {
            assert!(sql(&mut store, refused).is_err(), "{refused} ran");
            assert!(batch(&mut store, &[refused]).is_err(), "{refused} ran in a batch");
        }
    }

    #[test]
    fn compiled_modules_are_reused() {
        let runtime = runtime();
        run(&runtime, ADDER, Guards::default()).unwrap();
        run(&runtime, ADDER, Guards::default()).unwrap();
        assert_eq!(runtime.modules.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_module_cache_is_bounded() {
        let runtime = runtime();
        for i in 0..MAX_CACHED_MODULES + 8 {
            let wat = format!(
                "(module (func (export \"run\") (result i32) i32.const {i}))"
            );
            run(&runtime, &wat, Guards::default()).unwrap();
        }
        assert!(runtime.modules.lock().unwrap().len() <= MAX_CACHED_MODULES);
    }
}
