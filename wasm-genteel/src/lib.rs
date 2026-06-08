//! The genteel policy as a wasm guest. It exports one function over a pure scalar ABI —
//! observation ints in, action ordinal out — and simply forwards to the shared
//! `policy-genteel` crate, so the wasm guest and the host's native engine run identical
//! logic. Build: `cargo build -p wasm-genteel --target wasm32-unknown-unknown --release`.
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn genteel_decide(
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
    policy_genteel::genteel_decide(standing, purse, age, married, season, is_market, is_sunday, top, rng as u64)
}
