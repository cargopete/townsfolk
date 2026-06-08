//! The whole behaviour layer as one wasm guest. It exports `decide` over the scalar ABI
//! and forwards to the shared `policies` crate, so the guest and the host's native engine
//! run identical logic. Build: `ops/build-wasm.sh`.
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn decide(
    archetype: i32,
    standing: i32,
    purse: i32,
    age: i64,
    married: i32,
    season: i32,
    is_market: i32,
    is_sunday: i32,
    top: i32,
    rng: i64,
) -> i32 {
    policies::decide(archetype, standing, purse, age, married, season, is_market, is_sunday, top, rng as u64)
}
