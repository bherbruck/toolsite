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

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use wasmtime::{Config, Engine, Module, Store, StoreLimits, StoreLimitsBuilder};

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
}

pub struct Runtime {
    engine: Engine,
    modules: Mutex<HashMap<String, (Module, Instant)>>,
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

        Ok(Arc::new(Self {
            engine,
            modules: Mutex::new(HashMap::new()),
        }))
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
    pub fn store(&self, guards: Guards) -> Store<StoreState> {
        let state = StoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(guards.memory_bytes)
                .build(),
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

    /// Runs `wat`'s exported `run` function under `guards`.
    fn run(runtime: &Runtime, wat: &str, guards: Guards) -> anyhow::Result<i32> {
        let wasm = wat::parse_str(wat)?;
        let module = runtime.module(wat, &wasm)?;
        let mut store = runtime.store(guards);
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
