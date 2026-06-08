//! Thrushcombe — deterministic, event-sourced core.
//!
//! The whole simulation is a pure function of (seed, epoch, today). The clock is
//! *bound to the real calendar*: `t = today.julian - epoch.julian`. The cron driver
//! never increments `t`; it derives it and "catches up" to today, which gives exact
//! phase-lock and self-healing if days are missed.
//!
//! v0.1 scope: the clock + season machine + a small seeded cast + a daily incident
//! generator drawn from the current season's armed risks/windfalls, all logged to
//! SQLite for the chronicle. The full WASM behaviour layer and gossip diffusion come
//! later; this is the watchable spine.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::{Date, Month};

// ----------------------------------------------------------------------------- calendar

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Season {
    Winter,
    Lambing,
    Sowing,
    Hay,
    Harvest,
    Mart,
}

impl Season {
    pub fn of(date: Date) -> Season {
        match date.month() {
            Month::December | Month::January => Season::Winter,
            Month::February | Month::March => Season::Lambing,
            Month::April | Month::May => Season::Sowing,
            Month::June | Month::July => Season::Hay,
            Month::August | Month::September => Season::Harvest,
            Month::October | Month::November => Season::Mart,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Season::Winter => "Winter Holding",
            Season::Lambing => "Lambing",
            Season::Sowing => "Turnout & Sowing",
            Season::Hay => "Haymaking",
            Season::Harvest => "Harvest",
            Season::Mart => "Mart & Tup",
        }
    }
    /// What disasters/windfalls the shock layer may draw on this season.
    pub fn armed(self) -> &'static str {
        match self {
            Season::Winter => "fodder short · tithe & bills fall due",
            Season::Lambing => "lamb loss · cold snap",
            Season::Sowing => "late frost · scour",
            Season::Hay => "the storm flattens cut hay · breakdown",
            Season::Harvest => "wet harvest rots the corn",
            Season::Mart => "price crash at the mart",
        }
    }
}

// ----------------------------------------------------------------------------- world

#[derive(Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub archetype: String,
    pub seat: String,
    pub standing: i32, // 0..100, face / reputation
    pub purse: i32,    // pounds; may go negative
    // --- life cycle ---
    pub birth_day: i64,           // day-index of birth; founders are negative (born before epoch)
    pub sex: u8,                  // 0 = woman, 1 = man
    pub death_day: Option<i64>,   // None = living. Dead agents are kept (indices are stable) but inert.
    pub departed: bool,           // left Thrushcombe (married away / a situation in town) — alive but off-stage
    pub spouse: Option<usize>,
    pub parent: Option<usize>,    // mother/father index, for lineage & succession
    pub origin: Option<String>,   // where they came from; None = Thrushcombe-born
    pub trade: Option<String>,    // their specific occupation, distinct from the policy archetype
}

impl Agent {
    pub fn age(&self, day: i64) -> i64 {
        (day - self.birth_day).max(0) / 365
    }
    pub fn alive(&self) -> bool {
        self.death_day.is_none()
    }
    /// Present and on-stage: alive and not departed. The tracked cast.
    pub fn active(&self) -> bool {
        self.death_day.is_none() && !self.departed
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Animal {
    pub name: String,
    pub owner: String,
    pub species: String,
    pub health: i32, // 0..100
    pub gest: i32,    // days until calving/birth; <0 = none pending
    pub value: i32,
}

/// A piece of news loose in the town. It spreads across the social graph with delay
/// (one hop a day) and distortion (it grows in the telling), and each new pair of ears
/// nudges the subject's standing — gossip is *how* reputation actually moves.
#[derive(Clone, Serialize, Deserialize)]
pub struct News {
    pub id: u32,
    pub subject: usize, // index into agents — who the talk concerns
    pub topic: String,
    pub valence: i32, // reputational direction; magnitude grows as it distorts
    pub born: i64,
    pub knowers: Vec<usize>,
    pub distortion: u32,
    pub applied: i32,  // standing nudges spent so far (capped, so one rumour can't run away)
    pub broadcast: bool, // has "most of the town knows" already fired
}

#[derive(Clone, Serialize, Deserialize)]
pub struct World {
    pub agents: Vec<Agent>,
    pub animals: Vec<Animal>,
    pub news: Vec<News>,
    pub next_news_id: u32,
    /// Directed pairwise feeling: (from, to) → affinity in [-100, 100]. The relationship
    /// ledger — a slow variable that remembers every snub and kindness. Gossip moves it,
    /// so reputation becomes *personal*, not just a global score.
    pub affinity: BTreeMap<(u32, u32), i16>,
}

impl World {
    fn nudge_aff(&mut self, from: usize, to: usize, d: i16) {
        let e = self.affinity.entry((from as u32, to as u32)).or_insert(0);
        *e = (*e + d).clamp(-100, 100);
    }
    /// The agent's strongest ties, as (other_idx, feeling), positive or negative, among the
    /// living — for surfacing friends and rivals.
    fn ties(&self, idx: usize, positive: bool, limit: usize) -> Vec<(usize, i16)> {
        let mut v: Vec<(usize, i16)> = self
            .affinity
            .iter()
            .filter(|(&(f, _), _)| f as usize == idx)
            .filter(|(&(_, t), &val)| self.agents[t as usize].active() && if positive { val >= 25 } else { val <= -25 })
            .map(|(&(_, t), &val)| (t as usize, val))
            .collect();
        if positive {
            v.sort_by_key(|&(_, val)| std::cmp::Reverse(val));
        } else {
            v.sort_by_key(|&(_, val)| val);
        }
        v.truncate(limit);
        v
    }
}

impl World {
    fn seed() -> World {
        // age at founding, sex (0=w,1=m). birth_day = -age*365 so age grows with the run.
        let a = |name: &str, arch: &str, seat: &str, standing, purse, age: i64, sex: u8| Agent {
            name: name.into(),
            archetype: arch.into(),
            seat: seat.into(),
            standing,
            purse,
            birth_day: -age * 365,
            sex,
            death_day: None,
            departed: false,
            spouse: None,
            parent: None,
            origin: None,
            trade: None,
        };
        let mut agents = vec![
            // The Laurels (Provincial Lady)            idx
            a("Mrs Cynthia Pelham", "genteel_status_seeker", "The Laurels", 60, -18, 42, 0), // 0
            a("Mr Robert Pelham", "genteel_status_seeker", "The Laurels", 58, 5, 46, 1),      // 1
            a("Robin Pelham", "child", "The Laurels", 20, 0, 11, 1),                          // 2
            a("Vicky Pelham", "child", "The Laurels", 18, 0, 8, 0),                           // 3
            // Crale Court & the Home Farm (Clarkson)
            a("Lady Aldermaston", "genteel_status_seeker", "Crale Court", 90, 420, 70, 0),    // 4
            a("Mr Rupert Crale", "scheming_improver", "Home Farm", 55, -40, 28, 1),           // 5
            a("Tot Wragg", "blunt_hand", "Home Farm", 40, 4, 20, 1),                          // 6
            a("Sam Trotter", "blunt_hand", "Home Farm", 36, 6, 35, 1),                        // 7
            // The Vicarage
            a("Revd Mr Soames", "official", "The Vicarage", 72, 30, 58, 1),                   // 8
            a("Mrs Soames", "genteel_status_seeker", "The Vicarage", 66, 8, 54, 0),           // 9
            // The practice (Herriot)
            a("Mr Farran MRCVS", "practitioner", "Beck House", 68, 25, 45, 1),                // 10
            a("Mr James Herrick", "practitioner", "Beck House", 50, 10, 26, 1),               // 11
            // High Foldside (the Sunters)
            a("Mr Sunter", "hill_farmer", "High Foldside", 48, 12, 55, 1),                    // 12
            a("Mrs Sunter", "hill_farmer", "High Foldside", 46, 6, 52, 0),                    // 13
            a("Jack Sunter", "hill_farmer", "High Foldside", 38, 3, 21, 1),                   // 14
            // Gunnerside (a second hill farm)
            a("Mr Metcalfe", "hill_farmer", "Gunnerside", 50, 18, 48, 1),                     // 15
            a("Mrs Metcalfe", "hill_farmer", "Gunnerside", 47, 5, 44, 0),                     // 16
            a("Will Metcalfe", "hill_farmer", "Gunnerside", 38, 2, 19, 1),                    // 17
            // Five Elms (a second genteel family)
            a("Major Pringle", "genteel_status_seeker", "Five Elms", 74, 260, 60, 1),         // 18
            a("Mrs Pringle", "genteel_status_seeker", "Five Elms", 70, 40, 55, 0),            // 19
            a("Daphne Pringle", "genteel_status_seeker", "Five Elms", 52, 5, 24, 0),          // 20
            // The Pelican, the shop, the forge — the levellers & trade
            a("Mr Bunce", "blunt_hand", "The Pelican", 50, 30, 50, 1),                        // 21
            a("Mrs Bunce", "blunt_hand", "The Pelican", 47, 12, 47, 0),                       // 22
            a("Mr Pickering", "blunt_hand", "The Shop", 52, 35, 52, 1),                       // 23
            a("Mr Garth", "blunt_hand", "The Forge", 46, 14, 40, 1),                          // 24
            a("Mrs Toms (Cook)", "blunt_hand", "The Laurels", 38, 3, 45, 0),                  // 25
            a("Gladys", "blunt_hand", "The Laurels", 28, 1, 19, 0),                           // 26
            // The officials (Clarkson's friction)
            a("Mr Crisp", "official", "the Committee", 44, 40, 48, 1),                        // 27
            a("Constable Hodge", "official", "the Constabulary", 46, 12, 38, 1),              // 28
            // The leveller-busybody & the doctor
            a("Miss Pertwee", "genteel_status_seeker", "Ivy Cottage", 58, 22, 64, 0),         // 29
            a("Dr Lydgate", "practitioner", "Springs House", 70, 60, 50, 1),                  // 30
        ];
        // the trades — the town's working fabric (indices 31+, no kinship to disturb)
        let ta = |name: &str, arch: &str, seat: &str, standing, purse, age: i64, sex: u8, trade: &str| {
            let mut x = a(name, arch, seat, standing, purse, age, sex);
            x.trade = Some(trade.into());
            x
        };
        agents.extend([
            ta("Mrs Pollard", "blunt_hand", "the Post Office", 52, 18, 53, 0, "postmistress"),
            ta("Mr Haskins", "official", "the Station", 50, 26, 49, 1, "stationmaster"),
            ta("Mr Annis", "blunt_hand", "the Station", 38, 8, 27, 1, "railway porter"),
            ta("Mr Tranter", "blunt_hand", "the Bakehouse", 48, 22, 44, 1, "baker"),
            ta("Mr Dunnage", "blunt_hand", "the Shambles", 46, 30, 51, 1, "butcher"),
            ta("Miss Clewes", "blunt_hand", "the Draper's", 50, 16, 41, 0, "dressmaker"),
            ta("Mr Yeo", "blunt_hand", "the Mill", 47, 28, 56, 1, "miller"),
            ta("Miss Ferris", "official", "the School", 56, 14, 35, 0, "schoolmistress"),
            ta("Mr Tallin", "genteel_status_seeker", "Church Row", 66, 90, 54, 1, "solicitor"),
            ta("Mr Quint", "genteel_status_seeker", "the Bank House", 64, 140, 50, 1, "bank manager"),
            ta("Mr Coad", "blunt_hand", "the Carrier's Yard", 40, 16, 46, 1, "carrier"),
            ta("Mr Hollis", "blunt_hand", "the Crale estate", 42, 12, 43, 1, "gamekeeper"),
            ta("Old Burrow", "blunt_hand", "the Churchyard", 36, 6, 67, 1, "sexton"),
            ta("Mr Vye", "blunt_hand", "the Knacker's Yard", 30, 14, 48, 1, "knacker"),
            ta("Jeb Pascoe", "blunt_hand", "the docks at Plymouth", 34, 20, 30, 1, "docker (works away)"),
        ]);
        // kinship (indices match the comments above), and the warmth that comes with it
        let mut affinity: BTreeMap<(u32, u32), i16> = BTreeMap::new();
        for &(h, w) in &[(0, 1), (8, 9), (12, 13), (15, 16), (18, 19), (21, 22)] {
            agents[h].spouse = Some(w);
            agents[w].spouse = Some(h);
            affinity.insert((h as u32, w as u32), 38);
            affinity.insert((w as u32, h as u32), 38);
        }
        for &(child, parent) in &[(2, 0), (3, 0), (14, 12), (17, 15), (20, 18), (5, 4)] {
            agents[child].parent = Some(parent); // (5,4): Rupert is Lady Aldermaston's heir
            affinity.insert((child as u32, parent as u32), 30);
            affinity.insert((parent as u32, child as u32), 30);
        }
        // a seeded rivalry, so the town opens with something simmering
        affinity.insert((0, 4), -18); // Cynthia eyes Lady Aldermaston
        affinity.insert((6, 5), -22); // Tot's low opinion of Rupert's schemes

        // the children of the town — every household has its young (name, seat, age, sex, parent)
        let kid_specs: &[(&str, &str, i64, u8, usize)] = &[
            ("Sam Pelham", "The Laurels", 5, 1, 0),
            ("Tom Soames", "The Vicarage", 13, 1, 8),
            ("Milly Soames", "The Vicarage", 9, 0, 8),
            ("Tilly Sunter", "High Foldside", 14, 0, 12),
            ("Ned Sunter", "High Foldside", 10, 1, 12),
            ("Annie Metcalfe", "Gunnerside", 13, 0, 15),
            ("Georgie Metcalfe", "Gunnerside", 8, 1, 15),
            ("Rose Pringle", "Five Elms", 15, 0, 18),
            ("Phoebe Pringle", "Five Elms", 11, 0, 18),
            ("Bertie Bunce", "The Pelican", 12, 1, 21),
            ("Lottie Bunce", "The Pelican", 7, 0, 21),
            ("Dora Metcalfe", "Gunnerside", 5, 0, 16),
        ];
        for &(name, seat, age, sex, parent) in kid_specs {
            let mut c = a(name, "child", seat, 12, 0, age, sex);
            c.parent = Some(parent);
            let idx = agents.len();
            affinity.insert((idx as u32, parent as u32), 30);
            affinity.insert((parent as u32, idx as u32), 30);
            agents.push(c);
        }

        World {
            agents,
            animals: seed_animals(),
            news: Vec::new(),
            next_news_id: 0,
            affinity,
        }
    }

    fn agent_mut(&mut self, name: &str) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.name == name && a.death_day.is_none())
    }

