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
use time::{Date, Month};

// ----------------------------------------------------------------------------- calendar

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Clone)]
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

#[derive(Clone)]
pub struct Animal {
    pub name: String,
    pub owner: String,
    pub health: i32, // 0..100
    pub gest: i32,    // days until calving/birth; <0 = none pending
    pub value: i32,
}

/// A piece of news loose in the town. It spreads across the social graph with delay
/// (one hop a day) and distortion (it grows in the telling), and each new pair of ears
/// nudges the subject's standing — gossip is *how* reputation actually moves.
#[derive(Clone)]
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

#[derive(Clone)]
pub struct World {
    pub agents: Vec<Agent>,
    pub animals: Vec<Animal>,
    pub news: Vec<News>,
    pub next_news_id: u32,
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
        };
        let mut agents = vec![
            a("Mrs Cynthia Pelham", "genteel_status_seeker", "The Laurels", 60, -18, 42, 0),
            a("Mr Robert Pelham", "genteel_status_seeker", "The Laurels", 58, 5, 46, 1),
            a("Lady Aldermaston", "genteel_status_seeker", "Crale Court", 90, 420, 70, 0),
            a("Revd Mr Soames", "official", "The Vicarage", 70, 30, 58, 1),
            a("Mr Farran MRCVS", "practitioner", "Beck House", 65, 25, 45, 1),
            a("Mr Sunter", "hill_farmer", "High Foldside", 48, 12, 55, 1),
            a("Mr Rupert Crale", "scheming_improver", "Home Farm", 55, -40, 28, 1),
            a("Tot Wragg", "blunt_hand", "Home Farm", 40, 4, 20, 1),
            // the rising generation, who will grow up, marry, and inherit
            a("Jack Sunter", "hill_farmer", "High Foldside", 38, 3, 21, 1),
            a("Robin Pelham", "child", "The Laurels", 20, 0, 11, 1),
            a("Vicky Pelham", "child", "The Laurels", 18, 0, 8, 0),
        ];
        // kinship: indices follow the order above
        let (cynthia, robert) = (0usize, 1usize);
        let (ladya, rupert) = (2usize, 6usize);
        let (sunter, jack) = (5usize, 8usize);
        let (robin, vicky) = (9usize, 10usize);
        agents[cynthia].spouse = Some(robert);
        agents[robert].spouse = Some(cynthia);
        agents[rupert].parent = Some(ladya); // Crale Court's heir — the eventual earthquake
        agents[jack].parent = Some(sunter);
        agents[robin].parent = Some(cynthia);
        agents[vicky].parent = Some(cynthia);

        World {
            agents,
            animals: vec![
                Animal { name: "Strawberry".into(), owner: "Mr Sunter".into(), health: 68, gest: 4, value: 45 },
                Animal { name: "Captain".into(), owner: "Mr Rupert Crale".into(), health: 80, gest: -1, value: 30 },
            ],
            news: Vec::new(),
            next_news_id: 0,
        }
    }

    fn agent_mut(&mut self, name: &str) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.name == name && a.death_day.is_none())
    }

    fn alive_named(&self, name: &str) -> bool {
        self.agents.iter().any(|a| a.name == name && a.death_day.is_none())
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
fn step_day(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let mut rng = rng_for(seed, day);
    let mk = |kind: &str, actor: &str, text: String| Event {
        day,
        date: date.to_string(),
        kind: kind.into(),
        actor: actor.into(),
        text,
    };

    // --- day zero: the opening state, seeded onto the real date ---
    if day == 0 {
        out.push(mk("household", "Mrs Cynthia Pelham", "Cook has given notice — the fifth time this year.".into()));
        out.push(mk("scheme", "Mr Rupert Crale", "A new tractor arrived at the Home Farm, which Rupert cannot drive.".into()));
        out.push(mk("bureaucracy", "Revd Mr Soames", "A tithe demand for High Foldside sits in the Vicar's out-tray.".into()));
        world.spawn_news("Mr Rupert Crale", "Rupert's grand tractor he cannot drive", -2, day, &["Tot Wragg"]);
        world.spawn_news("Mr Sunter", "the tithe demand fallen on High Foldside", -1, day, &["Revd Mr Soames"]);
    }

    // --- gestation / calving (depends on world state) ---
    for i in 0..world.animals.len() {
        if world.animals[i].gest > 0 {
            world.animals[i].gest -= 1;
            if world.animals[i].gest == 0 {
                let name = world.animals[i].name.clone();
                let owner = world.animals[i].owner.clone();
                if rng.gen_bool(0.70) {
                    world.animals[i].value += 15;
                    world.animals[i].health = (world.animals[i].health + 5).clamp(0, 100);
                    world.animals[i].gest = -1;
                    if let Some(o) = world.agent_mut(&owner) { clamp_standing(o, 3); }
                    out.push(mk("calving", &owner, format!("{name} calved well — a fine heifer calf. Mr Farran was barely needed.")));
                    world.spawn_news(&owner, &format!("{name}'s fine new heifer calf"), 2, day, &["Mr Farran MRCVS"]);
                } else {
                    world.animals[i].value -= 5;
                    world.animals[i].health = (world.animals[i].health - 10).clamp(0, 100);
                    world.animals[i].gest = -1;
                    if let Some(v) = world.agent_mut("Mr Farran MRCVS") { clamp_standing(v, 2); }
                    out.push(mk("calving", &owner, format!("{name}'s calving went hard; Mr Farran worked till dawn, but the calf stands.")));
                    world.spawn_news("Mr Farran MRCVS", &format!("Mr Farran's long night saving {name}"), 1, day, &[&owner]);
                }
            }
        }
    }

    // --- scheduled social event: Lady Aldermaston's garden party, the 14th ---
    if date.month() == Month::June && date.day() == 14 {
        if let Some(l) = world.agent_mut("Lady Aldermaston") { clamp_standing(l, 3); }
        if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") {
            clamp_standing(c, 1);
            c.purse -= 8; // the dress, on credit — face bought with money she hasn't got
        }
        out.push(mk("party", "Lady Aldermaston",
            "Lady Aldermaston's garden party at Crale Court. Mrs Pelham attended in a made-over frock and a brave face.".into()));
        world.spawn_news("Lady Aldermaston", "Lady Aldermaston's splendid garden party", 1, day, &[]);
        world.spawn_news("Mrs Cynthia Pelham", "Mrs Pelham's made-over frock at the party", -1, day, &["Lady Aldermaston"]);
    }

    // --- the daily incident: drawn from the season's register ---
    // (Scripted incidents assume their principals are alive; once a seat passes to an
    // heir the named beats stop firing. The WASM policy layer will replace this.)
    if rng.gen_bool(0.55) {
        let season = Season::of(date);
        // alternate the lens between household comedy and farm friction
        let farm = rng.gen_bool(0.5);
        if farm {
            if world.alive_named("Mr Rupert Crale") || world.alive_named("Mr Sunter") {
                farm_incident(world, season, day, &mut rng, &mk, &mut out);
            }
        } else if world.alive_named("Mrs Cynthia Pelham") || world.alive_named("Lady Aldermaston") {
            household_incident(world, day, &mut rng, &mk, &mut out);
        }
    }

    // --- the slow turn of the cast: birth, marriage, ageing, death, succession ---
    out.extend(life_tick(world, day, date, seed));

    // --- the news spreads, with delay and distortion ---
    out.extend(diffuse(world, day, date, seed));

    out
}

