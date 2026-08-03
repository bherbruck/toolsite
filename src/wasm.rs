//! The sandbox published apps run their server-side code in.
//!
//! Three limits matter, and none of them are advisory:
//!
//! * **fuel** — a hard ceiling on executed instructions, so an infinite loop
//!   dies deterministically rather than pinning a core.
//! * **epochs** — wall-clock deadline, which catches guests that block without
//!   burning fuel.
//! * **memory** — a cap enforced when the guest asks to grow.
//!
//! A store is built fresh per request. Reusing one would leak state between
//! requests, and app state belongs in that app's database instead.

use crate::{config::Config as SiteConfig, db};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Module, Store, StoreLimits, StoreLimitsBuilder,
};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// Generates the host side of wit/toolsite.wit: the `App` world's exported
// `handle`, and traits for every import we grant.
wasmtime::component::bindgen!({
    path: "wit",
    world: "app",
});

use self::toolsite::app::db::{Error as WitDbError, Rows as WitRows, Value as WitValue};
// Request and Response already land at module scope from bindgen; User sits
// under its interface, so re-export it rather than making callers spell out
// the generated path.
pub use self::toolsite::app::identity::User;

/// How often the epoch ticker advances. Wall-clock deadlines are rounded up to
/// a multiple of this, so it trades timeout precision against wakeups.
const EPOCH_TICK: Duration = Duration::from_millis(50);

/// Compiled modules kept in memory. Eviction only costs a recompile, because
/// the `.wasm` itself stays on disk.
const MAX_CACHED_MODULES: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct Guards {
    /// Instructions the guest may execute before it is killed.
    pub fuel: u64,
    /// Ceiling on the guest's linear memory.
    pub memory_bytes: usize,
    /// Wall-clock ceiling, enforced even if the guest never burns fuel.
    pub wall_clock: Duration,
}

impl Default for Guards {
    fn default() -> Self {
        Self {
            fuel: 200_000_000,
            memory_bytes: 64 * 1024 * 1024,
            wall_clock: Duration::from_secs(5),
        }
    }
}

/// What every guest store carries. Capabilities get added here as they are
/// granted; anything absent is something the guest simply cannot do.
pub struct StoreState {
    limits: StoreLimits,
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
}

impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl self::toolsite::app::db::Host for StoreState {
    fn query(
        &mut self,
        sql: String,
        params: Vec<WitValue>,
    ) -> Result<WitRows, WitDbError> {
        if !self.site.databases {
            return Err(WitDbError::Denied("databases are not enabled".into()));
        }

        let params: Vec<serde_json::Value> = params.into_iter().map(json_of).collect();
        match db::run(&self.site, &self.app, &sql, &params) {
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
            Err(message) if message.contains("not authorized") => {
                Err(WitDbError::Denied(message))
            }
            Err(message) => Err(WitDbError::Failed(message)),
        }
    }
}

/// No functions, only shared records — but the world's `use` still requires
/// the trait to be present.
impl self::toolsite::app::http::Host for StoreState {}

impl self::toolsite::app::identity::Host for StoreState {
    fn current_user(&mut self) -> Option<User> {
        self.user.clone()
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

        Ok(Arc::new(Self {
            engine,
            modules: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            linker,
        }))
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
        let state = StoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(guards.memory_bytes)
                .build(),
            // Deliberately empty: no dirs, no env, no network, no stdio.
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            app: app.to_string(),
            site,
            user,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(guards.fuel).expect("fuel is enabled");

        let ticks = guards.wall_clock.as_millis().div_ceil(EPOCH_TICK.as_millis());
        store.set_epoch_deadline(ticks.max(1) as u64);
        store
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
            true,
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
            fuel: 100_000,
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
            fuel: u64::MAX,
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

    #[test]
    fn each_request_gets_a_store_with_its_own_fuel() {
        let runtime = runtime();
        let guards = Guards {
            fuel: 100_000,
            ..Guards::default()
        };
        // A store that ran to exhaustion must not affect the next one.
        let _ = run(&runtime, INFINITE_LOOP, guards);
        assert_eq!(run(&runtime, ADDER, guards).unwrap(), 42);
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