    fn animal_idx(&self, name: &str) -> Option<usize> {
        self.animals.iter().position(|x| x.name == name)
    }

    fn idx(&self, name: &str) -> Option<usize> {
        self.agents.iter().position(|a| a.name == name)
    }

    /// Set a piece of news loose. `seeds` are the first knowers besides the subject.
    fn spawn_news(&mut self, subject: &str, topic: &str, valence: i32, day: i64, seeds: &[&str]) {
        let subject = match self.idx(subject) {
            Some(i) => i,
            None => return,
        };
        let mut knowers: Vec<usize> = seeds.iter().filter_map(|s| self.idx(s)).collect();
        if !knowers.contains(&subject) {
            knowers.push(subject);
        }
        let id = self.next_news_id;
        self.next_news_id += 1;
        self.news.push(News {
            id,
            subject,
            topic: topic.into(),
            valence,
            born: day,
            knowers,
            distortion: 0,
            applied: 0,
            broadcast: false,
        });
    }

    /// As `spawn_news`, but the subject is given by index (used by the life cycle, where
    /// the subject may have just died and a name lookup would be ambiguous).
    fn spawn_news_idx(&mut self, subject: usize, topic: &str, valence: i32, day: i64, seeds: &[usize]) {
        let mut knowers: Vec<usize> = seeds.to_vec();
        if !knowers.contains(&subject) {
            knowers.push(subject);
        }
        let id = self.next_news_id;
        self.next_news_id += 1;
        self.news.push(News {
            id,
            subject,
            topic: topic.into(),
            valence,
            born: day,
            knowers,
            distortion: 0,
            applied: 0,
            broadcast: false,
        });
    }
}

fn clamp_standing(a: &mut Agent, d: i32) {
    a.standing = (a.standing + d).clamp(0, 100);
}

// ----------------------------------------------------------------------------- events

#[derive(Clone)]
pub struct Event {
    pub day: i64,
    pub date: String,
    pub kind: String,
    pub actor: String,
    pub text: String,
}

fn rng_for(seed: u64, day: i64) -> ChaCha8Rng {
    let mix = seed ^ (day as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ChaCha8Rng::seed_from_u64(mix)
}

/// Advance the world by exactly one day, mutating it and returning that day's
/// events. Deterministic in (seed, day, world-state-from-prior-days).
/// Advance the world by one *phase* (a fifth of a day). Systems are scheduled to when
/// they'd really happen, so beats fall across the day instead of in a midnight lump.
/// Deterministic in (seed, day, phase, world-state-from-prior-slots).
fn step_slot(world: &mut World, day: i64, phase: Phase, date: Date, seed: u64, engine: &dyn PolicyEngine, interventions: &BTreeMap<i64, Vec<Intervention>>, weather: &BTreeMap<i64, DayWeather>) -> Vec<Event> {
    let mut out = Vec::new();
    let mk = |kind: &str, actor: &str, text: String| Event { day, date: date.to_string(), kind: kind.into(), actor: actor.into(), text };

    // --- the morning: weather, the stock, the day's circumstance ---
    if matches!(phase, Phase::Dawn) {
        if day == 0 {
            out.push(mk("household", "Mrs Cynthia Pelham", "Cook has given notice — the fifth time this year.".into()));
            out.push(mk("scheme", "Mr Rupert Crale", "A new tractor arrived at the Home Farm, which Rupert cannot drive.".into()));
            out.push(mk("bureaucracy", "Revd Mr Soames", "A tithe demand for High Foldside sits in the Vicar's out-tray.".into()));
            world.spawn_news("Mr Rupert Crale", "Rupert's grand tractor he cannot drive", -2, day, &["Tot Wragg"]);
            world.spawn_news("Mr Sunter", "the tithe demand fallen on High Foldside", -1, day, &["Revd Mr Soames"]);
        }
        out.extend(animal_events(world, day, date, seed));
        if let Some(list) = interventions.get(&day) {
            out.extend(apply_interventions(world, day, date, list));
        }
        out.extend(seasonal_shock(world, day, date, seed, weather.get(&day).copied()));
    }

    // --- the afternoon: the set-pieces ---
    if matches!(phase, Phase::Afternoon) && date.month() == Month::June && date.day() == 14 {
        if let Some(l) = world.agent_mut("Lady Aldermaston") { clamp_standing(l, 3); }
        if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") {
            clamp_standing(c, 1);
            c.purse -= 8;
        }
        out.push(mk("party", "Lady Aldermaston",
            "Lady Aldermaston's garden party at Crale Court. Mrs Pelham attended in a made-over frock and a brave face.".into()));
        world.spawn_news("Lady Aldermaston", "Lady Aldermaston's splendid garden party", 1, day, &[]);
        world.spawn_news("Mrs Cynthia Pelham", "Mrs Pelham's made-over frock at the party", -1, day, &["Lady Aldermaston"]);
    }

    // --- the behaviour layer: whoever is out and about this phase acts in character ---
    behaviour_phase(world, day, phase, date, seed, engine, &mut out);

    // --- the forenoon hub: feuds and friendships at the market & church door ---
    if matches!(phase, Phase::Forenoon) {
        out.extend(relationship_events(world, day, date, seed));
    }

    // --- nightfall: the slow turn of the cast, and the day's news settling ---
    if matches!(phase, Phase::Night) {
        out.extend(life_tick(world, day, date, seed));
        out.extend(diffuse(world, day, date, seed));
    }

    out
}

fn behaviour_phase(world: &mut World, day: i64, phase: Phase, date: Date, seed: u64, engine: &dyn PolicyEngine, out: &mut Vec<Event>) {
    let top = world.agents.iter().filter(|a| a.active()).map(|a| a.standing).max().unwrap_or(0);
    let actors: Vec<usize> = (0..world.agents.len())
        .filter(|&i| world.agents[i].active() && acts_in_phase(&world.agents[i].archetype, phase))
        .collect();
    for i in actors {
        let obs = observe(world, i, day, date, top, seed, phase);
        let action = engine.decide(&world.agents[i].archetype, &obs);
        if !matches!(action, Action::Idle) {
            arbitrate(world, i, action, day, date, phase, out, seed);
        }
    }
}

// ----------------------------------------------------------------------------- behaviour
//
// The policy layer. Every present adult decides a day's intention from a flat
// Observation and returns an Action the host arbitrates. This is the contract a WASM
// guest would implement verbatim: `decide(observation) -> action`, one module per
// archetype, host materialises the observation and applies the action. Today the
// policies are native Rust behind that boundary; swapping in wasmtime is a substrate
// change, not a redesign. Crucially it is *generative for any holder of a role* — a
// great-grandchild who inherits The Laurels behaves in character with no new code.

/// What an agent can see of the world on a given day. Flat and self-contained.
pub struct Observation {
    pub standing: i32,
    pub purse: i32,
    pub age: i64,
    pub married: bool,
    pub season: Season,
    pub is_market: bool, // Wednesday
    pub is_sunday: bool,
    pub top_standing: i32, // the grandest in town — the bar status is measured against
    pub rng: u64,          // per-agent, per-day deterministic seed for stochastic choice
}

/// A day's intention. Most days are `Idle` (routine, no beat).
pub enum Action {
    Idle,
    PayCall,    // genteel: cheap standing
    GiveDinner, // genteel: standing up, purse down — face bought with money
    Economise,  // genteel: purse up, a small loss of face
    KeepUp,     // genteel: spend to hold standing
    TendStock,  // farmer
    Haggle,     // farmer: deal at market
    Graft,      // hand: the work done, the master deflated
    Scheme,     // improver: risky, gain or grief
    Press,      // official: tithe / inspection / the law
    Minister,   // official/parson: the parish in good order
    Round,      // vet: the rounds, the connector
}

fn observe(world: &World, i: usize, day: i64, date: Date, top: i32, seed: u64, phase: Phase) -> Observation {
    let a = &world.agents[i];
    Observation {
        standing: a.standing,
        purse: a.purse,
        age: a.age(day),
        married: a.spouse.is_some(),
        season: Season::of(date),
        is_market: date.weekday() == Weekday::Wednesday,
        is_sunday: date.weekday() == Weekday::Sunday,
        top_standing: top,
        rng: seed
            ^ 0xB6E1_0000_0000
            ^ ((day * PHASES + phase.ord()) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (i as u64).wrapping_mul(0xD1B5_4A32_D192_ED03),
    }
}

impl Action {
    /// The stable action ordinal shared with the policy crates / wasm guests.
    pub fn from_i32(n: i32) -> Action {
        match n {
            1 => Action::PayCall,
            2 => Action::GiveDinner,
            3 => Action::Economise,
            4 => Action::KeepUp,
            5 => Action::TendStock,
            6 => Action::Haggle,
            7 => Action::Graft,
            8 => Action::Scheme,
            9 => Action::Press,
            10 => Action::Minister,
            11 => Action::Round,
            _ => Action::Idle,
        }
    }

    /// A present-tense gloss of what the agent is about.
    pub fn label(&self) -> &'static str {
        match self {
            Action::Idle => "about the day's round",
            Action::PayCall => "paying calls",
            Action::GiveDinner => "giving a dinner",
            Action::Economise => "economising",
            Action::KeepUp => "keeping up appearances",
            Action::TendStock => "tending the stock",
            Action::Haggle => "dealing at the mart",
            Action::Graft => "at the work",
            Action::Scheme => "hatching a scheme",
            Action::Press => "pressing the forms",
            Action::Minister => "about the parish",
            Action::Round => "on the rounds",
        }
    }
}

/// The five phases of the day; in companion mode the current one is read off the clock.
#[derive(Clone, Copy)]
pub enum Phase {
    Dawn,
    Forenoon,
    Afternoon,
    Evening,
    Night,
}