type Mk<'a> = dyn Fn(&str, &str, String) -> Event + 'a;

fn household_incident(world: &mut World, day: i64, rng: &mut ChaCha8Rng, mk: &Mk, out: &mut Vec<Event>) {
    match rng.gen_range(0..6) {
        0 => out.push(mk("household", "Mrs Cynthia Pelham", "Cook gave notice once more, and was talked round before luncheon.".into())),
        1 => {
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { clamp_standing(c, -1); }
            out.push(mk("household", "Mrs Cynthia Pelham", "The soufflé collapsed before the Hendersons. Conversation was found for it.".into()));
            world.spawn_news("Mrs Cynthia Pelham", "the soufflé that collapsed before the Hendersons", -1, day, &[]);
        }
        2 => {
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { c.purse -= 3; clamp_standing(c, -1); }
            out.push(mk("status", "Mrs Cynthia Pelham", "The account at the draper's was mentioned, very gently, across the counter.".into()));
            world.spawn_news("Mrs Cynthia Pelham", "Mrs Pelham's unpaid account at the draper's", -2, day, &[]);
        }
        3 => {
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { clamp_standing(c, 2); }
            out.push(mk("status", "Mrs Cynthia Pelham", "Mrs Pelham paid a successful call at the Vicarage; the seed-cake was praised.".into()));
            world.spawn_news("Mrs Cynthia Pelham", "Mrs Pelham's much-praised seed-cake", 1, day, &["Revd Mr Soames"]);
        }
        4 => {
            if let Some(l) = world.agent_mut("Lady Aldermaston") { clamp_standing(l, 2); }
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { clamp_standing(c, -1); }
            out.push(mk("status", "Lady Aldermaston", "Lady Aldermaston's new motorcar was much admired in the square.".into()));
            world.spawn_news("Lady Aldermaston", "Lady Aldermaston's splendid new motorcar", 1, day, &["Mrs Cynthia Pelham"]);
        }
        _ => out.push(mk("household", "Tot Wragg", "Gladys broke the second-best teapot and blamed the cat.".into())),
    }
}

