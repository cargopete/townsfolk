//! Every archetype's policy as pure `no_std` integer logic — one `decide(...)` over the
//! canonical scalar ABI. The same compiled code is the host's native engine *and* the
//! wasm guest, so swapping the substrate is bit-for-bit transparent.
//!
//! Archetype ordinals: 0 genteel · 1 hill_farmer · 2 practitioner · 3 improver ·
//! 4 blunt_hand · 5 official.
//! Action ordinals: 0 Idle · 1 PayCall · 2 GiveDinner · 3 Economise · 4 KeepUp ·
//! 5 TendStock · 6 Haggle · 7 Graft · 8 Scheme · 9 Press · 10 Minister · 11 Round.
//! Season ordinals: 0 Winter · 1 Lambing · 2 Sowing · 3 Hay · 4 Harvest · 5 Mart.
#![no_std]

#[inline]
fn mix(s: &mut u64) -> u64 {
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

/// The one boundary the host and the wasm guest share. Flat observation in, action out.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    archetype: i32,
    standing: i32,
    purse: i32,
    _age: i64,
    _married: i32,
    season: i32,
    is_market: i32,
    is_sunday: i32,
    top: i32,
    rng: u64,
) -> i32 {
    let mut s = rng ^ (0x9101 + archetype as u64);
    match archetype {
        // genteel_status_seeker — solvency vs face
        0 => {
            if !chance(&mut s, 160) {
                return 0;
            }
            let behind = standing < top - 8;
            if purse < -12 {
                if behind && chance(&mut s, 400) { 4 } else { 3 }
            } else if behind && purse > 0 {
                if chance(&mut s, 500) { 2 } else { 1 }
            } else {
                1
            }
        }
        // hill_farmer — the mart on market day, else the stock
        1 => {
            if is_market != 0 && chance(&mut s, 250) {
                6
            } else if chance(&mut s, 80) {
                5
            } else {
                0
            }
        }
        // practitioner — the rounds, the connector
        2 => {
            if chance(&mut s, 180) { 11 } else { 0 }
        }
        // scheming_improver — the doomed improvement
        3 => {
            if chance(&mut s, 140) { 8 } else { 0 }
        }
        // blunt_hand — the work, quietly
        4 => {
            if chance(&mut s, 70) { 7 } else { 0 }
        }
        // official — the sermon on Sunday, the form in the lean seasons
        5 => {
            if is_sunday != 0 && chance(&mut s, 500) {
                10
            } else if (season == 0 || season == 5) && chance(&mut s, 60) {
                9
            } else {
                0
            }
        }
        _ => 0,
    }
}