/// Phases per day — the granularity of a simulation step.
pub const PHASES: i64 = 5;

impl Phase {
    pub fn from_hour(h: u8) -> Phase {
        match h {
            5..=7 => Phase::Dawn,
            8..=11 => Phase::Forenoon,
            12..=16 => Phase::Afternoon,
            17..=21 => Phase::Evening,
            _ => Phase::Night,
        }
    }
    pub fn ord(self) -> i64 {
        match self {
            Phase::Dawn => 0,
            Phase::Forenoon => 1,
            Phase::Afternoon => 2,
            Phase::Evening => 3,
            Phase::Night => 4,
        }
    }
    pub fn from_ord(o: i64) -> Phase {
        match o.rem_euclid(PHASES) {
            0 => Phase::Dawn,
            1 => Phase::Forenoon,
            2 => Phase::Afternoon,
            3 => Phase::Evening,
            _ => Phase::Night,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Phase::Dawn => "dawn",
            Phase::Forenoon => "forenoon",
            Phase::Afternoon => "afternoon",
            Phase::Evening => "evening",
            Phase::Night => "night",
        }
    }
}

/// When in the day each archetype is out and about and liable to generate a beat —
/// the routine table as a behaviour gate. Gentry in the afternoon and evening; the
/// working town in the forenoon; farmers also at dawn.
fn acts_in_phase(arch: &str, phase: Phase) -> bool {
    matches!(
        (arch, phase),
        ("genteel_status_seeker", Phase::Afternoon | Phase::Evening)
            | ("hill_farmer", Phase::Dawn | Phase::Forenoon)
            | ("scheming_improver", Phase::Dawn | Phase::Forenoon)
            | ("practitioner", Phase::Forenoon | Phase::Afternoon)
            | ("blunt_hand", Phase::Forenoon)
            | ("official", Phase::Forenoon)
    )
}

/// Where an agent is this phase, from the routine table — market day and Sunday pull the
/// whole town to the square and the church.
fn placement(a: &Agent, phase: Phase, wd: Weekday) -> String {
    let home = a.seat.clone();
    let sunday = wd == Weekday::Sunday;
    let market = wd == Weekday::Wednesday;
    let pubnight = matches!(wd, Weekday::Friday | Weekday::Saturday);
    if sunday && matches!(phase, Phase::Forenoon) {
        return "the church".into();
    }
    match a.archetype.as_str() {
        "genteel_status_seeker" => match phase {
            Phase::Dawn => home,
            Phase::Forenoon => if market { "the market square".into() } else { "at the writing-desk".into() },
            Phase::Afternoon => "paying calls about the parish".into(),
            Phase::Evening | Phase::Night => home,
        },
        "hill_farmer" | "scheming_improver" => match phase {
            Phase::Dawn => "the byre".into(),
            Phase::Forenoon => if market { "the market".into() } else { "the fields".into() },
            Phase::Afternoon => "the fields".into(),
            Phase::Evening => if pubnight { "The Pelican".into() } else { home },
            Phase::Night => home,
        },
        "practitioner" => match phase {
            Phase::Dawn => "the surgery".into(),
            Phase::Forenoon | Phase::Afternoon => "on the rounds".into(),
            Phase::Evening => if pubnight { "The Pelican".into() } else { "the surgery".into() },
            Phase::Night => "on call".into(),
        },
        "blunt_hand" => match phase {
            Phase::Dawn => "the yard".into(),
            // a tradesperson is at their own place of work; a hired hand is "about the town"
            Phase::Forenoon | Phase::Afternoon => if a.trade.is_some() { home } else { "at work about the town".into() },
            Phase::Evening => "The Pelican".into(),
            Phase::Night => a.seat.clone(),
        },
        "official" => match phase {
            Phase::Dawn => "the study".into(),
            Phase::Forenoon | Phase::Afternoon => if a.trade.is_some() { home } else { "on parish visits".into() },
            Phase::Evening => "the vestry".into(),
            Phase::Night => a.seat.clone(),
        },
        "child" => match phase {
            Phase::Forenoon | Phase::Afternoon => "the school".into(),
            _ => home,
        },
        _ => home,
    }
}

/// What an agent is *about* when they aren't doing anything notable — the routine verb for
/// the phase, so "doing now" never reads as idle while the placement implies activity.
fn routine_doing(a: &Agent, phase: Phase, wd: Weekday) -> String {
    if wd == Weekday::Sunday && matches!(phase, Phase::Forenoon) {
        return "at church".into();
    }
    let trade_verb = |t: &str| match t {
        "baker" => "at the oven",
        "butcher" => "at the block",
        "miller" => "at the wheel",
        "postmistress" => "at the counter",
        "dressmaker" => "at her needle",
        "carrier" => "on the road",
        "stationmaster" | "railway porter" => "at the station",
        "schoolmistress" => "hearing lessons",
        "gamekeeper" => "out on the estate",
        "sexton" => "tending the churchyard",
        "knacker" => "about his grim trade",
        "solicitor" | "bank manager" => "at his desk",
        _ => "at their trade",
    };
    match a.archetype.as_str() {
        "genteel_status_seeker" => match phase {
            Phase::Afternoon => "paying calls",
            Phase::Evening => "at dinner",
            _ => "at home",
        }
        .into(),
        "hill_farmer" | "scheming_improver" => match phase {
            Phase::Dawn => "at the milking",
            Phase::Forenoon => if wd == Weekday::Wednesday { "at the market" } else { "in the fields" },
            Phase::Afternoon => "in the fields",
            Phase::Evening => "by the fire",
            Phase::Night => "abed",
        }
        .into(),
        "practitioner" => "on the rounds".into(),
        "official" => a.trade.as_deref().map(trade_verb).unwrap_or("about the parish").into(),
        "blunt_hand" => match phase {
            Phase::Evening => "at the Pelican".into(),
            _ => a.trade.as_deref().map(trade_verb).unwrap_or("at their work").into(),
        },
        "child" => "at lessons and mischief".into(),
        _ => "about the day".into(),
    }
}

pub fn season_ord(s: Season) -> i32 {
    match s {
        Season::Winter => 0,
        Season::Lambing => 1,
        Season::Sowing => 2,
        Season::Hay => 3,
        Season::Harvest => 4,
        Season::Mart => 5,
    }
}

/// How an agent's behaviour is computed. The native engine runs the policies in-process;
/// a wasm engine (in the host binary) routes archetypes to sandboxed guest modules behind
/// this exact boundary. `decide(observation) -> action`, nothing else crosses.
pub trait PolicyEngine {
    fn decide(&self, archetype: &str, o: &Observation) -> Action;
}

/// The in-process engine. Genteel runs the shared `policy-genteel` crate — the very code
/// the wasm guest also compiles — so native and wasm decisions are identical.
pub struct NativePolicies;

impl PolicyEngine for NativePolicies {
    fn decide(&self, archetype: &str, o: &Observation) -> Action {
        decide(archetype, o)
    }
}