fn farm_incident(world: &mut World, season: Season, day: i64, rng: &mut ChaCha8Rng, mk: &Mk, out: &mut Vec<Event>) {
    if matches!(season, Season::Hay) {
        match rng.gen_range(0..6) {
            0 => out.push(mk("weather", "Mr Sunter", "Rain threatened the cut hay; every hand in the dale turned out to cock it.".into())),
            1 => {
                if let Some(r) = world.agent_mut("Mr Rupert Crale") { r.purse -= 5; clamp_standing(r, -1); }
                out.push(mk("scheme", "Mr Rupert Crale", "The elevator jammed at the Home Farm. Tot had said it would.".into()));
                world.spawn_news("Mr Rupert Crale", "Rupert's elevator come to grief", -2, day, &["Tot Wragg"]);
            }
            2 => {
                if let Some(s) = world.agent_mut("Mr Sunter") { s.purse += 6; clamp_standing(s, 2); }
                out.push(mk("windfall", "Mr Sunter", "A clear day — the hay came in dry and sweet, and was got under cover.".into()));
                world.spawn_news("Mr Sunter", "the Sunters getting their hay in dry", 2, day, &[]);
            }
            3 => {
                if let Some(r) = world.agent_mut("Mr Rupert Crale") { clamp_standing(r, -2); }
                out.push(mk("scheme", "Mr Rupert Crale", "The new tractor stuck fast in the gateway. Tot fetched the horses, saying nothing.".into()));
                world.spawn_news("Mr Rupert Crale", "Rupert's tractor stuck fast in the gateway", -2, day, &["Tot Wragg"]);
            }
            4 => out.push(mk("practice", "Mr Farran MRCVS", "Mr Farran was called to a lame carthorse, and stayed for his tea.".into())),
            _ => out.push(mk("market", "Mr Sunter", "The milk lorry was late again, and the churns stood warming in the sun.".into())),
        }
    } else {
        // generic farm texture for other seasons (until each gets its own table)
        match rng.gen_range(0..3) {
            0 => out.push(mk("practice", "Mr Farran MRCVS", "Mr Farran drove out to the hill farms on his rounds.".into())),
            1 => out.push(mk("market", "Mr Sunter", format!("Quiet work on the land; it is {} hereabouts.", season.name().to_lowercase()))),
            _ => out.push(mk("household", "Tot Wragg", "Tot got the day's work done despite the master's improvements.".into())),
        }
    }
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

fn make_agent(name: &str, arch: &str, seat: &str, standing: i32, sex: u8, age: i64, day: i64) -> Agent {
    Agent {
        name: name.into(),
        archetype: arch.into(),
        seat: seat.into(),
        standing: standing.clamp(0, 100),
        purse: 0,
        birth_day: day - age * 365,
        sex,
        death_day: None,
        departed: false,
        spouse: None,
        parent: None,
    }
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
                let heir = make_agent(&hname, &stratum_archetype(&arch), &seat, (standing - 12).max(20), 1, 34, day);
                out.push(mk("succession", &hname, format!("{seat} passes to {hname}, lately come from the town.")));
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
        if is_heir || !rng.gen_bool(0.82) {
            let parent_arch = parent
                .map(|p| world.agents[p].archetype.clone())
                .unwrap_or_else(|| "genteel_status_seeker".into());
            world.agents[i].archetype = stratum_archetype(&parent_arch);
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
                let mut sp = make_agent(&sname, &world.agents[i].archetype.clone(), &world.agents[i].seat.clone(),
                    (world.agents[i].standing - 5).max(20), osex, age, day);
                sp.spouse = Some(i);
                world.agents[i].spouse = Some(idx_new);
                let ni = world.agents[i].name.clone();
                out.push(mk("marriage", &ni, format!("{ni} is to wed {sname}, lately come to Thrushcombe.")));
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
            if young < 3 && rng.gen_bool((0.16 / 365.0_f64).clamp(0.0, 1.0)) {
                let bsex = if rng.gen_bool(0.5) { 1 } else { 0 };
                let first = if bsex == 1 { pick(&mut rng, FIRST_M) } else { pick(&mut rng, FIRST_F) };
                let surname = mother_name.rsplit(' ').next().unwrap_or("Pelham");
                let mut child = make_agent(&format!("{first} {surname}"), "child", &seat, (standing / 3).clamp(0, 100), bsex, 0, day);
                child.parent = Some(i);
                out.push(mk("birth", &mother_name, format!("A child, {first}, was born at {seat}.")));
                newcomers.push(child);
            }
        }
    }

    world.agents.extend(newcomers);
    out
}

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
    if wd == Weekday::Sunday {
        r += 0.20; // everyone at church
    }
    r.clamp(0.0, 0.95)
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
    "death", "succession", "marriage", "birth", "comingofage",
];

