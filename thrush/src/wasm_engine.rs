//! The wasm-backed behaviour engine. It loads one sandboxed guest holding the whole
//! behaviour layer and routes every archetype's decision through it across the scalar
//! policy ABI. Same `decide` boundary as native — the policies now executing in wasmtime.

use std::cell::RefCell;

use thrush_core::{arch_ord, season_ord, Action, Observation, PolicyEngine};
use wasmtime::{Engine, Instance, Module, Store, TypedFunc};

/// The guest's exported signature: (archetype, observation…, rng) -> action ordinal.
type DecideArgs = (i32, i32, i32, i64, i32, i32, i32, i32, i32, i32, i32, i32, i32, i64);
type DecideFn = TypedFunc<DecideArgs, i32>;

pub struct WasmPolicies {
    store: RefCell<Store<()>>,
    decide: DecideFn,
}

impl WasmPolicies {
    /// Instantiate the guest once (pooled) and bind its exported `decide`.
    pub fn load(path: &str) -> Result<Self, String> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path).map_err(|e| format!("load {path}: {e}"))?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).map_err(|e| e.to_string())?;
        let decide = instance
            .get_typed_func::<DecideArgs, i32>(&mut store, "decide")
            .map_err(|e| e.to_string())?;
        Ok(Self { store: RefCell::new(store), decide })
    }
}

impl PolicyEngine for WasmPolicies {
    fn decide(&self, archetype: &str, o: &Observation) -> Action {
        let ord = arch_ord(archetype);
        if ord < 0 {
            return Action::Idle;
        }
        let mut st = self.store.borrow_mut();
        let n = self
            .decide
            .call(
                &mut *st,
                (
                    ord,
                    o.standing,
                    o.purse,
                    o.age,
                    o.married as i32,
                    season_ord(o.season),
                    o.is_market as i32,
                    o.is_sunday as i32,
                    o.top_standing,
                    o.goal,
                    o.mood,
                    o.friend_present as i32,
                    o.rival_present as i32,
                    o.rng as i64,
                ),
            )
            .expect("wasm decide trapped");
        Action::from_i32(n)
    }
}