/// The behaviour-layer archetypes, by ordinal (shared with the policy crate / wasm guest).
pub fn arch_ord(archetype: &str) -> i32 {
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

/// The pooled native policy — every archetype computed by the shared `policies` crate (the
/// very code the wasm guest also runs), so native and wasm decisions agree bit-for-bit.
fn decide(archetype: &str, o: &Observation) -> Action {
    let ord = arch_ord(archetype);
    if ord < 0 {
        return Action::Idle;
    }
    Action::from_i32(policies::decide(
        ord,
        o.standing,
        o.purse,
        o.age,
        o.married as i32,
        season_ord(o.season),
        o.is_market as i32,
        o.is_sunday as i32,
        o.top_standing,
        o.rng,
    ))
}

/// Apply an action: mutate the world, emit a chronicle beat, and (for the juicy ones)
/// set news loose. The actor is named, so descendants generate beats too.
fn arbitrate(world: &mut World, i: usize, action: Action, day: i64, date: Date, phase: Phase, out: &mut Vec<Event>, seed: u64) {
    let mut rng = rng_for(seed ^ 0xA7B1_0000_0000, (day * PHASES + phase.ord()) ^ (i as i64).rotate_left(17));
    let name = world.agents[i].name.clone();
    let seat = world.agents[i].seat.clone();
    let mk = |kind: &str, text: String| Event {
        day,
        date: date.to_string(),
        kind: kind.into(),
        actor: name.clone(),
        text,
    };
    match action {
        Action::PayCall => {
            clamp_standing(&mut world.agents[i], 2);
            out.push(mk("status", format!("{name} paid a round of calls, and was thought to look very well.")));
        }
        Action::GiveDinner => {
            clamp_standing(&mut world.agents[i], 3);
            world.agents[i].purse -= 6;
            out.push(mk("status", format!("{name} gave a little dinner — rather beyond the means of {seat}, but handsomely done.")));
            world.spawn_news(&name, &format!("{name}'s handsome little dinner"), 2, day, &[]);
        }
        Action::Economise => {
            world.agents[i].purse += 4;
            clamp_standing(&mut world.agents[i], -1);
            out.push(mk("household", format!("{name} made do and mended, and hoped no one would notice the turned collar.")));
            if rng.gen_bool(0.4) {
                world.spawn_news(&name, &format!("the straitened economies at {seat}"), -2, day, &[]);
            }
        }
        Action::KeepUp => {
            world.agents[i].purse -= 4;
            clamp_standing(&mut world.agents[i], 1);
            out.push(mk("status", format!("{name} kept up appearances, whatever the bank might think of it.")));
        }
        Action::TendStock => {
            out.push(mk("practice", format!("{name} was out among the stock before light.")));
        }
        Action::Haggle => {
            let good = rng.gen_bool(0.55);
            world.agents[i].purse += if good { 6 } else { -2 };
            out.push(mk("market", if good {
                format!("{name} drove a hard bargain at the mart and came home pleased.")
            } else {
                format!("{name} found the mart slow, and the buyers slower.")
            }));
        }
        Action::Graft => {
            out.push(mk("household", format!("{name} got the work done, and said little about how.")));
        }
        Action::Scheme => {
            let win = rng.gen_bool(0.45);
            if win {
                world.agents[i].purse += 8;
                clamp_standing(&mut world.agents[i], 2);
                out.push(mk("scheme", format!("{name}'s latest improvement actually answered, to general astonishment.")));
                world.spawn_news(&name, &format!("{name}'s scheme that, against all odds, worked"), 2, day, &[]);
            } else {
                world.agents[i].purse -= 7;
                clamp_standing(&mut world.agents[i], -2);
                out.push(mk("scheme", format!("{name}'s latest improvement came to grief in the mud. Tot, or his like, had said it would.")));
                world.spawn_news(&name, &format!("{name}'s improvement come to grief"), -2, day, &[]);
            }
        }
        Action::Press => {
            out.push(mk("bureaucracy", format!("{name} came round with a form and a fixed, courteous smile.")));
            world.spawn_news(&name, &format!("{name} and his ruinous bit of paper"), -1, day, &[]);
        }
        Action::Minister => {
            clamp_standing(&mut world.agents[i], 1);
            out.push(mk("household", format!("{name} kept the parish in good order, and the sermon to a decent length.")));
        }
        Action::Round => {
            out.push(mk("practice", format!("{name} drove the rounds from farm to farm, carrying the news from door to door.")));
        }
        Action::Idle => {}
    }
}

// ----------------------------------------------------------------------------- providence
//
// The player is the novelist: not a god-game, but a source of *circumstance*. Each verb is
// an event the world recognises — a letter, a called loan, a legacy, a scandal, a stranger
// at the empty cottage — injected at a day, folded deterministically, and the autonomous
// agents react to it in character.

#[derive(Clone)]
pub struct Intervention {
    pub kind: String,
    pub target: String,
    pub amount: i32,
    pub note: String,
}

/// A day's real weather (Sofia), the companion-mode resonance: your actual sky drives the
/// town's shocks. A recorded external input — fetched once, stored, folded deterministically.
#[derive(Clone, Copy)]
pub struct DayWeather {
    pub precip: f64, // mm
    pub tmax: f64,   // °C
    pub tmin: f64,
}

/// Apply the day's providence to the world, returning the beats it sets down.
fn apply_interventions(world: &mut World, day: i64, date: Date, list: &[Intervention]) -> Vec<Event> {
    let mut out = Vec::new();
    let mk = |kind: &str, actor: &str, text: String| Event { day, date: date.to_string(), kind: kind.into(), actor: actor.into(), text };
    for iv in list {
        let t = &iv.target;
        match iv.kind.as_str() {
            "letter" => {
                let what = if iv.note.is_empty() { "a letter, postmarked far away".to_string() } else { iv.note.clone() };
                out.push(mk("providence", t, format!("A letter came for {t} — {what}.")));
                world.spawn_news(t, &format!("the letter that came for {t}"), iv.amount.signum().max(0) + 1, day, &[]);
            }
            "loan" => {
                let amt = if iv.amount > 0 { iv.amount } else { 40 };
                if let Some(a) = world.agent_mut(t) { a.purse -= amt; }
                out.push(mk("providence", t, format!("The bank called in {t}'s loan — £{amt}, and no putting it off.")));
                world.spawn_news(t, &format!("the bank calling in {t}'s loan"), -2, day, &[]);
            }
            "legacy" => {
                let amt = if iv.amount > 0 { iv.amount } else { 60 };
                if let Some(a) = world.agent_mut(t) { a.purse += amt; clamp_standing(a, 2); }
                out.push(mk("providence", t, format!("A legacy of £{amt} fell to {t}, from a relation barely remembered.")));
                world.spawn_news(t, &format!("the legacy come to {t}"), 2, day, &[]);
            }
            "scandal" => {
                let what = if iv.note.is_empty() { format!("something concerning {t}") } else { iv.note.clone() };
                out.push(mk("providence", t, format!("An ugly whisper began to go round — {what}.")));
                world.spawn_news(t, &what, -3, day, &[]);
            }
            "stranger" => {
                let name = if iv.note.is_empty() { "A stranger".to_string() } else { iv.note.clone() };
                let mut agent = make_agent(&name, "blunt_hand", "the empty cottage", 25, 12, 1, 33, day);
                agent.origin = Some("parts unknown".into());
                out.push(mk("providence", &name, format!("{name} arrived in Thrushcombe and took the empty cottage. Nobody knew quite who they were.")));
                world.agents.push(agent);
            }
            other => {
                out.push(mk("providence", t, format!("Providence ({other}) touched {t}.")));
            }
        }
    }
    out
}

fn farmers_of(world: &World) -> Vec<usize> {
    (0..world.agents.len())
        .filter(|&i| world.agents[i].active() && matches!(world.agents[i].archetype.as_str(), "hill_farmer" | "scheming_improver"))
        .collect()
}

/// The external-shock layer. With real weather (Sofia), the sky drives it deterministically;
/// otherwise the season rolls its own dice. Either way only what the season has armed.
fn seasonal_shock(world: &mut World, day: i64, date: Date, seed: u64, wx: Option<DayWeather>) -> Vec<Event> {
    match wx {
        Some(w) => weather_shock(world, day, date, w),
        None => rng_shock(world, day, date, seed),
    }
}

/// Real Sofia weather → the town's day. Hard rain rots the hay/harvest; heat burns the
/// grass; a hard frost takes lambs or freezes the pump.
fn weather_shock(world: &mut World, day: i64, date: Date, w: DayWeather) -> Vec<Event> {
    let mut out = Vec::new();
    let farmers = farmers_of(world);
    let mk = |kind: &str, text: String| Event { day, date: date.to_string(), kind: kind.into(), actor: "Thrushcombe".into(), text };
    let season = Season::of(date);
    if w.precip >= 12.0 {
        match season {
            Season::Hay => {
                for &f in &farmers { world.agents[f].purse -= 3; }
                out.push(mk("weather", "A hard rain came over the tops and flattened the cut hay across the dale — a bad day for everyone with grass down.".into()));
            }
            Season::Harvest => {
                for &f in &farmers { world.agents[f].purse -= 3; }
                out.push(mk("weather", "The rain set in over the harvest, and the corn stood sprouting in the stook.".into()));
            }
            _ => {
                out.push(mk("weather", "Rain all day; the becks ran high and the lane to the church was a river.".into()));
            }
        }
    } else if w.tmax >= 32.0 && matches!(season, Season::Sowing | Season::Hay | Season::Harvest) {
        for &f in &farmers { world.agents[f].purse -= 1; }
        out.push(mk("weather", "A scorching day — the grass burnt brown and the becks shrank to a trickle.".into()));
    } else if w.tmin <= -6.0 {
        match season {
            Season::Lambing | Season::Sowing => {
                for &f in &farmers { world.agents[f].purse -= 2; }
                out.push(mk("weather", "A hard frost in the night; the lambs suffered for it and the early sowing with them.".into()));
            }
            Season::Winter => {
                out.push(mk("weather", "Bitter cold — the pump froze solid and the milk to ice in the churn.".into()));
            }
            _ => {
                out.push(mk("weather", "A sharp, unseasonable frost whitened the fields by dawn.".into()));
            }
        }
    }
    out
}

/// The season's own dice, when there's no recorded weather (backdated runs, future days
/// past the forecast). Unchanged from before, so weather-free worlds fold identically.
fn rng_shock(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let mut rng = rng_for(seed ^ 0x5403_0000_0000, day);
    if !rng.gen_bool(0.04) {
        return out;
    }
    let farmers = farmers_of(world);
    let mk = |kind: &str, text: String| Event { day, date: date.to_string(), kind: kind.into(), actor: "Thrushcombe".into(), text };
    match Season::of(date) {
        Season::Hay => {
            for &f in &farmers {
                world.agents[f].purse -= 3;
            }
            out.push(mk("weather", "A storm came over the tops and flattened the cut hay across the dale. A bad day for everyone with grass down.".into()));
        }
        Season::Harvest => {
            for &f in &farmers {
                world.agents[f].purse -= 3;
            }
            out.push(mk("weather", "Wet weather set in over the harvest, and the corn stood sprouting in the stook.".into()));
        }
        Season::Mart => {
            for &f in &farmers {
                world.agents[f].purse -= 2;
            }
            out.push(mk("market", "The Board cut the price again, and every farmer in the room did the same sum and reached the same gloom.".into()));
        }
        Season::Winter => {
            out.push(mk("bureaucracy", "The tithe and the winter bills fell due together, as they always contrive to.".into()));
        }
        _ => {}
    }
    out
}

// ----------------------------------------------------------------------------- life cycle

const FIRST_M: &[&str] = &["Albert", "Walter", "Cecil", "Harold", "Edmund", "Frank", "Stanley", "Arthur", "Reggie", "Cyril"];
const FIRST_F: &[&str] = &["Edith", "Mabel", "Dorothy", "Phyllis", "Constance", "Beatrice", "Winifred", "Nora", "Hilda", "Margery"];
const SURNAMES: &[&str] = &["Thorpe", "Bramley", "Critchlow", "Hollis", "Pennyfeather", "Garstang", "Wickett", "Mossop", "Treloar", "Fenwick"];

fn pick<'a>(rng: &mut ChaCha8Rng, xs: &'a [&str]) -> &'a str {
    xs[rng.gen_range(0..xs.len())]
}

/// What an heir or incomer becomes, by the stratum they step into.
fn stratum_archetype(arch: &str) -> String {
    match arch {
        "official" => "official",
        "practitioner" => "practitioner",
        "hill_farmer" => "hill_farmer",
        "blunt_hand" => "blunt_hand",
        _ => "genteel_status_seeker",
    }
    .to_string()
}

/// Annual probability of death by age — gentle until it isn't.
fn hazard(age: i64) -> f64 {
    match age {
        a if a < 50 => 0.004,
        a if a < 65 => 0.018,
        a if a < 75 => 0.045,
        a if a < 85 => 0.11,
        _ => 0.24,
    }
}

fn an(name: &str, owner: &str, species: &str, health: i32, gest: i32, value: i32) -> Animal {
    Animal { name: name.into(), owner: owner.into(), species: species.into(), health, gest, value }
}

/// The town's notable beasts — first-class entities that can make or ruin a day.
fn seed_animals() -> Vec<Animal> {
    vec![
        an("Strawberry", "Mr Sunter", "shorthorn cow", 68, 4, 45), // in difficult calf before the Show
        an("Bluebell", "Mr Sunter", "shorthorn cow", 74, -1, 38),
        an("Captain", "Mr Rupert Crale", "carthorse", 80, -1, 30), // the homicidal one
        an("Floss", "Mr Metcalfe", "sheepdog", 88, -1, 8),
        an("Duchess", "Mr Metcalfe", "ewe", 70, 30, 12),
        an("the Major's hunter", "Major Pringle", "hunter", 76, -1, 60),
        an("Mr Pickering's spaniel", "Mr Pickering", "spaniel", 60, -1, 5), // over-fed
        an("Boxer", "Mr Garth", "dray horse", 72, -1, 28),
    ]
}

fn make_agent(name: &str, arch: &str, seat: &str, standing: i32, purse: i32, sex: u8, age: i64, day: i64) -> Agent {
    Agent {
        name: name.into(),
        archetype: arch.into(),
        seat: seat.into(),
        standing: standing.clamp(0, 100),
        purse,
        birth_day: day - age * 365,
        sex,
        death_day: None,
        departed: false,
        spouse: None,
        parent: None,
        origin: None,
        trade: None,
    }
}

/// What an adult of a given station arrives with — nobody starts on nothing.
fn starting_purse(arch: &str, rng: &mut ChaCha8Rng) -> i32 {
    match arch {
        "genteel_status_seeker" => rng.gen_range(60..220),
        "practitioner" => rng.gen_range(40..110),
        "official" => rng.gen_range(25..70),
        "hill_farmer" => rng.gen_range(12..45),
        "scheming_improver" => rng.gen_range(-15..40),
        _ => rng.gen_range(4..20), // working folk
    }
}

const ORIGINS: &[&str] = &[
    "Exeter", "Taunton", "the next valley", "Bristol", "away north", "the coast", "Tiverton", "London", "the shires",
];

/// Mint an outsider arriving in Thrushcombe — working folk for the most part, with an
/// origin set so the town remembers they came from away.
fn new_incomer(rng: &mut ChaCha8Rng, day: i64) -> Agent {
    let roles = ["blunt_hand", "blunt_hand", "blunt_hand", "hill_farmer", "genteel_status_seeker", "official", "practitioner"];
    let arch = roles[rng.gen_range(0..roles.len())];
    let sex = if rng.gen_bool(0.5) { 1 } else { 0 };
    let first = if sex == 1 { pick(rng, FIRST_M) } else { pick(rng, FIRST_F) };
    let title = if sex == 1 { "Mr" } else { "Miss" };
    let name = format!("{title} {first} {}", pick(rng, SURNAMES));
    let purse = starting_purse(arch, rng);
    let mut a = make_agent(&name, arch, "a cottage in the town", rng.gen_range(20..45), purse, sex, rng.gen_range(22..50), day);
    a.origin = Some(pick(rng, ORIGINS).to_string());
    a
}

/// Who inherits a dead agent's seat: the living spouse, else the eldest living child,
/// else nobody (an incomer is generated by the caller).
fn eldest_active_child(world: &World, parent: usize) -> Option<usize> {
    let mut kids: Vec<usize> = (0..world.agents.len())
        .filter(|&j| world.agents[j].parent == Some(parent) && world.agents[j].active())
        .collect();
    kids.sort_by_key(|&j| world.agents[j].birth_day); // oldest first
    kids.first().copied()
}

fn find_heir(world: &World, dead: usize) -> Option<usize> {
    if let Some(sp) = world.agents[dead].spouse {
        if world.agents[sp].active() {
            return Some(sp);
        }
    }
    eldest_active_child(world, dead)
}