fn ensure_aux(conn: &Connection) -> rusqlite::Result<()> {
    // The recorded oracle: an LLM is non-deterministic at generation time, so we log
    // its output once, keyed by event, and replay the stored text forever.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS narration(event_id INTEGER PRIMARY KEY, text TEXT NOT NULL);",
    )
}

impl Sim {
    /// Open an existing world (must have been `init`ed).
    pub fn open(path: &str) -> rusqlite::Result<Sim> {
        let conn = Connection::open(path)?;
        ensure_aux(&conn)?;
        let seed: i64 = conn.query_row("SELECT val FROM meta WHERE key='seed'", [], |r| r.get(0))?;
        let ej: i64 = conn.query_row("SELECT val FROM meta WHERE key='epoch_julian'", [], |r| r.get(0))?;
        Ok(Sim { conn, seed: seed as u64, epoch: Date::from_julian_day(ej as i32).unwrap() })
    }

    /// Create a new world. `epoch` is the day "play" was pressed (default: today).
    pub fn init(path: &str, epoch: Date, seed: u64) -> rusqlite::Result<Sim> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, val INTEGER);
             CREATE TABLE IF NOT EXISTS events(
                id INTEGER PRIMARY KEY,
                day INTEGER NOT NULL, date TEXT NOT NULL,
                kind TEXT NOT NULL, actor TEXT NOT NULL, text TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_events_day ON events(day);",
        )?;
        ensure_aux(&conn)?;
        conn.execute("INSERT OR REPLACE INTO meta(key,val) VALUES('seed',?1)", params![seed as i64])?;
        conn.execute("INSERT OR REPLACE INTO meta(key,val) VALUES('epoch_julian',?1)", params![epoch.to_julian_day() as i64])?;
        Ok(Sim { conn, seed, epoch })
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

    /// Advance the log forward until it has caught up to `today`. Returns days added.
    /// Missing days self-heal: this regenerates every day from last+1..=target.
    pub fn catch_up(&mut self, today: Date) -> rusqlite::Result<i64> {
        let target = self.target_day(today);
        let from = self.last_day() + 1;
        if target < from {
            return Ok(0);
        }
        // Rebuild world state up to `from-1`, then generate the new days.
        let mut world = World::seed();
        for d in 0..from {
            let date = Date::from_julian_day((self.epoch.to_julian_day() as i64 + d) as i32).unwrap();
            let _ = step_day(&mut world, d, date, self.seed);
        }
        let tx = self.conn.transaction()?;
        let mut added = 0;
        for d in from..=target {
            let date = Date::from_julian_day((self.epoch.to_julian_day() as i64 + d) as i32).unwrap();
            for e in step_day(&mut world, d, date, self.seed) {
                tx.execute(
                    "INSERT INTO events(day,date,kind,actor,text) VALUES(?1,?2,?3,?4,?5)",
                    params![e.day, e.date, e.kind, e.actor, e.text],
                )?;
                added += 1;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// Fold the world to `today` (in memory) and read recent chronicle for display.
    pub fn report(&self, today: Date) -> rusqlite::Result<Report> {
        let target = self.target_day(today).max(0);
        let mut world = World::seed();
        for d in 0..=target {
            let date = Date::from_julian_day((self.epoch.to_julian_day() as i64 + d) as i32).unwrap();
            let _ = step_day(&mut world, d, date, self.seed);
        }

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

        // news in flight: live rumours, freshest first
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
        let news: Vec<String> = live
            .iter()
            .take(6)
            .map(|it| {
                let known = it.knowers.iter().filter(|&&k| world.agents[k].active()).count();
                let grown = if it.distortion >= 2 { " · grown in the telling" } else { "" };
                format!("{}  {}/{} know{}", it.topic, known, living, grown)
            })
            .collect();

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
            animals: world.animals.clone(),
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
