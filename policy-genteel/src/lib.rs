//! The `genteel_status_seeker` policy — the Provincial-Lady tension of solvency vs face.
//!
//! Pure `no_std` integer logic with no dependencies, so the *same* compiled code runs
//! both as the host's native engine and as a wasm guest. That is what makes swapping the
//! substrate transparent: identical decisions, identical chronicle, bit-for-bit.
//!
//! The signature is the canonical policy ABI (a flat observation in, an action ordinal
//! out). Every archetype will share this shape, so the host calls any of them the same
//! way and a wasm guest implements it verbatim.
#![no_std]

#[inline]
fn mix(s: &mut u64) -> u64 {
    // splitmix64 — small, fast, fully deterministic across native and wasm
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline]
fn chance(s: &mut u64, permille: u64) -> bool {
    mix(s) % 1000 < permille
}

/// Action ordinals (must match `Action::from_i32` in the host).
/// 0 Idle · 1 PayCall · 2 GiveDinner · 3 Economise · 4 KeepUp
pub fn genteel_decide(
    standing: i32,
    purse: i32,
    _age: i64,
    _married: i32,
    _season: i32,
    _is_market: i32,
    _is_sunday: i32,
    top: i32,
    rng: u64,
) -> i32 {
    let mut s = rng ^ 0x6765_6E74; // "gent"
    if !chance(&mut s, 160) {
        return 0; // Idle — most days are routine
    }
    let behind = standing < top - 8;
    if purse < -12 {
        // broke — but face sometimes wins anyway (the comedy of the overdraft)
        if behind && chance(&mut s, 400) {
            4 // KeepUp: spend to hold standing
        } else {
            3 // Economise: mend and make do
        }
    } else if behind && purse > 0 {
        if chance(&mut s, 500) {
            2 // GiveDinner
        } else {
            1 // PayCall
        }
    } else {
        1 // PayCall: routine maintenance
    }
}