/// Birth, marriage, ageing, death, succession — the slow turn of the cast that makes a
/// run *history* rather than a loop. Runs once a day on its own RNG stream.
fn life_tick(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let mut rng = rng_for(seed ^ 0x11FE_0000_0000, day);
    let n = world.agents.len();
    let pop = world.agents.iter().filter(|a| a.active() && a.archetype != "child").count(); // adults — kids are additional
    let mut newcomers: Vec<Agent> = Vec::new();
    let mk = |kind: &str, actor: &str, text: String| Event {
        day,
        date: date.to_string(),
        kind: kind.into(),
        actor: actor.into(),
        text,
    };

    // --- deaths & succession ---
    let mut died = Vec::new();
    for i in 0..n {
        let ag = &world.agents[i];
        if !ag.active() || ag.archetype == "child" {
            continue;
        }
        if rng.gen_bool((hazard(ag.age(day)) / 365.0).clamp(0.0, 1.0)) {
            died.push(i);
        }
    }
    for i in died {
        let name = world.agents[i].name.clone();
        let seat = world.agents[i].seat.clone();
        let standing = world.agents[i].standing;
        let arch = world.agents[i].archetype.clone();
        let estate = world.agents[i].purse.max(0); // the capital that passes on (not the debts)
        world.agents[i].death_day = Some(day);
        out.push(mk("death", &name, format!("{name}, of {seat}, is dead.")));
        world.spawn_news_idx(i, &format!("the death of {name}"), 0, day, &[]);
        if let Some(sp) = world.agents[i].spouse {
            if world.agents[sp].alive() {
                world.agents[sp].spouse = None; // widowed
            }
        }
        match find_heir(world, i) {
            Some(h) => {
                world.agents[h].seat = seat.clone();
                world.agents[h].standing = (standing - 8).max(world.agents[h].standing).clamp(0, 100);
                world.agents[h].purse += estate; // inherit the estate's capital
                if world.agents[h].archetype == "child" {
                    world.agents[h].archetype = stratum_archetype(&arch);
                }
                // keep the bloodline in the line of succession: the dead's children now
                // look to the new holder of the seat (so an inheriting widow doesn't strand them)
                for c in 0..world.agents.len() {
                    if c != h && world.agents[c].alive() && world.agents[c].parent == Some(i) {
                        world.agents[c].parent = Some(h);
                    }
                }
                let hname = world.agents[h].name.clone();
                out.push(mk("succession", &hname, format!("{seat} passes to {hname}.")));
                world.spawn_news(&hname, &format!("{hname} coming into {seat}"), 2, day, &[]);
            }
            None => {
                let hname = format!("Mr {} {}", pick(&mut rng, FIRST_M), pick(&mut rng, SURNAMES));
                let st = stratum_archetype(&arch);
                let purse = estate + starting_purse(&st, &mut rng); // the estate, plus their own means
                let mut heir = make_agent(&hname, &st, &seat, (standing - 12).max(20), purse, 1, 34, day);
                let from = pick(&mut rng, ORIGINS).to_string();
                heir.origin = Some(from.clone());
                out.push(mk("succession", &hname, format!("{seat} passes to {hname}, a relation lately come from {from}.")));
                newcomers.push(heir);
            }
        }
    }

    // --- coming of age ---
    // The eldest child of a seated parent is the heir and stays on; the rest go out into
    // the world (married away, a situation in town). This keeps the tracked cast a small
    // turning-over principal set, not an exploding census.
    for i in 0..n {
        if !(world.agents[i].active() && world.agents[i].archetype == "child" && world.agents[i].age(day) >= 18) {
            continue;
        }
        let parent = world.agents[i].parent;
        let is_heir = parent.is_some_and(|p| world.agents[p].active() && eldest_active_child(world, p) == Some(i));
        let nm = world.agents[i].name.clone();
        // when the town is full, more of the young go out into the world
        let stay_roll = if pop > SOFT_CAP { 0.92 } else { 0.80 };
        if is_heir || !rng.gen_bool(stay_roll) {
            let parent_arch = parent
                .map(|p| world.agents[p].archetype.clone())
                .unwrap_or_else(|| "genteel_status_seeker".into());
            let arch = stratum_archetype(&parent_arch);
            world.agents[i].purse += starting_purse(&arch, &mut rng); // a settlement on coming of age
            world.agents[i].archetype = arch;
            out.push(mk("comingofage", &nm, format!("{nm} is grown now, and takes a place in the town.")));
        } else {
            world.agents[i].departed = true;
            out.push(mk("departure", &nm, format!("{nm} is grown, and gone out into the world beyond Thrushcombe.")));
        }
    }

    // --- marriage (at most one a day, to keep it gentle) ---
    let elig: Vec<usize> = (0..n)
        .filter(|&i| {
            let a = &world.agents[i];
            // marrying age; the elderly don't generally remarry in this model (and a late
            // remarriage shouldn't quietly disinherit a bloodline)
            a.active() && a.archetype != "child" && a.spouse.is_none() && (18..=50).contains(&a.age(day))
        })
        .collect();
    for &i in &elig {
        if world.agents[i].spouse.is_some() {
            continue;
        }
        if !rng.gen_bool((0.22 / 365.0_f64).clamp(0.0, 1.0)) {
            continue;
        }
        let partner = elig.iter().copied().find(|&j| {
            j != i
                && world.agents[j].spouse.is_none()
                && world.agents[j].sex != world.agents[i].sex
                && (world.agents[j].age(day) - world.agents[i].age(day)).abs() <= 16
        });
        match partner {
            Some(j) => {
                world.agents[i].spouse = Some(j);
                world.agents[j].spouse = Some(i);
                let (ni, nj) = (world.agents[i].name.clone(), world.agents[j].name.clone());
                let cross = stratum_archetype(&world.agents[i].archetype) != stratum_archetype(&world.agents[j].archetype);
                let note = if cross { " — a match that set tongues wagging across the class line" } else { "" };
                out.push(mk("marriage", &ni, format!("{ni} and {nj} are to be married{note}.")));
                world.spawn_news(&ni, &format!("the engagement of {ni} and {nj}"), if cross { -2 } else { 1 }, day, &[]);
            }
            None => {
                let osex = 1 - world.agents[i].sex;
                let (first, title) = if osex == 1 {
                    (pick(&mut rng, FIRST_M), "Mr")
                } else {
                    (pick(&mut rng, FIRST_F), "Miss")
                };
                let sname = format!("{title} {first} {}", pick(&mut rng, SURNAMES));
                let idx_new = n + newcomers.len();
                let age = world.agents[i].age(day);
                let sp_arch = world.agents[i].archetype.clone();
                let sp_purse = starting_purse(&sp_arch, &mut rng);
                let mut sp = make_agent(&sname, &sp_arch, &world.agents[i].seat.clone(),
                    (world.agents[i].standing - 5).max(20), sp_purse, osex, age, day);
                sp.spouse = Some(i);
                sp.origin = Some(pick(&mut rng, ORIGINS).to_string());
                let from = sp.origin.clone().unwrap();
                world.agents[i].spouse = Some(idx_new);
                let ni = world.agents[i].name.clone();
                out.push(mk("marriage", &ni, format!("{ni} is to wed {sname}, lately of {from}.")));
                newcomers.push(sp);
                world.spawn_news(&ni, &format!("{ni}'s engagement to {sname}"), 1, day, &[]);
            }
        }
        break;
    }

    // --- births (gentle, and capped per household so the town doesn't balloon) ---
    for i in 0..n {
        let (active, sex, child, spouse, age, standing, seat, mother_name) = {
            let a = &world.agents[i];
            (a.active(), a.sex, a.archetype == "child", a.spouse, a.age(day), a.standing, a.seat.clone(), a.name.clone())
        };
        if active && sex == 0 && !child && spouse.is_some() && (18..=42).contains(&age) {
            let young = world
                .agents
                .iter()
                .filter(|c| c.active() && c.parent == Some(i) && c.age(day) < 18)
                .count();
            if young < 4 && rng.gen_bool((0.15 / 365.0_f64).clamp(0.0, 1.0)) {
                let bsex = if rng.gen_bool(0.5) { 1 } else { 0 };
                let first = if bsex == 1 { pick(&mut rng, FIRST_M) } else { pick(&mut rng, FIRST_F) };
                let surname = mother_name.rsplit(' ').next().unwrap_or("Pelham");
                let mut child = make_agent(&format!("{first} {surname}"), "child", &seat, (standing / 3).clamp(0, 100), 0, bsex, 0, day);
                child.parent = Some(i);
                out.push(mk("birth", &mother_name, format!("A child, {first}, was born at {seat}.")));
                newcomers.push(child);
            }
        }
    }

    // --- adults drift out: single, unsettled cottage-folk take work elsewhere ---
    // (gated above the floor, and never seat-holders or those with family, so lineages
    // and households are left intact)
    let active_ct = world.agents.iter().filter(|a| a.active() && a.archetype != "child").count();
    if active_ct > MIN_TOWNSFOLK + 3 {
        let leavers: Vec<usize> = (0..n)
            .filter(|&i| {
                let a = &world.agents[i];
                a.active()
                    && a.archetype != "child"
                    && a.spouse.is_none()
                    && (a.seat.starts_with("a cottage") || a.seat == "the empty cottage")
                    && (18..=60).contains(&a.age(day))
            })
            .collect();
        for i in leavers {
            let has_kin = (0..world.agents.len()).any(|j| world.agents[j].active() && world.agents[j].parent == Some(i));
            if has_kin {
                continue;
            }
            if rng.gen_bool((0.4 / 365.0_f64).clamp(0.0, 1.0)) {
                world.agents[i].departed = true;
                let nm = world.agents[i].name.clone();
                let to = pick(&mut rng, ORIGINS);
                out.push(mk("departure", &nm, format!("{nm} left Thrushcombe for {to}, and the cottage stood empty again.")));
            }
        }
    }

    // --- outsiders drift in: a steady trickle of new blood, up to a comfortable size ---
    if active_ct < SOFT_CAP && rng.gen_bool((2.5 / 365.0_f64).clamp(0.0, 1.0)) {
        let a = new_incomer(&mut rng, day);
        let from = a.origin.clone().unwrap_or_else(|| "away".into());
        out.push(mk("newcomer", &a.name, format!("{}, lately of {from}, came to Thrushcombe and took a situation.", a.name)));
        newcomers.push(a);
    }

    // --- the floor: Thrushcombe never falls below a living town ---
    let active_now = world.agents.iter().filter(|a| a.active() && a.archetype != "child").count() + newcomers.iter().filter(|a| a.active() && a.archetype != "child").count();
    if active_now < MIN_TOWNSFOLK && rng.gen_bool(0.6) {
        let a = new_incomer(&mut rng, day);
        let from = a.origin.clone().unwrap_or_else(|| "away".into());
        out.push(mk("newcomer", &a.name, format!("{}, lately of {from}, took the empty cottage in the town.", a.name)));
        newcomers.push(a);
    }

    world.agents.extend(newcomers);
    out
}

/// Thrushcombe holds at least this many souls — the floor tops up with incomers.
const MIN_TOWNSFOLK: usize = 30;
/// …and immigration eases off above this, so the town settles at a browsable size.
const SOFT_CAP: usize = 48;

// ----------------------------------------------------------------------------- gossip

use time::Weekday;

fn farmside(arch: &str) -> bool {
    matches!(arch, "hill_farmer" | "scheming_improver" | "blunt_hand")
}
fn pubgoer(arch: &str) -> bool {
    farmside(arch) || arch == "practitioner"
}

