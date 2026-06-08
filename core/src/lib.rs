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
}

#[derive(Clone)]
pub struct Animal {
    pub name: String,
    pub owner: String,
    pub health: i32, // 0..100
    pub gest: i32,    // days until calving/birth; <0 = none pending
    pub value: i32,
}

#[derive(Clone)]
pub struct World {
    pub agents: Vec<Agent>,
    pub animals: Vec<Animal>,
}

impl World {
    fn seed() -> World {
        let a = |name: &str, arch: &str, seat: &str, standing, purse| Agent {
            name: name.into(),
            archetype: arch.into(),
            seat: seat.into(),
            standing,
            purse,
        };
        World {
            agents: vec![
                a("Mrs Cynthia Pelham", "genteel_status_seeker", "The Laurels", 60, -18),
                a("Lady Aldermaston", "genteel_status_seeker", "Crale Court", 90, 420),
                a("Revd Mr Soames", "official", "The Vicarage", 70, 30),
                a("Mr Farran MRCVS", "practitioner", "Beck House", 65, 25),
                a("The Sunters", "hill_farmer", "High Foldside", 48, 12),
                a("Mr Rupert Crale", "scheming_improver", "Home Farm", 55, -40),
                a("Tot Wragg", "blunt_hand", "Home Farm", 40, 4),
            ],
            animals: vec![
                Animal { name: "Strawberry".into(), owner: "The Sunters".into(), health: 68, gest: 4, value: 45 },
                Animal { name: "Captain".into(), owner: "Mr Rupert Crale".into(), health: 80, gest: -1, value: 30 },
            ],
        }
    }

    fn agent_mut(&mut self, name: &str) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.name == name)
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
                } else {
                    world.animals[i].value -= 5;
                    world.animals[i].health = (world.animals[i].health - 10).clamp(0, 100);
                    world.animals[i].gest = -1;
                    if let Some(v) = world.agent_mut("Mr Farran MRCVS") { clamp_standing(v, 2); }
                    out.push(mk("calving", &owner, format!("{name}'s calving went hard; Mr Farran worked till dawn, but the calf stands.")));
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
    }

    // --- the daily incident: drawn from the season's register ---
    if rng.gen_bool(0.55) {
        let season = Season::of(date);
        // alternate the lens between household comedy and farm friction
        let farm = rng.gen_bool(0.5);
        if farm {
            farm_incident(world, season, &mut rng, &mk, &mut out);
        } else {
            household_incident(world, &mut rng, &mk, &mut out);
        }
    }

    out
}

type Mk<'a> = dyn Fn(&str, &str, String) -> Event + 'a;

fn household_incident(world: &mut World, rng: &mut ChaCha8Rng, mk: &Mk, out: &mut Vec<Event>) {
    match rng.gen_range(0..6) {
        0 => out.push(mk("household", "Mrs Cynthia Pelham", "Cook gave notice once more, and was talked round before luncheon.".into())),
        1 => {
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { clamp_standing(c, -1); }
            out.push(mk("household", "Mrs Cynthia Pelham", "The soufflé collapsed before the Hendersons. Conversation was found for it.".into()));
        }
        2 => {
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { c.purse -= 3; clamp_standing(c, -1); }
            out.push(mk("status", "Mrs Cynthia Pelham", "The account at the draper's was mentioned, very gently, across the counter.".into()));
        }
        3 => {
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { clamp_standing(c, 2); }
            out.push(mk("status", "Mrs Cynthia Pelham", "Mrs Pelham paid a successful call at the Vicarage; the seed-cake was praised.".into()));
        }
        4 => {
            if let Some(l) = world.agent_mut("Lady Aldermaston") { clamp_standing(l, 2); }
            if let Some(c) = world.agent_mut("Mrs Cynthia Pelham") { clamp_standing(c, -1); }
            out.push(mk("status", "Lady Aldermaston", "Lady Aldermaston's new motorcar was much admired in the square.".into()));
        }
        _ => out.push(mk("household", "Tot Wragg", "Gladys broke the second-best teapot and blamed the cat.".into())),
    }
}

fn farm_incident(world: &mut World, season: Season, rng: &mut ChaCha8Rng, mk: &Mk, out: &mut Vec<Event>) {
    if matches!(season, Season::Hay) {
        match rng.gen_range(0..6) {
            0 => out.push(mk("weather", "The Sunters", "Rain threatened the cut hay; every hand in the dale turned out to cock it.".into())),
            1 => {
                if let Some(r) = world.agent_mut("Mr Rupert Crale") { r.purse -= 5; clamp_standing(r, -1); }
                out.push(mk("scheme", "Mr Rupert Crale", "The elevator jammed at the Home Farm. Tot had said it would.".into()));
            }
            2 => {
                if let Some(s) = world.agent_mut("The Sunters") { s.purse += 6; clamp_standing(s, 2); }
                out.push(mk("windfall", "The Sunters", "A clear day — the hay came in dry and sweet, and was got under cover.".into()));
            }
            3 => {
                if let Some(r) = world.agent_mut("Mr Rupert Crale") { clamp_standing(r, -2); }
                out.push(mk("scheme", "Mr Rupert Crale", "The new tractor stuck fast in the gateway. Tot fetched the horses, saying nothing.".into()));
            }
            4 => out.push(mk("practice", "Mr Farran MRCVS", "Mr Farran was called to a lame carthorse, and stayed for his tea.".into())),
            _ => out.push(mk("market", "The Sunters", "The milk lorry was late again, and the churns stood warming in the sun.".into())),
        }
    } else {
        // generic farm texture for other seasons (until each gets its own table)
        match rng.gen_range(0..3) {
            0 => out.push(mk("practice", "Mr Farran MRCVS", "Mr Farran drove out to the hill farms on his rounds.".into())),
            1 => out.push(mk("market", "The Sunters", format!("Quiet work on the land; it is {} hereabouts.", season.name().to_lowercase()))),
            _ => out.push(mk("household", "Tot Wragg", "Tot got the day's work done despite the master's improvements.".into())),
        }
    }
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
    pub chronicle: Vec<String>,
}

/// Event kinds worth rendering in voice. Pure flavour (market, vet rounds) keeps its
/// template line; the salient beats get the oracle.
pub const SALIENT: &[&str] = &[
    "calving", "party", "windfall", "scheme", "bureaucracy", "weather", "status", "household",
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

        Ok(Report {
            date: today.to_string(),
            day: target,
            weekday: today.weekday().to_string(),
            season: Season::of(today).name().to_string(),
            armed: Season::of(today).armed().to_string(),
            agents: world.agents.clone(),
            animals: world.animals.clone(),
            pending,
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
