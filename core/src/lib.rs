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
    // --- individuation: who they are, beyond their archetype ---
    pub goal: u8,                 // 0 Thrive · 1 ClearDebt · 2 Rise · 3 MarryOff · 4 Outdo · 5 Prosper
    pub goal_target: i32,         // a child/rival index for MarryOff/Outdo, else -1
    pub mood: i16,               // [-100,100] transient spirits; <0 low/grieving, >0 high/triumphant
    // --- planning: a multi-day intention with a throughline ---
    pub courting: i32,            // index of the soul they are courting, else -1
    pub courtship: i16,           // how far the courtship has come along
    pub acted_day: i64,           // last day this soul made a social set-piece — one per day, no per-phase repeats
    pub rival: i32,               // a declared nemesis (index) — a durable grudge that outlives the standing of the day, else -1
    pub feud: i16,                // how far the campaign against the rival has been pressed — a throughline toward a reckoning
    // --- a self-authored plan with a horizon: a dated ambition the soul set itself, pressed
    //     toward a public reckoning weeks on (made good, or come to nothing) ---
    pub intent: u8,               // 0 none · 1 mend their fortunes · 2 better their station · 3 a bold venture
    pub intent_goal: i32,         // the purse/standing threshold that would count as making good
    pub intent_age: i16,          // days the plan has been pressed — the throughline toward its reckoning
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
    pub fn aff(&self, from: usize, to: usize) -> i16 {
        *self.affinity.get(&(from as u32, to as u32)).unwrap_or(&0)
    }
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
            goal: 0,
            goal_target: -1,
            mood: 0,
            courting: -1,
            courtship: 0,
            acted_day: -1,
            rival: -1,
            feud: 0,
            intent: 0,
            intent_goal: 0,
            intent_age: 0,
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

        let mut w = World {
            agents,
            animals: seed_animals(),
            news: Vec::new(),
            next_news_id: 0,
            affinity,
        };
        // every adult opens with an ambition fitting their situation, at their resting mood
        for i in 0..w.agents.len() {
            let (g, t) = assess_goal(&w, i, 0);
            w.agents[i].goal = g;
            w.agents[i].goal_target = t;
            w.agents[i].mood = temperament(&w.agents[i].archetype).1;
        }
        w
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
        // Don't set the same rumour loose twice: if this subject already has news of this
        // very topic in flight, the town is talking about it — no second copy to flood the feed.
        if self.news.iter().any(|n| n.subject == subject && n.topic == topic) {
            return;
        }
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
#[allow(clippy::too_many_arguments)]
fn step_slot(world: &mut World, day: i64, phase: Phase, date: Date, seed: u64, engine: &dyn PolicyEngine, interventions: &BTreeMap<i64, Vec<Intervention>>, weather: &BTreeMap<i64, DayWeather>, wildcards: &BTreeMap<i64, Vec<Wildcard>>, decrees: &BTreeMap<i64, Vec<Decree>>) -> Vec<Event> {
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
        if let Some(list) = wildcards.get(&day) {
            out.extend(apply_wildcards(world, day, date, list));
        }
        if let Some(list) = decrees.get(&day) {
            out.extend(apply_decrees(world, day, date, list));
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
    // the year's great set-piece: the Thrushcombe & District Show
    if matches!(phase, Phase::Afternoon) && date.month() == Month::August && date.day() == 23 {
        out.extend(the_show(world, day, date, seed));
    }

    // --- the behaviour layer: whoever is out and about this phase acts in character ---
    behaviour_phase(world, day, phase, date, seed, engine, &mut out);

    // --- the forenoon hub: feuds and friendships at the market & church door ---
    if matches!(phase, Phase::Forenoon) {
        out.extend(relationship_events(world, day, date, seed));
    }

    // --- the rumour mill: scandal & romance at the market, after church, at the Pelican ---
    if matches!(phase, Phase::Forenoon | Phase::Evening) {
        out.extend(rumour_mill(world, day, phase, date, seed));
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
    // where everyone is this phase, so a soul can know who's about them
    let wd = date.weekday();
    let places: Vec<String> = (0..world.agents.len())
        .map(|j| if world.agents[j].active() { placement(&world.agents[j], phase, wd) } else { String::new() })
        .collect();
    for i in actors {
        let (friend, rival) = present_ties(world, i, &places);
        let obs = observe(world, i, day, date, top, seed, phase, friend, rival);
        let action = engine.decide(&world.agents[i].archetype, &obs);
        if matches!(action, Action::Idle) {
            continue;
        }
        // One social set-piece per soul per day: the gentry act in two phases, but a soul
        // does not give two dinners or pay two rounds of calls on the same day. Routine
        // practice (the stock, the rounds, the day's work) may recur across phases.
        let setpiece = is_setpiece(&action);
        if setpiece && world.agents[i].acted_day == day {
            continue;
        }
        arbitrate(world, i, action, day, date, phase, out, seed);
        if setpiece {
            world.agents[i].acted_day = day;
        }
    }
}

/// A "loud" act — a deliberate social move that draws notice (and often gossip), as opposed
/// to the day's routine practice. Capped to one per soul per day so the chronicle and the
/// rumour mill don't carry the same dinner or the same round of calls twice.
fn is_setpiece(action: &Action) -> bool {
    matches!(
        action,
        Action::PayCall | Action::GiveDinner | Action::Economise | Action::KeepUp | Action::Scheme
    )
}

/// A living adult who is not `i` nor their spouse — the pool of people a soul might cultivate.
fn society_member(world: &World, i: usize, j: usize) -> bool {
    j != i
        && Some(j) != world.agents[i].spouse
        && world.agents[j].active()
        && world.agents[j].archetype != "child"
}

/// One of the grandest few acquaintances a soul might cultivate — those of equal or higher
/// standing they don't actively dislike, picked with a little chance so the whole town isn't
/// forever courting the single titled lady.
fn cultivate_upward(world: &World, i: usize, rng: &mut ChaCha8Rng) -> Option<usize> {
    let a = &world.agents[i];
    let mut up: Vec<usize> = (0..world.agents.len())
        .filter(|&j| society_member(world, i, j) && world.agents[j].standing >= a.standing && world.aff(i, j) > -25)
        .collect();
    if up.is_empty() {
        return None;
    }
    up.sort_by_key(|&j| std::cmp::Reverse(world.agents[j].standing));
    up.truncate(4); // the top few worth knowing
    Some(up[rng.gen_range(0..up.len())])
}

/// Whom a soul calls on this afternoon. A riser or a soul bent on outdoing a superior
/// cultivates *upward* — one of the grandest acquaintances they don't dislike; everyone
/// else calls on their warmest friend. A call is a social strategy, not a wander.
fn call_target(world: &World, i: usize, rng: &mut ChaCha8Rng) -> Option<usize> {
    if world.agents[i].goal == 2 || world.agents[i].goal == 4 {
        if let Some(t) = cultivate_upward(world, i, rng) {
            return Some(t);
        }
    }
    if let Some(&(t, _)) = world.ties(i, true, 1).first() {
        return Some(t);
    }
    (0..world.agents.len())
        .filter(|&j| society_member(world, i, j))
        .max_by_key(|&j| world.aff(i, j))
}

/// Who is asked to dine — and who is pointedly left off the list. Guests are the host's
/// warmest ties, the soul they are courting, and (for a riser) a grand acquaintance worth
/// cultivating. The snub is the rival of standing — by name when the host's whole ambition
/// is to outdo them. This is how an evening's invitations become a campaign.
fn dinner_guests(world: &World, i: usize, rng: &mut ChaCha8Rng) -> (Vec<usize>, Option<usize>) {
    let a_goal = world.agents[i].goal;
    let a_target = world.agents[i].goal_target;
    let a_courting = world.agents[i].courting;
    let a_standing = world.agents[i].standing;
    let mut guests: Vec<usize> = world.ties(i, true, 3).into_iter().map(|(t, _)| t).collect();
    if a_goal == 2 {
        if let Some(t) = cultivate_upward(world, i, rng) {
            if !guests.contains(&t) {
                guests.push(t);
            }
        }
    }
    if a_courting >= 0 {
        let c = a_courting as usize;
        if world.agents[c].active() && !guests.contains(&c) {
            guests.push(c);
        }
    }
    guests.truncate(4);
    let snub = if a_goal == 4 && a_target >= 0 && world.agents[a_target as usize].active() && !guests.contains(&(a_target as usize)) {
        // a declared rival: the soul's whole ambition is to outdo them
        Some(a_target as usize)
    } else if let Some(r) = world
        .ties(i, false, 3)
        .into_iter()
        .map(|(t, _)| t)
        .find(|&t| world.agents[t].standing >= a_standing - 4 && !guests.contains(&t))
    {
        // an established cold tie of standing — pointedly left off
        Some(r)
    } else if a_goal == 2 && rng.gen_bool(0.4) {
        // a riser quietly omits the genteel rival just above them — competition, not yet enmity.
        // The coolness this breeds is what later hardens into a declared rivalry.
        (0..world.agents.len())
            .filter(|&j| {
                society_member(world, i, j)
                    && world.agents[j].archetype == "genteel_status_seeker"
                    && world.agents[j].standing > a_standing
                    && world.agents[j].standing <= a_standing + 12
                    && world.aff(i, j) < 10
                    && !guests.contains(&j)
            })
            .min_by_key(|&j| world.agents[j].standing)
    } else {
        None
    };
    (guests, snub)
}

/// Format a handful of names as English prose: "A", "A and B", "A, B and C".
fn name_list(world: &World, idxs: &[usize]) -> String {
    let names: Vec<&str> = idxs.iter().map(|&j| world.agents[j].name.as_str()).collect();
    match names.len() {
        0 => String::new(),
        1 => names[0].to_string(),
        2 => format!("{} and {}", names[0], names[1]),
        n => format!("{}, and {}", names[..n - 1].join(", "), names[n - 1]),
    }
}

/// Is a friend (warm) or a rival (cold) co-located with `i` this phase?
fn present_ties(world: &World, i: usize, places: &[String]) -> (bool, bool) {
    let (mut friend, mut rival) = (false, false);
    if places[i].is_empty() {
        return (false, false);
    }
    for j in 0..world.agents.len() {
        if j == i || places[j].is_empty() || places[j] != places[i] {
            continue;
        }
        let a = world.aff(i, j);
        if a >= 25 {
            friend = true;
        } else if a <= -25 {
            rival = true;
        }
        if friend && rival {
            break;
        }
    }
    (friend, rival)
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
    pub goal: i32,         // their ambition (kind ordinal)
    pub mood: i32,         // their present spirits
    pub friend_present: bool,
    pub rival_present: bool,
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

fn observe(world: &World, i: usize, day: i64, date: Date, top: i32, seed: u64, phase: Phase, friend: bool, rival: bool) -> Observation {
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
        goal: a.goal as i32,
        mood: a.mood as i32,
        friend_present: friend,
        rival_present: rival,
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
            Phase::Evening => if pubnight && a.sex == 1 { "The Pelican".into() } else { home },
            Phase::Night => home,
        },
        "practitioner" => match phase {
            Phase::Dawn => "the surgery".into(),
            Phase::Forenoon | Phase::Afternoon => "on the rounds".into(),
            Phase::Evening => if pubnight && a.sex == 1 { "The Pelican".into() } else { home },
            Phase::Night => home,
        },
        "blunt_hand" => match phase {
            Phase::Dawn => "the yard".into(),
            // a tradesperson is at their own place of work; a hired hand is "about the town"
            Phase::Forenoon | Phase::Afternoon => if a.trade.is_some() { home } else { "at work about the town".into() },
            Phase::Evening => if pubnight && a.sex == 1 { "The Pelican".into() } else { a.seat.clone() },
            Phase::Night => a.seat.clone(),
        },
        "official" => match phase {
            Phase::Dawn => "the study".into(),
            Phase::Forenoon | Phase::Afternoon => if a.trade.is_some() { home } else { "on parish visits".into() },
            // the parson sits over his sermon of an evening; the rest are at home
            Phase::Evening => if a.seat == "The Vicarage" { "the vestry".into() } else { a.seat.clone() },
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
/// the phase, so "doing now" reads sensibly and never out of its hour (no lessons at dinner).
fn routine_doing(a: &Agent, phase: Phase, wd: Weekday) -> String {
    if wd == Weekday::Sunday && matches!(phase, Phase::Forenoon) {
        return "at church".into();
    }
    // night: the town is abed
    if matches!(phase, Phase::Night) {
        return if a.archetype == "practitioner" { "abed, unless called out".into() } else { "abed".into() };
    }
    // evening: dinner and the fireside; the working men to the Pelican on a pub-night
    if matches!(phase, Phase::Evening) {
        let pub_men = matches!(wd, Weekday::Friday | Weekday::Saturday) && a.sex == 1;
        return match a.archetype.as_str() {
            "genteel_status_seeker" => "at dinner",
            "hill_farmer" | "scheming_improver" => if pub_men { "at the Pelican" } else { "by the fire" },
            "blunt_hand" => if pub_men { "at the Pelican" } else { "at home of an evening" },
            "practitioner" => "at home, of an evening",
            "official" if a.seat == "The Vicarage" => "at his sermon",
            "child" => "at supper, then bed",
            _ => "at home of an evening",
        }
        .into();
    }
    // the working day proper: dawn, forenoon, afternoon
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
            _ => "at home",
        }
        .into(),
        "hill_farmer" | "scheming_improver" => match phase {
            Phase::Dawn => "at the milking",
            Phase::Forenoon => if wd == Weekday::Wednesday { "at the market" } else { "in the fields" },
            _ => "in the fields",
        }
        .into(),
        "practitioner" => match phase {
            Phase::Dawn => "at the surgery".into(),
            _ => "on the rounds".into(),
        },
        "official" => match phase {
            Phase::Dawn => "at home".into(),
            _ => a.trade.as_deref().map(trade_verb).unwrap_or("about the parish").into(),
        },
        "blunt_hand" => match phase {
            Phase::Dawn => "in the yard".into(),
            _ => a.trade.as_deref().map(trade_verb).unwrap_or("at their work").into(),
        },
        "child" => match phase {
            Phase::Dawn => "at home".into(),
            _ => "at lessons and mischief".into(),
        },
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
        o.goal,
        o.mood,
        o.friend_present as i32,
        o.rival_present as i32,
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
            nudge_mood(&mut world.agents[i], 3);
            if let Some(t) = call_target(world, i, &mut rng) {
                let tname = world.agents[t].name.clone();
                // a call is a small kindness, warmly received and warmly paid
                world.nudge_aff(t, i, 2);
                world.nudge_aff(i, t, 1);
                let line = pick(&mut rng, &[
                    "{n} paid an afternoon call on {t}, and was thought to look very well.",
                    "{n} called on {t}, leaving a card and a good impression.",
                    "{n} took tea with {t}, and the visit was a success on both sides.",
                    "{n} called on {t} — cultivating the acquaintance, said the unkind.",
                ]).replace("{n}", &name).replace("{t}", &tname);
                out.push(mk("status", line));
            } else {
                let line = pick(&mut rng, &[
                    "{n} paid a round of calls, and was thought to look very well.",
                    "{n} went visiting, and was everywhere civilly received.",
                ]).replace("{n}", &name);
                out.push(mk("status", line));
            }
        }
        Action::GiveDinner => {
            clamp_standing(&mut world.agents[i], 3);
            world.agents[i].purse -= 6;
            nudge_mood(&mut world.agents[i], 7);
            let (guests, snub) = dinner_guests(world, i, &mut rng);
            for &g in &guests {
                world.nudge_aff(g, i, 3); // a good evening warms the guest to the host
                world.nudge_aff(i, g, 2);
            }
            let mut line = if guests.is_empty() {
                match rng.gen_range(0..2) {
                    0 => format!("{name} gave a little dinner — rather beyond the means of {seat}, but handsomely done."),
                    _ => format!("{name} held a small evening party, the candles lit and the good silver out."),
                }
            } else {
                let who = name_list(world, &guests);
                match rng.gen_range(0..3) {
                    0 => format!("{name} had {who} to dine at {seat}; the table did not disgrace them."),
                    1 => format!("{name} gave a little dinner for {who} — beyond the means of {seat}, but handsomely done."),
                    _ => format!("{name} held an evening party, {who} among the company, the good silver out."),
                }
            };
            if let Some(s) = snub {
                let sname = world.agents[s].name.clone();
                if world.agents[s].standing > world.agents[i].standing {
                    // cutting someone *above* you is envy: it hardens the host's own heart toward
                    // the blocker far more than theirs — this is the seed of a declared rivalry.
                    world.nudge_aff(i, s, -8);
                    world.nudge_aff(s, i, -3);
                } else {
                    world.nudge_aff(s, i, -4); // an established rival, cut: they cool toward the host
                    world.nudge_aff(i, s, -2);
                }
                line.push_str(&format!(" {sname}, it was noted, was not asked."));
                world.spawn_news(&name, &format!("who {name} left off the dinner list"), -2, day, &[]);
            }
            out.push(mk("status", line));
            world.spawn_news(&name, &format!("the dinner-party at {seat}"), 2, day, &[]);
        }
        Action::Economise => {
            world.agents[i].purse += 4;
            clamp_standing(&mut world.agents[i], -1);
            nudge_mood(&mut world.agents[i], -5);
            let line = pick(&mut rng, &[
                "{n} made do and mended, and hoped no one would notice the turned collar.",
                "{n} let the fire go cold by four, and called it economy.",
                "{n} gave the cook the evening off, and dined plainly.",
            ]).replace("{n}", &name);
            out.push(mk("household", line));
            if rng.gen_bool(0.4) {
                world.spawn_news(&name, &format!("the straitened economies at {seat}"), -2, day, &[]);
            }
        }
        Action::KeepUp => {
            world.agents[i].purse -= 4;
            clamp_standing(&mut world.agents[i], 1);
            nudge_mood(&mut world.agents[i], -2);
            let line = pick(&mut rng, &[
                "{n} kept up appearances, whatever the bank might think of it.",
                "{n} ordered the new gloves all the same, and said nothing of the cost.",
                "{n} would not be seen to feel the pinch, and did not.",
            ]).replace("{n}", &name);
            out.push(mk("status", line));
        }
        Action::TendStock => {
            let line = pick(&mut rng, &[
                "{n} was out among the stock before light.",
                "{n} was up the top field with the beasts at first light.",
                "{n} saw to the byre before the rest of the house was stirring.",
            ]).replace("{n}", &name);
            out.push(mk("practice", line));
        }
        Action::Haggle => {
            let good = rng.gen_bool(0.55);
            world.agents[i].purse += if good { 6 } else { -2 };
            nudge_mood(&mut world.agents[i], if good { 6 } else { -7 });
            let line = if good {
                pick(&mut rng, &[
                    "{n} drove a hard bargain at the mart and came home pleased.",
                    "{n} sold well at the mart, and stood a round on the strength of it.",
                    "{n} got their price at the mart, the buyers grumbling.",
                ])
            } else {
                pick(&mut rng, &[
                    "{n} found the mart slow, and the buyers slower.",
                    "{n} brought half the stock home again, the trade being dead.",
                    "{n} took what was offered at the mart, and was sour about it.",
                ])
            }.replace("{n}", &name);
            out.push(mk("market", line));
        }
        Action::Graft => {
            let line = pick(&mut rng, &[
                "{n} got the work done, and said little about how.",
                "{n} put in a long day, and was not thanked for it.",
                "{n} saw the job through, the master taking the credit.",
            ]).replace("{n}", &name);
            out.push(mk("household", line));
        }
        Action::Scheme => {
            let win = rng.gen_bool(0.45);
            if win {
                world.agents[i].purse += 8;
                clamp_standing(&mut world.agents[i], 2);
                nudge_mood(&mut world.agents[i], 14);
                let (line, topic) = match rng.gen_range(0..3) {
                    0 => (
                        format!("{name}'s latest improvement actually answered, to general astonishment."),
                        format!("{name}'s scheme that, against all odds, worked"),
                    ),
                    1 => (
                        format!("{name}'s new contrivance paid for itself by harvest, the doubters silent."),
                        format!("{name}'s contrivance that paid"),
                    ),
                    _ => (
                        format!("{name} backed a notion the parish had laughed at, and was proved right."),
                        format!("{name} being proved right after all"),
                    ),
                };
                out.push(mk("scheme", line));
                world.spawn_news(&name, &topic, 2, day, &[]);
            } else {
                world.agents[i].purse -= 7;
                clamp_standing(&mut world.agents[i], -2);
                nudge_mood(&mut world.agents[i], -20);
                let (line, topic) = match rng.gen_range(0..3) {
                    0 => (
                        format!("{name}'s latest improvement came to grief in the mud. Tot, or his like, had said it would."),
                        format!("{name}'s improvement come to grief"),
                    ),
                    1 => (
                        format!("{name}'s grand scheme stuck fast in the wet, and the parish enjoyed it."),
                        format!("{name}'s scheme stuck in the mud"),
                    ),
                    _ => (
                        format!("{name} sank good money in a notion that failed, and heard about it at the Pelican."),
                        format!("the money {name} sank in a failed notion"),
                    ),
                };
                out.push(mk("scheme", line));
                world.spawn_news(&name, &topic, -2, day, &[]);
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
            let line = pick(&mut rng, &[
                "{n} drove the rounds from farm to farm, carrying the news from door to door.",
                "{n} was called out to a beast at one of the farms, and stopped for tea at two more.",
                "{n} went the rounds, and heard the half of the parish's business doing it.",
            ]).replace("{n}", &name);
            out.push(mk("practice", line));
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

/// An LLM-invented happening, now with *consequences*: the model picks an effect-kind from a
/// fixed vocabulary and writes the prose; the host applies a bounded, deterministic effect.
/// Recorded (so replay holds) and folded at its day, like providence.
#[derive(Clone)]
pub struct Wildcard {
    pub kind: String,   // fire|windfall|fair|blight|scandal|stranger|foundling|wonder
    pub target: String, // a townsperson's name, or "the town"
    pub text: String,   // the chronicler's prose
}

/// An LLM verdict at one soul's turning point — the genuine intelligence. A hinge (a feud
/// that might be forgiven, ruin faced, a match across the class line) is put to Qwen with
/// the soul's whole dossier; it chooses from a fixed vocabulary and writes the line. Recorded
/// and folded with a bounded effect, so the choice is the model's but the world stays exact.
#[derive(Clone)]
pub struct Decree {
    pub subject: String, // the soul deciding
    pub kind: String,    // feud | ruin | match
    pub target: String,  // the other party (a rival, a suitor), or ""
    pub choice: String,  // forgive|nurse · leave|stay|appeal · accept|refuse
    pub text: String,    // the chronicler's prose
}

/// Two souls the driver can set talking of their own accord, with the briefs to stage it.
pub struct ConversePair {
    pub a: usize,
    pub a_name: String,
    pub a_brief: String,
    pub b: usize,
    pub b_name: String,
    pub b_brief: String,
    pub relation: String,
}

/// A soul the driver can set to reflecting, with the dossier of their life to contemplate.
pub struct ReflectSubject {
    pub name: String,
    pub dossier: String,
}

/// A turning point the driver can put to the oracle — assembled from world state for the bin.
pub struct Hinge {
    pub subject: usize,
    pub subject_name: String,
    pub kind: String,
    pub target: i32,
    pub target_name: String,
    pub situation: String,   // the dilemma, in words
    pub options: Vec<String>,
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
                // --target is the newcomer's name; --note is flavour (their trade, their air).
                let name = if t.is_empty() { "A stranger".to_string() } else { t.clone() };
                let mut agent = make_agent(&name, "blunt_hand", "the empty cottage", 25, 12, 1, 33, day);
                agent.origin = Some("parts unknown".into());
                let blurb = if iv.note.is_empty() {
                    format!("{name} arrived in Thrushcombe and took the empty cottage. Nobody knew quite who they were.")
                } else {
                    format!("{name} — {} — arrived in Thrushcombe and took the empty cottage. Nobody knew quite who they were.", iv.note)
                };
                out.push(mk("providence", &name, blurb));
                world.spawn_news(&name, "the stranger lately come to the empty cottage", 1, day, &[]);
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
        goal: 0,
        goal_target: -1,
        mood: temperament(arch).1,
        courting: -1,
        courtship: 0,
        acted_day: -1,
        rival: -1,
        feud: 0,
        intent: 0,
        intent_goal: 0,
        intent_age: 0,
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

// ----------------------------------------------------------------------------- individuation
//
// Goals, memory and mood — what makes a soul an *individual* pursuing something, not just an
// archetype reacting to stats. All deterministic.

/// Temperament by type: (sensitivity %, resting baseline). Not everyone feels a blow or a
/// boon the same — the gentry are touchy about face, the hill folk stoic and a touch dour,
/// the improver mercurial, the working folk phlegmatic.
fn temperament(archetype: &str) -> (i32, i16) {
    match archetype {
        "genteel_status_seeker" => (135, 0),  // volatile about standing
        "hill_farmer" => (65, -8),            // taciturn, weathered
        "scheming_improver" => (140, 6),      // mercurial optimist
        "blunt_hand" => (80, 0),              // phlegmatic
        "practitioner" => (90, 4),            // even, good-humoured
        "official" => (85, 0),                // measured
        "child" => (110, 10),                 // resilient and sunny
        _ => (100, 0),
    }
}

/// Nudge a soul's mood, scaled by their temperament — a snub wounds the gentry more than
/// the hill folk.
fn nudge_mood(a: &mut Agent, d: i16) {
    let (sens, _) = temperament(&a.archetype);
    let scaled = (d as i32 * sens / 100) as i16;
    a.mood = (a.mood + scaled).clamp(-100, 100);
}

/// A word for a soul's present spirits.
pub fn mood_word(m: i16) -> &'static str {
    match m {
        x if x <= -55 => "grieving",
        x if x <= -28 => "low",
        x if x <= -9 => "out of sorts",
        x if x < 9 => "content",
        x if x < 28 => "in good humour",
        x if x < 55 => "in good spirits",
        _ => "triumphant",
    }
}

/// Their ambition, in words.
/// Strip the small oracle's stock filler tics ("I daresay", "I warrant" …) wherever they
/// fall, then tidy the spacing and sentence-capitalisation left behind. qwen leans on these
/// regardless of how the prompt scolds it, so excising them deterministically is far more
/// reliable than asking. Unicode-safe; only the ASCII filler words are touched.
pub fn strip_filler(line: &str) -> String {
    const FILLERS: [&str; 6] = ["i daresay", "i dare say", "i warrant", "i'll warrant", "i wager", "i'd wager"];
    let chars: Vec<char> = line.chars().collect();
    // normalise curly apostrophes to ASCII for matching — the model emits ’ as often as '
    let norm = |c: char| if c == '\u{2019}' || c == '\u{2018}' { '\'' } else { c };
    let low: Vec<char> = chars.iter().map(|c| norm(c.to_ascii_lowercase())).collect();
    let n = chars.len();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '\'' || c == '\u{2019}' || c == '\u{2018}';
    let mut out: Vec<char> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let boundary = i == 0 || !is_word(chars[i - 1]);
        let mut hit = 0usize;
        if boundary {
            for f in FILLERS {
                let fc: Vec<char> = f.chars().collect();
                if i + fc.len() <= n
                    && (0..fc.len()).all(|k| low[i + k] == fc[k])
                    && (i + fc.len() == n || !is_word(chars[i + fc.len()]))
                {
                    hit = fc.len();
                    break;
                }
            }
        }
        if hit > 0 {
            i += hit;
            while i < n && (chars[i] == ' ' || chars[i] == ',') { i += 1; } // swallow trailing comma/space
            while matches!(out.last(), Some(' ')) { out.pop(); }            // and any space we left behind
            // if the filler sat between a comma and a sentence end, drop the now-dangling comma
            if i < n && matches!(chars[i], '.' | '!' | '?' | ';' | ':') && matches!(out.last(), Some(',')) {
                out.pop();
            }
            // restore one separating space, unless at line start or the next char is punctuation
            if !out.is_empty() && i < n && chars[i] != ' ' && !matches!(chars[i], ',' | '.' | ';' | ':' | '!' | '?') {
                out.push(' ');
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    let mut s: String = out.into_iter().collect();
    s = s.trim().to_string();
    while s.contains("  ") { s = s.replace("  ", " "); }
    s = s.replace(" ,", ",").replace(" .", ".").replace(" —,", " —");
    // recapitalise the opening letter and the first letter of each new sentence
    let mut cap = true;
    s.chars()
        .map(|c| {
            if cap && c.is_alphabetic() { cap = false; return c.to_ascii_uppercase(); }
            if matches!(c, '.' | '!' | '?') { cap = true; }
            c
        })
        .collect()
}

pub fn goal_label(world: &World, kind: u8, target: i32) -> String {
    let who = |t: i32| world.agents.get(t as usize).map(|a| a.name.clone()).unwrap_or_else(|| "—".into());
    match kind {
        1 => "to clear their debts".into(),
        2 => "to rise in the world".into(),
        3 => format!("to see {} well married", who(target)),
        4 => format!("to get the better of {}", who(target)),
        5 => "to make their fortune".into(),
        _ => "to keep their place".into(),
    }
}

/// Declare, sustain, and lay to rest the town's rivalries. A grudge that hardens past a
/// threshold becomes a *named nemesis* a soul carries until it is resolved — the rival dies
/// or leaves, the quarrel is made up, or the soul gets the better of them. Unlike a goal
/// recomputed from the day's standings, a declared rivalry is a durable relationship: once
/// set it endures, and drives the soul's whole ambition until something settles it.
fn tend_rivalries(world: &mut World, day: i64, date: Date, out: &mut Vec<Event>) {
    let mk = |actor: &str, text: String| Event {
        day,
        date: date.to_string(),
        kind: "rivalry".into(),
        actor: actor.into(),
        text,
    };
    let n = world.agents.len();
    for i in 0..n {
        if !world.agents[i].active() || world.agents[i].archetype == "child" {
            continue;
        }
        let r = world.agents[i].rival;
        if r >= 0 {
            let r = r as usize;
            if !world.agents[r].active() {
                // the rival is gone — the old quarrel is buried with them
                world.agents[i].rival = -1;
                if world.agents[i].goal == 4 {
                    let (g, t) = assess_goal(world, i, day);
                    world.agents[i].goal = g;
                    world.agents[i].goal_target = t;
                }
            } else if world.aff(i, r) > -12 {
                // the feeling has cooled into civility — they have made up the quarrel
                let (a, b) = (world.agents[i].name.clone(), world.agents[r].name.clone());
                world.agents[i].rival = -1;
                if world.agents[i].goal == 4 {
                    let (g, t) = assess_goal(world, i, day);
                    world.agents[i].goal = g;
                    world.agents[i].goal_target = t;
                }
                out.push(mk(&a, format!("{a} and {b} have made up their old quarrel, to the parish's mild disappointment.")));
            }
            continue;
        }
        // no rival yet: a grudge hardened past bearing against a living peer-or-superior becomes one
        let target = (0..n)
            .filter(|&j| {
                j != i
                    && Some(j) != world.agents[i].spouse
                    && world.agents[j].active()
                    && world.agents[j].parent != Some(i)
                    && world.agents[i].parent != Some(j)
                    && world.agents[j].standing >= world.agents[i].standing - 4 // a peer or a superior — you don't war on a clear inferior
                    && world.aff(i, j) <= -40
            })
            .min_by_key(|&j| world.aff(i, j));
        if let Some(t) = target {
            world.agents[i].rival = t as i32;
            world.agents[i].goal = 4;
            world.agents[i].goal_target = t as i32;
            let (a, b) = (world.agents[i].name.clone(), world.agents[t].name.clone());
            out.push(mk(&a, format!("{a} has set themselves against {b}, and means to get the better of them.")));
            world.spawn_news(&a, &format!("the bad blood between {a} and {b}"), -2, day, &[]);
        }
    }
}

/// Derive a soul's ambition from their situation.
fn assess_goal(world: &World, i: usize, day: i64) -> (u8, i32) {
    let a = &world.agents[i];
    if a.archetype == "child" {
        return (0, -1);
    }
    // A declared nemesis is a soul's consuming ambition, above solvency and all else. The
    // rivalry is a *durable relationship* (maintained by `tend_rivalries`), not a fact
    // recomputed from the standings of the day — so it endures, and the goal endures with it.
    if a.rival >= 0 && world.agents.get(a.rival as usize).map_or(false, |r| r.active()) {
        return (4, a.rival);
    }
    if a.purse < -15 {
        return (1, -1); // ClearDebt — solvency comes before all
    }
    if let Some(child) = (0..world.agents.len()).find(|&c| {
        let x = &world.agents[c];
        x.active() && x.parent == Some(i) && x.archetype != "child" && x.spouse.is_none() && x.age(day) >= 18
    }) {
        return (3, child as i32); // MarryOff
    }
    let top = world.agents.iter().filter(|x| x.active()).map(|x| x.standing).max().unwrap_or(0);
    match a.archetype.as_str() {
        "genteel_status_seeker" => {
            if a.standing < top - 8 {
                (2, -1) // Rise
            } else {
                (0, -1)
            }
        }
        "hill_farmer" | "scheming_improver" => if (25..90).contains(&a.purse) { (5, -1) } else { (0, -1) },
        _ => (0, -1),
    }
}

fn goal_fulfilled(world: &World, i: usize, top: i32) -> bool {
    let a = &world.agents[i];
    match a.goal {
        1 => a.purse >= 0,
        2 => a.standing >= top - 2,
        3 => a.goal_target < 0 || world.agents.get(a.goal_target as usize).map_or(true, |c| c.spouse.is_some() || !c.active()),
        4 => false, // a rivalry is resolved by the feud campaign (tend_feuds), not a silent overtake
        5 => a.purse >= 100,
        _ => false,
    }
}

fn goal_triumph(world: &World, i: usize) -> String {
    let a = &world.agents[i];
    let nm = &a.name;
    let who = world.agents.get(a.goal_target as usize).map(|x| x.name.clone()).unwrap_or_else(|| "—".into());
    match a.goal {
        1 => format!("{nm} has cleared their debts at last, and walks the lighter for it."),
        2 => format!("{nm} has risen to the front rank of the town, and knows it."),
        3 => format!("{nm} has seen {who} well married, and is well content."),
        4 => format!("{nm} has, at long last, got the better of {who}."),
        5 => format!("{nm} has made a fortune, and means everyone to know it."),
        _ => format!("{nm} is well content with their lot."),
    }
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
        world.agents[i].courting = -1; // the dead court no one
        out.push(mk("death", &name, format!("{name}, of {seat}, is dead.")));
        world.spawn_news_idx(i, &format!("the death of {name}"), 0, day, &[]);
        // grief falls on the kin
        for k in 0..world.agents.len() {
            if world.agents[k].active() && (world.agents[k].spouse == Some(i) || world.agents[k].parent == Some(i) || world.agents[i].parent == Some(k)) {
                nudge_mood(&mut world.agents[k], -35);
            }
        }
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

    // --- courtship & marriage: a suit pursued over weeks, not a sudden match ---
    let elig: Vec<usize> = (0..n)
        .filter(|&i| {
            let a = &world.agents[i];
            // marrying age; the elderly don't generally remarry in this model (and a late
            // remarriage shouldn't quietly disinherit a bloodline)
            a.active() && a.archetype != "child" && a.spouse.is_none() && (18..=50).contains(&a.age(day))
        })
        .collect();

    // 1. carry on the courtships already begun — toward a wedding, or to nothing
    for &i in &elig {
        let t = world.agents[i].courting;
        if t < 0 {
            continue;
        }
        let tj = t as usize;
        let lost = tj >= world.agents.len()
            || !world.agents[tj].active()
            || world.agents[tj].spouse.is_some()
            || (world.agents[tj].courting >= 0 && world.agents[tj].courting != i as i32);
        if lost {
            let (ni, nj) = (world.agents[i].name.clone(), world.agents.get(tj).map(|a| a.name.clone()).unwrap_or_default());
            world.agents[i].courting = -1;
            world.agents[i].courtship = 0;
            nudge_mood(&mut world.agents[i], -12);
            out.push(mk("courtship", &ni, format!("{ni}'s hopes of {nj} came to nothing — another got there first.")));
            continue;
        }
        // a step in the courtship; the suit warms slower when courting above one's station
        let up = world.agents[tj].standing > world.agents[i].standing + 15;
        world.nudge_aff(i, tj, 5);
        world.nudge_aff(tj, i, if up { 2 } else { 4 });
        world.agents[i].courtship += 1;
        if rng.gen_bool(0.28) {
            let (ni, nj) = (world.agents[i].name.clone(), world.agents[tj].name.clone());
            let line = match rng.gen_range(0..3) {
                0 => format!("{ni} walked out with {nj} again, the long way round."),
                1 => format!("{ni} called on {nj}, and was asked to stay to tea."),
                _ => format!("{ni} and {nj} were seen with their heads together after church."),
            };
            out.push(mk("courtship", &ni, line));
        }
        let mutual = world.aff(i, tj) >= 30 && world.aff(tj, i) >= 28;
        if world.agents[i].courtship >= 30 && mutual {
            world.agents[i].spouse = Some(tj);
            world.agents[tj].spouse = Some(i);
            world.agents[i].courting = -1;
            world.agents[i].courtship = 0;
            world.agents[tj].courting = -1;
            world.agents[tj].courtship = 0;
            nudge_mood(&mut world.agents[i], 22);
            nudge_mood(&mut world.agents[tj], 22);
            let (ni, nj) = (world.agents[i].name.clone(), world.agents[tj].name.clone());
            let cross = stratum_archetype(&world.agents[i].archetype) != stratum_archetype(&world.agents[tj].archetype);
            let note = if cross { " — a match that set tongues wagging across the class line" } else { "" };
            out.push(mk("marriage", &ni, format!("{ni} and {nj} are to be married{note}, after a long and proper courtship.")));
            world.spawn_news(&ni, &format!("the engagement of {ni} and {nj}"), if cross { -2 } else { 1 }, day, &[]);
        } else if world.agents[i].courtship >= 75 && !mutual {
            world.agents[i].courting = -1;
            world.agents[i].courtship = 0;
            nudge_mood(&mut world.agents[i], -10);
            let (ni, nj) = (world.agents[i].name.clone(), world.agents[tj].name.clone());
            out.push(mk("courtship", &ni, format!("{ni} gave up the pursuit of {nj} at last; the feeling was never returned.")));
        }
    }

    // 2. a new courtship or two begun — a deliberate intention, not an accident
    let mut started = 0;
    for &i in &elig {
        if started >= 2 {
            break;
        }
        if world.agents[i].courting >= 0 || world.agents[i].spouse.is_some() {
            continue;
        }
        if !rng.gen_bool((1.6 / 365.0_f64).clamp(0.0, 1.0)) {
            continue;
        }
        let target = elig.iter().copied().find(|&j| {
            j != i
                && world.agents[j].spouse.is_none()
                && world.agents[j].sex != world.agents[i].sex
                && (world.agents[j].age(day) - world.agents[i].age(day)).abs() <= 16
                && !world.agents.iter().any(|a| a.active() && a.courting == j as i32) // not already spoken for
        });
        if let Some(j) = target {
            world.agents[i].courting = j as i32;
            world.agents[i].courtship = 0;
            let (ni, nj) = (world.agents[i].name.clone(), world.agents[j].name.clone());
            out.push(mk("courtship", &ni, format!("{ni} has begun to pay attentions to {nj}.")));
            started += 1;
        }
    }

    // 3. now and then a soul weds someone lately come to the town (no resident match made)
    for &i in &elig {
        if world.agents[i].spouse.is_some() || world.agents[i].courting >= 0 {
            continue;
        }
        if !rng.gen_bool((0.06 / 365.0_f64).clamp(0.0, 1.0)) {
            continue;
        }
        let osex = 1 - world.agents[i].sex;
        let (first, title) = if osex == 1 { (pick(&mut rng, FIRST_M), "Mr") } else { (pick(&mut rng, FIRST_F), "Miss") };
        let sname = format!("{title} {first} {}", pick(&mut rng, SURNAMES));
        let idx_new = n + newcomers.len();
        let age = world.agents[i].age(day);
        let sp_arch = world.agents[i].archetype.clone();
        let sp_purse = starting_purse(&sp_arch, &mut rng);
        let mut sp = make_agent(&sname, &sp_arch, &world.agents[i].seat.clone(), (world.agents[i].standing - 5).max(20), sp_purse, osex, age, day);
        sp.spouse = Some(i);
        sp.origin = Some(pick(&mut rng, ORIGINS).to_string());
        let from = sp.origin.clone().unwrap();
        world.agents[i].spouse = Some(idx_new);
        let ni = world.agents[i].name.clone();
        out.push(mk("marriage", &ni, format!("{ni} is to wed {sname}, lately of {from}.")));
        newcomers.push(sp);
        world.spawn_news(&ni, &format!("{ni}'s engagement to {sname}"), 1, day, &[]);
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

    // --- rivalries: declare, sustain, and lay to rest the durable grudges ---
    tend_rivalries(world, day, date, &mut out);

    // --- the feud, pressed: a declared rivalry is not a standing fact but a campaign, waged
    // over weeks with whispers and snubs that chip at the rival's face, toward a public reckoning ---
    for i in 0..world.agents.len() {
        let r = world.agents[i].rival;
        if r < 0 || !world.agents[i].active() {
            world.agents[i].feud = 0; // no standing quarrel — nothing to press
            continue;
        }
        let r = r as usize;
        if r >= world.agents.len() || !world.agents[r].active() {
            continue; // a dead or departed rival is tend_rivalries' to lay to rest
        }
        // press the campaign: the grudge deepens by the day, and now and then lands in the open
        world.agents[i].feud += 1;
        world.nudge_aff(i, r, -2);
        if rng.gen_bool(0.12) {
            world.agents[r].standing = (world.agents[r].standing - 1).max(0); // a whisper that tells
            let (a, b) = (world.agents[i].name.clone(), world.agents[r].name.clone());
            let line = match rng.gen_range(0..4) {
                0 => format!("{a} was heard running down {b} at the Pelican, and not for the first time."),
                1 => format!("{a} cut {b} dead in the high street, plain for all to see."),
                2 => format!("{a} let it be known, in the right ears, just what {a} thought of {b}."),
                _ => format!("{a} and {b} traded cold words after church, and the parish marked it."),
            };
            out.push(mk("rivalry", &a, line));
        }
        // the reckoning: once the campaign has been pressed home, it comes to a head
        if world.agents[i].feud >= 30 {
            let (sa, sb) = (world.agents[i].standing, world.agents[r].standing);
            let (a, b) = (world.agents[i].name.clone(), world.agents[r].name.clone());
            // whatever the outcome, a reckoned grudge is a *spent* one: the bad blood lifts
            // toward wary civility, so the quarrel settles instead of re-igniting every month
            let settle = |w: &mut World, i: usize, r: usize| {
                w.nudge_aff(i, r, 70);
                w.agents[i].rival = -1;
                w.agents[i].feud = 0;
                if w.agents[i].goal == 4 {
                    let (g, t) = assess_goal(w, i, day);
                    w.agents[i].goal = g;
                    w.agents[i].goal_target = t;
                }
            };
            if sa >= sb {
                // the schemer has the upper hand — they get the better of their rival in the open
                world.agents[r].standing = (world.agents[r].standing - 6).max(0);
                world.agents[i].standing = (world.agents[i].standing + 3).min(100);
                nudge_mood(&mut world.agents[i], 20);
                nudge_mood(&mut world.agents[r], -16);
                settle(world, i, r);
                out.push(mk("feud", &a, format!("{a} got the better of {b} at last, in front of the whole parish — and {b} felt the fall of it.")));
                world.spawn_news(&a, &format!("how {a} bested {b}"), 2, day, &[]);
            } else if sb >= sa + 12 {
                // the campaign broke on the rival's standing — the schemer is the one diminished
                world.agents[i].standing = (world.agents[i].standing - 4).max(0);
                nudge_mood(&mut world.agents[i], -18);
                settle(world, i, r);
                out.push(mk("feud", &a, format!("{a}'s long campaign against {b} came to nothing but {a}'s own embarrassment, and the parish tittered.")));
            } else if world.agents[i].feud >= 60 {
                // neither could land the blow — the quarrel guttered out, worn down to civility
                settle(world, i, r);
                out.push(mk("feud", &a, format!("the quarrel between {a} and {b} guttered out at last, both parties wearied of it.")));
            }
            // else: still simmering, the blow not yet landed — it goes on another day
        }
    }

    // --- the self-authored plan, run out to its reckoning: an ambition a soul set itself in a
    // reflective hour, carried over weeks while the policy layer pursues the matching goal, then
    // judged in the open — made good (by the purse, or the standing they aimed at) or come to nothing ---
    const PLAN_HORIZON: i16 = 28;
    for i in 0..world.agents.len() {
        if world.agents[i].intent == 0 {
            continue;
        }
        if !world.agents[i].active() {
            world.agents[i].intent = 0;
            world.agents[i].intent_age = 0;
            continue;
        }
        world.agents[i].intent_age += 1;
        if world.agents[i].intent_age < PLAN_HORIZON {
            continue; // still in the pursuit — the weeks of it play out through their daily acts
        }
        let a = world.agents[i].name.clone();
        let kind = world.agents[i].intent;
        let made_good = match kind {
            1 | 3 => world.agents[i].purse >= world.agents[i].intent_goal, // fortune / venture: by the purse
            2 => world.agents[i].standing >= world.agents[i].intent_goal,  // rise: by the standing
            _ => false,
        };
        let what = match kind { 1 => "mend their fortunes", 2 => "better their station", _ => "a bold venture" };
        if made_good {
            world.agents[i].standing = (world.agents[i].standing + if kind == 3 { 5 } else { 3 }).min(100);
            nudge_mood(&mut world.agents[i], if kind == 3 { 20 } else { 15 });
            out.push(mk("intent", &a, format!("{a} made good on the resolve to {what}, and the parish marked the doing of it.")));
            world.spawn_news(&a, &format!("how {a} made good on a resolve to {what}"), 1, day, &[]);
        } else if kind == 3 {
            // a bold venture that fails costs the schemer dear — purse and face both
            world.agents[i].purse -= 20;
            world.agents[i].standing = (world.agents[i].standing - 3).max(0);
            nudge_mood(&mut world.agents[i], -15);
            out.push(mk("intent", &a, format!("{a}'s bold venture came to nothing but loss, as the wiser heads had foretold.")));
            world.spawn_news(&a, &format!("the money {a} sank in a failed notion"), -1, day, &[]);
        } else {
            nudge_mood(&mut world.agents[i], -12);
            out.push(mk("intent", &a, format!("{a}'s resolve to {what} came to nothing, for all the trying.")));
            world.spawn_news(&a, &format!("how {a}'s hopes of {what} came to nothing"), -1, day, &[]);
        }
        world.agents[i].intent = 0;
        world.agents[i].intent_age = 0;
        world.agents[i].intent_goal = 0;
        let (g, t) = assess_goal(world, i, day); // re-take their bearings now the plan is done
        world.agents[i].goal = g;
        world.agents[i].goal_target = t;
    }

    // --- goals: a fulfilled ambition is a triumph; otherwise the odd fresh resolve ---
    let top = world.agents.iter().filter(|x| x.active()).map(|x| x.standing).max().unwrap_or(0);
    for i in 0..world.agents.len() {
        if !world.agents[i].active() || world.agents[i].archetype == "child" {
            continue;
        }
        if world.agents[i].goal != 0 && goal_fulfilled(world, i, top) {
            let nm = world.agents[i].name.clone();
            out.push(mk("triumph", &nm, goal_triumph(world, i)));
            nudge_mood(&mut world.agents[i], 25);
            let won = world.agents[i].goal;
            world.agents[i].goal = 0; // their ambition met, they rest content (until the next resolve)
            world.agents[i].goal_target = -1;
            if won == 4 {
                world.agents[i].rival = -1; // the rivalry is won and laid to rest
            }
        } else if day % 365 == (i as i64) % 365 {
            let (g, t) = assess_goal(world, i, day);
            world.agents[i].goal = g;
            world.agents[i].goal_target = t;
        }
    }
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
        // Only faint feelings fade with neglect; a real grudge or a true bond is remembered,
        // so a rivalry can accumulate across the years instead of dissolving every week.
        for v in world.affinity.values_mut() {
            if v.abs() < 10 {
                *v -= v.signum();
            }
        }
        for i in 0..world.agents.len() {
            if !world.agents[i].active() {
                continue;
            }
            let base = temperament(&world.agents[i].archetype).1; // spirits drift toward their baseline
            world.agents[i].mood += (base - world.agents[i].mood).signum() * 2;
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
        nudge_mood(&mut world.agents[f], -8);
        nudge_mood(&mut world.agents[t], -5);
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
        nudge_mood(&mut world.agents[f], 6);
        nudge_mood(&mut world.agents[t], 6);
        let text = match rng.gen_range(0..2) {
            0 => format!("{nf} and {nt} had their heads together at the {place}, the best of friends."),
            _ => format!("{nf} and {nt} were thick as thieves at the {place}."),
        };
        out.push(mk("bond", &nf, text));
    }
    out
}

/// The Thrushcombe & District Show — the year's great set-piece. Classes are judged, rosettes
/// and the silver cup awarded; a win lifts a soul's standing and spirits, and the losing of it
/// (especially by the improver, to a hill farmer) is its own small tragedy. Deterministic.
fn the_show(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut rng = rng_for(seed ^ 0x5409_0000_0000, day);
    let mk = |actor: &str, text: String| Event { day, date: date.to_string(), kind: "show".into(), actor: actor.into(), text };
    let mut out = vec![mk(
        "Thrushcombe",
        "The Thrushcombe & District Show was held on the green, the whole town turned out among the marquees, the prize beasts and the produce tents.".into(),
    )];

    let active: Vec<usize> = (0..world.agents.len()).filter(|&i| world.agents[i].active() && world.agents[i].archetype != "child").collect();
    // a win is mostly merit (standing) but the rosette is never certain — luck judges too
    let pick = |rng: &mut ChaCha8Rng, cands: &[usize], merit: &dyn Fn(usize) -> i64| -> Option<usize> {
        if cands.is_empty() {
            return None;
        }
        let w: Vec<i64> = cands.iter().map(|&i| (merit(i) + rng.gen_range(0..45)).max(1)).collect();
        let total: i64 = w.iter().sum();
        let mut r = rng.gen_range(0..total);
        for (k, &c) in cands.iter().enumerate() {
            r -= w[k];
            if r < 0 {
                return Some(c);
            }
        }
        cands.last().copied()
    };
    let award = |world: &mut World, i: usize, st: i32, md: i16, purse: i32| {
        clamp_standing(&mut world.agents[i], st);
        nudge_mood(&mut world.agents[i], md);
        world.agents[i].purse += purse;
    };

    let mut champions: Vec<usize> = Vec::new();

    // Best Beast — the farmers' class, and the improver's yearly heartbreak
    let farmers: Vec<usize> = active.iter().copied().filter(|&i| matches!(world.agents[i].archetype.as_str(), "hill_farmer" | "scheming_improver")).collect();
    if let Some(w) = pick(&mut rng, &farmers, &|i| world.agents[i].standing as i64 + world.agents[i].purse.max(0) as i64 / 6) {
        award(world, w, 6, 18, 4);
        let nm = world.agents[w].name.clone();
        out.push(mk(&nm, format!("{nm}'s beast took the red rosette for the champion of the show.")));
        world.spawn_news(&nm, &format!("{nm}'s prize beast at the Show"), 2, day, &[]);
        champions.push(w);
        // the improver, if beaten, takes it hard
        for &f in &farmers {
            if f != w && world.agents[f].archetype == "scheming_improver" {
                nudge_mood(&mut world.agents[f], -10);
                world.nudge_aff(f, w, -8);
                let fn_ = world.agents[f].name.clone();
                out.push(mk(&fn_, format!("{fn_}, who had counted on the beast prize, went home black as thunder.")));
            }
        }
    }

    // Best Garden & Produce — the gentlefolk's quiet war of marrows and roses
    let gentry: Vec<usize> = active.iter().copied().filter(|&i| world.agents[i].archetype == "genteel_status_seeker").collect();
    if let Some(w) = pick(&mut rng, &gentry, &|i| world.agents[i].standing as i64) {
        award(world, w, 4, 12, 0);
        let nm = world.agents[w].name.clone();
        out.push(mk(&nm, format!("{nm} carried off the cup for best garden produce, to no little satisfaction.")));
        champions.push(w);
    }

    // Best Preserves & Baking — the women's class
    let women: Vec<usize> = active.iter().copied().filter(|&i| world.agents[i].sex == 0).collect();
    if let Some(w) = pick(&mut rng, &women, &|i| world.agents[i].standing as i64 / 2 + 10) {
        award(world, w, 3, 10, 2);
        let nm = world.agents[w].name.clone();
        out.push(mk(&nm, format!("{nm}'s preserves took first in the produce tent, and the recipe was begged the whole afternoon.")));
        champions.push(w);
    }

    // the silver Champion's Cup — best in show, from among the day's winners
    if let Some(&best) = champions.iter().max_by_key(|&&i| world.agents[i].standing) {
        award(world, best, 5, 14, 0);
        let nm = world.agents[best].name.clone();
        out.push(mk(&nm, format!("And the silver Champion's Cup, best in the whole Show, went to {nm} — the talk of the green.")));
        world.spawn_news(&nm, &format!("{nm} taking the Champion's Cup at the Show"), 2, day, &[]);
    }

    // now and then a judging is disputed, and a friendship founders on a rosette
    if farmers.len() >= 2 && rng.gen_bool(0.18) {
        let a = farmers[rng.gen_range(0..farmers.len())];
        let mut b = farmers[rng.gen_range(0..farmers.len())];
        if a == b {
            b = *farmers.iter().find(|&&x| x != a).unwrap_or(&a);
        }
        if a != b {
            world.nudge_aff(a, b, -14);
            world.nudge_aff(b, a, -14);
            let (na, nb) = (world.agents[a].name.clone(), world.agents[b].name.clone());
            out.push(mk(&na, format!("{na} disputed the judging hotly with {nb}, and the two were not on speaking terms by teatime.")));
        }
    }
    out
}

/// The rumour mill — where the town's gossip is actually *made*: scandal and romance
/// whispered at the market, after church, and over the Pelican's beer. Spicier and more
/// frequent than the news that incidents throw off, and most of it unkind.
fn rumour_mill(world: &mut World, day: i64, phase: Phase, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let wd = date.weekday();
    let venue = if matches!(phase, Phase::Forenoon) && wd == Weekday::Sunday {
        "after church"
    } else if matches!(phase, Phase::Forenoon) && wd == Weekday::Wednesday {
        "at the market"
    } else if matches!(phase, Phase::Evening) {
        "over the Pelican's beer"
    } else {
        return out;
    };
    let mut rng = rng_for(seed ^ 0x59ED_0000_0000, day * PHASES + phase.ord());
    if !rng.gen_bool(0.42) {
        return out;
    }
    let adults: Vec<usize> = (0..world.agents.len())
        .filter(|&i| world.agents[i].active() && world.agents[i].archetype != "child")
        .collect();
    if adults.len() < 6 {
        return out;
    }
    let subj = adults[rng.gen_range(0..adults.len())];
    let mut other = subj;
    for _ in 0..4 {
        let o = adults[rng.gen_range(0..adults.len())];
        if o != subj {
            other = o;
            break;
        }
    }
    let sname = world.agents[subj].name.clone();
    let oname = world.agents[other].name.clone();
    let s_married = world.agents[subj].spouse.is_some();
    let o_married = world.agents[other].spouse.is_some();
    let cross = stratum_archetype(&world.agents[subj].archetype) != stratum_archetype(&world.agents[other].archetype);
    let broke = world.agents[subj].purse < -20;

    let (topic, valence, line): (String, i32, String) = match rng.gen_range(0..11) {
        0 | 1 if other != subj && (s_married || o_married) => (
            format!("the goings-on between {sname} and {oname}"),
            -3,
            format!("It was whispered {venue} that {sname} and {oname} are a deal too friendly — and one of them spoken for."),
        ),
        2 if other != subj && cross && world.agents[subj].sex != world.agents[other].sex => (
            format!("{sname} walking out with {oname}"),
            -2,
            format!("They do say {venue} that {sname} has been seen walking out with {oname}, and them not of the same sort at all."),
        ),
        3 if other != subj && world.agents[subj].sex != world.agents[other].sex => (
            format!("{sname} being sweet on {oname}"),
            1,
            format!("It's the talk {venue} that {sname} is sweet on {oname}."),
        ),
        4 => (
            format!("{sname}'s fondness for the bottle"),
            -2,
            format!("{sname} was the worse for drink {venue}, by all accounts — not for the first time."),
        ),
        5 if broke => (
            format!("{sname} being over the ears in debt"),
            -3,
            format!("It's no secret {venue} now that {sname} is over the ears in debt."),
        ),
        6 => (
            format!("the sorry state of {sname}'s affairs"),
            -2,
            format!("They were saying {venue} that {sname}'s affairs are in a worse tangle than anyone lets on."),
        ),
        7 => (
            format!("the airs {sname} gives themselves"),
            -1,
            format!("It was remarked {venue} that {sname} has been giving themselves no end of airs of late."),
        ),
        8 => (
            format!("a secret of {sname}'s"),
            -1,
            format!("There's a deal more to {sname} than meets the eye, they reckon {venue}."),
        ),
        _ => return out, // most idle sessions add nothing — keeps it spicy, not constant
    };
    out.push(Event { day, date: date.to_string(), kind: "gossip".into(), actor: sname.clone(), text: line });
    world.spawn_news(&sname, &topic, valence, day, &[oname.as_str()]);
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
    wildcards: BTreeMap<i64, Vec<Wildcard>>,
    decrees: BTreeMap<i64, Vec<Decree>>,
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
    pub wants: String,            // their ambition, in words
    pub mood: String,             // their present spirits, in a word
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
    "death", "succession", "marriage", "birth", "comingofage", "feud", "bond", "providence", "triumph", "courtship", "decree", "show", "rivalry", "talk", "intent",
];

/// Bump when World layout or step_day logic changes — older snapshots are then ignored
/// and the world re-folds from genesis (and writes fresh checkpoints).
const SNAPSHOT_VERSION: i64 = 25;
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
         CREATE TABLE IF NOT EXISTS weather(day INTEGER PRIMARY KEY, precip REAL, tmax REAL, tmin REAL);
         -- LLM-invented wildcards with bounded effects, recorded and folded at their day.
         CREATE TABLE IF NOT EXISTS wildcards(
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL, text TEXT NOT NULL);
         -- LLM verdicts at a soul's turning point, recorded and folded with a bounded effect.
         CREATE TABLE IF NOT EXISTS decrees(
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL, subject TEXT NOT NULL, kind TEXT NOT NULL,
            target TEXT NOT NULL, choice TEXT NOT NULL, text TEXT NOT NULL);
         -- Conversations a soul has had: the transcript, and what they took away (their memory).
         CREATE TABLE IF NOT EXISTS dialogues(
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL, source TEXT NOT NULL, target TEXT NOT NULL,
            transcript TEXT NOT NULL, memory TEXT NOT NULL);
         -- A soul's own reflections: the private thought they came to, hour by hour. Self-memory.
         CREATE TABLE IF NOT EXISTS reflections(
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL, subject TEXT NOT NULL, thought TEXT NOT NULL);",
    )
}

fn load_decrees(conn: &Connection) -> rusqlite::Result<BTreeMap<i64, Vec<Decree>>> {
    let mut stmt = conn.prepare("SELECT day, subject, kind, target, choice, text FROM decrees ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, Decree { subject: r.get(1)?, kind: r.get(2)?, target: r.get(3)?, choice: r.get(4)?, text: r.get(5)? }))
    })?;
    let mut map: BTreeMap<i64, Vec<Decree>> = BTreeMap::new();
    for row in rows {
        let (day, d) = row?;
        map.entry(day).or_default().push(d);
    }
    Ok(map)
}

/// Apply the day's hinge verdicts: the prose, and a bounded effect by (kind, choice). The
/// *choice* is the oracle's; the consequence is fixed here, so the world stays exact.
fn apply_decrees(world: &mut World, day: i64, date: Date, list: &[Decree]) -> Vec<Event> {
    let mut out = Vec::new();
    for d in list {
        let Some(si) = world.idx(&d.subject) else { continue };
        let ti = world.idx(&d.target);
        // a turning-point verdict is a public beat; a private conversation or a private thought is not
        if d.kind != "dialogue" && d.kind != "reflect" {
            out.push(Event { day, date: date.to_string(), kind: "decree".into(), actor: d.subject.clone(), text: d.text.clone() });
        }
        match (d.kind.as_str(), d.choice.as_str()) {
            ("feud", "forgive") => {
                if let Some(t) = ti {
                    world.affinity.insert((si as u32, t as u32), 12);
                    world.affinity.insert((t as u32, si as u32), 10);
                    nudge_mood(&mut world.agents[si], 12);
                    world.spawn_news(&d.subject, &format!("{}'s reconciliation with {}", d.subject, d.target), 1, day, &[]);
                }
            }
            ("feud", "nurse") => {
                if let Some(t) = ti {
                    world.affinity.insert((si as u32, t as u32), -60);
                    nudge_mood(&mut world.agents[si], -8);
                    world.spawn_news(&d.subject, &format!("the lasting grudge between {} and {}", d.subject, d.target), -1, day, &[]);
                }
            }
            ("ruin", "leave") => {
                world.agents[si].departed = true;
                world.agents[si].courting = -1;
                world.spawn_news(&d.subject, &format!("{} leaving Thrushcombe", d.subject), -1, day, &[]);
            }
            ("ruin", "stay") => {
                nudge_mood(&mut world.agents[si], 25);
                clamp_standing(&mut world.agents[si], 4);
            }
            ("ruin", "appeal") => {
                // a well-placed soul quietly comes to their aid
                let helper = (0..world.agents.len())
                    .filter(|&j| j != si && world.agents[j].active() && world.agents[j].purse > 40)
                    .max_by_key(|&j| world.agents[j].standing);
                world.agents[si].purse += 40;
                nudge_mood(&mut world.agents[si], 12);
                if let Some(h) = helper {
                    world.agents[h].purse -= 40;
                    clamp_standing(&mut world.agents[h], 2); // a charity that's noticed
                }
            }
            ("match", "accept") => {
                // the courted relents: the suit becomes mutual and will come to a wedding
                if let Some(t) = ti {
                    world.affinity.insert((si as u32, t as u32), 35);
                    world.affinity.insert((t as u32, si as u32), 40);
                    nudge_mood(&mut world.agents[si], 14);
                    nudge_mood(&mut world.agents[t], 18);
                }
            }
            ("match", "refuse") => {
                // the suit is ended; the suitor goes away the worse for it
                if let Some(t) = ti {
                    world.agents[t].courting = -1;
                    world.agents[t].courtship = 0;
                    nudge_mood(&mut world.agents[t], -16);
                    world.affinity.insert((si as u32, t as u32), -10);
                }
            }
            // the felt residue of a conversation (subject = the soul spoken to, target = who spoke).
            // choice is "warmth:sway" — a warming or cooling, and any sway over what they now want.
            ("dialogue", c) => {
                let mut parts = c.split(':');
                match parts.next().unwrap_or("") {
                    "warmer" => {
                        if let Some(t) = ti {
                            // regard is earned over several meetings, not vaulted in one civil chat
                            world.nudge_aff(si, t, 7);
                            world.nudge_aff(t, si, 4);
                            // the town only gossips once a pair has genuinely grown close — not after
                            // a single pleasant exchange (emit once per pair, from the lower index)
                            if si < t && world.aff(si, t) >= 30 {
                                let (a, b) = (world.agents[si].name.clone(), world.agents[t].name.clone());
                                out.push(Event { day, date: date.to_string(), kind: "talk".into(), actor: a.clone(), text: format!("{a} and {b} were seen with their heads together, and parted the warmer for it.") });
                                world.spawn_news(&a, &format!("how thick {a} and {b} have grown"), 1, day, &[b.as_str()]);
                            }
                        }
                        nudge_mood(&mut world.agents[si], 5);
                    }
                    "colder" => {
                        if let Some(t) = ti {
                            world.nudge_aff(si, t, -12);
                            world.nudge_aff(t, si, -6);
                            // likewise the town only marks a falling-out once there is real coldness
                            if si < t && world.aff(si, t) <= -30 {
                                let (a, b) = (world.agents[si].name.clone(), world.agents[t].name.clone());
                                out.push(Event { day, date: date.to_string(), kind: "talk".into(), actor: a.clone(), text: format!("{a} and {b} had words, by all accounts, and parted stiffly.") });
                                world.spawn_news(&a, &format!("the words between {a} and {b}"), -1, day, &[b.as_str()]);
                            }
                        }
                        nudge_mood(&mut world.agents[si], -7);
                    }
                    _ => {}
                }
                // a conversation can change a soul's mind about what they want
                match parts.next().unwrap_or("none") {
                    "debt" => world.agents[si].goal = 1,    // resolved to clear their debts
                    "rise" => world.agents[si].goal = 2,    // spurred to rise in the world
                    "prosper" => world.agents[si].goal = 5, // talked into making their fortune
                    "content" => world.agents[si].goal = 0, // talked down, to rest content
                    "reconcile" => {
                        if let Some(t) = ti {
                            world.affinity.insert((si as u32, t as u32), world.aff(si, t).max(15));
                        }
                    }
                    _ => {}
                }
            }
            // the felt residue of an hour's reflection — a private settling of spirits, a turn
            // of ambition, a hardened feeling about one soul, and now and then a resolve to act
            // on it. choice is "mood:sway:regard:resolve"; target (if any) is the soul mused on.
            ("reflect", c) => {
                let mut parts = c.split(':');
                match parts.next().unwrap_or("") {
                    "lifts" => nudge_mood(&mut world.agents[si], 8),
                    "sinks" => nudge_mood(&mut world.agents[si], -8),
                    _ => {} // steadies — they come away even
                }
                match parts.next().unwrap_or("none") {
                    "debt" => world.agents[si].goal = 1,    // resolved to clear their debts
                    "rise" => world.agents[si].goal = 2,    // determined to rise in the world
                    "prosper" => world.agents[si].goal = 5, // set on making their fortune
                    "content" => world.agents[si].goal = 0, // made their peace, to rest content
                    _ => {}
                }
                let regard = parts.next().unwrap_or("none");
                let resolve = parts.next().unwrap_or("none");
                let plan = parts.next().unwrap_or("none");
                if let Some(t) = ti {
                    if t != si {
                        // a thought can warm or sour how they hold one particular soul
                        match regard {
                            "warmer" => world.nudge_aff(si, t, 10),
                            "colder" => world.nudge_aff(si, t, -10),
                            _ => {}
                        }
                        // …and, rarely, resolve them to act on it of their own accord
                        match resolve {
                            // pay court — begin a suit, if both are free and the match is possible
                            "court" => {
                                let (a, b) = (&world.agents[si], &world.agents[t]);
                                if a.courting < 0 && a.spouse.is_none() && b.spouse.is_none()
                                    && a.sex != b.sex && a.archetype != "child" && b.archetype != "child"
                                {
                                    world.agents[si].courting = t as i32;
                                    world.agents[si].courtship = 0;
                                }
                            }
                            // set themselves against them — a self-authored feud, pressed toward a reckoning
                            "confront" => {
                                world.agents[si].rival = t as i32;
                                world.agents[si].feud = 0;
                                world.nudge_aff(si, t, -15);
                            }
                            // resolve to make peace — spend the grudge they had been carrying
                            "mend" => {
                                world.nudge_aff(si, t, 25);
                                if world.agents[si].rival == t as i32 {
                                    world.agents[si].rival = -1;
                                    world.agents[si].feud = 0;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // a dated plan the soul set itself this hour — one at a time; an active plan
                // runs its course. the target threshold is captured from their present state,
                // so making good is a real test of the weeks of pursuit that follow.
                if world.agents[si].intent == 0 {
                    let (purse, standing) = (world.agents[si].purse, world.agents[si].standing);
                    match plan {
                        "fortune" => {
                            world.agents[si].intent = 1;
                            world.agents[si].intent_goal = purse + 30;
                            world.agents[si].intent_age = 0;
                            world.agents[si].goal = if purse < 0 { 1 } else { 5 }; // clear debt / make fortune
                        }
                        "rise" => {
                            world.agents[si].intent = 2;
                            world.agents[si].intent_goal = (standing + 6).min(100);
                            world.agents[si].intent_age = 0;
                            world.agents[si].goal = 2;
                        }
                        "venture" => {
                            world.agents[si].intent = 3;
                            world.agents[si].intent_goal = purse + 70;
                            world.agents[si].intent_age = 0;
                            world.agents[si].goal = 5;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn load_wildcards(conn: &Connection) -> rusqlite::Result<BTreeMap<i64, Vec<Wildcard>>> {
    let mut stmt = conn.prepare("SELECT day, kind, target, text FROM wildcards ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, Wildcard { kind: r.get(1)?, target: r.get(2)?, text: r.get(3)? }))
    })?;
    let mut map: BTreeMap<i64, Vec<Wildcard>> = BTreeMap::new();
    for row in rows {
        let (day, wc) = row?;
        map.entry(day).or_default().push(wc);
    }
    Ok(map)
}

/// Apply the day's wildcards: emit the prose, and a bounded effect by kind. Town-wide kinds
/// touch many; targeted kinds touch one. Magnitudes are fixed (deterministic).
fn apply_wildcards(world: &mut World, day: i64, date: Date, list: &[Wildcard]) -> Vec<Event> {
    let mut out = Vec::new();
    for wc in list {
        let actor = if world.idx(&wc.target).is_some() { wc.target.clone() } else { "Thrushcombe".into() };
        out.push(Event { day, date: date.to_string(), kind: "wildcard".into(), actor, text: wc.text.clone() });
        let t = &wc.target;
        let ti = world.idx(t);
        match wc.kind.as_str() {
            "fire" => {
                if let Some(i) = ti {
                    world.agents[i].purse -= 30;
                    clamp_standing(&mut world.agents[i], -1);
                    nudge_mood(&mut world.agents[i], -20);
                }
                world.spawn_news(t, &format!("the fire at {t}'s"), -1, day, &[]);
            }
            "windfall" => {
                if let Some(i) = ti {
                    world.agents[i].purse += 25;
                    clamp_standing(&mut world.agents[i], 1);
                    nudge_mood(&mut world.agents[i], 18);
                }
                world.spawn_news(t, &format!("{t}'s stroke of luck"), 2, day, &[]);
            }
            "fair" => {
                for a in world.agents.iter_mut().filter(|a| a.active() && a.archetype != "child") {
                    a.standing = (a.standing + 1).clamp(0, 100);
                    if matches!(a.archetype.as_str(), "blunt_hand" | "hill_farmer") {
                        a.purse += 3; // good trade at the fair
                    }
                }
            }
            "blight" => {
                for a in world.agents.iter_mut().filter(|a| a.active() && matches!(a.archetype.as_str(), "hill_farmer" | "scheming_improver")) {
                    a.purse -= 12;
                }
                for an in world.animals.iter_mut().filter(|an| an.health > 0) {
                    an.health = (an.health - 6).clamp(0, 100);
                }
            }
            "scandal" => {
                if let Some(i) = ti {
                    clamp_standing(&mut world.agents[i], -2);
                    nudge_mood(&mut world.agents[i], -15);
                }
                world.spawn_news(t, &format!("the scandal of {t}"), -3, day, &[]);
            }
            "stranger" => {
                let mut rng = rng_for(0x57A2_0000_0000 ^ day as u64, day);
                let agent = new_incomer(&mut rng, day);
                world.agents.push(agent);
            }
            "foundling" => {
                let seat = ti.map(|i| world.agents[i].seat.clone()).unwrap_or_else(|| "the parish".into());
                let mut child = make_agent("the foundling", "child", &seat, 6, 0, 0, 0, day);
                child.parent = ti;
                world.agents.push(child);
                world.spawn_news(t, "the foundling left in the parish", 0, day, &[]);
            }
            _ => {} // "wonder" and anything unknown: pure talk, no material effect
        }
    }
    out
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
        let wildcards = load_wildcards(&conn)?;
        let decrees = load_decrees(&conn)?;
        Ok(Sim { conn, seed: seed as u64, epoch: Date::from_julian_day(ej as i32).unwrap(), engine: Box::new(NativePolicies), interventions, weather, wildcards, decrees })
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
        Ok(Sim { conn, seed, epoch, engine: Box::new(NativePolicies), interventions: BTreeMap::new(), weather: BTreeMap::new(), wildcards: BTreeMap::new(), decrees: BTreeMap::new() })
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

    /// Drop folded artefacts at or after `day` so a re-fold regenerates them cleanly.
    /// Narration is keyed by event_id, and event ids are unstable across a re-fold (the
    /// rows are deleted and re-inserted), so stale narration must go too — otherwise an
    /// old voiced line mis-attaches to a freshly inserted event and that event is taken
    /// for already-narrated, silently swallowing it (e.g. an injected stranger).
    fn invalidate_from(&self, day: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM narration WHERE event_id IN (SELECT id FROM events WHERE day >= ?1)",
            params![day],
        )?;
        self.conn.execute("DELETE FROM events WHERE day >= ?1", params![day])?;
        self.conn.execute("DELETE FROM snapshots WHERE day >= ?1", params![day * PHASES])?; // snapshots keyed by slot
        Ok(())
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
        self.invalidate_from(day)?;
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

    /// Record an LLM-invented wildcard (kind from the vocabulary, the model's prose) at today,
    /// then invalidate the frontier so it folds in — with its bounded effect — at once.
    /// Caller should `catch_up`. Recorded, so replay stays deterministic.
    pub fn record_wildcard(&mut self, date: Date, kind: &str, target: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO wildcards(day,kind,target,text) VALUES(?1,?2,?3,?4)",
            params![day, kind, target, text],
        )?;
        self.wildcards = load_wildcards(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// The day-index of the most recent wildcard, to throttle how often they happen.
    pub fn last_wildcard_day(&self) -> rusqlite::Result<Option<i64>> {
        self.conn.query_row("SELECT MAX(day) FROM wildcards", [], |r| r.get(0))
    }

    /// The present grown souls' names, for colouring a wildcard prompt.
    pub fn grown_names(&self, today: Date) -> Vec<String> {
        self.world_snapshot(today)
            .agents
            .iter()
            .filter(|a| a.active() && a.archetype != "child")
            .map(|a| a.name.clone())
            .collect()
    }

    /// Record the oracle's verdict at a soul's turning point, then invalidate the frontier so
    /// it folds in — with its bounded effect — at once. Caller should `catch_up`.
    pub fn record_decree(&mut self, date: Date, subject: &str, kind: &str, target: &str, choice: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,?3,?4,?5,?6)",
            params![day, subject, kind, target, choice, text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Record a conversation: store its transcript and the memory the target keeps, and fold
    /// its felt residue (a warming or cooling, a lift or sinking of spirits) into the world.
    pub fn record_dialogue(&mut self, date: Date, source: &str, target: &str, transcript: &str, memory: &str, warmth: &str, sway: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO dialogues(day,source,target,transcript,memory) VALUES(?1,?2,?3,?4,?5)",
            params![day, source, target, transcript, memory],
        )?;
        // the world-effect rides the decree mechanism: subject = the soul spoken to, target = who spoke.
        // choice encodes both the warming/cooling and any sway over what the soul now wants.
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'dialogue',?3,?4,?5)",
            params![day, target, source, format!("{warmth}:{sway}"), memory],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Pick two souls who would plausibly fall into conversation of their own accord — a
    /// courting pair, friends, rivals, or two who simply met — with the briefs to stage it.
    pub fn converse_pair(&self, today: Date) -> Option<ConversePair> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let adults: Vec<usize> = (0..w.agents.len()).filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        if adults.len() < 4 {
            return None;
        }
        let mut rng = rng_for(self.seed ^ 0xC04E_0000_0000, day);
        let (mut courting, mut warm, mut cold) = (Vec::new(), Vec::new(), Vec::new());
        for &i in &adults {
            let c = w.agents[i].courting;
            if c >= 0 && w.agents.get(c as usize).map_or(false, |a| a.active()) {
                courting.push((i, c as usize));
            }
            for &j in &adults {
                if i < j {
                    let a = w.aff(i, j);
                    if a >= 30 {
                        warm.push((i, j));
                    } else if a <= -30 {
                        cold.push((i, j));
                    }
                }
            }
        }
        let pickv = |rng: &mut ChaCha8Rng, v: &[(usize, usize)]| if v.is_empty() { None } else { Some(v[rng.gen_range(0..v.len())]) };
        let (a, b) = match rng.gen_range(0..10) {
            0..=3 => pickv(&mut rng, &courting).or_else(|| pickv(&mut rng, &warm)).or_else(|| pickv(&mut rng, &cold)),
            4..=6 => pickv(&mut rng, &warm).or_else(|| pickv(&mut rng, &cold)),
            7..=8 => pickv(&mut rng, &cold).or_else(|| pickv(&mut rng, &warm)),
            _ => None,
        }
        .or_else(|| {
            let (a, b) = (adults[rng.gen_range(0..adults.len())], adults[rng.gen_range(0..adults.len())]);
            (a != b).then_some((a, b))
        })?;

        let brief = |i: usize| {
            let ag = &w.agents[i];
            let role = ag.trade.clone().unwrap_or_else(|| match ag.archetype.as_str() {
                "genteel_status_seeker" => "gentlefolk", "hill_farmer" => "a hill farmer", "practitioner" => "of the practice",
                "scheming_improver" => "an improver", "blunt_hand" => "working folk", "official" => "of the parish", _ => "of the town",
            }.to_string());
            format!("{}, {}, of {}, aged {}, standing {} of a hundred, presently {}, who wants {}.",
                ag.name, role, ag.seat, ag.age(day), ag.standing, mood_word(ag.mood), goal_label(&w, ag.goal, ag.goal_target))
        };
        let mut relation = if w.agents[a].spouse == Some(b) {
            "They are man and wife.".to_string()
        } else if w.agents[a].courting == b as i32 || w.agents[b].courting == a as i32 {
            format!("{} is courting {}.", w.agents[a].name, w.agents[b].name)
        } else {
            let af = w.aff(a, b);
            if af >= 30 { "They are the best of friends.".into() }
            else if af <= -30 { "They are at bitter odds.".into() }
            else if af > 0 { "They are on friendly enough terms.".into() }
            else { "They are barely acquainted.".into() }
        };
        // what each already carries of the other, so the talk has a history behind it
        let recall = |from: &str, of: &str| -> String {
            self.memories_of(from, 6).ok().into_iter().flatten()
                .find(|(who, _)| who == of)
                .map(|(_, m)| format!(" {from} recalls of {of}: {m}"))
                .unwrap_or_default()
        };
        relation.push_str(&recall(&w.agents[a].name, &w.agents[b].name));
        relation.push_str(&recall(&w.agents[b].name, &w.agents[a].name));
        // and what has lately been on each one's own mind, so they bring a present self to the talk
        let mused = |who: &str| -> String {
            self.self_reflections(who, 1).ok().into_iter().flatten().next()
                .map(|m| format!(" Of late {who} has been thinking: {m}"))
                .unwrap_or_default()
        };
        relation.push_str(&mused(&w.agents[a].name));
        relation.push_str(&mused(&w.agents[b].name));
        // the parish as it actually stands, so they have real matter to talk of
        relation.push_str(&format!(" The season is {}.", Season::of(today).name()));
        if let Ok(recent) = self.chronicle(4) {
            let lines: Vec<String> = recent.into_iter().rev().map(|e| e.text).collect();
            if !lines.is_empty() {
                relation.push_str(&format!(" Lately about the parish: {}", lines.join(" ")));
            }
        }
        Some(ConversePair {
            a, a_name: w.agents[a].name.clone(), a_brief: brief(a),
            b, b_name: w.agents[b].name.clone(), b_brief: brief(b),
            relation,
        })
    }

    /// The day-index of the most recent conversation, to keep the town from chattering nonstop.
    pub fn last_dialogue_day(&self) -> rusqlite::Result<Option<i64>> {
        self.conn.query_row("SELECT MAX(day) FROM dialogues", [], |r| r.get(0))
    }

    /// What a soul has come to remember of others, from the conversations they've had.
    pub fn memories_of(&self, name: &str, limit: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, memory FROM dialogues WHERE target = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![name, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Record an hour's reflection: store the private thought (self-memory) and fold its
    /// felt residue through the decree mechanism. mood ∈ [lifts, sinks, steadies]; sway ∈
    /// [none, debt, rise, prosper, content]; regard ∈ [none, warmer, colder] and resolve ∈
    /// [none, court, confront, mend] both bear on `toward` (the one soul mused on, or "").
    #[allow(clippy::too_many_arguments)]
    pub fn record_reflection(&mut self, date: Date, subject: &str, thought: &str, mood: &str, sway: &str, toward: &str, regard: &str, resolve: &str, plan: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO reflections(day,subject,thought) VALUES(?1,?2,?3)",
            params![day, subject, thought],
        )?;
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'reflect',?3,?4,?5)",
            params![day, subject, toward, format!("{mood}:{sway}:{regard}:{resolve}:{plan}"), thought],
        )?;
        // if the hour hardened their regard for a particular soul, keep it as a memory of that
        // soul, so it surfaces on their page and colours the next time the two of them speak.
        if !toward.is_empty() && regard != "none" {
            self.conn.execute(
                "INSERT INTO dialogues(day,source,target,transcript,memory) VALUES(?1,?2,?3,?4,?5)",
                params![day, toward, subject, thought, thought],
            )?;
        }
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Re-read the recorded inputs (decrees, weather, wildcards, interventions) from the db, so a
    /// long-running reader process picks up what another process (the hourly driver) has since
    /// written. The fold then reflects new feuds, courtships, plans and the like without a restart.
    pub fn reload_inputs(&mut self) -> rusqlite::Result<()> {
        self.decrees = load_decrees(&self.conn)?;
        self.weather = load_weather(&self.conn)?;
        self.wildcards = load_wildcards(&self.conn)?;
        self.interventions = load_interventions(&self.conn)?;
        Ok(())
    }

    /// A soul's own recent thoughts — the inner life they carry forward into talk and the next hour.
    pub fn self_reflections(&self, name: &str, limit: i64) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT thought FROM reflections WHERE subject = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![name, limit], |r| r.get(0))?;
        rows.collect()
    }

    /// The town's inner life, newest first — every soul's reflections, for the public feed.
    /// Returns (day, subject, thought).
    pub fn recent_reflections(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT day, subject, thought FROM reflections ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// The day-index a soul last reflected, per soul — for choosing who is most overdue.
    fn last_reflected(&self) -> rusqlite::Result<BTreeMap<String, i64>> {
        let mut stmt = self.conn.prepare("SELECT subject, MAX(day) FROM reflections GROUP BY subject")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect()
    }

    /// Pick the soul most overdue for a reflection — the active adult who has gone longest
    /// without one (never-reflected come first), tie-broken by a salt so a tied field varies.
    /// Returns the dossier they would contemplate: who they are, their ties, their late history,
    /// what they carry of others and of their own thinking, and how the parish stands.
    pub fn reflect_subject(&self, today: Date, salt: u64) -> Option<ReflectSubject> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let last = self.last_reflected().unwrap_or_default();
        let idx = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .min_by_key(|&i| {
                let since = last.get(&w.agents[i].name).copied().unwrap_or(-1);
                let jitter = (i as u64 ^ salt).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33;
                (since, jitter)
            })?;
        let ag = &w.agents[idx];
        let role = ag.trade.clone().unwrap_or_else(|| match ag.archetype.as_str() {
            "genteel_status_seeker" => "gentlefolk", "hill_farmer" => "a hill farmer", "practitioner" => "of the practice",
            "scheming_improver" => "an improver", "blunt_hand" => "working folk", "official" => "of the parish", _ => "of the town",
        }.to_string());
        let named = |v: Vec<(usize, i16)>| v.into_iter().map(|(j, _)| w.agents[j].name.clone()).collect::<Vec<_>>().join(", ");
        let friends = named(w.ties(idx, true, 3));
        let odds = named(w.ties(idx, false, 3));
        let feud = (ag.rival >= 0).then(|| w.agents.get(ag.rival as usize).map(|r| r.name.clone())).flatten();

        let mut dossier = format!(
            "{name}, {role}, of {seat}, aged {age}. Standing {standing} of a hundred, purse {purse}£, presently {mood}. They want {goal}.",
            name = ag.name, seat = ag.seat, age = ag.age(day), standing = ag.standing, purse = ag.purse,
            mood = mood_word(ag.mood), goal = goal_label(&w, ag.goal, ag.goal_target),
        );
        if !friends.is_empty() { dossier.push_str(&format!("\nThey are close to: {friends}.")); }
        if !odds.is_empty() { dossier.push_str(&format!("\nThey are at odds with: {odds}.")); }
        if let Some(r) = feud { dossier.push_str(&format!("\nThey nurse a running grudge against {r}.")); }
        if ag.intent != 0 {
            let what = match ag.intent { 1 => "to mend their fortunes", 2 => "to better their station", _ => "a bold venture" };
            dossier.push_str(&format!("\nThey are already set on a plan — {what} — resolved {} days since, and not yet come to its head.", ag.intent_age));
        }

        if let Ok(es) = self.person_events(&ag.name, 5) {
            let lines: Vec<String> = es.into_iter().rev().map(|e| format!("  {} — {}", e.date, e.text)).collect();
            if !lines.is_empty() { dossier.push_str(&format!("\nLately for them:\n{}", lines.join("\n"))); }
        }
        if let Ok(ms) = self.memories_of(&ag.name, 4) {
            let lines: Vec<String> = ms.into_iter().map(|(who, m)| format!("  of {who}: {m}")).collect();
            if !lines.is_empty() { dossier.push_str(&format!("\nWhat they carry of others:\n{}", lines.join("\n"))); }
        }
        if let Ok(ts) = self.self_reflections(&ag.name, 3) {
            if !ts.is_empty() { dossier.push_str(&format!("\nTheir own late thoughts:\n{}", ts.iter().map(|t| format!("  {t}")).collect::<Vec<_>>().join("\n"))); }
        }
        dossier.push_str(&format!("\nThe season is {}.", Season::of(today).name()));
        if let Ok(recent) = self.chronicle(4) {
            let lines: Vec<String> = recent.into_iter().rev().map(|e| e.text).collect();
            if !lines.is_empty() { dossier.push_str(&format!(" About the parish lately: {}", lines.join(" "))); }
        }
        Some(ReflectSubject { name: ag.name.clone(), dossier })
    }

    /// Find a soul at a genuine turning point — a long feud that might be forgiven, ruin to be
    /// faced, a match across the class line — for the driver to put to the oracle. Skips any
    /// soul+kind decided in the last ~half-year, so the same hinge isn't put twice running.
    pub fn pending_hinge(&self, today: Date) -> Option<Hinge> {
        let world = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let recent = |subj: &str, kind: &str| -> bool {
            self.conn
                .query_row(
                    "SELECT MAX(day) FROM decrees WHERE subject = ?1 AND kind = ?2",
                    params![subj, kind],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten()
                .map(|last| day - last < 180)
                .unwrap_or(false)
        };
        let name = |i: usize| world.agents[i].name.clone();
        let n = world.agents.len();

        // a match stalled at the class line — the courted decides (accept across station, or refuse)
        for c in 0..n {
            let t = world.agents[c].courting;
            if t < 0 || !world.agents[c].active() {
                continue;
            }
            let cd = t as usize;
            if world.agents[c].courtship >= 22
                && world.agents[cd].standing > world.agents[c].standing + 15
                && world.aff(cd, c) < 26
                && !recent(&name(cd), "match")
            {
                return Some(Hinge {
                    subject: cd,
                    subject_name: name(cd),
                    kind: "match".into(),
                    target: c as i32,
                    target_name: name(c),
                    situation: format!(
                        "{} (of {}, standing {}) has been courting {} these many weeks — a suit from well below their station. {} must decide whether to accept {} against what the town expects, or to refuse the match.",
                        name(c), world.agents[c].seat, world.agents[c].standing, name(cd), name(cd), name(c)
                    ),
                    options: vec!["accept".into(), "refuse".into()],
                });
            }
        }

        // ruin — deep in debt and low in spirits; leave, weather it, or appeal for help
        let mut ruined: Vec<usize> = (0..n)
            .filter(|&i| world.agents[i].active() && world.agents[i].archetype != "child" && world.agents[i].purse < -40 && world.agents[i].mood < -25 && !recent(&world.agents[i].name, "ruin"))
            .collect();
        ruined.sort_by_key(|&i| world.agents[i].purse);
        if let Some(&i) = ruined.first() {
            return Some(Hinge {
                subject: i,
                subject_name: name(i),
                kind: "ruin".into(),
                target: -1,
                target_name: String::new(),
                situation: format!(
                    "{} (of {}, once standing {}) is ruined — {}£ in debt and very low. They must decide: leave Thrushcombe for good, weather it out and stay, or swallow their pride and appeal to the town for help.",
                    name(i), world.agents[i].seat, world.agents[i].standing, world.agents[i].purse
                ),
                options: vec!["leave".into(), "stay".into(), "appeal".into()],
            });
        }

        // a long, bitter feud — to forgive at last, or to nurse the grudge
        let mut worst: Option<(i16, usize, usize)> = None;
        for i in 0..n {
            if !world.agents[i].active() {
                continue;
            }
            for j in 0..n {
                if i == j || !world.agents[j].active() {
                    continue;
                }
                let a = world.aff(i, j);
                if a <= -55 && world.aff(j, i) <= -45 && !recent(&name(i), "feud") {
                    if worst.map(|(w, _, _)| a < w).unwrap_or(true) {
                        worst = Some((a, i, j));
                    }
                }
            }
        }
        if let Some((_, i, j)) = worst {
            return Some(Hinge {
                subject: i,
                subject_name: name(i),
                kind: "feud".into(),
                target: j as i32,
                target_name: name(j),
                situation: format!(
                    "{} and {} have been at bitter odds for a long time now. {} must decide whether to forgive {} and mend the quarrel at last, or to nurse the grudge.",
                    name(i), name(j), name(i), name(j)
                ),
                options: vec!["forgive".into(), "nurse".into()],
            });
        }
        None
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
            let _ = step_slot(&mut world, day, phase, date, self.seed, &*self.engine, &self.interventions, &self.weather, &self.wildcards, &self.decrees);
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
                let o = observe(&world, idx, day, date, top, self.seed, phase, false, false);
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
                let o = observe(&world, i, day, self.date_of(day), top, self.seed, ph, false, false);
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
            // when a soul is out on a notable errand, the where follows the doing
            let location = match doing.as_str() {
                "paying calls" => "out paying calls".to_string(),
                "dealing at the mart" => "the market".to_string(),
                "on the rounds" => "on the rounds".to_string(),
                "about the parish" => "about the parish".to_string(),
                _ => placement(a, phase, wd),
            };
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
                location,
                doing,
                next,
                spouse: a.spouse.map(|s| world.agents[s].name.clone()),
                parent: a.parent.map(|p| world.agents[p].name.clone()),
                children,
                origin: a.origin.clone(),
                wants: if a.courting >= 0 {
                    format!("to win {}'s hand", world.agents.get(a.courting as usize).map(|x| x.name.as_str()).unwrap_or("—"))
                } else if a.archetype == "child" {
                    "to grow up".into()
                } else {
                    goal_label(&world, a.goal, a.goal_target)
                },
                mood: mood_word(a.mood).to_string(),
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
                ('death','succession','birth','marriage','party','calving','gossip','newcomer','weather','bureaucracy','wildcard'))
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
            for e in step_slot(&mut world, day, ph, date, seed, &*self.engine, &self.interventions, &self.weather, &self.wildcards, &self.decrees) {
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
        // the Thrushcombe & District Show, 23 August
        if let Ok(show) = Date::from_calendar_date(today.year(), Month::August, 23) {
            let days = (show.to_julian_day() - today.to_julian_day()) as i64;
            if days > 0 {
                p.push(format!("the Thrushcombe & District Show — in {days}d"));
            }
        }
        p
    }
}

#[cfg(test)]
mod filler_tests {
    use super::strip_filler;
    #[test]
    fn strips_the_tic() {
        assert_eq!(strip_filler("I daresay you've seen more of the parish than I."),
                   "You've seen more of the parish than I.");
        assert_eq!(strip_filler("Aye, and I daresay they'll be chaffing the grass."),
                   "Aye, and they'll be chaffing the grass.");
        assert_eq!(strip_filler("The sheep's wool will be fine, I daresay, come what may."),
                   "The sheep's wool will be fine, come what may.");
        assert_eq!(strip_filler("I warrant the moon has more sense than a man."),
                   "The moon has more sense than a man.");
        // a clean line is untouched
        assert_eq!(strip_filler("You'd think the air was thick with smoke."),
                   "You'd think the air was thick with smoke.");
        // curly apostrophe (U+2019) — the model emits these as often as ASCII; still stripped,
        // and a comma left dangling before the sentence end is cleaned up
        assert_eq!(strip_filler("They\u{2019}ll have their way, I\u{2019}ll warrant."),
                   "They\u{2019}ll have their way.");
    }
}

#[cfg(test)]
mod feud_tests {
    use super::*;

    // A declared grudge is now a campaign waged over weeks, not a standing fact: it climbs
    // toward a public reckoning, and the upper-handed schemer carries it home.
    #[test]
    fn a_pressed_grudge_comes_to_a_reckoning() {
        let mut w = World::seed();
        let genteel: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype == "genteel_status_seeker")
            .collect();
        assert!(genteel.len() >= 2, "the seed town should hold at least two genteel souls");
        let (i, j) = (genteel[0], genteel[1]);

        // both squarely middle-aged, to keep death's hazard out of a determinism test
        w.agents[i].birth_day = -40 * 365;
        w.agents[j].birth_day = -42 * 365;
        // the schemer stands above their rival, so the campaign can be carried home
        w.agents[i].standing = 70;
        w.agents[j].standing = 50;
        w.agents[i].rival = j as i32;
        w.agents[i].goal = 4;
        w.agents[i].goal_target = j as i32;
        // a grudge hardened well past the made-up threshold, so tend_rivalries won't dissolve it
        w.nudge_aff(i, j, -90);

        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();
        let rival_standing_before = w.agents[j].standing;
        let mut reckoned: Option<(i64, String)> = None;
        for day in 0..70i64 {
            let feud_before = w.agents[i].feud;
            let evs = life_tick(&mut w, day, date, 42);
            // while the grudge stands unresolved, the campaign must only ever press harder
            if w.agents[i].rival >= 0 {
                assert!(w.agents[i].feud >= feud_before, "the feud regressed on day {day}");
            }
            if let Some(e) = evs.iter().find(|e| e.kind == "feud") {
                reckoned = Some((day, e.text.clone()));
                break;
            }
        }

        let (day, text) = reckoned.expect("the feud should reach a reckoning within ten weeks");
        assert!(day >= 29, "a reckoning is a campaign, not an overnight thing (came on day {day})");
        assert!(text.contains("got the better of"), "the upper-handed schemer should win, got: {text}");
        assert_eq!(w.agents[i].rival, -1, "the rivalry is laid to rest once reckoned");
        assert_eq!(w.agents[i].feud, 0, "the campaign counter resets once reckoned");
        assert_ne!(w.agents[i].goal, 4, "with the rival bested, the goal moves on");
        assert!(w.agents[j].standing < rival_standing_before, "the bested rival's standing should fall");
    }
}

#[cfg(test)]
mod intent_tests {
    use super::*;

    // A plan a soul sets itself is carried for weeks, then judged in the open. A purse already
    // past the threshold makes good; the resolve is spent and a public beat thrown off.
    #[test]
    fn a_self_set_plan_comes_to_a_reckoning() {
        let mut w = World::seed();
        let i = (0..w.agents.len())
            .find(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .expect("the seed town holds a grown soul");
        w.agents[i].birth_day = -45 * 365; // middle-aged, to keep death's hazard out of the test

        // they resolved to mend their fortunes, and the threshold sits at or below what they hold,
        // so the weeks of pursuit will be judged to have made good
        w.agents[i].intent = 1;
        w.agents[i].intent_goal = w.agents[i].purse; // already met — a made-good reckoning
        w.agents[i].intent_age = 0;
        let standing_before = w.agents[i].standing;

        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();
        let mut reckoned: Option<(i64, String)> = None;
        for day in 0..40i64 {
            let age_before = w.agents[i].intent_age;
            let evs = life_tick(&mut w, day, date, 42);
            if w.agents[i].intent != 0 {
                assert!(w.agents[i].intent_age >= age_before, "the plan regressed on day {day}");
            }
            if let Some(e) = evs.iter().find(|e| e.kind == "intent") {
                reckoned = Some((day, e.text.clone()));
                break;
            }
        }

        let (day, text) = reckoned.expect("a plan should reach its reckoning within six weeks");
        assert!(day >= 27, "a plan is carried over weeks, not settled overnight (came on day {day})");
        assert!(text.contains("made good"), "a met threshold should make good, got: {text}");
        assert_eq!(w.agents[i].intent, 0, "the plan is spent once reckoned");
        assert_eq!(w.agents[i].intent_age, 0, "the plan counter resets once reckoned");
        assert!(w.agents[i].standing >= standing_before, "making good should not cost them standing");
    }
}