/// Daily probability that, if one of {a,b} knows a thing, the other comes to hear it.
/// The best-connecting channel wins; Sunday church gathers everyone.
fn meet_rate(a: &Agent, b: &Agent, date: Date) -> f64 {
    let wd = date.weekday();
    let has = |x: &str| a.archetype == x || b.archetype == x;
    let mut r: f64 = 0.08; // a small town: some path always exists

    // the vet, traversing every farm on his rounds — the fast connector
    if has("practitioner") {
        r = r.max(if farmside(&a.archetype) || farmside(&b.archetype) { 0.6 } else { 0.34 });
    }
    // the parson's parish visits — slower, reaches every home
    if has("official") {
        r = r.max(0.25);
    }
    // the servants' grapevine between drawing-rooms, ×market day
    if a.archetype == "genteel_status_seeker" && b.archetype == "genteel_status_seeker" {
        r = r.max(if wd == Weekday::Wednesday { 0.50 } else { 0.20 });
    }
    // gentry ↔ farm, laundered through servants at the market
    if has("genteel_status_seeker") && (farmside(&a.archetype) || farmside(&b.archetype)) {
        r = r.max(if wd == Weekday::Wednesday { 0.24 } else { 0.12 });
    }
    // The Pelican of an evening — the men, louder at week's end
    if pubgoer(&a.archetype) && pubgoer(&b.archetype) {
        r = r.max(if matches!(wd, Weekday::Friday | Weekday::Saturday) { 0.49 } else { 0.35 });
    }
    // the trades that traffic in news: the post office hears everything; the station and
    // the carrier bring word from away
    let trade = |x: &Agent, t: &str| x.trade.as_deref() == Some(t);
    let either = |t: &str| trade(a, t) || trade(b, t);
    if either("postmistress") {
        r = r.max(0.48); // the Cranford engine
    }
    if either("stationmaster") || either("railway porter") || either("carrier") || either("docker (works away)") {
        r = r.max(0.40); // word from the wider world
    }
    if wd == Weekday::Sunday {
        r += 0.20; // everyone at church
    }
    r.clamp(0.0, 0.95)
}

/// On hub days (market, church) the simmering and the warm boil over: feuds deepen,
/// friendships show. Emergent from the relationship ledger.
fn relationship_events(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let wd = date.weekday();
    if !matches!(wd, Weekday::Wednesday | Weekday::Sunday) {
        return out;
    }
    let mut rng = rng_for(seed ^ 0x4E1A_0000_0000, day);

    // Sunday upkeep: grudges and warmth both fade unless fed (relationships need tending);
    // living families are reinforced, so blood stays thicker than water.
    if wd == Weekday::Sunday {
        for v in world.affinity.values_mut() {
            *v -= v.signum();
        }
        for i in 0..world.agents.len() {
            if !world.agents[i].active() {
                continue;
            }
            if let Some(s) = world.agents[i].spouse {
                if world.agents[s].active() {
                    world.nudge_aff(i, s, 3);
                }
            }
            if let Some(p) = world.agents[i].parent {
                if world.agents[p].active() {
                    world.nudge_aff(i, p, 2);
                    world.nudge_aff(p, i, 2);
                }
            }
        }
    }

    let mk = |kind: &str, actor: &str, text: String| Event { day, date: date.to_string(), kind: kind.into(), actor: actor.into(), text };
    let elig = |w: &World, f: usize, t: usize| {
        w.agents[f].active() && w.agents[t].active() && w.agents[f].archetype != "child" && w.agents[t].archetype != "child"
    };
    let cold: Vec<(usize, usize)> = world.affinity.iter()
        .filter(|(_, &v)| v <= -48)
        .map(|(&(f, t), _)| (f as usize, t as usize))
        .filter(|&(f, t)| elig(world, f, t))
        .collect();
    let warm: Vec<(usize, usize)> = world.affinity.iter()
        .filter(|(_, &v)| v >= 60)
        .map(|(&(f, t), _)| (f as usize, t as usize))
        .filter(|&(f, t)| elig(world, f, t))
        .collect();
    let place = if wd == Weekday::Sunday { "church door" } else { "market" };

    if !cold.is_empty() && rng.gen_bool(0.10) {
        let (f, t) = cold[rng.gen_range(0..cold.len())];
        let (nf, nt) = (world.agents[f].name.clone(), world.agents[t].name.clone());
        world.nudge_aff(f, t, -4); // a public airing deepens it (hysteresis)
        let text = match rng.gen_range(0..3) {
            0 => format!("{nf} cut {nt} dead at the {place}, and the whole town marked it."),
            1 => format!("There was a frost between {nf} and {nt} at the {place} that could have iced the milk."),
            _ => format!("{nf} and {nt} had words again at the {place} — the old coldness, deeper for the airing."),
        };
        out.push(mk("feud", &nf, text));
        world.spawn_news(&nf, &format!("the bad blood between {nf} and {nt}"), -1, day, &[]);
    }
    if !warm.is_empty() && rng.gen_bool(0.08) {
        let (f, t) = warm[rng.gen_range(0..warm.len())];
        let (nf, nt) = (world.agents[f].name.clone(), world.agents[t].name.clone());
        world.nudge_aff(f, t, 3);
        let text = match rng.gen_range(0..2) {
            0 => format!("{nf} and {nt} had their heads together at the {place}, the best of friends."),
            _ => format!("{nf} and {nt} were thick as thieves at the {place}."),
        };
        out.push(mk("bond", &nf, text));
    }
    out
}

/// The animal & agricultural layer: births across the stock, the vet's ailments, the odd
/// catastrophe — first-class beasts that make or ruin a day.
fn animal_events(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let mut rng = rng_for(seed ^ 0x0A11_0000_0000, day);
    let mk = |kind: &str, actor: &str, text: String| Event { day, date: date.to_string(), kind: kind.into(), actor: actor.into(), text };

    for i in 0..world.animals.len() {
        if world.animals[i].health < 0 {
            continue; // gone to the knacker
        }
        // gestation → birth
        if world.animals[i].gest > 0 {
            world.animals[i].gest -= 1;
            if world.animals[i].gest == 0 {
                let (nm, owner, sp) = (world.animals[i].name.clone(), world.animals[i].owner.clone(), world.animals[i].species.clone());
                world.animals[i].gest = -1;
                let young = if sp.contains("ewe") || sp.contains("sheep") { "lambs" } else { "a calf" };
                if rng.gen_bool(0.72) {
                    world.animals[i].value += 12;
                    world.animals[i].health = (world.animals[i].health + 4).clamp(0, 100);
                    if let Some(o) = world.agent_mut(&owner) { clamp_standing(o, 2); }
                    out.push(mk("calving", &owner, format!("{nm} brought {young} safely; {owner} was well pleased.")));
                    world.spawn_news(&owner, &format!("{nm}'s fine new {young}"), 2, day, &[]);
                } else {
                    world.animals[i].value -= 4;
                    world.animals[i].health = (world.animals[i].health - 10).clamp(0, 100);
                    if let Some(v) = world.agent_mut("Mr Farran MRCVS") { clamp_standing(v, 2); }
                    out.push(mk("calving", &owner, format!("{nm} had a hard time of it; the vet worked till dawn, but {nm} and {young} stand.")));
                }
            }
        }
        // slow recovery toward health
        if world.animals[i].gest != 0 && world.animals[i].health < 92 {
            world.animals[i].health = (world.animals[i].health + 1).clamp(0, 100);
        }
    }

    // re-breeding: cows go back in calf in spring, ewes to the tup come autumn
    let season = Season::of(date);
    for i in 0..world.animals.len() {
        if world.animals[i].health < 35 || world.animals[i].gest > 0 {
            continue;
        }
        let sp = world.animals[i].species.clone();
        let g = if sp.contains("cow") && matches!(season, Season::Sowing) {
            Some(50)
        } else if (sp.contains("ewe") || sp.contains("sheep")) && matches!(season, Season::Mart) {
            Some(28)
        } else {
            None
        };
        if let Some(g) = g {
            if rng.gen_bool(0.03) {
                world.animals[i].gest = g;
            }
        }
    }

    // a daily ailment somewhere in the parish — the vet's bread and butter
    if !world.animals.is_empty() && rng.gen_bool(0.06) {
        let i = rng.gen_range(0..world.animals.len());
        if world.animals[i].health > 0 {
            let (nm, owner, sp) = (world.animals[i].name.clone(), world.animals[i].owner.clone(), world.animals[i].species.clone());
            world.animals[i].health -= rng.gen_range(8..22);
            if world.animals[i].health <= 0 {
                // the beast is lost — the knacker comes for it
                world.animals[i].health = -1;
                world.animals[i].value = 0;
                if let Some(o) = world.agent_mut(&owner) { o.purse -= 4; clamp_standing(o, -1); }
                out.push(mk("calving", &owner, format!("{owner}'s {sp} {nm} could not be saved; Mr Vye the knacker came for it at first light.")));
                world.spawn_news(&owner, &format!("the loss of {owner}'s {nm}"), -1, day, &[]);
            } else {
                if let Some(o) = world.agent_mut(&owner) { o.purse -= 2; }
                out.push(mk("practice", "Mr Farran MRCVS", format!("The vet was fetched to {owner}'s {sp} {nm}, off its feed and dull in the eye.")));
            }
        }
    }

    // Captain the homicidal carthorse, now and again
    if rng.gen_bool(0.012) && world.animal_idx("Captain").is_some() {
        out.push(mk("practice", "Mr Rupert Crale", "Captain put a hoof through the byre door and chased the boy clear round the yard.".into()));
    }
    out
}

/// Live rumours as display strings (freshest first): topic, reach, and whether it's grown.
fn news_in_flight(world: &World, target: i64) -> Vec<String> {
    let living = world.agents.iter().filter(|a| a.active()).count();
    let mut live: Vec<&News> = world
        .news
        .iter()
        .filter(|it| {
            let known = it.knowers.iter().filter(|&&k| world.agents[k].active()).count();
            target - it.born <= 21 && known < living
        })
        .collect();
    live.sort_by_key(|it| std::cmp::Reverse(it.born));
    live.iter()
        .take(8)
        .map(|it| {
            let known = it.knowers.iter().filter(|&&k| world.agents[k].active()).count();
            let grown = if it.distortion >= 2 { " · grown in the telling" } else { "" };
            format!("{}  {}/{} know{}", it.topic, known, living, grown)
        })
        .collect()
}

/// News spreads one hop a day from the start-of-day knowers; each fresh pair of ears
/// nudges the subject's standing (capped) and may garble the tale.
fn diffuse(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let mut news = std::mem::take(&mut world.news);
    let mut rng = rng_for(seed ^ 0xD1FF_0000_0000, day);
    let n = world.agents.len();
    let alive: Vec<bool> = world.agents.iter().map(|a| a.active()).collect();
    let living = alive.iter().filter(|&&x| x).count();
    let mk = |kind: &str, actor: &str, text: String| Event {
        day,
        date: date.to_string(),
        kind: kind.into(),
        actor: actor.into(),
        text,
    };

    for item in news.iter_mut() {
        let age = day - item.born;
        let living_knowers = item.knowers.iter().filter(|&&k| alive[k]).count();
        if age < 1 || age > 21 || living_knowers >= living {
            continue; // not yet (delay), stale, or every living soul already knows
        }
        let decay = (1.0 - age as f64 / 30.0).max(0.0);
        let juice = 1.0 + 0.15 * item.valence.unsigned_abs() as f64;

        // who knew at the start of today — delay is one hop per day; the dead don't talk
        let known: Vec<bool> = (0..n).map(|i| item.knowers.contains(&i)).collect();
        let mut learners = Vec::new();
        for b in 0..n {
            if known[b] || !alive[b] {
                continue;
            }
            for a in 0..n {
                if !known[a] || !alive[a] {
                    continue;
                }
                let p = (meet_rate(&world.agents[a], &world.agents[b], date) * decay * juice).clamp(0.0, 0.95);
                if rng.gen_bool(p) {
                    learners.push(b);
                    break; // b heard it from someone; move on
                }
            }
        }

        let subject = item.subject;
        for b in learners {
            item.knowers.push(b);
            if item.applied < 6 && alive[subject] {
                clamp_standing(&mut world.agents[subject], item.valence.signum());
                item.applied += 1;
            }
            // gossip is personal: the hearer's own opinion of the subject shifts, and it
            // persists (no decay) — every snub remembered, every kindness too
            if b != subject && alive[subject] {
                world.nudge_aff(b, subject, (item.valence.clamp(-3, 3) * 3) as i16);
            }
            // the story grows in the telling
            if rng.gen_bool(0.15) {
                item.distortion += 1;
                if item.distortion == 2 {
                    item.valence += item.valence.signum(); // amplified
                    let topic = item.topic.clone();
                    out.push(mk("gossip", &world.agents[subject].name,
                        format!("By the telling and re-telling, {topic} had grown somewhat in the carriage.")));
                }
            }
        }

        // milestone: most of the town now knows — but only worth a beat if it's juicy
        let now_known = item.knowers.iter().filter(|&&k| alive[k]).count();
        if !item.broadcast && now_known * 5 >= living * 3 {
            item.broadcast = true;
            if item.valence.abs() >= 2 {
                let topic = item.topic.clone();
                out.push(mk("gossip", &world.agents[subject].name,
                    format!("By now there was scarcely a soul in Thrushcombe who had not heard of {topic}.")));
            }
        }
    }

    // prune stale news so world state (and regeneration cost) stays bounded; the chronicle
    // keeps the history in the event log regardless.
    news.retain(|it| day - it.born <= 21);
    world.news = news;
    out
}

