//! Every archetype's policy as pure integer logic: one `decide(...)`, no I/O, no clock, its only
//! randomness the per-soul, per-slot seed it is handed.
//!
//! The souls are now *situation-aware*: they decide with their ambition (goal), their
//! spirits (mood), and who's about them (a friend or a rival present this phase).
//!
//! Archetype: 0 genteel · 1 hill_farmer · 2 practitioner · 3 improver · 4 blunt_hand · 5 official.
//! Goal: 0 Thrive · 1 ClearDebt · 2 Rise · 3 MarryOff · 4 Outdo · 5 Prosper.
//! Action: 0 Idle · 1 PayCall · 2 GiveDinner · 3 Economise · 4 KeepUp · 5 TendStock ·
//!         6 Haggle · 7 Graft · 8 Scheme · 9 Press · 10 Minister · 11 Round.
//! Season: 0 Winter · 1 Lambing · 2 Sowing · 3 Hay · 4 Harvest · 5 Mart.

use crate::{present_ties, Action, Season, SoulView};
use time::Weekday;

fn season_ord(s: Season) -> i32 {
    match s {
        Season::Winter => 0,
        Season::Lambing => 1,
        Season::Sowing => 2,
        Season::Hay => 3,
        Season::Harvest => 4,
        Season::Mart => 5,
    }
}

fn arch_ord(archetype: &str) -> i32 {
    match archetype {
        "genteel_status_seeker" => 0,
        "hill_farmer" => 1,
        "practitioner" => 2,
        "scheming_improver" => 3,
        "blunt_hand" => 4,
        "official" => 5,
        _ => -1,
    }
}

/// A soul's intention for this phase, read from the whole of them.
pub fn decide(v: &SoulView) -> Action {
    let ord = arch_ord(&v.me.archetype);
    if ord < 0 {
        return Action::Idle;
    }
    let (friend, rival) = present_ties(v.world, v.i, v.places);
    Action::from_i32(choose(
        ord,
        v.me.standing,
        v.me.purse,
        v.me.age(v.day),
        v.me.spouse.is_some() as i32,
        season_ord(Season::of(v.date)),
        (v.date.weekday() == Weekday::Wednesday) as i32,
        (v.date.weekday() == Weekday::Sunday) as i32,
        v.top_standing,
        v.me.goal as i32,
        v.me.mood as i32,
        friend as i32,
        rival as i32,
        v.rng,
    ))
}

#[inline]
fn mix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline]
fn chance(s: &mut u64, permille: i64) -> bool {
    permille > 0 && (mix(s) % 1000) < permille as u64
}

/// The archetype tables over the scalars `decide` reads off the soul.
#[allow(clippy::too_many_arguments)]
fn choose(
    archetype: i32,
    standing: i32,
    purse: i32,
    _age: i64,
    _married: i32,
    season: i32,
    is_market: i32,
    is_sunday: i32,
    top: i32,
    goal: i32,
    mood: i32,
    friend: i32, // a friend is about this phase
    rival: i32,  // a rival is about this phase
    rng: u64,
) -> i32 {
    let mut s = rng ^ (0x9101 + archetype as u64);

    // the grieving and low-spirited withdraw, whoever they are
    if mood <= -45 && chance(&mut s, 600) {
        return 0;
    }

    match archetype {
        // genteel_status_seeker — the status game, played harder when there's an audience
        0 => {
            // out and about more when a friend (company) or a rival (can't be outdone) is by
            let act = 160 + if friend != 0 { 130 } else { 0 } + if rival != 0 { 90 } else { 0 };
            if !chance(&mut s, act) {
                return 0;
            }
            let behind = standing < top - 8;
            if goal == 1 {
                // clearing debt: mend and make do, though face sometimes still wins
                return if behind && chance(&mut s, 280) { 4 } else { 3 };
            }
            if goal == 2 || goal == 4 || rival != 0 {
                // rising, outdoing, or squaring up to a rival present — be *seen*
                return if purse > 0 && chance(&mut s, 560) { 2 } else { 1 };
            }
            if mood >= 45 && purse > 0 && chance(&mut s, 450) {
                return 2; // triumphant: grow lavish
            }
            if purse < -12 {
                if behind && chance(&mut s, 400) { 4 } else { 3 }
            } else if behind && purse > 0 {
                if chance(&mut s, 500) { 2 } else { 1 }
            } else {
                1
            }
        }
        // hill_farmer — the mart on market day, else the stock; the prospering deal harder
        1 => {
            if goal == 5 && chance(&mut s, 130) {
                6
            } else if is_market != 0 && chance(&mut s, 250) {
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
        // scheming_improver — the doomed improvement, harder when fortune is the goal
        3 => {
            let p = if goal == 5 { 200 } else { 140 };
            if chance(&mut s, p) { 8 } else { 0 }
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
