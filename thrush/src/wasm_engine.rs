//! The wasm-backed behaviour engine. It loads sandboxed guest modules and routes the
//! relevant archetypes to them across the scalar policy ABI; everything else falls back
//! to the in-process native engine. This is the substrate swap — same `decide` boundary,
//! the policy now executing inside wasmtime.

use std::cell::RefCell;

use thrush_core::{season_ord, Action, NativePolicies, Observation, PolicyEngine};
use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

/// The genteel guest's exported signature (the canonical scalar policy ABI).
type DecideFn = TypedFunc<(i32, i32, i64, i32, i32, i32, i32, i32, i64), i32>;

pub struct WasmPolicies {
    native: NativePolicies,
    store: RefCell<Store<()>>,
    genteel: DecideFn,
}

impl WasmPolicies {
    /// Instantiate the guest once (pooled) and bind its exported `decide`.
    pub fn load(path: &str) -> Result<Self, String> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path).map_err(|e| format!("load {path}: {e}"))?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).map_err(|e| e.to_string())?;
        let genteel = instance
            .get_typed_func::<(i32, i32, i64, i32, i32, i32, i32, i32, i64), i32>(&mut store, "genteel_decide")
            .map_err(|e| e.to_string())?;
        Ok(Self { native: NativePolicies, store: RefCell::new(store), genteel })
    }
}

impl PolicyEngine for WasmPolicies {
    fn decide(&self, archetype: &str, o: &Observation) -> Action {
        if archetype == "genteel_status_seeker" {
            let mut st = self.store.borrow_mut();
            let n = self
                .genteel
                .call(
                    &mut *st,
                    (
                        o.standing,
                        o.purse,
                        o.age,
                        o.married as i32,
                        season_ord(o.season),
                        o.is_market as i32,
                        o.is_sunday as i32,
                        o.top_standing,
                        o.rng as i64,
                    ),
                )
                .expect("wasm genteel_decide trapped");
            Action::from_i32(n)
        } else {
            self.native.decide(archetype, o)
        }
    }
}