// ----------------------------------------------------------------------------- store + sim

pub struct Sim {
    conn: Connection,
    seed: u64,
    epoch: Date,
    engine: Box<dyn PolicyEngine>,
    interventions: BTreeMap<i64, Vec<Intervention>>,
    weather: BTreeMap<i64, DayWeather>,
}

/// A structured chronicle line for readers (web/legends).
pub struct ChronEntry {
    pub date: String,
    pub phase: i64,
    pub kind: String,
    pub actor: String,
    pub text: String,
}

/// One phase of a soul's day: where they were and what they were about.
pub struct DayLine {
    pub phase: String,
    pub location: String,
    pub doing: String,
    pub beat: bool, // true if this was an actual recorded happening (not just routine)
}

/// Everything we can surface about one soul, right now.
pub struct PersonDetail {
    pub idx: usize,
    pub name: String,
    pub archetype: String,
    pub trade: Option<String>,
    pub seat: String,
    pub age: i64,
    pub standing: i32,
    pub purse: i32,
    pub married: bool,
    pub location: String,         // where they are this phase
    pub doing: String,            // what they're about today
    pub next: String,             // what they're likely about tomorrow
    pub spouse: Option<String>,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub origin: Option<String>,   // Some = came from away
    pub friends: Vec<String>,     // strongest warm ties
    pub rivals: Vec<String>,      // strongest cold ties
    pub recent: Vec<ChronEntry>,  // their latest beats
}

/// A full, detailed read of the town at a moment — for the dashboard and the TUI.
pub struct TownDetail {
    pub date: String,
    pub weekday: String,
    pub season: String,
    pub armed: String,
    pub phase: String,
    pub weather: Option<String>, // today's real Sofia sky, if recorded
    pub population: usize,
    pub people: Vec<PersonDetail>,
    pub gossip: Vec<String>,
    pub upcoming: Vec<String>,
    pub global_today: Vec<String>,
    pub recent: Vec<ChronEntry>,
}

pub struct Report {
    pub date: String,
    pub day: i64,
    pub weekday: String,
    pub season: String,
    pub armed: String,
    pub agents: Vec<Agent>,
    pub animals: Vec<Animal>,
    pub pending: Vec<String>,
    pub news: Vec<String>,
    pub chronicle: Vec<String>,
}

/// Event kinds worth rendering in voice. Pure flavour (market, vet rounds) keeps its
/// template line; the salient beats get the oracle.
pub const SALIENT: &[&str] = &[
    "calving", "party", "windfall", "scheme", "bureaucracy", "weather", "status", "household", "gossip",
    "death", "succession", "marriage", "birth", "comingofage", "feud", "bond", "providence",
];

/// Bump when World layout or step_day logic changes — older snapshots are then ignored
/// and the world re-folds from genesis (and writes fresh checkpoints).
const SNAPSHOT_VERSION: i64 = 8;
/// Checkpoint cadence in days. A read folds at most this many days past the last one.
const SNAPSHOT_EVERY: i64 = 365 * 5; // a year of slots

fn ensure_aux(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        // The recorded oracle: an LLM is non-deterministic at generation time, so we log
        // its output once, keyed by event, and replay the stored text forever.
        "CREATE TABLE IF NOT EXISTS narration(event_id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         -- Folded-world checkpoints, so a read need not re-fold from genesis.
         CREATE TABLE IF NOT EXISTS snapshots(day INTEGER PRIMARY KEY, version INTEGER NOT NULL, blob BLOB NOT NULL);
         -- Providence: the player's diegetic interventions, folded in at their day.
         CREATE TABLE IF NOT EXISTS interventions(
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL,
            kind TEXT NOT NULL, target TEXT NOT NULL, amount INTEGER NOT NULL, note TEXT NOT NULL);
         -- Real weather (Sofia), recorded so the fold stays deterministic.
         CREATE TABLE IF NOT EXISTS weather(day INTEGER PRIMARY KEY, precip REAL, tmax REAL, tmin REAL);",
    )
}

fn load_weather(conn: &Connection) -> rusqlite::Result<BTreeMap<i64, DayWeather>> {
    let mut stmt = conn.prepare("SELECT day, precip, tmax, tmin FROM weather")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, DayWeather { precip: r.get(1)?, tmax: r.get(2)?, tmin: r.get(3)? }))
    })?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (d, w) = row?;
        map.insert(d, w);
    }
    Ok(map)
}

fn load_interventions(conn: &Connection) -> rusqlite::Result<BTreeMap<i64, Vec<Intervention>>> {
    let mut stmt = conn.prepare("SELECT day, kind, target, amount, note FROM interventions ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, Intervention { kind: r.get(1)?, target: r.get(2)?, amount: r.get(3)?, note: r.get(4)? }))
    })?;
    let mut map: BTreeMap<i64, Vec<Intervention>> = BTreeMap::new();
    for row in rows {
        let (day, iv) = row?;
        map.entry(day).or_default().push(iv);
    }
    Ok(map)
}

impl Sim {
    /// Open an existing world (must have been `init`ed).
    pub fn open(path: &str) -> rusqlite::Result<Sim> {
        let conn = Connection::open(path)?;
        ensure_aux(&conn)?;
        let seed: i64 = conn.query_row("SELECT val FROM meta WHERE key='seed'", [], |r| r.get(0))?;
        let ej: i64 = conn.query_row("SELECT val FROM meta WHERE key='epoch_julian'", [], |r| r.get(0))?;
        let interventions = load_interventions(&conn)?;
        let weather = load_weather(&conn)?;
        Ok(Sim { conn, seed: seed as u64, epoch: Date::from_julian_day(ej as i32).unwrap(), engine: Box::new(NativePolicies), interventions, weather })
    }

    /// Create a new world. `epoch` is the day "play" was pressed (default: today).
    pub fn init(path: &str, epoch: Date, seed: u64) -> rusqlite::Result<Sim> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, val INTEGER);
             CREATE TABLE IF NOT EXISTS events(
                id INTEGER PRIMARY KEY,
                day INTEGER NOT NULL, phase INTEGER NOT NULL DEFAULT 0, date TEXT NOT NULL,
                kind TEXT NOT NULL, actor TEXT NOT NULL, text TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_events_day ON events(day);",
        )?;
        ensure_aux(&conn)?;
        conn.execute("INSERT OR REPLACE INTO meta(key,val) VALUES('seed',?1)", params![seed as i64])?;
        conn.execute("INSERT OR REPLACE INTO meta(key,val) VALUES('epoch_julian',?1)", params![epoch.to_julian_day() as i64])?;
        Ok(Sim { conn, seed, epoch, engine: Box::new(NativePolicies), interventions: BTreeMap::new(), weather: BTreeMap::new() })
    }

    /// Record a day's real weather, but only for days not yet folded (so it can't rewrite
    /// history). Returns true if stored. Fetch-then-tick keeps the frontier ahead of it.
    pub fn record_weather(&mut self, date: Date, precip: f64, tmax: f64, tmin: f64) -> rusqlite::Result<bool> {
        let day = self.target_day(date);
        if day <= self.last_day() {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO weather(day,precip,tmax,tmin) VALUES(?1,?2,?3,?4)",
            params![day, precip, tmax, tmin],
        )?;
        self.weather.insert(day, DayWeather { precip, tmax, tmin });
        Ok(true)
    }

    /// Swap the behaviour engine (e.g. a wasm-backed one). Must be set before any
    /// `catch_up`/`report` so generation is consistent.
    pub fn set_engine(&mut self, engine: Box<dyn PolicyEngine>) {
        self.engine = engine;
    }

    /// Inject a providence verb at today: a letter, a called loan, a legacy, a scandal, a
    /// stranger at the cottage. It's logged, then folded into the world from today forward
    /// (the frontier is regenerated so it takes effect at once). Caller should `catch_up`.
    pub fn providence(&mut self, today: Date, kind: &str, target: &str, amount: i32, note: &str) -> rusqlite::Result<()> {
        let day = self.target_day(today).max(0);
        self.conn.execute(
            "INSERT INTO interventions(day,kind,target,amount,note) VALUES(?1,?2,?3,?4,?5)",
            params![day, kind, target, amount, note],
        )?;
        self.interventions = load_interventions(&self.conn)?;
        // invalidate the frontier so regeneration picks up the intervention
        self.conn.execute("DELETE FROM events WHERE day >= ?1", params![day])?;
        self.conn.execute("DELETE FROM snapshots WHERE day >= ?1", params![day * PHASES])?; // snapshots keyed by slot
        Ok(())
    }

    /// Salient events not yet rendered in voice, as (event_id, date, template_text).
    pub fn unnarrated_salient(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String, String)>> {
        let placeholders = SALIENT.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT e.id, e.date, e.text FROM events e
             LEFT JOIN narration n ON n.event_id = e.id
             WHERE n.event_id IS NULL AND e.kind IN ({placeholders})
             ORDER BY e.id ASC LIMIT ?",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = SALIENT
            .iter()
            .map(|k| k as &dyn rusqlite::ToSql)
            .chain(std::iter::once(&limit as &dyn rusqlite::ToSql))
            .collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        rows.collect()
    }

    /// Record the oracle's rendering of an event, verbatim.
    pub fn save_narration(&self, event_id: i64, text: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO narration(event_id, text) VALUES(?1, ?2)",
            params![event_id, text],
        )?;
        Ok(())
    }

    fn last_day(&self) -> i64 {
        self.conn
            .query_row("SELECT COALESCE(MAX(day), -1) FROM events", [], |r| r.get(0))
            .unwrap_or(-1)
    }

    pub fn target_day(&self, today: Date) -> i64 {
        (today.to_julian_day() - self.epoch.to_julian_day()) as i64
    }

    fn date_of(&self, day: i64) -> Date {
        Date::from_julian_day((self.epoch.to_julian_day() as i64 + day) as i32).unwrap()
    }

    /// Render a day-index as its calendar date (for readers).
    pub fn day_to_date(&self, day: i64) -> String {
        self.date_of(day).to_string()
    }

    /// Load the nearest checkpoint <= `up_to` (current version); returns the folded world
    /// and the next day to fold from. Falls back to genesis if there's no usable snapshot.
    fn load_checkpoint(&self, up_to: i64) -> (World, i64) {
        let row: Option<(i64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT day, blob FROM snapshots WHERE day <= ?1 AND version = ?2 ORDER BY day DESC LIMIT 1",
                params![up_to, SNAPSHOT_VERSION],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        match row.and_then(|(d, b)| bincode::deserialize::<World>(&b).ok().map(|w| (w, d))) {
            Some((world, d)) => (world, d + 1),
            None => (World::seed(), 0),
        }
    }

    /// A slot is a (day, phase): slot = day*PHASES + phase. The atomic simulation step.
    fn target_slot(&self, today: Date, phase: Phase) -> i64 {
        self.target_day(today).max(0) * PHASES + phase.ord()
    }
    fn decompose(&self, slot: i64) -> (i64, Phase, Date) {
        let day = slot.div_euclid(PHASES);
        (day, Phase::from_ord(slot), self.date_of(day))
    }
    /// The last slot actually generated into the log — the frontier readers fold to.
    fn last_slot(&self) -> i64 {
        self.conn
            .query_row(&format!("SELECT COALESCE(MAX(day*{PHASES}+phase), -1) FROM events"), [], |r| r.get(0))
            .unwrap_or(-1)
    }

    /// The folded world as of `slot`, using checkpoints (read-only; nothing is written).
    fn world_at(&self, slot: i64) -> World {
        let (mut world, from) = self.load_checkpoint(slot);
        for s in from..=slot {
            let (day, phase, date) = self.decompose(s);
            let _ = step_slot(&mut world, day, phase, date, self.seed, &*self.engine, &self.interventions, &self.weather);
        }
        world
    }

    /// The full folded world at the current frontier (all agents, living and gone, with
    /// indices) — for readers that need lineage and the complete cast.
    pub fn world_snapshot(&self, _today: Date) -> World {
        self.world_at(self.last_slot().max(0))
    }

    /// The most recent chronicle entries, oracle prose preferred over the template line.
    pub fn chronicle(&self, limit: i64) -> rusqlite::Result<Vec<ChronEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.date, e.phase, e.kind, e.actor, COALESCE(n.text, e.text)
             FROM events e LEFT JOIN narration n ON n.event_id = e.id
             ORDER BY e.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(ChronEntry { date: r.get(0)?, phase: r.get(1)?, kind: r.get(2)?, actor: r.get(3)?, text: r.get(4)? })
        })?;
        rows.collect()
    }

    /// Every chronicle entry that names a person — their life as the town recorded it.
    pub fn person_events(&self, name: &str, limit: i64) -> rusqlite::Result<Vec<ChronEntry>> {
        let like = format!("%{name}%");
        let mut stmt = self.conn.prepare(
            "SELECT e.date, e.phase, e.kind, e.actor, COALESCE(n.text, e.text)
             FROM events e LEFT JOIN narration n ON n.event_id = e.id
             WHERE e.actor = ?1 OR e.text LIKE ?2
             ORDER BY e.id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![name, like, limit], |r| {
            Ok(ChronEntry { date: r.get(0)?, phase: r.get(1)?, kind: r.get(2)?, actor: r.get(3)?, text: r.get(4)? })
        })?;
        rows.collect()
    }

    /// All the chronicle of a given date, in order (for time-travel to a day).
    pub fn events_on(&self, date: &str, limit: i64) -> rusqlite::Result<Vec<ChronEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.date, e.phase, e.kind, e.actor, COALESCE(n.text, e.text)
             FROM events e LEFT JOIN narration n ON n.event_id = e.id
             WHERE e.date = ?1 ORDER BY e.id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![date, limit], |r| {
            Ok(ChronEntry { date: r.get(0)?, phase: r.get(1)?, kind: r.get(2)?, actor: r.get(3)?, text: r.get(4)? })
        })?;
        rows.collect()
    }

    /// The full folded world at the end of `date` — for the historical town board.
    pub fn world_on(&self, date: Date) -> World {
        self.world_at(self.target_day(date).max(0) * PHASES + (PHASES - 1))
    }

    /// A soul's whole day on `date`: each phase, where they were and what they were about —
    /// their recorded beats slotted into the routine.
    pub fn person_day(&self, idx: usize, date: Date) -> rusqlite::Result<Vec<DayLine>> {
        let day = self.target_day(date).max(0);
        let world = self.world_at(day * PHASES + (PHASES - 1));
        let wd = date.weekday();
        let Some(a) = world.agents.get(idx) else { return Ok(Vec::new()) };
        let top = world.agents.iter().filter(|x| x.active()).map(|x| x.standing).max().unwrap_or(0);

        // recorded beats for this soul on this day, by phase
        let mut stmt = self.conn.prepare(
            "SELECT e.phase, COALESCE(n.text, e.text) FROM events e LEFT JOIN narration n ON n.event_id = e.id
             WHERE e.actor = ?1 AND e.day = ?2 ORDER BY e.id",
        )?;
        let mut beats: BTreeMap<i64, String> = BTreeMap::new();
        let rows = stmt.query_map(params![a.name, day], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (ph, txt) = row?;
            beats.entry(ph).or_insert(txt);
        }

        let mut lines = Vec::new();
        for ph in 0..PHASES {
            let phase = Phase::from_ord(ph);
            let (doing, beat) = if let Some(txt) = beats.get(&ph) {
                (txt.clone(), true)
            } else if a.archetype != "child" && acts_in_phase(&a.archetype, phase) {
                let o = observe(&world, idx, day, date, top, self.seed, phase);
                match self.engine.decide(&a.archetype, &o) {
                    Action::Idle => (routine_doing(a, phase, wd), false),
                    act => (act.label().to_string(), false),
                }
            } else {
                (routine_doing(a, phase, wd), false)
            };
            lines.push(DayLine { phase: phase.name().to_string(), location: placement(a, phase, wd), doing, beat });
        }
        Ok(lines)
    }

    /// A full detailed read of the town: every present soul's place, doings, kin and
    /// record, plus the day's global events, the gossip in flight, and what's upcoming.
    pub fn detail(&self, today: Date, phase: Phase) -> rusqlite::Result<TownDetail> {
        let target = self.target_day(today).max(0);
        let world = self.world_at(self.last_slot().max(0));
        let wd = today.weekday();
        let top = world.agents.iter().filter(|a| a.active()).map(|a| a.standing).max().unwrap_or(0);

        // what a soul is about in a given phase: their notable action if they roll one,
        // else the routine verb for the phase (so it never reads as idle).
        let do_at = |i: usize, day: i64, ph: Phase| {
            let a = &world.agents[i];
            if acts_in_phase(&a.archetype, ph) {
                let o = observe(&world, i, day, self.date_of(day), top, self.seed, ph);
                match self.engine.decide(&a.archetype, &o) {
                    Action::Idle => routine_doing(a, ph, wd),
                    act => act.label().to_string(),
                }
            } else {
                routine_doing(a, ph, wd)
            }
        };

        let mut people = Vec::new();
        for i in 0..world.agents.len() {
            let a = &world.agents[i];
            if !a.active() {
                continue;
            }
            let doing = do_at(i, target, phase);
            let next = {
                let s = target * PHASES + phase.ord() + 1;
                do_at(i, s.div_euclid(PHASES), Phase::from_ord(s))
            };
            let children: Vec<String> = (0..world.agents.len())
                .filter(|&j| world.agents[j].parent == Some(i) && world.agents[j].active())
                .map(|j| world.agents[j].name.clone())
                .collect();
            people.push(PersonDetail {
                idx: i,
                name: a.name.clone(),
                archetype: a.archetype.clone(),
                trade: a.trade.clone(),
                seat: a.seat.clone(),
                age: a.age(target),
                standing: a.standing,
                purse: a.purse,
                married: a.spouse.is_some(),
                location: placement(a, phase, wd),
                doing,
                next,
                spouse: a.spouse.map(|s| world.agents[s].name.clone()),
                parent: a.parent.map(|p| world.agents[p].name.clone()),
                children,
                origin: a.origin.clone(),
                friends: world.ties(i, true, 3).iter().map(|&(j, _)| world.agents[j].name.clone()).collect(),
                rivals: world.ties(i, false, 3).iter().map(|&(j, _)| world.agents[j].name.clone()).collect(),
                recent: self.person_events(&a.name, 4)?,
            });
        }
        people.sort_by(|x, y| y.standing.cmp(&x.standing));

        // global events on the current day — shocks, deaths, parties, gossip milestones
        let mut gstmt = self.conn.prepare(
            "SELECT COALESCE(n.text, e.text) FROM events e LEFT JOIN narration n ON n.event_id = e.id
             WHERE e.day = ?1 AND (e.actor = 'Thrushcombe' OR e.kind IN
                ('death','succession','birth','marriage','party','calving','gossip','newcomer','weather','bureaucracy'))
             ORDER BY e.id",
        )?;
        let global_today: Vec<String> = gstmt
            .query_map(params![target], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;

        Ok(TownDetail {
            date: today.to_string(),
            weekday: wd.to_string(),
            season: Season::of(today).name().to_string(),
            armed: Season::of(today).armed().to_string(),
            phase: phase.name().to_string(),
            weather: self.weather.get(&target).map(|w| {
                let sky = if w.precip >= 8.0 { "rain" } else if w.precip >= 1.0 { "showers" } else if w.tmax >= 30.0 { "hot" } else if w.tmin <= -3.0 { "frost" } else { "fair" };
                format!("Sofia: {sky}, {:.0}°/{:.0}°C, {:.0}mm", w.tmax, w.tmin, w.precip)
            }),
            population: people.len(),
            people,
            gossip: news_in_flight(&world, target),
            upcoming: self.pending(today, &world),
            global_today,
            recent: self.chronicle(16)?,
        })
    }

    /// Advance the log forward until it has caught up to the current (day, phase). Returns
    /// events added. Missing slots self-heal; checkpoints written every SNAPSHOT_EVERY slots.
    pub fn catch_up(&mut self, today: Date, phase: Phase) -> rusqlite::Result<i64> {
        let target = self.target_slot(today, phase);
        let from = self.last_slot() + 1;
        if target < from {
            return Ok(0);
        }
        let mut world = self.world_at(from - 1); // cheap: nearest checkpoint + remainder
        let seed = self.seed;
        let epoch_jd = self.epoch.to_julian_day() as i64;
        let tx = self.conn.transaction()?;
        let mut added = 0;
        for s in from..=target {
            let day = s.div_euclid(PHASES);
            let ph = Phase::from_ord(s);
            let date = Date::from_julian_day((epoch_jd + day) as i32).unwrap();
            for e in step_slot(&mut world, day, ph, date, seed, &*self.engine, &self.interventions, &self.weather) {
                tx.execute(
                    "INSERT INTO events(day,phase,date,kind,actor,text) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![e.day, ph.ord(), e.date, e.kind, e.actor, e.text],
                )?;
                added += 1;
            }
            if s % SNAPSHOT_EVERY == 0 {
                let blob = bincode::serialize(&world).expect("serialize world");
                tx.execute(
                    "INSERT OR REPLACE INTO snapshots(day,version,blob) VALUES(?1,?2,?3)",
                    params![s, SNAPSHOT_VERSION, blob],
                )?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// Fold the world to `today` (via checkpoints) and read recent chronicle for display.
    pub fn report(&self, today: Date) -> rusqlite::Result<Report> {
        let target = self.target_day(today).max(0);
        let world = self.world_at(self.last_slot().max(0));

        let mut chronicle = Vec::new();
        // Prefer the oracle's rendering once it exists; fall back to the template line.
        let mut stmt = self.conn.prepare(
            "SELECT e.date, COALESCE(n.text, e.text)
             FROM events e LEFT JOIN narration n ON n.event_id = e.id
             ORDER BY e.id DESC LIMIT 14",
        )?;
        let rows = stmt.query_map([], |r| {
            let date: String = r.get(0)?;
            let text: String = r.get(1)?;
            Ok(format!("{date}  {text}"))
        })?;
        for row in rows {
            chronicle.push(row?);
        }
        chronicle.reverse();

        let pending = self.pending(today, &world);

        let news = news_in_flight(&world, target);

        // the present cast, grandest first
        let mut agents: Vec<Agent> = world.agents.iter().filter(|a| a.active()).cloned().collect();
        agents.sort_by(|x, y| y.standing.cmp(&x.standing));

        Ok(Report {
            date: today.to_string(),
            day: target,
            weekday: today.weekday().to_string(),
            season: Season::of(today).name().to_string(),
            armed: Season::of(today).armed().to_string(),
            agents,
            animals: world.animals.iter().filter(|a| a.health >= 0).cloned().collect(),
            pending,
            news,
            chronicle,
        })
    }

    fn pending(&self, today: Date, world: &World) -> Vec<String> {
        let mut p = Vec::new();
        // garden party, next occurrence of June 14
        if let Ok(party) = Date::from_calendar_date(today.year(), Month::June, 14) {
            let days = (party.to_julian_day() - today.to_julian_day()) as i64;
            if days > 0 {
                p.push(format!("Garden party at Crale Court — in {days}d"));
            }
        }
        for an in &world.animals {
            if an.gest > 0 {
                p.push(format!("{} in calf — due in {}d  (health {})", an.name, an.gest, an.health));
            }
        }
        // the Show, ~18 July
        if let Ok(show) = Date::from_calendar_date(today.year(), Month::July, 18) {
            let days = (show.to_julian_day() - today.to_julian_day()) as i64;
            if days > 0 {
                p.push(format!("Agricultural Show — in {days}d"));
            }
        }
        p
    }
}
