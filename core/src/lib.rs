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
    // --- the body: a soul is not a disembodied mind. Their flesh has states that shape the spirits
    //     and bias the day's choices — a soul worked to the bone, or ill with the damp, is short of
    //     temper and slow to generosity; a rested, well one moves easier. Drained by labour and the
    //     hard seasons, restored by rest and the Sabbath; the old tire sooner and mend slower. ---
    pub vigour: i16,             // [0,100] bodily energy; drained by the day's work, restored by rest
    pub health: i16,             // [0,100] physical wellness; the aged carry a lower ceiling, illness lowers it
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
    // --- an open killing's shadow: how heavily the town's finger points at this soul. The
    //     parish has no proof, only its fears and its grudges; suspicion accretes onto whoever
    //     those already point at, and a soul it settles on hangs — guilty or not, unknowable ---
    pub suspicion: i32,           // 0 none; rises with the town's dread and what it holds against them
    pub cleared: bool,            // a solid alibi has put them beyond suspicion — the pointing slides off
    // --- episodic memory: the specific things that happened TO this soul, that they carry and
    //     act on. A continuous self is mostly continuous memory. Salient fold-events deposit an
    //     engram; it fades with time (charged ones slower — flashbulb), capped to the few that
    //     still grip. This is what makes the relationship ledger *personal and remembered*, and
    //     what a soul's stream of consciousness is grounded in. Pure fold state — deterministic. ---
    pub memories: Vec<Memory>,
    // The autobiography: the defining moments of the whole life — a bereavement, a wedding, the day
    // accused, a buried thing — consolidated here at full strength the moment they happen, and carried
    // for life. The working store above turns over within weeks; this does not fade, only fills. It is
    // what gives a soul a continuous self across the years, not just a memory of the last fortnight.
    pub lifelong: Vec<Memory>,
    // --- a predictive self-model: the things a soul expects, held with a confidence, that the
    //     world then confirms or violates. Surprise — the gap between what they were sure of and
    //     what came to pass — is the engine: it scales the felt blow (a betrayal stings far past
    //     a thing half-feared), stamps a memory the harder (flashbulb), and bends what they
    //     believe. A soul that can be *wrong*, and feel it, and revise — that is the lever. ---
    pub expectations: Vec<Expectation>,
    // --- the recursive social mirror: the soul's own read of how the parish regards them — not
    //     their actual standing, but what they *believe* others make of them, recursed. It lags
    //     and distorts the truth (the anxious over-read a slight, the thick-skinned miss it), and
    //     it drives them: feeling judged sinks the spirits, and — under a killing — a soul who
    //     believes themselves suspected behaves furtively, which draws the very suspicion they
    //     dread. What I think they think of me, made flesh and turned back on the world. ---
    pub seen_as: i16, // [-100,100]; <0 = "they think ill of me / suspect me", >0 = "i am well thought of"
    // --- the global workspace: the ONE thing uppermost in their mind right now. The soul's many
    //     concerns — a grief, the dread of the killing, a courtship, a scheme — contend each day,
    //     and a single winner is broadcast: their preoccupation. It gates the rest — a mind gripped
    //     by the murder cannot freely turn to a courtship; the workspace is occupied. This is the
    //     integration that makes them one mind with a focus, not a heap of parallel ledgers. ---
    pub focus: Preoccupation,
    // A grounded private truth this soul carries and will not tell — a real fact the kernel holds
    // (a hidden guilt, a thing they did, a thing they know), fed ONLY into their own inner life so
    // it surfaces consistently and never contradicts itself. Empty for most. The murderer carries
    // theirs here as a buried truth; it leaks as dread and compulsion but is never plainly confessed.
    pub secret: String,
}

/// What is uppermost in a soul's mind — the winner of the day's contention among their concerns,
/// broadcast to gate attention, initiative, and what their reflection turns on. The mechanism a
/// leading theory of consciousness (the global workspace) puts at the centre — a single focus, not
/// many parallel processes. (It models *access*, not experience: it does not, and cannot, settle
/// whether anything is felt. But it is the architecture of a mind that can be *preoccupied*.)
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Preoccupation {
    pub topic: String,  // dread | grief | haunt | betrayal | wrong | courtship | feud | venture | work
    pub target: i32,    // the soul it concerns, or -1
    pub intensity: i16, // 0..100 — how wholly it holds the mind
}

/// One thing that happened to a soul and stuck — an engram. Distinct from the affinity ledger
/// (a running sum that forgets the particulars): this remembers the *occasion*, dated and charged,
/// so a soul can carry "the day they were accused" or "the day they buried their husband" and have
/// it bias what they do, and colour what they think. Salience is how much it still grips (0 = let
/// go); valence its emotional sign. `who` is the other soul it concerns, or -1 for a thing with no
/// face (a grief at large, a dread with no recallable cause).
#[derive(Clone, Serialize, Deserialize)]
pub struct Memory {
    pub day: i64,        // when it happened
    pub kind: String,    // grief | snub | kindness | accused | cleared | wed | haunt | betrayed | reprieve | wronged | vindicated | ...
    pub who: i32,        // the other soul concerned, or -1 for a faceless one
    pub valence: i16,    // emotional charge [-100, 100]
    pub salience: i16,   // how much it still grips [0, 100]; decays daily, charged ones slower
}

/// A held expectation: what a soul predicts of something they have a stake in, and how sure they
/// are of it. When the world resolves it the other way, the *surprise* (error × confidence) is
/// what bites — and a confident expectation betrayed bites far harder than a thing half-feared.
/// `topic` says what is predicted; `predicted` is the value expected on that topic's own scale.
#[derive(Clone, Serialize, Deserialize)]
pub struct Expectation {
    pub about: i32,      // the soul it concerns; -1 = the parish/their own standing at large
    pub topic: u8,       // 0 regard (how `about` will hold them) | 1 standing (how the parish holds them)
    pub predicted: i16,  // the value they expect, on the topic's scale (an affinity, or a hold 0..100)
    pub confidence: i16, // [0, 100] how sure they are — scales the surprise when it is wrong
    pub set_on: i64,     // the day it was last formed or re-held
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

/// An open murder the town is trying to solve. There is no recorded culprit — not in this
/// struct, not anywhere: the killer is one of the town's own, and which one is unknown even
/// to the chronicle. The inquest is the manhunt itself — fear, gossip, and grudge converging
/// on whoever the parish's existing tensions point at. It may hang an innocent. Nobody knows.
#[derive(Clone, Serialize, Deserialize)]
pub struct Inquest {
    pub victim: usize,        // the murdered, by index (kept in the cast, but inert)
    pub victim_name: String,
    pub opened: i64,          // the day the body was found
    pub accused: i32,         // -1 until the town fixes on a soul to hang
    pub accused_since: i64,   // the day it fixed on them
    pub hanged: bool,         // the town has had its blood
    pub closed: bool,         // the inquest is over — by a hanging, or given up
    pub investigator: i32,    // -1, or the soul who leads the official inquiry, bending it their way
    pub public_inquiry: bool, // the magistrate is compelled to question every soul and read it out
    pub held_until: i64,      // 0, or a day through which the magistrate, having weighed a charge and
                              // stayed his hand, is not pressed to decide again — the LLM ruling's cooldown
    pub culprit: i32,         // -1 by design (an unsolved killing), or the index of who ACTUALLY did it —
                              // the buried truth. Hidden: never surfaced to the town or other souls; it
                              // only grounds the real killer's own inner life. The thread the parish has
                              // is too thin to reach it, and the eye stays on the scapegoat regardless.
}

/// A death the parish has yet to bury — the funeral is held some days on, a great occasion the
/// whole town marks together. Kept in the world so it folds deterministically toward its day.
#[derive(Clone, Serialize, Deserialize)]
pub struct Funeral {
    pub who: usize,        // the dead, by index (inert, but the name is kept for the rite)
    pub name: String,
    pub scheduled: i64,    // the day the parish gathers
    pub murdered: bool,    // a murdered soul's funeral is charged — the killer among the mourners
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
    /// The town's fear in the wake of an unpunished killing — 0 at peace. While an inquest is
    /// open it lingers high; a hanging breaks it; then the unease ebbs over weeks.
    pub dread: i16,
    /// An open murder, if the town is living under one.
    pub inquest: Option<Inquest>,
    /// Deaths awaiting burial — the funerals the parish will gather for, each on its day.
    pub funerals: Vec<Funeral>,
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
            vigour: 78,
            health: 88,
            courting: -1,
            courtship: 0,
            acted_day: -1,
            rival: -1,
            feud: 0,
            intent: 0,
            intent_goal: 0,
            intent_age: 0,
            suspicion: 0,
            cleared: false,
            memories: Vec::new(),
            lifelong: Vec::new(),
            expectations: Vec::new(),
            seen_as: 0,
            focus: Preoccupation::default(),
            secret: String::new(),
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
            dread: 0,
            inquest: None,
            funerals: Vec::new(),
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

    /// Set loose a piece of news that was announced in the open — read out before the assembled
    /// parish, cried at the inquiry — so the whole town hears it at once. It starts as common
    /// knowledge rather than a single-knower whisper that must trickle hop by hop.
    fn spawn_news_open(&mut self, subject: &str, topic: &str, valence: i32, day: i64) {
        if let Some(s) = self.idx(subject) {
            let heard: Vec<usize> = (0..self.agents.len()).filter(|&i| self.agents[i].active()).collect();
            self.spawn_news_idx(s, topic, valence, day, &heard);
        }
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

    /// Lay down an engram: a thing that happened to `idx` and stuck. Same occasion on the same
    /// day only deepens (the salience held, not doubled) rather than crowding the store with
    /// duplicates. The store is kept to the few that still grip most — a soul carries a handful
    /// of live memories, not a ledger of everything.
    fn remember(&mut self, idx: usize, kind: &str, who: i32, valence: i16, salience: i16, day: i64) {
        let salience = salience.clamp(0, 100);
        let valence = valence.clamp(-100, 100);
        let store = &mut self.agents[idx].memories;
        if let Some(m) = store.iter_mut().find(|m| m.kind == kind && m.who == who && m.day == day) {
            m.salience = m.salience.max(salience);
        } else {
            store.push(Memory { day, kind: kind.into(), who, valence, salience });
            if store.len() > MEMORY_CAP {
                store.sort_by_key(|m| std::cmp::Reverse(m.salience));
                store.truncate(MEMORY_KEEP);
            }
        }
        // consolidate the defining moments into the lifelong store, at full strength, the instant
        // they happen — so even as the working memory above turns over within weeks, the shape of a
        // whole life is kept: the bereavements, the matches, the day one stood accused, the buried thing.
        let defining = salience >= CONSOLIDATE_AT
            || matches!(kind, "grief" | "wed" | "accused" | "cleared" | "vindicated" | "haunt");
        if defining {
            let life = &mut self.agents[idx].lifelong;
            if let Some(m) = life.iter_mut().find(|m| m.kind == kind && m.who == who && m.day == day) {
                m.salience = m.salience.max(salience);
            } else {
                life.push(Memory { day, kind: kind.into(), who, valence, salience });
                if life.len() > LIFELONG_CAP {
                    life.sort_by_key(|m| std::cmp::Reverse(m.salience)); // laid down strong; only the faintest are let go
                    life.truncate(LIFELONG_CAP);
                }
            }
        }
    }

    /// What a soul most carries right now — their live engrams, the most gripping first.
    fn carried(&self, idx: usize) -> Vec<&Memory> {
        let mut v: Vec<&Memory> = self.agents[idx].memories.iter().filter(|m| m.salience > 0).collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.salience));
        v
    }

    /// The single live grievance this soul holds against `other`, if any — a remembered wound
    /// (snub, accusation) still gripping. This is what makes a grudge *stick* past the weekly
    /// fade: there is a particular occasion behind it, not just a cooled number.
    fn grievance(&self, idx: usize, other: usize) -> i16 {
        self.agents[idx].memories.iter()
            .filter(|m| m.who == other as i32 && m.valence < 0)
            .map(|m| m.salience)
            .max()
            .unwrap_or(0)
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
                // family visit family as family — a parent calling on a child is no courtship
                let kin = world.agents[i].spouse == Some(t)
                    || world.agents[i].parent == Some(t)
                    || world.agents[t].parent == Some(i)
                    || (world.agents[i].parent.is_some() && world.agents[i].parent == world.agents[t].parent);
                let templates: &[&str] = if kin {
                    &[
                        "{n} looked in on {t}, as family will.",
                        "{n} spent an hour with {t} by the fire.",
                        "{n} called on {t} to see how they did.",
                    ]
                } else {
                    &[
                        "{n} paid an afternoon call on {t}, and was thought to look very well.",
                        "{n} called on {t}, leaving a card and a good impression.",
                        "{n} took tea with {t}, and the visit was a success on both sides.",
                        "{n} called on {t} — cultivating the acquaintance, said the unkind.",
                    ]
                };
                let line = pick(&mut rng, templates).replace("{n}", &name).replace("{t}", &tname);
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
                // --target is the newcomer's name. --note is either plain flavour, or a structured
                // "archetype|trade|seat|age|standing|purse|blurb" to drop in a fully-formed incomer.
                // An 8-field form "archetype|trade|seat|age|standing|purse|sex|blurb" sets the sex too
                // (0 = woman, 1 = man); the 7-field form defaults to a man, as before.
                let name = if t.is_empty() { "A stranger".to_string() } else { t.clone() };
                let known = ["genteel_status_seeker", "hill_farmer", "practitioner", "scheming_improver", "blunt_hand", "official"];
                let p: Vec<&str> = iv.note.split('|').collect();
                let (arch, trade, seat, age, standing, purse, sex, blurb): (&str, &str, &str, i64, i32, i32, u8, &str) = if p.len() >= 8 {
                    let a = if known.contains(&p[0].trim()) { p[0].trim() } else { "blunt_hand" };
                    let seat = if p[2].trim().is_empty() { "the empty cottage" } else { p[2].trim() };
                    (a, p[1].trim(), seat, p[3].trim().parse().unwrap_or(33), p[4].trim().parse().unwrap_or(25), p[5].trim().parse().unwrap_or(12), p[6].trim().parse::<u8>().unwrap_or(1).min(1), p[7].trim())
                } else if p.len() >= 7 {
                    let a = if known.contains(&p[0].trim()) { p[0].trim() } else { "blunt_hand" };
                    let seat = if p[2].trim().is_empty() { "the empty cottage" } else { p[2].trim() };
                    (a, p[1].trim(), seat, p[3].trim().parse().unwrap_or(33), p[4].trim().parse().unwrap_or(25), p[5].trim().parse().unwrap_or(12), 1, p[6].trim())
                } else {
                    ("blunt_hand", "", "the empty cottage", 33, 25, 12, 1, iv.note.as_str())
                };
                let mut agent = make_agent(&name, arch, seat, standing, purse, sex, age, day);
                agent.origin = Some("parts unknown".into());
                if !trade.is_empty() {
                    agent.trade = Some(trade.to_string());
                }
                let blurb_txt = if blurb.is_empty() {
                    format!("{name} arrived in Thrushcombe and took {seat}. Nobody knew quite who they were.")
                } else {
                    format!("{name} — {blurb} — arrived in Thrushcombe and took up at {seat}. Nobody knew quite who they were.")
                };
                out.push(mk("providence", &name, blurb_txt));
                world.spawn_news(&name, &format!("the stranger lately come to {seat}"), 1, day, &[]);
                world.agents.push(agent);
            }
            "appoint" => {
                // An existing soul takes on a vacated office — a rise in standing and means. If a
                // killing is open, the parish marks the one who profits by a death, and suspicion
                // gathers on them: the negative talk both lands now and feeds the inquest for weeks.
                let role = if iv.note.is_empty() { "new duties in the town".to_string() } else { iv.note.clone() };
                let rise = if iv.amount > 0 { iv.amount } else { 8 };
                if let Some(a) = world.agent_mut(t) {
                    clamp_standing(a, rise);
                    a.purse += rise * 2; // the office's emolument
                    a.trade = Some(role.clone());
                }
                out.push(mk("succession", t, format!("{t} has taken on {role}, and risen in the town's eyes for it.")));
                world.spawn_news(t, &format!("{t} coming into {role}"), 2, day, &[]);
                let open_victim = world.inquest.as_ref().filter(|q| !q.closed).map(|q| q.victim_name.clone());
                if let Some(vn) = open_victim {
                    if let Some(idx) = world.idx(t) {
                        world.agents[idx].suspicion += 20; // the town notes who gains
                        world.spawn_news(t, &format!("how it is {t} who profits by {vn}'s death"), -2, day, &[]);
                    }
                }
            }
            "funeral" => {
                // hold a soul's funeral now — a great occasion the parish gathers for. Used to lay
                // a death to rest on the novelist's cue; any pending automatic funeral is consumed.
                match world.idx(t) {
                    Some(w) if !world.agents[w].active() => {
                        let murdered = world.inquest.as_ref().map(|q| q.victim == w).unwrap_or(false)
                            || world.funerals.iter().find(|f| f.who == w).map(|f| f.murdered).unwrap_or(false);
                        world.funerals.retain(|f| f.who != w); // no second burial
                        hold_funeral(world, w, t, murdered, day, date, &mut out);
                    }
                    Some(_) => out.push(mk("providence", t, format!("{t} is alive and well — there is no funeral to hold."))),
                    None => out.push(mk("providence", t, format!("There is no {t} in the parish to bury."))),
                }
            }
            "inquiry" => {
                // public outcry and pressure from the county compel the magistrate to question
                // every soul and read the transcripts in the open. Sets the flag the inquest runs on.
                if let Some(q) = world.inquest.as_mut().filter(|q| !q.closed) {
                    q.public_inquiry = true;
                }
                let who = world.inquest.as_ref().and_then(|q| world.agents.get(q.investigator as usize)).map(|a| a.name.clone()).unwrap_or_else(|| "the magistrate".into());
                out.push(mk("inquest", &who, format!("Under an outcry in the parish and pressure from the county, {who} is compelled to question every soul in Thrushcombe, and the transcripts are to be read in the open.")));
                world.spawn_news_open(&who, "how the magistrate must question the whole town", 0, day);
            }
            "investigate" => {
                // Name the soul who will lead the official inquiry into an open killing. They bend it
                // their way: a genteel magistrate shields the respectable and presses the working folk
                // and the incomers (see tend_inquest). Resolve the index first to dodge the borrow.
                let who = world.idx(t);
                if let Some(q) = world.inquest.as_mut().filter(|q| !q.closed) {
                    q.investigator = who.map(|i| i as i32).unwrap_or(-1);
                }
                if let Some(idx) = who { clamp_standing(&mut world.agents[idx], 2); }
                out.push(mk("inquest", t, format!("{t} has taken the inquiry into {}'s murder in hand, sitting in judgement over the parish.",
                    world.inquest.as_ref().map(|q| q.victim_name.clone()).unwrap_or_default())));
                world.spawn_news_open(t, &format!("how {t} sits in judgement over the murder"), 1, day);
            }
            "murder" => {
                // A killing in the parish, by one of its own. The victim falls inert; the town is
                // thrown into dread; an inquest opens that the manhunt (tend_inquest) will press.
                // No culprit is recorded — the killer is unknown even to the chronicle.
                match world.idx(t) {
                    Some(v) if world.agents[v].active() => {
                        let seat = world.agents[v].seat.clone();
                        let how = if iv.note.is_empty() {
                            "no natural death — a life taken by a hand the parish does not yet know".to_string()
                        } else {
                            iv.note.clone()
                        };
                        world.agents[v].death_day = Some(day);
                        world.agents[v].courting = -1;
                        world.agents[v].rival = -1;
                        out.push(mk("murder", t, format!("{t}, of {seat}, was found dead — and {how}. Murder, in Thrushcombe St Mary, by one of its own.")));
                        // every soul knows a murder at once; what they do not know is whose hand
                        let all: Vec<usize> = (0..world.agents.len()).filter(|&i| world.agents[i].active()).collect();
                        world.spawn_news_idx(v, &format!("the murder of {t} — and the killer still among them"), 0, day, &all);
                        // grief on any kin; fear on everyone — doors barred, neighbour eyeing neighbour
                        for k in 0..world.agents.len() {
                            if !world.agents[k].active() { continue; }
                            let kin = world.agents[k].spouse == Some(v) || world.agents[k].parent == Some(v) || world.agents[v].parent == Some(k);
                            nudge_mood(&mut world.agents[k], if kin { -45 } else { -22 });
                        }
                        world.dread = 85;
                        world.funerals.push(Funeral { who: v, name: t.clone(), scheduled: day + FUNERAL_DELAY, murdered: true });
                        world.inquest = Some(Inquest {
                            victim: v,
                            victim_name: t.clone(),
                            opened: day,
                            accused: -1,
                            accused_since: 0,
                            hanged: false,
                            closed: false,
                            investigator: -1,
                            public_inquiry: false,
                            held_until: 0,
                            culprit: -1,
                        });
                    }
                    _ => out.push(mk("providence", t, format!("A killing was spoken of, but {t} was not there to be found."))),
                }
            }
            "haunt" => {
                // A buried thing laid on a soul — a charged engram with no face to it (who = -1),
                // that does not fade with time and surfaces unbidden as a dread the soul cannot
                // account for. Private: it touches no public chronicle, spreads no gossip. This is
                // how a repression is carried without the kernel recording its cause — the parish,
                // and the chronicle, remain unable to know what sits behind it.
                if let Some(s) = world.idx(t) {
                    let sal = if iv.amount > 0 { iv.amount.clamp(1, 100) as i16 } else { 90 };
                    world.remember(s, "haunt", -1, -90, sal, day);
                }
            }
            "secret" => {
                // A grounded private truth laid on a soul: a real fact the kernel now holds, fed ONLY
                // into their own inner life so it surfaces consistently and never contradicts itself.
                // Private — it touches no public chronicle, spreads no gossip, and is never shown to
                // any other soul. amount = 1 marks them the TRUE killer of the open murder: the buried
                // truth the town cannot reach. The kernel knows; the parish, by design, never will.
                if let Some(s) = world.idx(t) {
                    world.agents[s].secret = iv.note.clone();
                    if iv.amount == 1 {
                        if let Some(q) = world.inquest.as_mut().filter(|q| !q.closed) {
                            q.culprit = s as i32;
                        }
                    }
                }
            }
            "bond" => {
                // One soul takes another into their household — a stray, a lodger, an orphan. --target
                // is the one taken in; --note names the host; --amount sets the warmth of the new tie
                // (default 35). The taken-in moves to the host's seat, and a warm bond forms both ways —
                // the sheltered the more grateful — with a kindness the taken-in carries.
                let host_name = iv.note.trim();
                if let (Some(taken), Some(host)) = (world.idx(t), world.idx(host_name)) {
                    if taken != host {
                        let warmth: i16 = if iv.amount > 0 { iv.amount.clamp(1, 100) as i16 } else { 35 };
                        let seat = world.agents[host].seat.clone();
                        world.agents[taken].seat = seat.clone();
                        world.nudge_aff(host, taken, warmth);
                        world.nudge_aff(taken, host, (warmth + 15).min(100));
                        world.remember(taken, "reprieve", host as i32, 55, 60, day);
                        let hn = world.agents[host].name.clone();
                        out.push(mk("providence", &hn, format!("{hn} took {t} in — a roof and a place at {seat} where there had been none.")));
                        world.spawn_news(&hn, &format!("how {hn} took {t} in"), 1, day, &[]);
                    }
                }
            }
            "proclaim" => {
                // A town-wide proclamation from one in authority — pinned to the church door, read out
                // in the square, carried into every parlour. --target is who proclaims it, --note the
                // words. A public beat that the whole parish hears; --amount is its temper: positive
                // eases the dread and lifts the spirits a little (direction, hope), negative deepens them.
                if let Some(s) = world.idx(t) {
                    let msg = iv.note.trim();
                    let nm = world.agents[s].name.clone();
                    let text = if msg.is_empty() {
                        format!("{nm} put out a proclamation to the whole parish.")
                    } else {
                        format!("{nm} had it put about the parish — pinned to the church door and read out in the square: {msg}")
                    };
                    out.push(mk("providence", &nm, text));
                    world.spawn_news(&nm, &format!("{nm}'s proclamation to the parish"), 1, day, &[]);
                    let ease: i16 = iv.amount.clamp(-20, 20) as i16;
                    if ease != 0 {
                        world.dread = (world.dread - ease).clamp(0, 100);
                        for a in world.agents.iter_mut().filter(|a| a.active()) {
                            nudge_mood(a, ease / 2);
                        }
                    }
                }
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
        vigour: 78,
        health: 88,
        courting: -1,
        courtship: 0,
        acted_day: -1,
        rival: -1,
        feud: 0,
        intent: 0,
        intent_goal: 0,
        intent_age: 0,
        suspicion: 0,
        cleared: false,
        memories: Vec::new(),
            lifelong: Vec::new(),
        expectations: Vec::new(),
        seen_as: 0,
        focus: Preoccupation::default(),
        secret: String::new(),
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

/// Render one engram as a felt weight, in the chronicle's voice — for the dossier a soul
/// contemplates and the dashboard's "what they carry". A repressed haunt is given only as a
/// nameless dread; its cause is never named, because the soul cannot reach it.
fn engram_phrase(w: &World, m: &Memory) -> String {
    let who = (m.who >= 0).then(|| w.agents.get(m.who as usize).map(|a| a.name.as_str())).flatten().unwrap_or("");
    let grip = if m.salience >= 70 { "still raw" } else if m.salience >= 40 { "not yet settled" } else { "fading now" };
    match m.kind.as_str() {
        "grief"   => format!("a grief, {grip} — the loss of {who}"),
        "accused" => format!("the terror of having stood named for murder before the whole parish, {grip}"),
        "cleared" => format!("the relief of having been believed and cleared, {grip}"),
        "snub"    => format!("a slight from {who} they have not forgiven, {grip}"),
        "wed"     => format!("the joy of their match with {who}, {grip}"),
        "haunt"   => "a dread that rises in them with no cause they can name — leaving them adrift, floating, strange to themselves".to_string(),
        "betrayed"   => format!("the sting of {who} turning cold on them, where they had been so sure of warmth, {grip}"),
        "reprieve"   => format!("warmth come from {who} where they had given up hoping for it, {grip}"),
        "wronged"    => format!("the parish turning against them for no thing they have done — a wrong they cannot make answer to, {grip}"),
        "vindicated" => format!("having come through the parish's suspicion they so feared, {grip}"),
        other     => format!("something of {other}{}, {grip}", if who.is_empty() { String::new() } else { format!(" concerning {who}") }),
    }
}

/// Render a held expectation as the thing a soul is counting on, in their own forward-looking
/// terms — to set in the dossier beside what they carry, so the hour can reckon with whether it
/// is holding. Confidence becomes a word: a thing they are sure of, or merely hope.
fn expectation_phrase(w: &World, idx: usize, e: &Expectation) -> String {
    let sure = if e.confidence >= 75 { "are sure" } else if e.confidence >= 55 { "trust" } else { "half-hope" };
    match e.topic {
        0 => {
            let who = (e.about >= 0).then(|| w.agents.get(e.about as usize).map(|a| a.name.as_str())).flatten().unwrap_or("them");
            if e.predicted >= 25 { format!("they {sure} of {who}'s good regard") }
            else if e.predicted <= -25 { format!("they {sure} {who} is set against them, and expect no better") }
            else { format!("they are uncertain quite where they stand with {who}") }
        }
        _ => {
            let hold = parish_hold(&w.agents[idx]);
            if hold + 12 < e.predicted { format!("they {sure} the parish will see them clear of the killing — though it is not going as they expected") }
            else { format!("they {sure} the parish holds them as it should, and will see them clear of the killing") }
        }
    }
}

/// How a soul imagines the parish regards them — the recursive mirror in words. This is their
/// *belief* about others' minds, which may sit well wide of the truth; phrased so a reflection
/// reasons from how they feel themselves seen, not from their real standing.
pub fn self_regard_phrase(seen_as: i16) -> &'static str {
    match seen_as {
        x if x <= -55 => "they feel the parish has turned against them — that they are watched, doubted, thought the worst of",
        x if x <= -25 => "they feel themselves under a cloud of late, less well thought of than they were",
        x if x < 25 => "they feel they stand about as they always have in the parish's eyes",
        x if x < 55 => "they feel well thought of in the parish",
        _ => "they feel the parish holds them high, and warm to them",
    }
}

/// A word for a soul's present spirits.
pub fn mood_word(m: i16) -> &'static str {
    match m {
        x if x <= -55 => "downcast",
        x if x <= -28 => "low",
        x if x <= -9 => "out of sorts",
        x if x < 9 => "content",
        x if x < 28 => "in good humour",
        x if x < 55 => "in good spirits",
        _ => "triumphant",
    }
}

/// The mood word a soul actually wears — the numeric band (`mood_word`), but with the deepest
/// band reading "grieving" ONLY for a soul carrying a real, still-gripping bereavement. A town
/// merely worn down by drought and dread is *downcast*, not bereaved; grief is named for grief.
pub fn mood_of(a: &Agent) -> &'static str {
    if a.mood <= -55
        && a.memories.iter().any(|m| m.kind == "grief" && m.salience >= 50)
    {
        return "grieving";
    }
    mood_word(a.mood)
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

/// What a soul most wants, in words — courtship-aware. A suit toward marriage reads as winning a
/// hand; an attachment where either party is already wed cannot end in marriage, so it reads as
/// being *drawn to* them — an affair, not a courtship. Kin are never rendered as romance.
pub fn want_phrase(w: &World, idx: usize) -> String {
    let a = &w.agents[idx];
    if a.archetype == "child" { return "to grow up".into(); }
    // a love-interest is the deepest want — it shows even for the suspected or the plotting
    if a.courting >= 0 {
        if let Some(t) = w.agents.get(a.courting as usize) {
            let affair = a.spouse.is_some() || t.spouse.is_some();
            return if affair { format!("drawn to {}", t.name) } else { format!("to win {}'s hand", t.name) };
        }
    }
    // the plotter's active answer to the crisis — grounded in their own secret (the player's view)
    if a.secret.contains("Pringle") && (a.secret.contains("bench") || a.secret.contains("petition") || a.secret.contains("rid of")) {
        return "to see Major Pringle off the bench".into();
    }
    // under the cloud of an open murder, survival eclipses ambition — but only for the genuinely
    // hunted: the rope for the worst-suspected, clearing one's name for the rest of the cloud.
    if w.inquest.is_some() && a.suspicion >= 75 {
        let maxs = w.agents.iter().filter(|x| x.active()).map(|x| x.suspicion).max().unwrap_or(0);
        return if a.suspicion >= maxs - 25 { "to clear their name before the noose".into() }
               else { "to clear their name".into() };
    }
    // a placeless incomer, still finding a footing
    if a.origin.is_some() && a.standing <= 20 { return "to find a footing in the town".into(); }
    goal_label(w, a.goal, a.goal_target)
}

/// A soul's actual ties, drawn straight from the model — marriage, blood, and courtship — so the
/// oracle reasons from the truth and never invents a spouse, a child, or an attachment that does
/// not exist. Two souls who are kin are stated as kin and never as suitors. Fed into every dossier.
pub fn relationships_brief(w: &World, idx: usize, day: i64) -> String {
    let a = &w.agents[idx];
    let nm = |i: usize| w.agents[i].name.clone();
    let mut parts: Vec<String> = Vec::new();
    match a.spouse {
        Some(s) => parts.push(format!("married to {}", nm(s))),
        None => parts.push("not married".into()),
    }
    if let Some(p) = a.parent {
        if w.agents.get(p).is_some_and(|x| x.active()) { parts.push(format!("a son or daughter of {}", nm(p))); }
    }
    let kids: Vec<String> = (0..w.agents.len())
        .filter(|&j| w.agents[j].parent == Some(idx) && w.agents[j].active())
        .map(|j| format!("{} ({})", w.agents[j].name, w.agents[j].age(day)))
        .collect();
    if !kids.is_empty() { parts.push(format!("parent of {}", kids.join(", "))); }
    if a.courting >= 0 {
        if let Some(t) = w.agents.get(a.courting as usize) {
            let affair = a.spouse.is_some() || t.spouse.is_some();
            parts.push(if affair {
                format!("carrying an attachment outside marriage to {} (it cannot end in a wedding)", t.name)
            } else {
                format!("paying court to {}", t.name)
            });
        }
    }
    let suitors: Vec<String> = (0..w.agents.len())
        .filter(|&j| w.agents[j].courting == idx as i32 && w.agents[j].active())
        .map(nm).collect();
    if !suitors.is_empty() { parts.push(format!("being courted by {}", suitors.join(", "))); }

    format!(
        "Their actual ties in the parish, and the whole of them — reason only from these; do NOT invent a spouse, a child, or any attachment beyond what is named: {}.",
        parts.join("; ")
    )
}

/// How `other` stands to `me` in blood or courtship, if at all — stated plainly (and from `me`'s
/// vantage) so two souls in talk never invent, nor forget, a marriage, a parentage, or a suit
/// between them. Returns None when they are unrelated.
pub fn pair_relation(w: &World, me: usize, other: usize) -> Option<String> {
    let (a, b) = (&w.agents[me], &w.agents[other]);
    let term = |male: &'static str, female: &'static str| if b.sex == 1 { male } else { female };
    if a.spouse == Some(other) { return Some(format!("{} is your own {}", b.name, term("husband", "wife"))); }
    if a.parent == Some(other) { return Some(format!("{} is your {}", b.name, term("father", "mother"))); }
    if b.parent == Some(me) { return Some(format!("{} is your own {}", b.name, term("son", "daughter"))); }
    if a.parent.is_some() && a.parent == b.parent { return Some(format!("{} is your {}", b.name, term("brother", "sister"))); }
    if a.courting == other as i32 && b.courting == me as i32 { return Some(format!("you and {} are courting one another", b.name)); }
    if a.courting == other as i32 {
        let affair = a.spouse.is_some() || b.spouse.is_some();
        return Some(if affair { format!("you carry an attachment to {}, outside of marriage", b.name) } else { format!("you are paying court to {}", b.name) });
    }
    if b.courting == me as i32 {
        let affair = a.spouse.is_some() || b.spouse.is_some();
        return Some(if affair { format!("{} carries an attachment to you, outside of marriage", b.name) } else { format!("{} is paying court to you", b.name) });
    }
    None
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

/// The manhunt for an unsolved killing, pressed day by day. There is no recorded culprit:
/// suspicion accretes onto whoever the town's standing fears and grudges already point at —
/// the soul who quarrelled with the dead, the one with a secret, the incomer, the desperate.
/// When it settles past bearing on one of them the parish fixes on them, and — doubt failing —
/// hangs them, guilty or innocent, a thing no living soul will ever truly know. A hanging
/// breaks the dread; until then it festers, and a case that collapses only deepens the fear.
///
/// The one thing the fold does NOT decide on its own is the *charge*: once the cloud over the
/// lead suspect crosses JUDGE_AT the magistrate must rule (accuse | hold | widen), and that ruling
/// is the oracle's — recorded as a `judgment` decree and folded like any other. See pending_judgment.
const JUDGE_AT: i32 = 75;   // the cloud over the lead suspect at which the magistrate is summoned to a ruling
const HOLD_DAYS: i64 = 4;   // having weighed a charge and stayed his hand, he is not pressed again for this long

// The general act loop: a soul is moved to take an outward action only when something genuinely
// grips them (a preoccupation that fills the mind, or a plan ripe for a move) past ACT_FLOOR, and
// only if they have not acted within ACT_COOLDOWN days. Both gates keep the town a quiet market
// town — most souls, most days, simply live the ambient sim — rather than a stage of constant motion.
const ACT_FLOOR: i16 = 45;
const ACT_COOLDOWN: i64 = 3;

// The departure decision: when a soul's lot has come past bearing — ruin, or the parish's suspicion
// in an open murder — they may be put to the gravest choice of all, to leave Thrushcombe for good.
// DEPART_FLOOR is how far past bearing it must be before the choice arises; DEPART_COOLDOWN keeps a
// soul who chose to stay from being asked again every day (the question is momentous, not nagging).
const DEPART_FLOOR: i32 = 40;
const DEPART_COOLDOWN: i64 = 12;

// The betrothal decision: a long, mutual courtship is no longer joined by the fold of its own accord
// — when it has ripened (BETROTH_AT steps, mutual warmth) the COURTED soul is put to the proposal and
// rules accept|refuse. The first two-sided decision: a suitor's pursuit, the other's answer.
const BETROTH_AT: i16 = 30;
// The crop-gamble decision: a farmer, in a growing season, weighs a bold risk on the land against
// the small sure return of honest husbandry. GAMBLE_COOLDOWN keeps it to about once a season.
const GAMBLE_COOLDOWN: i64 = 60;
fn tend_inquest(world: &mut World, day: i64, date: Date, rng: &mut ChaCha8Rng, out: &mut Vec<Event>) {
    let mk = |actor: &str, text: String| Event { day, date: date.to_string(), kind: "inquest".into(), actor: actor.into(), text };
    let Some(inq) = world.inquest.clone() else { return };
    if inq.closed {
        // the town has had its reckoning; the unease ebbs over weeks, then lifts entirely
        world.dread = (world.dread - 2).max(0);
        if world.dread == 0 {
            world.inquest = None;
        }
        return;
    }
    let v = inq.victim;
    let vn = inq.victim_name.clone();
    let n = world.agents.len();

    // dread festers while a killer walks free: it drifts down, but never settles
    world.dread = (world.dread - 1).max(45);

    // --- suspicion accretes onto whoever the town already mistrusts ---
    let has_secret: Vec<bool> = (0..n)
        .map(|i| world.news.iter().any(|nw| nw.subject == i && nw.valence < 0 && day - nw.born <= 40))
        .collect();
    let top_standing = world.agents.iter().filter(|a| a.active()).map(|a| a.standing).max().unwrap_or(0);
    // the official inquiry, if one has been taken in hand, bends the suspicion its leader's way.
    // A genteel magistrate (Major Pringle) shields his own kind and turns the questions on the
    // working folk and the incomers — class justice, baked into the accrual, not just narrated.
    let investigator = inq.investigator;
    let inv_genteel = (investigator >= 0)
        .then(|| world.agents.get(investigator as usize))
        .flatten()
        .is_some_and(|a| matches!(a.archetype.as_str(), "genteel_status_seeker" | "official"));
    for i in 0..n {
        if i == v || !world.agents[i].active() || world.agents[i].archetype == "child" {
            world.agents[i].suspicion = (world.agents[i].suspicion - 1).max(0);
            continue;
        }
        if world.agents[i].cleared {
            world.agents[i].suspicion = 0; // a solid alibi — the pointing slides off them
            continue;
        }
        let mut d = 0i32;
        // bad blood with the victim — the first motive the town reaches for
        let av = world.aff(i, v).min(world.aff(v, i));
        if av <= -40 { d += 5; } else if av <= -20 { d += 3; } else if av <= -8 { d += 1; }
        if has_secret[i] { d += 3; }                              // something to hide
        if world.agents[i].origin.is_some() { d += 3; }          // an incomer the parish never trusted
        if world.agents[i].purse < -25 { d += 2; }               // the cornered and the desperate
        if world.agents[i].mood <= -40 { d += 1; }
        if world.agents[i].standing < top_standing - 30 { d += 1; } // ill repute
        if rng.gen_bool(0.18) { d += rng.gen_range(1..=3); }     // the rumour lights where it will
        // the magistrate's thumb on the scale
        if i as i32 == investigator {
            d -= 10;                                             // the man who sits in judgement is never the suspect
        } else if inv_genteel {
            let respectable = matches!(world.agents[i].archetype.as_str(), "genteel_status_seeker" | "official" | "practitioner")
                || world.agents[i].standing >= top_standing - 12;
            if respectable { d -= 3; } else { d += 3; }          // shields his own; presses the labourer and the stranger
        }
        // the recursive mirror, turned back on the world: a soul who believes the parish already
        // takes them for the killer carries themselves furtively — won't meet an eye, starts at a
        // question, keeps to their cottage — and the parish reads the furtiveness as a guilty
        // conscience. Believing oneself seen as guilty helps make it so. A tragic little engine.
        if i as i32 != investigator && world.agents[i].seen_as <= -45 {
            d += if world.agents[i].seen_as <= -70 { 2 } else { 1 };
        }
        if d == 0 { d = -2; }                                    // suspicion is fickle; unfed, it cools
        world.agents[i].suspicion = (world.agents[i].suspicion + d).clamp(0, 200);
    }

    // the soul the finger points at hardest today
    let most = (0..n)
        .filter(|&i| world.agents[i].active() && i != v && world.agents[i].archetype != "child")
        .max_by_key(|&i| world.agents[i].suspicion);

    // --- public finger-pointing: the most-suspected take the parish's hard looks ---
    if let Some(m) = most {
        if world.agents[m].suspicion >= 40 && inq.accused < 0 && rng.gen_bool(0.22) {
            world.agents[m].standing = (world.agents[m].standing - 1).max(0);
            nudge_mood(&mut world.agents[m], -6);
            let nm = world.agents[m].name.clone();
            let line = match rng.gen_range(0..4) {
                0 => format!("At the Pelican the talk turned ugly, and more than one head turned toward {nm}."),
                1 => format!("They are saying in the lanes that {nm} was abroad the night {vn} died."),
                2 => format!("{nm} found the shop gone quiet at their entering, and every eye sliding away."),
                _ => format!("A stone was thrown at {nm}'s door in the night, and no grown soul rebuked the throwing."),
            };
            out.push(mk(&nm, line));
            world.spawn_news(&nm, &format!("the whisper that {nm} had a hand in {vn}'s death"), -3, day, &[]);
            world.dread = (world.dread + 3).min(100);
        }
    }

    // --- the reckoning: once the town has FIXED on a soul, the trial runs its course toward a
    //     hanging or a release. The fixing itself is no longer automatic: when the cloud over the
    //     lead suspect crosses JUDGE_AT the magistrate is *called to a ruling* (see pending_judgment
    //     / the `judge` job), and only an LLM-authored `judgment` decree of "accuse" sets inq.accused.
    //     So whether an innocent is charged at all now turns on a mind weighing it, not a threshold. ---
    const TRIAL_DAYS: i64 = 6;
    let mut inq = inq;
    if inq.accused >= 0 {
        let m = inq.accused as usize;
        if !world.agents[m].active() {
            inq.accused = -1; // gone by some other road — the hunt resets
        } else if day - inq.accused_since >= TRIAL_DAYS {
            // doubt may save them: standing, or staunch defenders, gives the parish pause; a
            // friendless soul of low face it does not. Innocence does not enter into it.
            let defenders = (0..n).filter(|&j| j != m && world.agents[j].active() && world.aff(j, m) >= 40).count();
            let saved = world.agents[m].standing >= top_standing - 12 || defenders >= 2;
            let nm = world.agents[m].name.clone();
            if saved {
                world.agents[m].suspicion = 30; // spared, not cleared — the doubt clings
                inq.accused = -1;
                nudge_mood(&mut world.agents[m], 12);
                out.push(mk(&nm, format!("The case against {nm} fell apart for want of proof, and the parish let them go — half of it unconvinced still. {vn}'s killer walks free yet.")));
                world.spawn_news(&nm, &format!("how the case against {nm} came to nothing"), 1, day, &[]);
                world.dread = (world.dread + 6).min(100); // a killer still loose — the fear deepens
            } else {
                // the town has its blood. Guilty or innocent, no one will ever know.
                world.agents[m].death_day = Some(day);
                inq.hanged = true;
                inq.closed = true;
                out.push(Event { day, date: date.to_string(), kind: "murder".into(), actor: nm.clone(), text: format!("{nm} was hanged for the murder of {vn} — whether by a just hand or a frightened one, the parish will never be sure.") });
                world.spawn_news_idx(m, &format!("the hanging of {nm} for {vn}'s murder"), 0, day, &(0..n).filter(|&i| world.agents[i].active()).collect::<Vec<_>>());
                for k in 0..n {
                    if world.agents[k].active() { nudge_mood(&mut world.agents[k], 8); } // relief, and horror
                    world.agents[k].suspicion = 0; // a corpse to blame — the pointing stops
                }
                world.dread = 20; // the dread breaks, though a shadow stays on the town
            }
        }
    }
    world.inquest = Some(inq);
}

/// Who sits in judgement: the soul leading the inquiry if there is one, else the natural justice of
/// the peace — the highest-standing genteel/official adult the parish would look to. None if the
/// town has emptied of anyone fit to sit (a degenerate case). Used to address the magistrate's ruling.
fn magistrate_idx(world: &World) -> Option<usize> {
    let inq = world.inquest.as_ref()?;
    if inq.investigator >= 0 && world.agents.get(inq.investigator as usize).is_some_and(|a| a.active()) {
        return Some(inq.investigator as usize);
    }
    (0..world.agents.len())
        .filter(|&i| world.agents[i].active() && world.agents[i].archetype != "child"
            && matches!(world.agents[i].archetype.as_str(), "genteel_status_seeker" | "official" | "practitioner"))
        .max_by_key(|&i| world.agents[i].standing)
}

/// How many days after a death the parish gathers to bury them.
const FUNERAL_DELAY: i64 = 3;

// A soul carries only a handful of live memories: the store is trimmed to MEMORY_KEEP whenever
// it grows past MEMORY_CAP, keeping the most salient. The rest are let go — forgotten.
const MEMORY_CAP: usize = 12;
const MEMORY_KEEP: usize = 8;
// A memory is "charged" — flashbulb — past this absolute valence; it fades slower than a plain one.
const CHARGED: i16 = 50;
// Lifelong memory: the working store above turns over within weeks, but the DEFINING moments of a
// life — a bereavement, a wedding, the day one stood accused, a buried thing — are consolidated into
// a separate store the moment they are laid down, at full strength, and carried for the whole of the
// life. This is the autobiography a continuous self is grounded in; it does not fade, only fills.
const LIFELONG_CAP: usize = 48;        // a soul keeps up to this many defining life-memories, forever
const CONSOLIDATE_AT: i16 = 65;        // a memory this salient is worth keeping for life, whatever its kind

// A held expectation is left to stand this many days before the world is read back against it —
// long enough for gossip and event to actually move things, so the soul is measuring a real arc.
const EXPECT_AFTER: i64 = 6;
// Below this, the surprise is noise — a small confirmation, not a felt jolt; nothing is stamped.
const SURPRISE_FLOOR: i32 = 12;
// A soul holds only so many live expectations at once — the stakes that most weigh on them.
const EXPECT_CAP: usize = 6;
// A preoccupation this strong, and of a heavy kind, fills the mind — the workspace is occupied,
// and the soul cannot freely take up a new courtship or scheme while it grips them.
const GRIP: i16 = 55;

/// The parish gathers to bury one of its own — a great occasion the whole town marks together.
/// It renews grief on the kin and casts a communal pall; a murdered soul's funeral is charged,
/// the killer somewhere among the mourners, and the burying of the victim sharpens the dread.
fn hold_funeral(world: &mut World, who: usize, name: &str, murdered: bool, day: i64, date: Date, out: &mut Vec<Event>) {
    let seat = world.agents.get(who).map(|a| a.seat.clone()).unwrap_or_default();
    let text = if murdered {
        format!("The whole parish gathered to bury {name} of {seat}, murdered and now in the ground — and over the coffin every soul weighed every other, for the hand that did it stood somewhere among the mourners.")
    } else {
        format!("The parish gathered in the churchyard to see {name} of {seat} into the ground, and Thrushcombe was the quieter for the loss.")
    };
    out.push(Event { day, date: date.to_string(), kind: "funeral".into(), actor: name.into(), text });
    for k in 0..world.agents.len() {
        if !world.agents[k].active() { continue; }
        let kin = world.agents[k].spouse == Some(who) || world.agents[k].parent == Some(who) || world.agents[who].parent == Some(k);
        nudge_mood(&mut world.agents[k], if kin { -20 } else { -6 }); // a communal grief, deeper for the kin
    }
    if murdered {
        world.dread = (world.dread + 8).min(100);
    }
}

/// Hold the funerals whose day has come.
fn tend_funerals(world: &mut World, day: i64, date: Date, out: &mut Vec<Event>) {
    let due: Vec<Funeral> = world.funerals.iter().filter(|f| f.scheduled <= day).cloned().collect();
    world.funerals.retain(|f| f.scheduled > day);
    for f in due {
        hold_funeral(world, f.who, &f.name, f.murdered, day, date, out);
    }
}

/// How the parish presently holds a soul, on a 0..100 scale — the value a "standing" expectation
/// is measured against. A cleared soul stands high; suspicion erodes it sharply (the felt thing a
/// soul under a cloud is losing). Folds together their face and the shadow over them.
fn parish_hold(a: &Agent) -> i16 {
    if a.cleared { return (a.standing + 20).clamp(0, 100) as i16; }
    (a.standing - a.suspicion).clamp(0, 100) as i16
}

/// The predictive self-model. Each day a soul reads the world back against what they were sure
/// of: where it confirms their expectation, little stirs; where it betrays one held with
/// confidence, the *surprise* bites — scaling the blow to their spirits, stamping a memory the
/// harder, and teaching them (the expectation is revised toward what actually came to pass). Then
/// fresh stakes are taken up. Pure arithmetic over folded state — deterministic.
fn tend_expectations(world: &mut World, day: i64) {
    let n = world.agents.len();
    for i in 0..n {
        if !world.agents[i].active() || world.agents[i].archetype == "child" { continue; }

        // 1. resolve the expectations that have stood long enough to be measured
        let mut resolved: Vec<(i32, u8)> = Vec::new();
        let ripe: Vec<Expectation> = world.agents[i].expectations.iter()
            .filter(|e| day - e.set_on >= EXPECT_AFTER)
            .cloned().collect();
        for e in ripe {
            // what actually came to pass, on this topic's scale
            let actual: i16 = match e.topic {
                0 => { // regard: how `about` now holds them
                    match (e.about >= 0).then(|| world.agents.get(e.about as usize)).flatten() {
                        Some(o) if o.active() => world.aff(e.about as usize, i),
                        _ => { resolved.push((e.about, e.topic)); continue; } // the other is gone — the question lapses
                    }
                }
                _ => parish_hold(&world.agents[i]), // standing: how the parish holds them
            };
            let error = (actual - e.predicted) as i32;       // + better than hoped, − worse than feared
            let surprise = error.abs() * e.confidence as i32 / 100;
            if surprise >= SURPRISE_FLOOR {
                // losses loom larger than gains — a confident hope betrayed cuts deeper than a like relief
                let felt = if error < 0 { surprise * 13 / 10 } else { surprise };
                nudge_mood(&mut world.agents[i], (error.signum() * felt).clamp(-45, 45) as i16);
                // surprise stamps the memory: the bigger the shock, the deeper it sets
                let sal = (surprise + 25).clamp(0, 100) as i16;
                let (kind, who, val): (&str, i32, i16) = match (e.topic, error < 0) {
                    (0, true)  => ("betrayed", e.about, -(surprise.min(90) as i16)),   // someone they trusted turned cold
                    (0, false) => ("reprieve", e.about,  (surprise.min(80) as i16)),    // warmth they'd given up on
                    (_, true)  => ("wronged", -1, -(surprise.min(95) as i16)),          // the parish turning on them, against all they expected
                    (_, false) => ("vindicated", -1, (surprise.min(85) as i16)),        // believed, after they feared the worst
                };
                world.remember(i, kind, who, val, sal, day);
            }
            resolved.push((e.about, e.topic));
            // learn: the expectation is revised toward what came to pass, and held a touch less surely
            if let Some(slot) = world.agents[i].expectations.iter_mut().find(|x| x.about == e.about && x.topic == e.topic) {
                slot.predicted = actual;
                slot.confidence = (e.confidence - (surprise as i16 / 3)).clamp(20, 95);
                slot.set_on = day;
            }
        }

        // 2. take up fresh stakes the soul has none on yet
        // (a) regard — their single strongest tie, friend or rival: they expect it to hold as it is
        let strongest: Option<(usize, i16)> = world.ties(i, true, 1).into_iter()
            .chain(world.ties(i, false, 1))
            .max_by_key(|&(_, v)| v.abs());
        if let Some((other, _)) = strongest {
            if !world.agents[i].expectations.iter().any(|e| e.topic == 0 && e.about == other as i32) {
                let pred = world.aff(other, i);
                let conf = (pred.abs() + 20).clamp(0, 90);
                world.agents[i].expectations.push(Expectation { about: other as i32, topic: 0, predicted: pred, confidence: conf, set_on: day });
            }
        }
        // (b) standing — a soul under an open cloud, not yet cleared or named, expects to come
        //     through it: they are sure the parish will see they are no murderer. As suspicion
        //     mounts the world falls ever further below that hope — the felt injustice, emergent.
        let under_cloud = world.inquest.as_ref().is_some_and(|q| !q.closed)
            && world.agents[i].suspicion > 0 && !world.agents[i].cleared
            && world.inquest.as_ref().is_some_and(|q| q.accused != i as i32);
        if under_cloud && !world.agents[i].expectations.iter().any(|e| e.topic == 1) {
            // they expect to stand roughly as they did before the shadow — innocence vindicated
            let pred = (world.agents[i].standing + 5).clamp(0, 100) as i16;
            world.agents[i].expectations.push(Expectation { about: -1, topic: 1, predicted: pred, confidence: 55, set_on: day });
        }

        // keep only the stakes that weigh most (highest confidence), and drop any whose subject died
        let mut exps = std::mem::take(&mut world.agents[i].expectations);
        exps.retain(|e| e.about < 0 || world.agents.get(e.about as usize).is_some_and(|a| a.active()));
        world.agents[i].expectations = exps;
        let _ = resolved;
        if world.agents[i].expectations.len() > EXPECT_CAP {
            world.agents[i].expectations.sort_by_key(|e| std::cmp::Reverse(e.confidence));
            world.agents[i].expectations.truncate(EXPECT_CAP);
        }
    }
}

/// The global workspace, resolved for one soul: their many concerns contend, and the single
/// strongest is broadcast as their preoccupation — what is uppermost in their mind. The murder's
/// dread, a fresh grief, a buried haunt, the wound of a betrayal, a courtship, a feud, a scheme:
/// each presses with a weight, and the winner takes the mind. A settled soul's mind rests on the
/// day's ordinary work. Pure derived state — recomputed each day from all the rest.
fn compute_focus(world: &mut World, i: usize) {
    let a = &world.agents[i];
    // baseline: an unburdened mind is on the day's work
    let mut cands: Vec<(&str, i32, i32)> = vec![("work", -1, 18)];
    // the killing — heaviest when the eye is turning toward them
    if let Some(q) = &world.inquest {
        if !q.closed && !a.cleared && i != q.victim {
            let w = a.suspicion + world.dread as i32 / 2 + (-a.seen_as as i32).max(0) / 2;
            if w > 8 { cands.push(("dread", -1, w)); }
        }
    }
    // the occasions that grip them, each pressing as its own kind of concern
    for m in &a.memories {
        let topic = match m.kind.as_str() {
            "haunt" => "haunt", "grief" => "grief", "betrayed" => "betrayal",
            "wronged" => "wrong", "accused" => "dread", _ => continue,
        };
        cands.push((topic, m.who, m.salience as i32));
    }
    // the live pursuits — a suit, a grudge campaign, a staked scheme
    if a.courting >= 0 { cands.push(("courtship", a.courting, 40 + a.courtship as i32)); }
    if a.rival >= 0 { cands.push(("feud", a.rival, 30 + a.feud as i32 * 2)); }
    if a.intent != 0 { cands.push(("venture", -1, 25 + a.intent_age as i32 / 2)); }
    let (topic, target, w) = cands.into_iter().max_by_key(|c| c.2).unwrap();
    world.agents[i].focus = Preoccupation { topic: topic.into(), target, intensity: w.clamp(0, 100) as i16 };
}

/// Is the soul's mind so taken up by a heavy concern that they cannot freely turn to a new
/// courtship or scheme? The workspace, occupied — the gate the global focus puts on initiative.
fn mind_occupied(a: &Agent) -> bool {
    a.focus.intensity >= GRIP && matches!(a.focus.topic.as_str(), "dread" | "grief" | "haunt" | "betrayal" | "wrong")
}

/// What is uppermost in a soul's mind, in words — for the dossier they contemplate and the
/// dashboard. None when their mind is easy, resting on the day's ordinary work.
fn focus_phrase(w: &World, idx: usize) -> Option<String> {
    let f = &w.agents[idx].focus;
    let who = (f.target >= 0).then(|| w.agents.get(f.target as usize)).flatten().map(|a| a.name.as_str()).unwrap_or("");
    Some(match f.topic.as_str() {
        "dread"     => "the killing, and the parish's eye turning toward them — it crowds out all else".to_string(),
        "grief"     => if who.is_empty() { "their grief, sitting over everything".to_string() } else { format!("their grief — the loss of {who} — which sits over everything") },
        "haunt"     => "a dread they can put no name to nor reach the bottom of, that will not leave them be".to_string(),
        "betrayal"  => format!("the wound of {who}'s coldness, turned over and over"),
        "wrong"     => "the injustice of the parish's suspicion, which they cannot make answer to".to_string(),
        "courtship" => format!("their hopes of {who}"),
        "feud"      => format!("their reckoning with {who}"),
        "venture"   => "the scheme they have staked themselves on".to_string(),
        _ => return None,
    })
}

/// The recursive social mirror. Each soul holds a read of how the parish regards them — not their
/// real standing, but what they *believe* others make of them — and it lags and distorts the truth
/// by their disposition: the thin-skinned over-read a slight and discount good regard (the anxious
/// self), the thick-skinned barely register either. Feeling ill-thought-of sinks the spirits.
/// This is "what I think they think of me," carried as state — and (in tend_inquest) turned back
/// on the world, where a soul who believes themselves suspected acts the part and draws the eye.
fn update_self_regard(world: &mut World, _day: i64) {
    let n = world.agents.len();
    let adults: Vec<usize> = (0..n).filter(|&j| world.agents[j].active() && world.agents[j].archetype != "child").collect();
    for &i in &adults {
        // how the parish in truth bears them: their face, the mean of what others feel toward them,
        // and — heaviest of all under a killing — the suspicion that has settled on them
        let mut sum = 0i32; let mut cnt = 0i32;
        for &j in &adults { if j != i { sum += world.aff(j, i) as i32; cnt += 1; } }
        let mean_aff = if cnt > 0 { sum / cnt } else { 0 };
        let true_regard = ((world.agents[i].standing - 50) + mean_aff / 2 - world.agents[i].suspicion.min(120)).clamp(-100, 100) as i16;
        // move toward the truth, but slowly and crookedly. Bad news travels fast for the sensitive
        // and slow for the stoic; good news the other way — so the anxious live worse-regarded than
        // they are, and never quite trust the warmth they're shown.
        let sens = temperament(&world.agents[i].archetype).0 as i32; // ~65 stoic .. ~140 raw
        let gap = (true_regard - world.agents[i].seen_as) as i32;
        let toward = if gap < 0 { gap * sens / 100 } else { gap * 100 / sens };
        let step = (toward / 4).clamp(-12, 12) as i16;
        world.agents[i].seen_as = (world.agents[i].seen_as + step).clamp(-100, 100);
        // the felt weight of being judged: believing oneself ill-thought-of is its own slow ache;
        // believing oneself well-regarded is a quiet warmth. A daily drip, not a shock.
        let sa = world.agents[i].seen_as;
        if sa <= -30 { nudge_mood(&mut world.agents[i], (sa / 22).clamp(-4, -1)); }
        else if sa >= 45 { nudge_mood(&mut world.agents[i], (sa / 45).clamp(1, 2)); }
    }
}

/// Endogenous aims — souls taking up intentions nobody handed them, out of their own disposition
/// and what they carry. Not providence, not an LLM's prompt: the deterministic fold itself lets a
/// soul *initiate* — turn a remembered wound into a declared enmity, or set themselves a bold
/// venture because that is the cut of them. Each is then pursued across days by the machinery that
/// already presses feuds and plans toward a reckoning. Initiative, sprung from within.
fn form_aims(world: &mut World, day: i64, date: Date, seed: u64) -> Vec<Event> {
    let mut out = Vec::new();
    let mk = |actor: &str, text: String| Event { day, date: date.to_string(), kind: "intent".into(), actor: actor.into(), text };
    let mut rng = rng_for(seed ^ 0xA14E_0000_0000, day);
    let n = world.agents.len();
    for i in 0..n {
        if !world.agents[i].active() || world.agents[i].archetype == "child" { continue; }
        // the workspace, occupied: a mind wholly taken up by grief or dread cannot turn to take up
        // a fresh enmity or scheme — there is no room in it. Initiative waits on an easier mind.
        if mind_occupied(&world.agents[i]) { continue; }

        // (1) a carried grievance hardens into a declared enmity — memory becoming initiative. A
        //     soul with a real wound (a slight, a betrayal) still gripping, and no nemesis yet, may
        //     resolve to make an enemy of the one who dealt it, and press it toward satisfaction.
        if world.agents[i].rival < 0 {
            let grudge = world.agents[i].memories.iter()
                .filter(|m| matches!(m.kind.as_str(), "snub" | "betrayed") && m.who >= 0 && m.salience >= 45)
                .filter(|m| {
                    let w = m.who as usize;
                    // you do not take up arms against your own hearth — spouse and blood are spared
                    Some(w) != world.agents[i].spouse
                        && world.agents[i].parent != Some(w)
                        && world.agents[w].parent != Some(i)
                        && world.agents.get(w).is_some_and(|a| a.active() && a.archetype != "child")
                })
                .max_by_key(|m| m.salience)
                .map(|m| m.who as usize);
            if let Some(foe) = grudge {
                // the dispositionally proud and mercurial nurse enmity sooner; the placid let it lie
                let bent = match world.agents[i].archetype.as_str() {
                    "scheming_improver" | "genteel_status_seeker" => 0.11,
                    "official" => 0.04,
                    _ => 0.06,
                };
                if rng.gen_bool(bent) {
                    world.agents[i].rival = foe as i32;
                    world.agents[i].feud = 0;
                    world.agents[i].goal = 4;
                    world.agents[i].goal_target = foe as i32;
                    let (a, b) = (world.agents[i].name.clone(), world.agents[foe].name.clone());
                    out.push(mk(&a, format!("{a}, brooding on an old wrong, has come to count {b} an enemy, and means to have satisfaction of it.")));
                    world.spawn_news(&a, &format!("how {a} has set themselves against {b}"), -1, day, &[b.as_str()]);
                    world.agents[i].memories.retain(|m| !(matches!(m.kind.as_str(), "snub" | "betrayed") && m.who == foe as i32));
                    continue; // one aim taken up this day is enough
                }
            }
        }

        // (2) a bold venture set from sheer disposition — the improver and the striving farmer, with
        //     means enough to stake and spirits to match, set themselves a scheme that may make them
        //     or ruin them (Crale and his field). No one bid them; it is the cut of them.
        if world.agents[i].intent == 0 && world.agents[i].rival < 0 {
            let a = &world.agents[i];
            let venturer = matches!(a.archetype.as_str(), "scheming_improver" | "hill_farmer")
                && (20..120).contains(&a.purse) && a.mood >= -25; // the schemer schemes even out of sorts
            if venturer && rng.gen_bool(if a.archetype == "scheming_improver" { 0.09 } else { 0.045 }) {
                world.agents[i].intent = 3;
                world.agents[i].intent_goal = world.agents[i].purse + 70;
                world.agents[i].intent_age = 0;
                world.agents[i].goal = 5;
                let nm = world.agents[i].name.clone();
                out.push(mk(&nm, format!("{nm} has set themselves a bold scheme of their own devising, to make their fortune or break upon it.")));
                world.spawn_news(&nm, &format!("the bold scheme {nm} has lately set themselves"), 1, day, &[]);
            }
        }
    }
    out
}

/// What the shadow of an open (or freshly-closed) killing means for one soul, in words the
/// oracle can carry into a reflection or a conversation — so the dread is *felt*, not just
/// tracked. None when the town is at peace.
fn inquest_brief(w: &World, idx: usize) -> Option<String> {
    let inq = w.inquest.as_ref()?;
    let vn = &inq.victim_name;
    if inq.closed {
        if w.dread <= 0 { return None; }
        let who = (inq.accused >= 0).then(|| w.agents.get(inq.accused as usize).map(|a| a.name.clone())).flatten();
        return Some(match who {
            Some(name) => format!("The town is still raw from the hanging of {name} for the murder of {vn}. No one is quite sure the right neck was stretched, and the unease has not lifted."),
            None => format!("The town is still raw from the murder of {vn}, and the unease has not lifted."),
        });
    }
    // an open, unsolved killing — the dominant fact of every soul's days
    let mut s = format!(
        "A killing hangs over Thrushcombe: {vn} was murdered, and the killer — one of the town's own — has not been found. Fear walks the lanes; doors are barred early; every soul weighs every other, and no one feels safe."
    );
    let most = (0..w.agents.len())
        .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child" && i != inq.victim)
        .max_by_key(|&i| w.agents[i].suspicion);
    if inq.accused == idx as i32 {
        s.push_str(" And it is THEY the parish has fixed upon: they stand accused of the murder, and the talk is openly of hanging. They protest, but fear does not listen.");
    } else if w.agents[idx].suspicion >= 40 {
        s.push_str(" And the whispers have begun to fall on THEM — they have caught the sliding eyes, the silences; some in the parish wonder if it was their hand.");
    } else if let Some(m) = most {
        if w.agents[m].suspicion >= 40 && m != idx {
            s.push_str(&format!(" The town's suspicion is settling on {} — though whether justly, who can say.", w.agents[m].name));
        }
    }
    // the official inquiry, and whose side of the class line the soul sits on
    if inq.investigator >= 0 && inq.investigator != idx as i32 {
        if let Some(inv) = w.agents.get(inq.investigator as usize) {
            let inv_genteel = matches!(inv.archetype.as_str(), "genteel_status_seeker" | "official");
            let respectable = matches!(w.agents[idx].archetype.as_str(), "genteel_status_seeker" | "official" | "practitioner");
            s.push_str(&format!(" {} sits as magistrate over the inquiry.", inv.name));
            if inv_genteel && !respectable {
                s.push_str(" And it is plain his questions fall on the working folk and the strangers, on people like them — never on his own kind. There is no justice in it for the likes of them, only the need of a name to hang.");
            } else if inv_genteel && respectable {
                s.push_str(" And being of his own rank, they know the inquiry will not trouble people like them, whatever the truth of it.");
            }
        }
    } else if inq.investigator == idx as i32 {
        // they ARE the magistrate: the unsolved killing is theirs to answer for, and it is NOT closed
        // while the killer walks free — whatever ruling they have made, a murderer is still at large.
        s.push_str(" And it is THEY who sit as magistrate over it — the parish looks to them for an answer they have not got. Whatever they have done on the bench, the matter is not closed while the killer walks free among the very souls they must face each day: a murderer is still at large in Thrushcombe, unnamed and unpunished, and that weighs on them however they carry it.");
    }
    if inq.public_inquiry {
        s.push_str(" Every soul is being questioned now, and the statements read out in the open — so the whole parish weighs each neighbour's account, and who named whom.");
    }
    if w.agents[idx].cleared {
        s.push_str(" They themselves have given their account, and it held — the magistrate let them go; the eyes have slid off them, for now, and there is some relief in that, however uneasy.");
    }
    Some(s)
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

/// The body, day by day. A soul is not a disembodied mind: the day's labour drains their vigour
/// (the hard trades and the hay/harvest seasons hardest, the old soonest), a night's rest and the
/// Sabbath restore it, and health drifts with age and the odd ailment. And the flesh tells on the
/// spirits — a soul worked to the bone or ill with the damp sinks; a rested, well one is eased.
/// This is what gives the inner life a body under it. Deterministic, on its own RNG stream.
fn tend_body(world: &mut World, day: i64, date: Date, seed: u64) {
    let mut rng = rng_for(seed ^ 0xB0D4_0000_0000, day);
    let season = Season::of(date);
    let sunday = date.weekday() == Weekday::Sunday;
    for a in world.agents.iter_mut() {
        if !a.active() { continue; }
        let age = a.age(day);
        // the day's exertion, by trade — the working folk spend themselves hardest
        let labour: i16 = match a.archetype.as_str() {
            "blunt_hand" | "hill_farmer" => 18,
            "scheming_improver" | "practitioner" => 12,
            "official" => 8,
            "genteel_status_seeker" => 5,
            "child" => 8,
            _ => 12,
        };
        // the hay and the harvest break the backs of the working folk
        let season_toll: i16 = if matches!(season, Season::Hay | Season::Harvest)
            && matches!(a.archetype.as_str(), "blunt_hand" | "hill_farmer") { 8 } else { 0 };
        let age_toll: i16 = if age >= 60 { 6 } else if age >= 45 { 3 } else { 0 };
        let rest: i16 = if sunday { 32 } else { 23 }; // a night's sleep, and the day of rest the more
        a.vigour = (a.vigour - labour - season_toll - age_toll + rest).clamp(0, 100);

        // health: the aged carry a lower ceiling, and an ailment comes now and then (a chill, a
        // fever going round, the damp in old bones); otherwise the body mends itself, slowly.
        let ceiling: i16 = if age >= 65 { 70 } else if age >= 50 { 84 } else { 100 };
        if a.health > ceiling {
            a.health -= 1;
        } else if rng.gen_bool(if age >= 55 { 0.020 } else { 0.012 }) {
            a.health = (a.health - rng.gen_range(8..22)).max(0); // an ailment takes them
        } else if a.health < ceiling {
            a.health = (a.health + 2).min(ceiling); // mending
        }

        // the flesh tells on the spirits — centred so an ordinary day is net-neutral, the spent and
        // the ill sink, the rested and well are eased a little
        let body_mood = ((a.vigour as i32 - 72) + (a.health as i32 - 82)) / 24;
        if body_mood != 0 { nudge_mood(a, body_mood as i16); }
    }
}

/// How a soul's body feels to them tonight — for their inner life to reason from. Embodiment in words.
fn body_phrase(a: &Agent, day: i64) -> String {
    let age = a.age(day);
    let mut s = if a.vigour <= 22 { "worn to the bone, every limb heavy, fit only to fall into bed" }
        else if a.vigour <= 42 { "tired, the day's work still aching in you" }
        else if a.vigour >= 82 { "rested and easy in your body, your strength your own" }
        else { "neither fresh nor spent — an ordinary tiredness" }.to_string();
    if a.health <= 38 {
        s.push_str("; and you are unwell — ill, or ailing badly, and it drags at everything");
    } else if a.health <= 64 {
        s.push_str(if age >= 55 { "; and your old complaints nag at you, the damp deep in your bones" }
                   else { "; and you are a little under the weather, not your best self" });
    }
    s
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

    // --- the body: the day's labour and the season tell on the flesh, and the flesh on the spirits ---
    tend_body(world, day, date, seed);

    // --- memory fades ---
    // Every soul lets the day's edge come off what they carry. A plain memory loses a point a
    // day; a charged one (a real grief, a real terror) holds far longer — flashbulb. What falls
    // to nothing is forgotten and dropped. A repressed engram (a thing the soul cannot face) does
    // not fade on its own — it sits, and surfaces unbidden; only a reckoning lets it go.
    for i in 0..n {
        if !world.agents[i].active() { continue; }
        let mut haunted = 0i16;
        for m in world.agents[i].memories.iter_mut() {
            if m.kind == "haunt" { haunted = haunted.max(m.salience); continue; } // the buried thing does not fade with time
            let wear = if m.valence.abs() >= CHARGED { 1 } else { 2 };
            m.salience = (m.salience - wear).max(0);
        }
        world.agents[i].memories.retain(|m| m.salience > 0);
        // a repressed engram surfaces unbidden: every few days, with no occasion the soul can
        // name, a dread rises and the spirits dip — the floating, drunk-without-drink unease a
        // watchful neighbour might mark. It is not reasoned, not triggered; it simply comes.
        if haunted > 0 {
            let mut hr = rng_for(seed ^ 0x4855_4E54_0000, day ^ (i as i64));
            if hr.gen_bool((haunted as f64 / 320.0).clamp(0.0, 0.4)) {
                nudge_mood(&mut world.agents[i], -(haunted / 6).max(6));
            }
        }
    }

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
        world.funerals.push(Funeral { who: i, name: name.clone(), scheduled: day + FUNERAL_DELAY, murdered: false });
        // grief falls on the kin — and is *carried*: a charged engram that fades slowly and
        // dampens their recovery for weeks, so a bereavement isn't shaken off by Sunday.
        for k in 0..world.agents.len() {
            if world.agents[k].active() && (world.agents[k].spouse == Some(i) || world.agents[k].parent == Some(i) || world.agents[i].parent == Some(k)) {
                nudge_mood(&mut world.agents[k], -35);
                world.remember(k, "grief", i as i32, -80, 85, day);
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
        // A ripe, mutual courtship is no longer joined by the fold of its own accord: when it has come
        // far enough (BETROTH_AT, mutual warmth) the COURTED soul is put to the proposal and answers
        // accept|refuse (pending_betrothal → a `betroth` decree). The joining of two lives turns on the
        // answer, not a counter. The fold here only lets a suit that is never returned wither away.
        let mutual = world.aff(i, tj) >= 30 && world.aff(tj, i) >= 28;
        if world.agents[i].courtship >= 75 && !mutual {
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
        // a mind filled with grief or the murder's dread does not turn to begin a courtship
        if mind_occupied(&world.agents[i]) {
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

    // --- the inquest, pressed: an open murder hunts itself toward a hanging, day by day ---
    tend_inquest(world, day, date, &mut rng, &mut out);

    // --- the funerals whose day has come: the parish gathers to bury its dead ---
    tend_funerals(world, day, date, &mut out);

    // --- the predictive self-model: souls read the world back against what they were sure of,
    //     and the surprise of being wrong scales the blow, stamps the memory, and teaches them.
    //     Run after the inquest, so a soul under a mounting cloud feels each day's betrayal of
    //     their hope to be cleared. ---
    tend_expectations(world, day);

    // --- the recursive social mirror: each soul updates their read of how the parish regards
    //     them (lagging, distorted by disposition), and feels the weight of being judged. ---
    update_self_regard(world, day);

    // --- endogenous aims: souls take up intentions of their own — a carried wound hardened into
    //     enmity, a bold venture set from disposition — then pursued by the feud/plan machinery. ---
    out.extend(form_aims(world, day, date, seed));

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

    // --- the global workspace, resolved: with the day's grief, dread, hopes and schemes all
    //     settled, each soul's concerns contend and a single one is broadcast as uppermost. This
    //     is computed last, on the freshest state, and stands until tomorrow — gating what they
    //     take up next, colouring what their reflection turns on, shown as what fills their mind. ---
    for i in 0..world.agents.len() {
        if world.agents[i].active() && world.agents[i].archetype != "child" {
            compute_focus(world, i);
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
        // A wound with a *particular occasion* behind it does not fade — it stays raw. Where a
        // soul carries a live grievance against another, the cool feeling is held there (and a
        // sharp one kept sharp), so a remembered snub outlasts the general softening of time.
        let m = world.agents.len();
        for i in 0..m {
            if !world.agents[i].active() { continue; }
            for j in 0..m {
                if i == j || !world.agents[j].active() { continue; }
                let g = world.grievance(i, j);
                if g >= 40 && world.aff(i, j) > -25 {
                    world.nudge_aff(i, j, -3); // the memory keeps the breach open
                }
            }
        }
        for i in 0..world.agents.len() {
            if !world.agents[i].active() {
                continue;
            }
            let base = temperament(&world.agents[i].archetype).1; // spirits drift toward their baseline
            // ...but a soul carrying fresh grief or terror does not bounce back on schedule. The
            // more an unhealed wound still grips, the more it holds the spirits down — so a
            // bereavement or an accusation is *carried*, not shaken off by the next Sunday.
            let weight: i16 = world.agents[i].memories.iter()
                .filter(|m| m.valence <= -CHARGED)
                .map(|m| m.salience)
                .max()
                .unwrap_or(0);
            // even the heaviest wound lets the spirits creep back by a hair each day — grief is
            // carried, but never pins a soul to the floor forever; the parish breathes again in time.
            let recovery = if weight >= 35 { 1 } else { 2 };
            world.agents[i].mood += (base - world.agents[i].mood).signum() * recovery;
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
        // the one cut carries it as a particular wound — a remembered occasion that keeps the
        // grudge from quietly fading the way an unbacked coolness would
        world.remember(t, "snub", f as i32, -55, 60, day);
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

    // how abuzz the town is today — when the parish is assembled or afraid, word flies. Market
    // day and church Sunday throw everyone together; a killing has every tongue going at once.
    let buzz = {
        let mut b = 1.0_f64;
        match date.weekday() {
            Weekday::Wednesday => b += 1.2, // the market — the whole town in the square
            Weekday::Sunday => b += 1.0,    // church — the whole parish gathered
            _ => {}
        }
        if world.dread > 30 { b += 1.0; } // fear is the fastest courier of all
        b
    };

    for item in news.iter_mut() {
        let age = day - item.born;
        let living_knowers = item.knowers.iter().filter(|&&k| alive[k]).count();
        if age < 1 || age > 21 || living_knowers >= living {
            continue; // not yet (delay), stale, or every living soul already knows
        }
        let decay = (1.0 - age as f64 / 30.0).max(0.0);
        let juice = (1.0 + 0.15 * item.valence.unsigned_abs() as f64) * buzz;

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

/// One beat for the Discord feed: a line plus where its speaker stands and who they are, so the
/// feed can post it to the right place-channel under the townsperson's own face. `voice` tells the
/// feed how to render it — `narration` (a happening, observed), `thought` (private reflection), or
/// `speech` (words said aloud). `src`+`id` is the per-table cursor the feed advances.
#[derive(Serialize)]
pub struct DiscordBeat {
    pub src: String,    // "e" events · "t" reflections · "d" dialogues
    pub id: i64,
    pub voice: String,  // narration | thought | speech
    pub actor: String,
    pub idx: i32,
    pub seat: String,
    pub sex: i32,
    pub kind: String,
    pub text: String,
}

/// Where a soul is this phase — for the Discord channel-topic "who's here now".
#[derive(Serialize)]
pub struct PresenceRow {
    pub name: String,
    pub idx: i32,
    pub location: String,   // where they are this phase (may be a roving activity)
    pub seat: String,       // their home/workplace
    pub doing: String,
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
    /// When a killing hangs over the town: the banner of dread and where suspicion falls.
    pub fear: Option<String>,
}

/// Event kinds worth rendering in voice. Pure flavour (market, vet rounds) keeps its
/// template line; the salient beats get the oracle.
pub const SALIENT: &[&str] = &[
    "calving", "party", "windfall", "scheme", "bureaucracy", "weather", "status", "household", "gossip",
    "death", "succession", "marriage", "birth", "comingofage", "feud", "bond", "providence", "triumph", "courtship", "decree", "show", "rivalry", "talk", "intent",
    "murder", "inquest", "funeral",
];

/// Bump when World layout or step_day logic changes — older snapshots are then ignored
/// and the world re-folds from genesis (and writes fresh checkpoints).
const SNAPSHOT_VERSION: i64 = 42;
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
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL, subject TEXT NOT NULL, thought TEXT NOT NULL);
         -- A soul's biography: the life the parish would tell of them. Flavour, recorded once
         -- (never folded); injected into talk and reflection so every soul knows the others' stories.
         CREATE TABLE IF NOT EXISTS biographies(name TEXT PRIMARY KEY, text TEXT NOT NULL);
         -- A bespoke voice for a special soul: a full character prompt that replaces the generic
         -- villager scaffolding when they speak (Aldric Fynch and the like). Optional, per name.
         CREATE TABLE IF NOT EXISTS personas(name TEXT PRIMARY KEY, prompt TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS testimony(
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL, subject TEXT NOT NULL,
            alibi TEXT NOT NULL, accuses TEXT NOT NULL, public INTEGER NOT NULL, text TEXT NOT NULL);
         -- The evolving life-model a soul reasons over: their self-concept (about=''), and their
         -- belief about each other soul they know (about=name) — a tracked, updating theory of mind.
         -- Flavour, recorded by the `introspect` job (never folded); injected into reflection and talk.
         CREATE TABLE IF NOT EXISTS psyche(
            id INTEGER PRIMARY KEY, day INTEGER NOT NULL, subject TEXT NOT NULL,
            about TEXT NOT NULL, text TEXT NOT NULL);",
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
        // a turning-point verdict is a public beat; a private conversation, a private thought, or a
        // statement to the magistrate (surfaced on its own page + as gossip) is not a chronicle beat
        if d.kind != "dialogue" && d.kind != "reflect" && d.kind != "testimony" && d.kind != "psyche" && d.kind != "judgment" && d.kind != "townhall" {
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
                let revise = parts.next().unwrap_or("keep");
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
                            // pay court — begin a suit, but only a MATCH that could really be one: both
                            // free, of opposite sex, of marrying age, and within a generation of each
                            // other. (No 70-year-old widow resolving to court a lad of nineteen.)
                            "court" => {
                                let (a, b) = (&world.agents[si], &world.agents[t]);
                                let plausible = (18..=55).contains(&a.age(day))
                                    && (18..=55).contains(&b.age(day))
                                    && (a.age(day) - b.age(day)).abs() <= 16;
                                if a.courting < 0 && a.spouse.is_none() && b.spouse.is_none()
                                    && a.sex != b.sex && a.archetype != "child" && b.archetype != "child"
                                    && plausible
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
                // a dated plan the soul set itself this hour — one at a time. With no plan running,
                // this hour may take one up. With a plan already in train, the hour may instead
                // *revise* it: a plan is a living commitment a soul revisits as the world pushes back,
                // not a thing fixed once and merely waited out.
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
                } else {
                    match revise {
                        // they think better of it and set the plan down — a quiet defeat, or a wiser peace
                        "abandon" => {
                            world.agents[si].intent = 0;
                            world.agents[si].intent_age = 0;
                            world.agents[si].intent_goal = 0;
                            nudge_mood(&mut world.agents[si], -10);
                            let (g, t) = assess_goal(world, si, day);
                            world.agents[si].goal = g;
                            world.agents[si].goal_target = t;
                            out.push(Event { day, date: date.to_string(), kind: "intent".into(), actor: world.agents[si].name.clone(),
                                text: format!("{} has given up the plan they had set themselves, thinking better of it.", world.agents[si].name) });
                        }
                        // they renew the resolve and raise their sights — the horizon resets, the aim hardens
                        "harder" => {
                            world.agents[si].intent_age = 0; // a renewed commitment buys fresh weeks
                            let bump = match world.agents[si].intent { 2 => 4, _ => 25 };
                            world.agents[si].intent_goal += bump;
                            nudge_mood(&mut world.agents[si], 6);
                        }
                        _ => {} // keep — the plan stands, the weeks of pursuit go on
                    }
                }
            }
            // a statement given to the magistrate. choice is "alibi:visibility" (alibi ∈
            // none|weak|strong, vis ∈ pub|priv); target (if any) = the soul they cast blame on.
            // an alibi moves the witness's own suspicion; an accusation moves the accused's; what
            // is read out in the open becomes gossip the whole town turns over.
            ("testimony", c) => {
                if world.inquest.as_ref().is_some_and(|q| !q.closed) {
                    let mut parts = c.split(':');
                    let alibi = parts.next().unwrap_or("none");
                    let public = parts.next() == Some("pub");
                    let nm = world.agents[si].name.clone();
                    match alibi {
                        "strong" => {
                            world.agents[si].suspicion = (world.agents[si].suspicion - 45).max(0);
                            world.agents[si].cleared = true; // a solid alibi puts them beyond it
                            world.remember(si, "cleared", -1, 65, 70, day); // the relief of being believed
                            if public { world.spawn_news_open(&nm, &format!("how {nm}'s alibi cleared them before the magistrate"), 1, day); }
                        }
                        "weak" => {
                            world.agents[si].suspicion = (world.agents[si].suspicion + 12).min(200);
                            if public { world.spawn_news_open(&nm, &format!("how thin {nm}'s account to the magistrate sounded"), -2, day); }
                        }
                        _ => {
                            world.agents[si].suspicion = (world.agents[si].suspicion + 22).min(200);
                            if public { world.spawn_news_open(&nm, &format!("how {nm} could give no account of themselves to the magistrate"), -3, day); }
                        }
                    }
                    // casting blame: the named soul takes fresh suspicion, and bad blood opens both ways
                    if let Some(t) = ti {
                        if t != si && world.agents[t].active() && !world.agents[t].cleared {
                            world.agents[t].suspicion = (world.agents[t].suspicion + 18).min(200);
                            world.nudge_aff(si, t, -10);
                            world.nudge_aff(t, si, -12);
                            let tn = world.agents[t].name.clone();
                            if public { world.spawn_news_open(&tn, &format!("how {nm} named {tn} to the magistrate"), -3, day); }
                        }
                    }
                }
            }
            // the magistrate's ruling on the lead suspect, authored by the oracle and folded here.
            // subject = the magistrate; target = the suspect; choice ∈ accuse | hold | widen. The
            // *decision* is the oracle's; the consequence is fixed here, so the world stays exact.
            // d.text is the magistrate's recorded reasoning — read out as the chronicle beat.
            ("judgment", choice) => {
                if let Some(mut inq) = world.inquest.clone().filter(|q| !q.closed && q.accused < 0) {
                    let mag = world.agents[si].name.clone();
                    let ruling = d.text.trim().to_string(); // the magistrate's recorded reasoning, if any
                    let say = |text: String| Event { day, date: date.to_string(), kind: "inquest".into(), actor: mag.clone(), text };
                    match choice {
                        // he commits: the parish has its named soul, and the trial clock begins
                        "accuse" => if let Some(t) = ti.filter(|&t| world.agents[t].active() && t != si) {
                            inq.accused = t as i32;
                            inq.accused_since = day;
                            let tn = world.agents[t].name.clone();
                            world.agents[t].standing = (world.agents[t].standing - 10).max(0);
                            nudge_mood(&mut world.agents[t], -40);
                            world.remember(t, "accused", inq.victim as i32, -95, 100, day); // charged — grips for weeks
                            out.push(say(if ruling.is_empty() { format!("{mag} has brought {tn} to formal accusation for the murder of {}, and means to see them answer for it.", inq.victim_name) } else { ruling }));
                            world.spawn_news(&tn, &format!("that {mag} has named {tn} for {}'s murder", inq.victim_name), -4, day, &[]);
                            world.dread = (world.dread + 8).min(100);
                        },
                        // he stays his hand: not proof enough, and he will not be hurried to a charge
                        "hold" => {
                            inq.held_until = day + HOLD_DAYS;
                            let tn = ti.map(|t| world.agents[t].name.clone()).unwrap_or_else(|| "the suspect".into());
                            if let Some(t) = ti.filter(|&t| world.agents[t].active()) {
                                nudge_mood(&mut world.agents[t], 8); // a reprieve, however uneasy
                                world.remember(t, "reprieve", si as i32, 45, 55, day);
                            }
                            out.push(say(if ruling.is_empty() { format!("{mag} stayed his hand: there is not proof enough to charge {tn}, and he will not be hurried to it while the killer may yet be another.") } else { ruling }));
                            world.dread = (world.dread + 3).min(100); // the town, unsatisfied, frets on
                        }
                        // he refuses the fixation: widens the net, and turns the scrutiny off the one soul
                        "widen" => {
                            inq.held_until = day + HOLD_DAYS;
                            inq.public_inquiry = true;
                            if let Some(t) = ti.filter(|&t| world.agents[t].active()) {
                                world.agents[t].suspicion = (world.agents[t].suspicion - 20).max(0);
                                nudge_mood(&mut world.agents[t], 6);
                            }
                            out.push(say(if ruling.is_empty() { format!("{mag} would not fix on one soul on so little: he has widened the inquiry, and means to question the whole parish anew.") } else { ruling }));
                            world.spawn_news_open(&mag, &format!("how {mag} refused to be rushed and widened the inquiry into {}'s murder", inq.victim_name), 0, day);
                        }
                        _ => {}
                    }
                    world.inquest = Some(inq);
                }
            }
            // an emergency assembly over the open murder: the magistrate gives his account and the
            // parish voices its fear. The oracle decides how the room turns — calmer, inflamed toward a
            // scapegoat, or split — and the consequence is fixed here, town-wide. The account (d.text)
            // is the public beat, emitted below; it is also kept in full for the inquiry page.
            ("townhall", outcome) => {
                let n = world.agents.len();
                match outcome {
                    // the magistrate steadies them: the dread breaks, and the eye eases off the hunted
                    "calmed" => {
                        world.dread = (world.dread - 25).max(0);
                        for i in 0..n {
                            if !world.agents[i].active() { continue; }
                            nudge_mood(&mut world.agents[i], 6);
                            if world.agents[i].suspicion >= 40 { world.agents[i].suspicion = (world.agents[i].suspicion - 25).max(0); }
                        }
                        if let Some(q) = world.inquest.as_mut().filter(|q| !q.closed) { q.held_until = q.held_until.max(day + 5); }
                    }
                    // the room turns ugly: the fear deepens, the hunted are pressed harder, and the
                    // mob will not wait — the magistrate's hand is forced, his cooling broken
                    "inflamed" => {
                        world.dread = (world.dread + 20).min(100);
                        for i in 0..n {
                            if !world.agents[i].active() { continue; }
                            nudge_mood(&mut world.agents[i], -6);
                            if world.agents[i].suspicion >= 30 { world.agents[i].suspicion = (world.agents[i].suspicion + 20).min(300); }
                        }
                        if let Some(q) = world.inquest.as_mut().filter(|q| !q.closed) { q.held_until = 0; }
                    }
                    // a house divided — no common mind; the unease simply festers on
                    _ => { world.dread = (world.dread + 5).min(100); }
                }
                let who = magistrate_idx(world).map(|i| world.agents[i].name.clone()).unwrap_or_else(|| "the magistrate".into());
                let vn = world.inquest.as_ref().map(|q| q.victim_name.clone()).unwrap_or_default();
                out.push(Event { day, date: date.to_string(), kind: "inquest".into(), actor: who,
                    text: if d.text.trim().is_empty() { format!("The parish met in emergency assembly over {vn}'s murder, and came away {outcome}.") } else { d.text.clone() } });
            }
            // a plain townsperson's action, chosen by the oracle in the soul's own character: the
            // general lever by which a pressed soul authors their own next move. subject = the actor;
            // target = the soul acted upon (none for withdraw); verb = the choice. The *decision* is
            // the oracle's; each consequence is fixed and small here, so a town of choices stays sound.
            // The chronicle beat is emitted generically (above) from the oracle's recorded account.
            // Every act deposits a memory in the OTHER soul too — so an action begets a reaction: the
            // target now carries it, which may raise their own focus and move them to act in turn.
            ("act", verb) => {
                let an = world.agents[si].name.clone();
                let tn = ti.map(|t| world.agents[t].name.clone()).unwrap_or_default();
                match verb {
                    // a friendly call: warmth both ways, and a kindness the other will carry
                    "call" => if let Some(t) = ti.filter(|&t| world.agents[t].active() && t != si) {
                        world.nudge_aff(si, t, 10);
                        world.nudge_aff(t, si, 8);
                        nudge_mood(&mut world.agents[si], 6);
                        nudge_mood(&mut world.agents[t], 8);
                        world.remember(t, "reprieve", si as i32, 40, 45, day);
                        world.spawn_news(&an, &format!("{an}'s friendly call on {tn}"), 1, day, &[]);
                    },
                    // having it out: the relief of saying it, at the cost of the tie — and a slight the other keeps
                    "confront" => if let Some(t) = ti.filter(|&t| world.agents[t].active() && t != si) {
                        world.nudge_aff(si, t, -8);
                        world.nudge_aff(t, si, -14);
                        nudge_mood(&mut world.agents[si], 4);
                        nudge_mood(&mut world.agents[t], -10);
                        world.remember(t, "snub", si as i32, -50, 55, day);
                        world.spawn_news(&an, &format!("the hard words {an} had with {tn}"), -2, day, &[]);
                    },
                    // paying court: a strong reaching-out, warmly remembered — unless paid where
                    // it ought not be. Attentions to a married soul are not forbidden (the heart is
                    // no respecter of banns), but they land as impropriety: received with unease, a
                    // faint stain on the suitor's name, and they go nowhere.
                    "court" => if let Some(t) = ti.filter(|&t| world.agents[t].active() && t != si) {
                        if world.agents[t].spouse.is_some_and(|s| s != si) {
                            world.nudge_aff(si, t, 12);
                            world.nudge_aff(t, si, -6);
                            nudge_mood(&mut world.agents[si], 4);
                            nudge_mood(&mut world.agents[t], -4);
                            clamp_standing(&mut world.agents[si], -1);
                            world.remember(t, "wronged", si as i32, -30, 46, day);
                            world.spawn_news(&an, &format!("{an}'s unlooked-for attentions to {tn}"), -1, day, &[]);
                        } else {
                            world.nudge_aff(si, t, 22);
                            world.nudge_aff(t, si, 10);
                            nudge_mood(&mut world.agents[si], 8);
                            world.remember(t, "reprieve", si as i32, 50, 55, day);
                            world.spawn_news(&an, &format!("{an}'s attentions to {tn}"), 1, day, &[]);
                        }
                    },
                    // a material offer within their means: it costs the giver, lifts the taker, raises a name
                    "offer" => if let Some(t) = ti.filter(|&t| world.agents[t].active() && t != si) {
                        let amt = if world.agents[si].purse >= 5 { (world.agents[si].purse / 4).clamp(3, 15) } else { 0 };
                        world.agents[si].purse -= amt;
                        world.agents[t].purse += amt;
                        world.nudge_aff(t, si, 14);
                        nudge_mood(&mut world.agents[t], 8);
                        clamp_standing(&mut world.agents[si], 1);
                        world.remember(t, "reprieve", si as i32, 45, 50, day);
                        world.spawn_news(&an, &format!("the offer {an} made to {tn}"), 1, day, &[]);
                    },
                    // mending: warmth both ways, the grudge let go, the old slights between them released
                    "reconcile" => if let Some(t) = ti.filter(|&t| world.agents[t].active() && t != si) {
                        world.nudge_aff(si, t, 16);
                        world.nudge_aff(t, si, 14);
                        nudge_mood(&mut world.agents[si], 10);
                        nudge_mood(&mut world.agents[t], 10);
                        if world.agents[si].rival == t as i32 { world.agents[si].rival = -1; }
                        if world.agents[t].rival == si as i32 { world.agents[t].rival = -1; }
                        world.agents[si].memories.retain(|m| !(m.kind == "snub" && m.who == t as i32));
                        world.remember(si, "reprieve", t as i32, 45, 50, day);
                        world.remember(t, "reprieve", si as i32, 45, 50, day);
                        world.spawn_news(&an, &format!("the mending between {an} and {tn}"), 1, day, &[]);
                    },
                    // keeping to themselves: no outward move, the thing nursed alone a little deeper
                    "withdraw" => nudge_mood(&mut world.agents[si], -4),
                    _ => {}
                }
            }
            // the gravest choice of all: a soul at the end of their rope rules on their own life —
            // stay and endure, or leave Thrushcombe for good. "go" takes them off-stage (departed):
            // alive, but gone from the parish, and those who held them dear feel the gap they leave.
            // The chronicle beat is emitted generically (above) from the oracle's recorded account.
            ("depart", choice) => {
                let nm = world.agents[si].name.clone();
                match choice {
                    "go" => {
                        world.agents[si].departed = true;
                        world.spawn_news(&nm, &format!("how {nm} gave up their place and left Thrushcombe for good"), -1, day, &[]);
                        // those who held them dear carry the loss — a living bereavement
                        let close: Vec<usize> = (0..world.agents.len())
                            .filter(|&j| j != si && world.agents[j].active()
                                && (world.aff(j, si) >= 40 || world.agents[j].spouse == Some(si)))
                            .collect();
                        for j in close {
                            nudge_mood(&mut world.agents[j], -12);
                            world.remember(j, "grief", si as i32, -60, 65, day);
                        }
                    }
                    // they choose to stay and bear it — a grim resolve, no relief in it
                    "stay" => nudge_mood(&mut world.agents[si], -4),
                    _ => {}
                }
            }
            // the first TWO-SIDED decision: a ripe courtship comes to its question. si = the COURTED
            // soul, who answers; ti = the suitor who has paid the long court. The proposal is the
            // suitor's pursuit (ripened in the fold); the answer is si's, and it joins or breaks two
            // lives. The decider's own reasoning is the generic chronicle beat (above).
            ("betroth", choice) => {
                if let Some(suitor) = ti.filter(|&t| world.agents[t].active() && t != si) {
                    let (dn, sn) = (world.agents[si].name.clone(), world.agents[suitor].name.clone());
                    match choice {
                        // she takes him: two lives joined, the long courtship come to its end
                        "accept" => if world.agents[si].spouse.is_none() && world.agents[suitor].spouse.is_none() {
                            world.agents[si].spouse = Some(suitor);
                            world.agents[suitor].spouse = Some(si);
                            world.agents[si].courting = -1; world.agents[si].courtship = 0;
                            world.agents[suitor].courting = -1; world.agents[suitor].courtship = 0;
                            nudge_mood(&mut world.agents[si], 22);
                            nudge_mood(&mut world.agents[suitor], 24);
                            world.remember(si, "wed", suitor as i32, 80, 80, day);
                            world.remember(suitor, "wed", si as i32, 80, 80, day);
                            let cross = stratum_archetype(&world.agents[si].archetype) != stratum_archetype(&world.agents[suitor].archetype);
                            let note = if cross { " — a match that set tongues wagging across the class line" } else { "" };
                            out.push(Event { day, date: date.to_string(), kind: "marriage".into(), actor: sn.clone(),
                                text: format!("{sn} and {dn} are to be married{note}, the long courtship come at last to its end.") });
                            world.spawn_news(&sn, &format!("the engagement of {sn} and {dn}"), if cross { -2 } else { 1 }, day, &[]);
                        },
                        // she will not have him: the suit broken, the suitor carrying the sting of it
                        "refuse" => {
                            world.agents[suitor].courting = -1;
                            world.agents[suitor].courtship = 0;
                            nudge_mood(&mut world.agents[suitor], -16);
                            world.nudge_aff(si, suitor, -8);
                            world.remember(suitor, "snub", si as i32, -45, 55, day);
                            world.spawn_news(&sn, &format!("how {dn} would not have {sn} after all his courting"), -1, day, &[]);
                        }
                        _ => {}
                    }
                }
            }
            // a farmer's gamble on the land this season: the decision is the oracle's, the season's
            // fortune a fixed, replay-safe roll. `gamble` may make their year or set them back; `safe`
            // takes the small sure return. The decision prose is the generic beat (above); the arm
            // pushes the outcome beat that follows from it.
            ("gamble", choice) => {
                let nm = world.agents[si].name.clone();
                match choice {
                    "gamble" => {
                        let roll = ((day as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (si as u64).wrapping_mul(0xD1B5_4A32_D192_ED03)) % 100;
                        if roll < 55 {
                            world.agents[si].purse += 30;
                            clamp_standing(&mut world.agents[si], 1);
                            nudge_mood(&mut world.agents[si], 16);
                            out.push(Event { day, date: date.to_string(), kind: "decree".into(), actor: nm.clone(),
                                text: format!("And the gamble came good: the season turned {nm}'s way, and the money with it.") });
                            world.spawn_news(&nm, &format!("how {nm}'s bold gamble on the land paid off"), 2, day, &[]);
                        } else {
                            world.agents[si].purse -= 22;
                            nudge_mood(&mut world.agents[si], -18);
                            out.push(Event { day, date: date.to_string(), kind: "decree".into(), actor: nm.clone(),
                                text: format!("And the gamble failed: the season went against {nm}, and they are the poorer for it.") });
                            world.spawn_news(&nm, &format!("how {nm}'s gamble on the land came to grief"), -2, day, &[]);
                        }
                    }
                    // the small, sure return of honest husbandry — no glory, no ruin
                    "safe" => {
                        world.agents[si].purse += 6;
                        nudge_mood(&mut world.agents[si], 3);
                    }
                    _ => {}
                }
            }
            // the felt cost of facing (or refusing) a crack in one's own self-model: a reckoning
            // integrates it at a price; denial hardens against it, which costs more, and quietly.
            ("psyche", c) => {
                match c {
                    "reckoning" => nudge_mood(&mut world.agents[si], -6),
                    "denial" => nudge_mood(&mut world.agents[si], -12),
                    _ => {}
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

        Some(self.build_pair(&w, a, b, day, today, ""))
    }

    /// Like `converse_pair`, but salted with a `nonce` (so repeated calls on the same sim-day pick
    /// DIFFERENT pairs) and skipping any pair in `avoid` (the recently-staged ones) — so live
    /// encounters spread across the town instead of replaying the one ripest couple. Not folded.
    pub fn converse_pair_varied(&self, today: Date, nonce: u64, avoid: &std::collections::HashSet<(usize, usize)>) -> Option<ConversePair> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let adults: Vec<usize> = (0..w.agents.len()).filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        if adults.len() < 4 { return None; }
        let norm = |a: usize, b: usize| if a < b { (a, b) } else { (b, a) };
        let mut rng = rng_for(self.seed ^ 0xC04E_0000_0000, day ^ (nonce as i64));
        let (mut courting, mut warm, mut cold) = (Vec::new(), Vec::new(), Vec::new());
        for &i in &adults {
            let c = w.agents[i].courting;
            if c >= 0 && w.agents.get(c as usize).is_some_and(|a| a.active()) && !avoid.contains(&norm(i, c as usize)) {
                courting.push((i, c as usize));
            }
            for &j in &adults {
                if i < j && !avoid.contains(&(i, j)) {
                    let a = w.aff(i, j);
                    if a >= 30 { warm.push((i, j)); } else if a <= -30 { cold.push((i, j)); }
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
            for _ in 0..24 {
                let (a, b) = (adults[rng.gen_range(0..adults.len())], adults[rng.gen_range(0..adults.len())]);
                if a != b && !avoid.contains(&norm(a, b)) { return Some((a, b)); }
            }
            None
        })?;
        Some(self.build_pair(&w, a, b, day, today, ""))
    }

    /// The normalised (idx,idx) pairs of the most recently recorded conversations — so the live
    /// encounter loop can avoid replaying them.
    pub fn recent_dialogue_pairs(&self, today: Date, limit: i64) -> rusqlite::Result<std::collections::HashSet<(usize, usize)>> {
        let w = self.world_snapshot(today);
        let idx_of = |name: &str| w.agents.iter().position(|a| a.name == name);
        let mut stmt = self.conn.prepare("SELECT source, target FROM dialogues ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            let (s, t) = row?;
            if let (Some(a), Some(b)) = (idx_of(&s), idx_of(&t)) {
                set.insert(if a < b { (a, b) } else { (b, a) });
            }
        }
        Ok(set)
    }

    /// Force a conversation between two NAMED souls, with an optional scene — for staging a meeting
    /// the auto-picker would not have made (a busy evening at the Pelican, say). None if either is absent.
    pub fn converse_pair_between(&self, today: Date, a_name: &str, b_name: &str, setting: &str) -> Option<ConversePair> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let a = w.agents.iter().position(|x| x.name == a_name && x.active())?;
        let b = w.agents.iter().position(|x| x.name == b_name && x.active())?;
        (a != b).then(|| self.build_pair(&w, a, b, day, today, setting))
    }

    /// Assemble the conversation brief for a chosen pair — their briefs and the rich `relation`
    /// (history, recalls, present thoughts, biographies, the murder's pall, the season, recent news),
    /// optionally framed by a `setting` (where and when the talk happens). Shared by both pickers.
    fn build_pair(&self, w: &World, a: usize, b: usize, day: i64, today: Date, setting: &str) -> ConversePair {
        let brief = |i: usize| {
            let ag = &w.agents[i];
            let role = ag.trade.clone().unwrap_or_else(|| match ag.archetype.as_str() {
                "genteel_status_seeker" => "gentlefolk", "hill_farmer" => "a hill farmer", "practitioner" => "of the practice",
                "scheming_improver" => "an improver", "blunt_hand" => "working folk", "official" => "of the parish", _ => "of the town",
            }.to_string());
            format!("{}, {}, of {}, aged {}, standing {} of a hundred, presently {}, who wants {}. {}",
                ag.name, role, ag.seat, ag.age(day), ag.standing, mood_of(ag), want_phrase(w, i), relationships_brief(w, i, day))
        };
        let mut relation = if w.agents[a].spouse == Some(b) {
            "They are man and wife.".to_string()
        } else if w.agents[a].parent == Some(b) || w.agents[b].parent == Some(a) {
            let (p, c) = if w.agents[a].parent == Some(b) { (b, a) } else { (a, b) };
            format!("{} is the parent of {} — they are family, and speak as parent and child, never as sweethearts.", w.agents[p].name, w.agents[c].name)
        } else if w.agents[a].parent.is_some() && w.agents[a].parent == w.agents[b].parent {
            format!("{} and {} are brother and sister.", w.agents[a].name, w.agents[b].name)
        } else if w.agents[a].courting == b as i32 || w.agents[b].courting == a as i32 {
            let (sx, tx) = if w.agents[a].courting == b as i32 { (a, b) } else { (b, a) };
            if w.agents[a].spouse.is_some() || w.agents[b].spouse.is_some() {
                format!("{} carries an attachment to {} though one of them is already wed — it cannot end in marriage.", w.agents[sx].name, w.agents[tx].name)
            } else {
                format!("{} is paying court to {}.", w.agents[sx].name, w.agents[tx].name)
            }
        } else {
            let af = w.aff(a, b);
            if af >= 30 { "They are the best of friends.".into() }
            else if af <= -30 { "They are at bitter odds.".into() }
            else if af > 0 { "They are on friendly enough terms.".into() }
            else { "They are barely acquainted.".into() }
        };
        if !setting.is_empty() {
            relation = format!("The scene, here and now: {setting}. Let the talk arise naturally out of the occasion. {relation}");
        }
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
        // each knows the other's history — the life the parish tells of them
        let life = |who: &str| -> String {
            self.biography(who).ok().flatten().map(|b| format!(" The life of {who}: {b}")).unwrap_or_default()
        };
        relation.push_str(&life(&w.agents[a].name));
        relation.push_str(&life(&w.agents[b].name));
        // a killing in the parish colours everything — they cannot but speak of it, and of whom they fear
        if let Some(ib) = inquest_brief(w, a).or_else(|| inquest_brief(w, b)) {
            relation.push_str(&format!(" {ib}"));
        }
        // the parish as it actually stands, so they have real matter to talk of
        relation.push_str(&format!(" The season is {}.", Season::of(today).name()));
        if let Ok(recent) = self.chronicle(4) {
            let lines: Vec<String> = recent.into_iter().rev().map(|e| e.text).collect();
            if !lines.is_empty() {
                relation.push_str(&format!(" Lately about the parish: {}", lines.join(" ")));
            }
        }
        ConversePair {
            a, a_name: w.agents[a].name.clone(), a_brief: brief(a),
            b, b_name: w.agents[b].name.clone(), b_brief: brief(b),
            relation,
        }
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
    pub fn record_reflection(&mut self, date: Date, subject: &str, thought: &str, mood: &str, sway: &str, toward: &str, regard: &str, resolve: &str, plan: &str, revise: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO reflections(day,subject,thought) VALUES(?1,?2,?3)",
            params![day, subject, thought],
        )?;
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'reflect',?3,?4,?5)",
            params![day, subject, toward, format!("{mood}:{sway}:{regard}:{resolve}:{plan}:{revise}"), thought],
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

    /// Record a soul's statement to the magistrate: store the transcript (for the inquiry page)
    /// and fold its effect through a `testimony` decree. alibi ∈ [none, weak, strong]; accuses is
    /// a named soul or ""; public decides whether it is read out in the open (and so becomes gossip).
    #[allow(clippy::too_many_arguments)]
    pub fn record_testimony(&mut self, date: Date, subject: &str, alibi: &str, accuses: &str, public: bool, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO testimony(day,subject,alibi,accuses,public,text) VALUES(?1,?2,?3,?4,?5,?6)",
            params![day, subject, alibi, accuses, public as i64, text],
        )?;
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'testimony',?3,?4,?5)",
            params![day, subject, accuses, format!("{alibi}:{}", if public { "pub" } else { "priv" }), text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Is a ruling before the magistrate? When the cloud over the lead suspect has crossed JUDGE_AT
    /// and he is not within a prior ruling's cooldown, the case waits on his decision — to charge the
    /// suspect, to stay his hand, or to widen the net. Returns (magistrate, suspect, dossier) for the
    /// oracle to rule on, else None. The decision drives the fold (an `accuse` ruling sets the charge);
    /// this is the LLM authoring a world event, not narrating one. Replay-safe: the ruling is recorded.
    pub fn pending_judgment(&self, today: Date) -> Option<(String, String, String)> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let inq = w.inquest.as_ref().filter(|q| !q.closed && q.accused < 0)?;
        if day <= inq.held_until { return None; } // within a ruling's cooldown — he is not pressed again yet
        let mag = magistrate_idx(&w)?;
        // the soul the cloud sits hardest on — whom the magistrate is being pressed to charge
        let m = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child"
                && i != inq.victim && i != mag)
            .max_by_key(|&i| (w.agents[i].suspicion, std::cmp::Reverse(i)))?;
        if w.agents[m].suspicion < JUDGE_AT { return None; }

        let ag = &w.agents[m];
        let mn = w.agents[mag].name.clone();
        let role = ag.trade.clone().unwrap_or_else(|| match ag.archetype.as_str() {
            "genteel_status_seeker" => "gentlefolk", "hill_farmer" => "a hill farmer", "practitioner" => "of the practice",
            "scheming_improver" => "an improver", "blunt_hand" => "working folk", "official" => "of the parish", _ => "of the town",
        }.to_string());
        let days_open = day - inq.opened;
        let top = (0..w.agents.len()).filter(|&i| w.agents[i].active()).map(|i| w.agents[i].standing).max().unwrap_or(0);
        let mut d = format!(
            "You are {mn}, sitting as magistrate over the murder of {victim}, dead now {days_open} days and the killer still unknown. The parish is in a fright and clamours for an answer. The cloud of suspicion has settled hardest on one soul, and you must now rule what is to be done — for there is NO proof against them, only the town's fear and what it holds against them.\n\nThe soul under the cloud: {name}, {role}, of {seat}, aged {age}. The suspicion upon them stands at {susp} out of 100 — heavy, but it is suspicion, not evidence.",
            mn = mn, victim = inq.victim_name, name = ag.name, role = role, seat = ag.seat, age = ag.age(day), susp = ag.suspicion,
        );
        if let Ok(Some(bio)) = self.biography(&ag.name) {
            d.push_str(&format!("\nWho they are: {bio}"));
        }
        // their station relative to yours — a genteel magistrate is slow to hang his own and quick to hang a labourer
        let respectable = matches!(ag.archetype.as_str(), "genteel_status_seeker" | "official" | "practitioner");
        if respectable {
            d.push_str("\nThey are of standing — of your own sort — and to charge them would shake the parish's order.");
        } else if matches!(ag.archetype.as_str(), "blunt_hand") || ag.standing <= top - 30 {
            d.push_str("\nThey are common working folk, low and friendless, and the parish would think little of seeing them hang. The easy course is to give the town its blood.");
        }
        // what account they have given, if any
        match self.testimony_of(&ag.name) {
            Ok(Some((alibi, _, _))) if alibi == "strong" => d.push_str("\nThey have given the magistrate a strong, witnessed account of themselves the night of the killing — their alibi largely holds."),
            Ok(Some((alibi, _, _))) if alibi == "weak" => d.push_str("\nThe account they gave you was thin and unsupported — no one outside their own household can vouch for them."),
            Ok(Some(_)) => d.push_str("\nThey could give you no account of themselves at all for the night of the killing."),
            _ => d.push_str("\nThey have not yet been brought before you to give any account of themselves."),
        }
        // who would stand by them, who they are at odds with (the magistrate weighs both)
        let defenders = (0..w.agents.len()).filter(|&j| j != m && w.agents[j].active() && w.aff(j, m) >= 40).count();
        d.push_str(&match defenders {
            0 => "\nThey have no one of weight to speak for them.".to_string(),
            1 => "\nOne soul of the parish would speak in their defence.".to_string(),
            k => format!("\n{k} souls would stand and speak in their defence — to charge them would not pass quietly."),
        });
        let odds: Vec<String> = w.ties(m, false, 3).into_iter().map(|(j, _)| w.agents[j].name.clone()).collect();
        if !odds.is_empty() {
            d.push_str(&format!("\nThe killer might as easily be another: there is bad blood in the parish between the dead and {}.", odds.join(", ")));
        }
        d.push_str(&format!("\nThe town's dread stands at {} out of 100.", w.dread));
        if inq.public_inquiry {
            d.push_str(" You have already questioned the whole parish in open inquiry.");
        }
        d.push_str("\n\nYOUR RULING. You may: ACCUSE — bring them to formal trial for the murder (the town will likely hang them, guilty or not); HOLD — stay your hand for want of proof, and wait, though the parish will fret; or WIDEN — refuse to fix on this one soul, and turn the inquiry back upon the whole parish. Rule as the man you are — your station, your conscience, your fear of the mob or your contempt for it.");
        Some((mn, ag.name.clone(), d))
    }

    /// Record the magistrate's ruling as a `judgment` decree, to be folded at its day. ruling ∈
    /// [accuse, hold, widen]; text is his recorded reasoning, read out as the chronicle beat. The
    /// fold turns an `accuse` into the charge, a `hold`/`widen` into a stay. Replay reads it back.
    pub fn record_judgment(&mut self, date: Date, magistrate: &str, suspect: &str, ruling: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'judgment',?3,?4,?5)",
            params![day, magistrate, suspect, ruling, text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Assemble the brief for an emergency town meeting over the open murder: the magistrate before a
    /// frightened parish, where things stand, whom the fear has fixed on, and the question of how the
    /// room turns. The oracle renders the meeting and judges the outcome (calmed | inflamed | divided),
    /// which drives the town's dread and the cloud over the hunted. None if no murder is open.
    pub fn townhall_brief(&self, today: Date) -> Option<(String, String)> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let inq = w.inquest.as_ref().filter(|q| !q.closed)?;
        let days_open = day - inq.opened;
        let mag = magistrate_idx(&w).map(|i| w.agents[i].name.clone()).unwrap_or_else(|| "the magistrate".into());
        let mut suspects: Vec<(String, i32, bool)> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child" && i != inq.victim && w.agents[i].suspicion >= 30)
            .map(|i| {
                let respectable = matches!(w.agents[i].archetype.as_str(), "genteel_status_seeker" | "official" | "practitioner");
                (w.agents[i].name.clone(), w.agents[i].suspicion, respectable)
            })
            .collect();
        suspects.sort_by_key(|(_, s, _)| std::cmp::Reverse(*s));
        let suspect_line = if suspects.is_empty() {
            "no one in particular — the fear has found no fixed object".to_string()
        } else {
            suspects.iter().take(4)
                .map(|(n, s, r)| format!("{n} (the fear on them stands at {s}{})", if *r { ", though of standing" } else { ", an outsider or of the labouring poor" }))
                .collect::<Vec<_>>().join("; ")
        };
        // those few who have spoken for the most-suspected — the voices that might steady the room
        let defenders = suspects.first().and_then(|(name, _, _)| w.idx(name)).map(|m| {
            (0..w.agents.len()).filter(|&j| j != m && w.agents[j].active() && w.aff(j, m) >= 40)
                .map(|j| w.agents[j].name.clone()).collect::<Vec<_>>()
        }).unwrap_or_default();
        let mut d = format!(
            "{mag} has called the whole parish together in emergency assembly — the church hall full and restless, every pew taken — over the murder of {victim}, now {days_open} days unsolved and the killer still unknown and at large among them. The town's dread stands at {dread} of a hundred: fear walks the lanes, doors are barred at dusk, and the parish has come wanting an answer and an end to it. {mag} must stand before them, give his account of where the inquiry stands, and hear their fears voiced from the floor.\n\nWhere it TRULY stands: there is no proof against any living soul — only the town's fear and its old grudges. The fear has settled hardest on: {suspect_line}. {mag} has already twice refused to charge a man on suspicion alone, holding that a frightened town makes a poor substitute for evidence.",
            mag = mag, victim = inq.victim_name, days_open = days_open, dread = w.dread, suspect_line = suspect_line,
        );
        if !defenders.is_empty() {
            d.push_str(&format!("\nA few would speak up for the most-suspected if it came to it: {}.", defenders.join(", ")));
        }
        d.push_str("\n\nIn the room: the frightened majority who want a name to hang and the thing finished; the few who have misgivings about condemning a man on fear; the gentlefolk and the labouring poor, who do not fear the same things nor trust the same men. Render the meeting as it would truly unfold — the magistrate's address, the voices raised from the floor (name them where it lands), the temper of the room as it shifts. Then judge how the parish comes AWAY: CALMED (he steadies them, and they will let justice be done right and slow), INFLAMED (more afraid than before, and demanding a scapegoat be charged and hanged NOW), or DIVIDED (the room splits, no common mind). Decide as the real weight of the town's terror, the want of any proof, and the magistrate's steadying authority would actually settle it. There is no right answer.");
        Some((mag, d))
    }

    /// Record the town meeting and its outcome as a `townhall` decree, folded at its day. outcome ∈
    /// [calmed, inflamed, divided]; text is the full account, read out and kept for the inquiry page.
    pub fn record_townhall(&mut self, date: Date, magistrate: &str, outcome: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'townhall','',?3,?4)",
            params![day, magistrate, outcome, text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// The town meetings held over the murder, newest first — (day, outcome, full account) — for the
    /// inquiry page. The full transcript of each assembly, kept as it was read out.
    pub fn town_halls(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT day, choice, text FROM decrees WHERE kind='townhall' ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Names of souls already questioned in the inquiry, so the magistrate works through the rest.
    pub fn questioned(&self) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT subject FROM testimony")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map(|v| v.into_iter().collect())
    }

    /// One soul's own statement to the magistrate, if they have given one.
    pub fn testimony_of(&self, name: &str) -> rusqlite::Result<Option<(String, String, String)>> {
        match self.conn.query_row(
            "SELECT alibi, accuses, text FROM testimony WHERE subject = ?1 ORDER BY id DESC LIMIT 1",
            params![name],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        ) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The transcripts the magistrate has read out in the open — for the inquiry page, newest first.
    pub fn public_testimony(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String, String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT day, subject, alibi, accuses, text FROM testimony WHERE public = 1 ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.collect()
    }

    /// The murder as a given soul truly knows and feels it — for injecting their real awareness into
    /// a conversation, so they speak of it in character (and the magistrate knows it is not closed
    /// while the killer walks free). None when there is no killing weighing on the town.
    pub fn murder_brief(&self, today: Date, name: &str) -> Option<String> {
        let w = self.world_snapshot(today);
        let idx = w.agents.iter().position(|a| a.name == name && a.active())?;
        inquest_brief(&w, idx)
    }

    /// Every statement taken in the inquiry, read-out or not — for the full case file, newest first.
    /// Returns (day, subject, alibi, accuses, public, text); `public` marks what was read out aloud.
    pub fn all_testimony(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String, String, String, bool, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT day, subject, alibi, accuses, public, text FROM testimony ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? != 0, r.get(5)?))
        })?;
        rows.collect()
    }

    /// Assemble the soul the magistrate questions next — a named one, or the most-suspected of
    /// those not yet questioned — with the dossier their statement should be drawn from. None when
    /// the town is at peace or everyone has been heard.
    pub fn testimony_subject(&self, today: Date, target: Option<&str>) -> Option<ReflectSubject> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let inq = w.inquest.as_ref().filter(|q| !q.closed)?;
        let done = self.questioned().unwrap_or_default();
        let idx = match target {
            Some(t) => w.agents.iter().position(|a| a.name == t && a.active())?,
            None => (0..w.agents.len())
                .filter(|&i| {
                    w.agents[i].active() && w.agents[i].archetype != "child"
                        && i != inq.victim && i as i32 != inq.investigator
                        && !done.contains(&w.agents[i].name)
                })
                .max_by_key(|&i| (w.agents[i].suspicion, std::cmp::Reverse(i)))?,
        };
        let ag = &w.agents[idx];
        let role = ag.trade.clone().unwrap_or_else(|| match ag.archetype.as_str() {
            "genteel_status_seeker" => "gentlefolk", "hill_farmer" => "a hill farmer", "practitioner" => "of the practice",
            "scheming_improver" => "an improver", "blunt_hand" => "working folk", "official" => "of the parish", _ => "of the town",
        }.to_string());
        let mag = w.agents.get(inq.investigator as usize).map(|a| a.name.clone()).unwrap_or_else(|| "the magistrate".into());
        let mut d = format!(
            "{name}, {role}, of {seat}, aged {age}, is brought before {mag}, sitting as magistrate, to answer for their whereabouts the night {victim} was murdered.",
            name = ag.name, seat = ag.seat, age = ag.age(day), victim = inq.victim_name,
        );
        if let Ok(Some(bio)) = self.biography(&ag.name) {
            d.push_str(&format!("\nWho they are: {bio}"));
        }
        let odds: Vec<String> = w.ties(idx, false, 3).into_iter().map(|(j, _)| w.agents[j].name.clone()).collect();
        if !odds.is_empty() {
            d.push_str(&format!("\nThose they are at odds with (whom a frightened soul might be tempted to name): {}.", odds.join(", ")));
        }
        // the known facts of their alibi, where there are any settled ones
        if ag.name == "Mr Pete Peckers" {
            d.push_str("\nKNOWN ALIBI (settled fact — their account must reflect it): the night Quint died, Pete Peckers was up at High Foldside the whole evening, repairing Mr Sunter's broken tractor, and Mr Sunter and his household vouch for him. His alibi is solid and witnessed.");
        }
        if ag.suspicion >= 60 {
            d.push_str("\nThe town's eye is hard upon them already; they come frightened, and a frightened soul protests, or points elsewhere.");
        } else if ag.suspicion >= 30 {
            d.push_str("\nThere have been looks, whispers; they know they must give a good account of themselves.");
        }
        let respectable = matches!(ag.archetype.as_str(), "genteel_status_seeker" | "official" | "practitioner");
        d.push_str(if respectable {
            "\nBeing of standing, the magistrate handles them gently — theirs is a formality."
        } else {
            "\nBeing common folk or a stranger, the magistrate presses them hard, and is minded to believe the worst."
        });
        Some(ReflectSubject { name: ag.name.clone(), dossier: d })
    }

    /// Record (or replace) a soul's biography — the life the parish tells of them. Flavour, not
    /// folded; injected into talk and reflection so souls know one another's histories.
    pub fn record_biography(&mut self, name: &str, text: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO biographies(name,text) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET text=?2",
            params![name, text],
        )?;
        Ok(())
    }

    /// A soul's recorded biography, if one has been written.
    pub fn biography(&self, name: &str) -> rusqlite::Result<Option<String>> {
        match self.conn.query_row("SELECT text FROM biographies WHERE name = ?1", params![name], |r| r.get::<_, String>(0)) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A bespoke character prompt for a soul, if one has been set — used to give a special
    /// character (Aldric Fynch) their own distinctive voice in place of the generic scaffolding.
    pub fn custom_persona(&self, name: &str) -> rusqlite::Result<Option<String>> {
        match self.conn.query_row("SELECT prompt FROM personas WHERE name = ?1", params![name], |r| r.get::<_, String>(0)) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Set (or replace) a soul's bespoke character prompt.
    pub fn set_persona(&self, name: &str, prompt: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO personas(name,prompt) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET prompt=?2",
            params![name, prompt],
        )?;
        Ok(())
    }

    /// The living adults who have no biography yet — the backlog for the biographer to work through.
    pub fn souls_without_bio(&self, today: Date) -> Vec<String> {
        let w = self.world_snapshot(today);
        let mut have: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Ok(mut stmt) = self.conn.prepare("SELECT name FROM biographies") {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                for n in rows.flatten() {
                    have.insert(n);
                }
            }
        }
        w.agents.iter()
            .filter(|a| a.active() && a.archetype != "child" && !have.contains(&a.name))
            .map(|a| a.name.clone())
            .collect()
    }

    /// The settled facts the parish knows of a soul — name, station, household, age, kin, origin —
    /// the seed a biographer invents a consistent life around.
    pub fn bio_facts(&self, name: &str, today: Date) -> Option<String> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let idx = w.agents.iter().position(|a| a.name == name)?;
        let a = &w.agents[idx];
        let role = a.trade.clone().unwrap_or_else(|| match a.archetype.as_str() {
            "genteel_status_seeker" => "gentlefolk".into(),
            "hill_farmer" => "a hill farmer".into(),
            "practitioner" => "of the practice (a vet or doctor)".into(),
            "scheming_improver" => "an improver, full of modern schemes".into(),
            "blunt_hand" => "working folk".into(),
            "official" => "of the parish (church, school or law)".into(),
            _ => "of the town".into(),
        });
        let sex = if a.sex == 0 { "woman" } else { "man" };
        let mut f = format!("{}, a {sex}, {role}, of {}, aged {}.", a.name, a.seat, a.age(day));
        if let Some(o) = &a.origin {
            f.push_str(&format!(" Came to Thrushcombe from {o}."));
        }
        if let Some(s) = a.spouse {
            f.push_str(&format!(" Married to {}.", w.agents[s].name));
        }
        if let Some(p) = a.parent {
            f.push_str(&format!(" A child of {}.", w.agents[p].name));
        }
        let kids: Vec<String> = (0..w.agents.len())
            .filter(|&i| w.agents[i].parent == Some(idx))
            .map(|i| w.agents[i].name.clone())
            .collect();
        if !kids.is_empty() {
            f.push_str(&format!(" Their children: {}.", kids.join(", ")));
        }
        let standing = match a.standing {
            s if s >= 65 => "well thought of, near the top of the parish",
            s if s >= 45 => "of solid middling standing",
            _ => "of humble standing",
        };
        f.push_str(&format!(" They are {standing}."));
        Some(f)
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

    /// One soul's reflections, newest first, dated — for their own thought history.
    pub fn reflections_of(&self, name: &str, limit: i64) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT day, thought FROM reflections WHERE subject = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![name, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Record a consolidation of the inner life: the soul's revised self-concept, their updated
    /// beliefs about named others (a tracked theory of mind), and any fracture between who they
    /// believe they are and what is so. The texts are flavour (injected, never folded); a fracture
    /// rides a `psyche` decree so its felt cost on spirits stays deterministic.
    pub fn record_psyche(&mut self, date: Date, subject: &str, self_concept: &str, beliefs: &[(String, String)], fracture: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        if !self_concept.trim().is_empty() {
            self.conn.execute("INSERT INTO psyche(day,subject,about,text) VALUES(?1,?2,'',?3)", params![day, subject, self_concept])?;
        }
        for (about, text) in beliefs {
            if !about.trim().is_empty() && !text.trim().is_empty() {
                self.conn.execute("INSERT INTO psyche(day,subject,about,text) VALUES(?1,?2,?3,?4)", params![day, subject, about, text])?;
            }
        }
        if fracture != "none" {
            self.conn.execute(
                "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'psyche','',?3,?4)",
                params![day, subject, fracture, self_concept],
            )?;
            self.decrees = load_decrees(&self.conn)?;
            self.invalidate_from(day)?;
        }
        Ok(())
    }

    /// How a soul has come to see themselves — their current self-concept (latest consolidation).
    pub fn self_model(&self, name: &str) -> rusqlite::Result<Option<String>> {
        match self.conn.query_row(
            "SELECT text FROM psyche WHERE subject = ?1 AND about = '' ORDER BY id DESC LIMIT 1",
            params![name], |r| r.get::<_, String>(0),
        ) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// What a soul currently believes about another named soul — their theory of that person.
    pub fn belief_of(&self, name: &str, about: &str) -> rusqlite::Result<Option<String>> {
        match self.conn.query_row(
            "SELECT text FROM psyche WHERE subject = ?1 AND about = ?2 ORDER BY id DESC LIMIT 1",
            params![name, about], |r| r.get::<_, String>(0),
        ) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The beliefs a soul currently holds about others (latest per person) — their theory of mind.
    pub fn beliefs_held_by(&self, name: &str, limit: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT about, text FROM psyche WHERE subject = ?1 AND about <> '' AND id IN
               (SELECT MAX(id) FROM psyche WHERE subject = ?1 AND about <> '' GROUP BY about)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![name, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
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

    /// The sequence-id of each soul's most recent reflection — for choosing who is most overdue.
    /// Ranked by insertion order, not day, so many reflections within one day still cycle the
    /// whole town (lower id = reflected longer ago; never-reflected souls come first, at -1).
    fn last_reflected(&self) -> rusqlite::Result<BTreeMap<String, i64>> {
        let mut stmt = self.conn.prepare("SELECT subject, MAX(id) FROM reflections GROUP BY subject")?;
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
        let name = w.agents[idx].name.clone();
        let mut dossier = self.inner_dossier(&w, idx, day, today);
        // the continuity bridge: a soul's inner life is ONE unbroken thread, not a fresh musing each
        // hour. Lay before them everything they have done and said since they last sat with their
        // thoughts, so the next thought metabolises the whole interval and carries the one life forward.
        dossier.push_str(&self.life_since_reflection(&name, day));
        Some(ReflectSubject { name, dossier })
    }

    /// What a soul has done and said since they last sat alone with their thoughts — their own acts
    /// and conversations in the interval — framed so their next reflection takes honest account of
    /// the whole of it and carries their one continuous inner life forward, never starting afresh.
    fn life_since_reflection(&self, name: &str, day: i64) -> String {
        let since: i64 = self.conn
            .query_row("SELECT COALESCE(MAX(day), -1) FROM reflections WHERE subject=?1", params![name], |r| r.get(0))
            .unwrap_or(-1);
        let mut lines: Vec<String> = Vec::new();
        // their own outward actions in the interval
        if let Ok(mut stmt) = self.conn.prepare("SELECT text FROM decrees WHERE kind='act' AND subject=?1 AND day>?2 ORDER BY id") {
            if let Ok(rows) = stmt.query_map(params![name, since], |r| r.get::<_, String>(0)) {
                for t in rows.flatten() { lines.push(format!("you did this — {}", t.trim())); }
            }
        }
        // conversations they fell into, and what each left them thinking
        if let Ok(mut stmt) = self.conn.prepare("SELECT target, memory FROM dialogues WHERE source=?1 AND day>?2 ORDER BY id") {
            if let Ok(rows) = stmt.query_map(params![name, since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                for (who, mem) in rows.flatten() {
                    if !mem.trim().is_empty() { lines.push(format!("you fell into talk with {who}, and came away thinking: {}", mem.trim())); }
                }
            }
        }
        if lines.is_empty() {
            return String::new();
        }
        let gap = if since < 0 { day } else { day - since };
        let when = if since < 0 {
            "You have not sat alone with your thoughts before this hour, but your life has already been in motion".to_string()
        } else if gap <= 1 {
            "Since last you sat with your thoughts, scarcely a day".to_string()
        } else {
            format!("It is some {gap} days since you last sat alone with your thoughts, and your inner life did not pause in the meantime")
        };
        format!(
            "\nTHE THREAD CONTINUES — {when}: in that interval, {}. This is the same unbroken inner life, picking up exactly where it left off: take honest account of what you did and what was said, let it move the thought on, and carry the stream forward as the very next moment of one continuous life — never a fresh start.",
            lines.join("; ")
        )
    }

    /// The full inner dossier of a soul — who they are, their ties and grudges, their plan if any,
    /// the life behind them, the self-model they reason from, their theory of others, what has lately
    /// befallen them, what they carry as episodic memory, how they feel themselves seen, what is
    /// uppermost in their mind, what they are counting on, and the thread of their recent thinking.
    /// Shared by reflection, consolidation, and action — the one place a soul is written out whole.
    fn inner_dossier(&self, w: &World, idx: usize, day: i64, today: Date) -> String {
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
            mood = mood_of(ag), goal = want_phrase(w, idx),
        );
        dossier.push_str(&format!("\n{}", relationships_brief(w, idx, day)));
        if !friends.is_empty() { dossier.push_str(&format!("\nThey are close to: {friends}.")); }
        if !odds.is_empty() { dossier.push_str(&format!("\nThey are at odds with: {odds}.")); }
        if let Some(r) = feud { dossier.push_str(&format!("\nThey nurse a running grudge against {r}.")); }
        if ag.intent != 0 {
            let what = match ag.intent { 1 => "to mend their fortunes", 2 => "to better their station", _ => "a bold venture" };
            dossier.push_str(&format!("\nThey are already set on a plan — {what} — resolved {} days since, and not yet come to its head.", ag.intent_age));
        }
        if let Ok(Some(bio)) = self.biography(&ag.name) {
            dossier.push_str(&format!("\nThe life behind them: {bio}"));
        }
        // the evolving self-model they reason from, and their theory of those on their mind
        if let Ok(Some(sc)) = self.self_model(&ag.name) {
            dossier.push_str(&format!("\nHow they have come to see themselves (their settled sense of who they are — reason from it, and let it shift only if this hour truly moves it): {sc}"));
        }
        if let Ok(bs) = self.beliefs_held_by(&ag.name, 4) {
            let lines: Vec<String> = bs.into_iter().map(|(who, t)| format!("  what they make of {who}: {t}")).collect();
            if !lines.is_empty() { dossier.push_str(&format!("\nWhat they privately believe of others:\n{}", lines.join("\n"))); }
        }
        if let Some(brief) = inquest_brief(&w, idx) {
            dossier.push_str(&format!("\n{brief}"));
        }

        if let Ok(es) = self.person_events(&ag.name, 5) {
            let lines: Vec<String> = es.into_iter().rev().map(|e| format!("  {} — {}", e.date, e.text)).collect();
            if !lines.is_empty() { dossier.push_str(&format!("\nLately for them:\n{}", lines.join("\n"))); }
        }
        if let Ok(ms) = self.memories_of(&ag.name, 4) {
            let lines: Vec<String> = ms.into_iter().map(|(who, m)| format!("  of {who}: {m}")).collect();
            if !lines.is_empty() { dossier.push_str(&format!("\nWhat they carry of others:\n{}", lines.join("\n"))); }
        }
        // the particular occasions that still grip them — their episodic memory, the autobiography
        // a continuous self is grounded in. A repressed engram is given as a nameless dread, never
        // its cause: the soul cannot reach it, and neither can the telling.
        {
            let lines: Vec<String> = w.carried(idx).into_iter().take(5).map(|m| engram_phrase(&w, m)).collect();
            if !lines.is_empty() {
                dossier.push_str(&format!(
                    "\nWhat they carry within themselves (the occasions still gripping them — let these weigh on the hour's thought; do not merely list them):\n{}",
                    lines.iter().map(|l| format!("  · {l}")).collect::<Vec<_>>().join("\n")
                ));
            }
        }
        // the shape of the whole life — the defining moments consolidated across the years, carried
        // always. Distinct from the gripping-now above: not the mood of the hour but the load-bearing
        // memories of who the years have made them, read forward as a life. This is the deep, continuous
        // self — a soul who can reach back past the last fortnight to the bereavements, matches, and
        // reckonings that formed them.
        {
            let mut life: Vec<&Memory> = w.agents[idx].lifelong.iter().collect();
            if !life.is_empty() {
                life.sort_by_key(|m| std::cmp::Reverse(m.salience));
                life.truncate(12); // the dozen most defining, then read in order
                life.sort_by_key(|m| m.day);
                let lines: Vec<String> = life.iter().map(|m| {
                    let ago = (day - m.day).max(0);
                    let when = if ago >= 730 { format!("{} years past — ", ago / 365) }
                        else if ago >= 365 { "a year past — ".to_string() }
                        else if ago >= 60 { format!("{} months past — ", ago / 30) }
                        else if ago >= 14 { format!("{} weeks past — ", ago / 7) }
                        else { String::new() };
                    format!("  · {when}{}", engram_phrase(&w, m))
                }).collect();
                dossier.push_str(&format!(
                    "\nThe shape of their life — the moments that have made them, carried always (the load-bearing memories of who the years have made them; reach back to these, not only to this fortnight):\n{}",
                    lines.join("\n")
                ));
            }
        }
        // the body under the mind — a soul reasons and chooses partly from the flesh: bone-weary,
        // or ill, or rested and well. Embodiment: the day's labour and the season are in the thought.
        dossier.push_str(&format!("\nYour body, tonight: you are {}.", body_phrase(ag, day)));
        // how they imagine the parish regards them — the recursive mirror, which may sit wide of
        // the truth; they reason partly from how they feel themselves seen, not their real standing
        dossier.push_str(&format!("\nHow they feel themselves seen by the parish: {}.", self_regard_phrase(ag.seen_as)));
        // THE thing uppermost in their mind — the global workspace. Whatever else is true of them,
        // the hour's thought must be ruled by this; a gripped mind cannot wander freely elsewhere.
        match focus_phrase(&w, idx) {
            Some(p) => dossier.push_str(&format!("\nWHAT IS UPPERMOST IN THEIR MIND right now (let this rule the hour — a mind this taken up cannot freely turn elsewhere): {p}.")),
            None => dossier.push_str("\nTheir mind is tolerably easy just now, resting on the day's ordinary work — no one thing crowds it."),
        }
        // what they are presently counting on — the expectations they hold with confidence, that
        // the days may yet confirm or betray. A soul reasons partly from what they are sure of.
        {
            let lines: Vec<String> = w.agents[idx].expectations.iter()
                .filter(|e| e.confidence >= 45)
                .map(|e| expectation_phrase(&w, idx, e))
                .collect();
            if !lines.is_empty() {
                dossier.push_str(&format!(
                    "\nWhat they are counting on (what they presently expect — let the hour reckon with whether it is holding):\n{}",
                    lines.iter().map(|l| format!("  · {l}")).collect::<Vec<_>>().join("\n")
                ));
            }
        }
        // a grounded private truth they carry — fed ONLY to themselves, so it surfaces consistently
        // and never contradicts itself. The true killer's is REPRESSED: it leaks as dread and
        // compulsion, never a plain confession (so the town never learns it from them); an ordinary
        // secret is simply kept close. This is what makes their hidden depths real rather than invented.
        if !w.agents[idx].secret.is_empty() {
            let repressed = w.inquest.as_ref().is_some_and(|q| q.culprit == idx as i32)
                || w.carried(idx).iter().any(|m| m.kind == "haunt");
            if repressed {
                dossier.push_str(&format!(
                    "\nTHE THING YOU HAVE BURIED (you can scarcely let yourself know it for what it is, and you will NEVER say it plainly — not to another soul, not even to yourself in words; it surfaces only as the nameless dread, as a compulsion you cannot account for, as something you flinch from and cannot look at directly): {}. Do not confess it. Do not name it. Let it press on you only as an unease whose cause you cannot reach.",
                    w.agents[idx].secret
                ));
            } else {
                dossier.push_str(&format!(
                    "\nA PRIVATE TRUTH you carry and will tell no one (it colours how you move through the parish, but you keep it close and turn the talk aside from it): {}.",
                    w.agents[idx].secret
                ));
            }
        }
        // their running inner monologue these recent hours, oldest first — the thread to continue
        if let Ok(mut ts) = self.self_reflections(&ag.name, 6) {
            if !ts.is_empty() {
                ts.reverse(); // self_reflections is newest-first; a thread reads oldest → newest
                dossier.push_str(&format!(
                    "\nThe thread of their recent thinking (oldest first — this is the ongoing stream to carry forward, not restate):\n{}",
                    ts.iter().map(|t| format!("  · {t}")).collect::<Vec<_>>().join("\n")
                ));
            }
        }
        dossier.push_str(&format!("\nThe season is {}.", Season::of(today).name()));
        if let Ok(recent) = self.chronicle(4) {
            let lines: Vec<String> = recent.into_iter().rev().map(|e| e.text).collect();
            if !lines.is_empty() { dossier.push_str(&format!(" About the parish lately: {}", lines.join(" "))); }
        }
        dossier
    }

    /// Pick the soul most *pressed to act*, and lay out what they might do. A soul is moved to act
    /// when something grips them — a preoccupation that fills the mind, or a plan ripe for a move —
    /// and they have not lately acted (a cooldown keeps the town from a frenzy). Returns (actor,
    /// dossier): their whole inner state, the menu of plain townsperson's acts, and the souls they
    /// might act upon. The oracle chooses what they DO — and that choice drives the fold, recorded as
    /// an `act` decree. This is the general lever: every pressed soul authoring their own next move.
    pub fn action_subject(&self, today: Date) -> Option<(String, String)> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let acted = self.last_acted().unwrap_or_default();
        // how hard a soul is pressed to act: the grip of what fills their mind, or a ripe resolve
        let imp = |i: usize| -> i16 {
            let a = &w.agents[i];
            let focus = if a.focus.topic.is_empty() { 0 } else { a.focus.intensity };
            let aim = if a.intent != 0 && a.intent_age >= 4 { 50 } else { 0 };
            // a soul worn to the bone or ill is slow to go out and act — the body holds them back
            let spent = if a.vigour <= 22 { 18 } else if a.vigour <= 40 || a.health <= 40 { 8 } else { 0 };
            (focus.max(aim) - spent).max(0)
        };
        let idx = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .filter(|&i| imp(i) >= ACT_FLOOR)
            .filter(|&i| day - acted.get(&w.agents[i].name).copied().unwrap_or(i64::MIN / 2) >= ACT_COOLDOWN)
            .max_by_key(|&i| (imp(i), std::cmp::Reverse(i)))?;

        let mut d = self.inner_dossier(&w, idx, day, today);

        // the souls they might act upon — those close, those at odds, their rival, their spouse, and
        // the one their mind is fixed on. The oracle must choose a target from among these living.
        let mut cand: Vec<usize> = Vec::new();
        cand.extend(w.ties(idx, true, 3).into_iter().map(|(j, _)| j));
        cand.extend(w.ties(idx, false, 3).into_iter().map(|(j, _)| j));
        if w.agents[idx].rival >= 0 { cand.push(w.agents[idx].rival as usize); }
        if let Some(s) = w.agents[idx].spouse { cand.push(s); }
        if w.agents[idx].focus.target >= 0 { cand.push(w.agents[idx].focus.target as usize); }
        cand.retain(|&j| j != idx && w.agents.get(j).is_some_and(|a| a.active()));
        cand.sort_unstable(); cand.dedup();
        let people = cand.iter().map(|&j| w.agents[j].name.clone()).collect::<Vec<_>>().join(", ");

        d.push_str(&format!(
            "\n\nWHAT DO THEY DO NOW? Being so moved, this soul may take ONE plain action of the sort a townsperson takes — or none at all. Choose only what THIS soul, as they are and feel, would truly do today:\n  · call — pay a friendly call on someone, to warm or keep a tie\n  · confront — have it out with one they are at odds with, and say their piece\n  · court — pay court to one they are drawn to (never one of their own blood, and to press a suit on a married soul is improper, an attachment that cannot end in marriage)\n  · offer — make a material offer within their means: a gift, a loan, the promise of work, a bargain\n  · reconcile — go to mend a broken tie or put down a grudge\n  · withdraw — do nothing outward; keep to themselves and bear it alone\nThe soul they act upon must be one of the living named here: {people}. If they withdraw, name no one."
        ));
        Some((w.agents[idx].name.clone(), d))
    }

    /// The last day each soul took an outward action — for the cooldown that keeps the town calm.
    fn last_acted(&self) -> rusqlite::Result<BTreeMap<String, i64>> {
        let mut stmt = self.conn.prepare("SELECT subject, MAX(day) FROM decrees WHERE kind='act' GROUP BY subject")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect()
    }

    /// Record a soul's chosen action as an `act` decree, folded at its day. verb ∈ [call, confront,
    /// court, offer, reconcile, withdraw]; target is the soul acted upon (or "" for withdraw); text
    /// is the chronicle account, read out as the beat. The fold turns it into a bounded, real
    /// consequence (see apply_decrees). Replay-safe: the act is recorded, and re-folds read it back.
    pub fn record_act(&mut self, date: Date, actor: &str, verb: &str, target: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'act',?3,?4,?5)",
            params![day, actor, target, verb, text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Is a soul at the brink of leaving? When a soul's lot has come past bearing — ruin (deep debt,
    /// low standing, low spirits) or the parish's suspicion in an open murder — they may be put to
    /// the gravest choice: stay and endure, or leave Thrushcombe for good. Returns (actor, dossier)
    /// for the oracle to decide, else None. A cooldown keeps a soul who chose to stay from being
    /// asked again every day. The choice drives the fold (a recorded `depart` decree).
    pub fn pending_departure(&self, today: Date) -> Option<(String, String)> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        // the last day each soul was put to this choice — so the momentous question is not nagged
        let asked: BTreeMap<String, i64> = {
            let mut m = BTreeMap::new();
            if let Ok(mut s) = self.conn.prepare("SELECT subject, MAX(day) FROM decrees WHERE kind='depart' GROUP BY subject") {
                if let Ok(rows) = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                    for row in rows.flatten() { m.insert(row.0, row.1); }
                }
            }
            m
        };
        let accused = w.inquest.as_ref().map(|q| q.accused).unwrap_or(-1);
        let open_murder = w.inquest.as_ref().is_some_and(|q| !q.closed);
        // how far past bearing a soul's lot has come — ruin, or the hunt closing on them
        let brink = |i: usize| -> i32 {
            let a = &w.agents[i];
            let mut s = 0;
            if a.purse <= -15 && a.standing <= 30 && a.mood <= -20 {
                s = (15 - a.purse).max(0) + (30 - a.standing).max(0);
            }
            if open_murder && a.suspicion >= 65 && !a.cleared {
                s = s.max(a.suspicion);
            }
            s
        };
        let idx = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child" && i as i32 != accused)
            .filter(|&i| day - asked.get(&w.agents[i].name).copied().unwrap_or(i64::MIN / 2) >= DEPART_COOLDOWN)
            .filter(|&i| brink(i) >= DEPART_FLOOR)
            .max_by_key(|&i| (brink(i), std::cmp::Reverse(i)))?;

        let mut d = self.inner_dossier(&w, idx, day, today);
        d.push_str("\n\nIT HAS COME TO THIS: things have come to such a pass for this soul that they might leave Thrushcombe altogether — give up their place and go, to the towns, to a far-off relation, to anywhere but here, and not come back. Weigh it exactly as they would. A soul does not leave the only world they know lightly: set what holds them here — kin, the few who care for them, the one life they have ever known — against what drives them out — ruin, debt, the parish's suspicion, the shame of it. Do they STAY and endure what they must, or GO for good?");
        Some((w.agents[idx].name.clone(), d))
    }

    /// Record a soul's decision on leaving as a `depart` decree, folded at its day. choice ∈ [stay,
    /// go]; text is the chronicle account. `go` takes them off-stage for good (departed); replay-safe.
    pub fn record_departure(&mut self, date: Date, actor: &str, choice: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'depart','',?3,?4)",
            params![day, actor, choice, text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Is a proposal before a soul? When a courtship has ripened — the suitor has paid long court,
    /// both are free, the warmth is mutual — the COURTED soul is put to the question. Returns
    /// (courted, suitor, dossier) for the oracle to answer, else None. The first two-sided decision:
    /// the suit is the suitor's pursuit (built in the fold); the answer is the courted soul's, and a
    /// recorded `betroth` decree joins or breaks the two. (The fold no longer marries of its own accord.)
    pub fn pending_betrothal(&self, today: Date) -> Option<(String, String, String)> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        // the ripest courtship that has come to its question — long-paid, free on both sides, mutual
        let mut best: Option<(usize, usize, i16)> = None; // (suitor, courted, courtship-progress)
        for i in 0..w.agents.len() {
            if !w.agents[i].active() || w.agents[i].spouse.is_some() { continue; }
            let tj = w.agents[i].courting;
            if tj < 0 { continue; }
            let tj = tj as usize;
            if tj >= w.agents.len() || !w.agents[tj].active() || w.agents[tj].spouse.is_some() { continue; }
            if w.agents[i].courtship < BETROTH_AT { continue; }
            if !(w.aff(i, tj) >= 30 && w.aff(tj, i) >= 28) { continue; } // mutual enough to be a real proposal
            if best.is_none_or(|(_, _, c)| w.agents[i].courtship > c) {
                best = Some((i, tj, w.agents[i].courtship));
            }
        }
        let (suitor, courted, _) = best?;
        let role = w.agents[suitor].trade.clone().unwrap_or_else(|| match w.agents[suitor].archetype.as_str() {
            "genteel_status_seeker" => "of the gentry", "hill_farmer" => "a hill farmer", "practitioner" => "of the practice",
            "scheming_improver" => "an improver", "blunt_hand" => "a working man", "official" => "of the parish", _ => "of the town",
        }.to_string());
        let sn = w.agents[suitor].name.clone();
        let (seat, age) = (w.agents[suitor].seat.clone(), w.agents[suitor].age(day));
        let mut d = self.inner_dossier(&w, courted, day, today);
        d.push_str(&format!("\n\nA PROPOSAL BEFORE THEM: {sn} — {role}, of {seat}, aged {age} — has paid them long and faithful court, and it is come at last to the asking: will they marry? Weigh it exactly as THIS soul would, by their own heart and their station and what such a match would make of their life — his place and prospects set against their own, whether the warmth is truly returned or merely borne, what their kin and the parish would say of it. Do they ACCEPT {sn}, or REFUSE?"));
        Some((w.agents[courted].name.clone(), sn, d))
    }

    /// Record the courted soul's answer as a `betroth` decree. choice ∈ [accept, refuse]; subject =
    /// the one who answers, target = the suitor. `accept` weds them; `refuse` breaks the suit. Replay-safe.
    pub fn record_betrothal(&mut self, date: Date, courted: &str, suitor: &str, choice: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'betroth',?3,?4,?5)",
            params![day, courted, suitor, choice, text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Is a gamble on the land before a farmer? In a growing season, a farmer who has not lately
    /// weighed one is offered the choice: a bold, risky venture that may make their year or set them
    /// back, or the small sure return of honest husbandry. Returns (farmer, dossier), else None. The
    /// decision is the oracle's; the season's fortune is a fixed, replay-safe roll in the fold.
    pub fn pending_gamble(&self, today: Date) -> Option<(String, String)> {
        if matches!(Season::of(today), Season::Winter) { return None; } // no gambling on the land in the dead season
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let asked: BTreeMap<String, i64> = {
            let mut m = BTreeMap::new();
            if let Ok(mut s) = self.conn.prepare("SELECT subject, MAX(day) FROM decrees WHERE kind='gamble' GROUP BY subject") {
                if let Ok(rows) = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                    for row in rows.flatten() { m.insert(row.0, row.1); }
                }
            }
            m
        };
        // the hungriest eligible farmer first — the one with the most reason to chance it
        let idx = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && matches!(w.agents[i].archetype.as_str(), "hill_farmer" | "scheming_improver"))
            .filter(|&i| day - asked.get(&w.agents[i].name).copied().unwrap_or(i64::MIN / 2) >= GAMBLE_COOLDOWN)
            .min_by_key(|&i| (w.agents[i].purse, i as i64))?;
        let mut d = self.inner_dossier(&w, idx, day, today);
        d.push_str(&format!("\n\nTHE LAND, THIS {} SEASON: a chance has come to gamble on the land — to sink what they have into a bold, risky venture (a speculative cash crop, a costly beast, a scheme of improvement) that may make their year or ruin it; OR to play it safe and take the small, sure return of honest husbandry. Weigh it exactly as THIS farmer would, by their temper, their debts, their nerve, what they can bear to lose. Do they GAMBLE, or play it SAFE?", Season::of(today).name().to_uppercase()));
        Some((w.agents[idx].name.clone(), d))
    }

    /// Record a farmer's decision on the land as a `gamble` decree. choice ∈ [gamble, safe]; the
    /// fold resolves the season's fortune deterministically. Replay-safe.
    pub fn record_gamble(&mut self, date: Date, farmer: &str, choice: &str, text: &str) -> rusqlite::Result<()> {
        let day = self.target_day(date).max(0);
        self.conn.execute(
            "INSERT INTO decrees(day,subject,kind,target,choice,text) VALUES(?1,?2,'gamble','',?3,?4)",
            params![day, farmer, choice, text],
        )?;
        self.decrees = load_decrees(&self.conn)?;
        self.invalidate_from(day)?;
        Ok(())
    }

    /// Assemble the soul whose inner life is most overdue for consolidation — to step back from
    /// the hour-by-hour stream and reason over the whole: revise who they take themselves to be,
    /// update what they believe of the souls who weigh on them, and face any gap between the two.
    /// A named target overrides the pick. The dossier carries their current self-model, the recent
    /// thread, what has lately happened to them, and the people presently on their mind.
    pub fn psyche_subject(&self, today: Date, target: Option<&str>) -> Option<ReflectSubject> {
        let w = self.world_snapshot(today);
        let day = self.target_day(today).max(0);
        let last: BTreeMap<String, i64> = {
            let mut m = BTreeMap::new();
            if let Ok(mut stmt) = self.conn.prepare("SELECT subject, MAX(id) FROM psyche GROUP BY subject") {
                if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                    for row in rows.flatten() { m.insert(row.0, row.1); }
                }
            }
            m
        };
        let idx = match target {
            Some(t) => w.agents.iter().position(|a| a.name == t && a.active() && a.archetype != "child")?,
            None => (0..w.agents.len())
                .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
                .min_by_key(|&i| last.get(&w.agents[i].name).copied().unwrap_or(-1))?,
        };
        let ag = &w.agents[idx];
        let role = ag.trade.clone().unwrap_or_else(|| match ag.archetype.as_str() {
            "genteel_status_seeker" => "gentlefolk", "hill_farmer" => "a hill farmer", "practitioner" => "of the practice",
            "scheming_improver" => "an improver", "blunt_hand" => "working folk", "official" => "of the parish", _ => "of the town",
        }.to_string());
        let mut d = format!(
            "{name}, {role}, of {seat}, aged {age}. Standing {standing} of a hundred, purse {purse}£, presently {mood}.",
            name = ag.name, seat = ag.seat, age = ag.age(day), standing = ag.standing, purse = ag.purse, mood = mood_of(ag),
        );
        if let Ok(Some(bio)) = self.biography(&ag.name) {
            d.push_str(&format!("\nThe life the parish tells of them: {bio}"));
        }
        match self.self_model(&ag.name) {
            Ok(Some(sc)) => d.push_str(&format!("\nWho they have until now taken themselves to be: {sc}")),
            _ => d.push_str("\nThey have never yet sat and reckoned who they truly are; this is the first such hour."),
        }
        d.push_str(&format!("\nHow they feel themselves seen by the parish (reckon with whether this is true, or a thing they only fear): {}.", self_regard_phrase(ag.seen_as)));
        if let Some(p) = focus_phrase(&w, idx) {
            d.push_str(&format!("\nWhat has been uppermost in their mind of late: {p} — any honest reckoning of themselves must pass through it."));
        }
        // the people presently weighing on them — from their recent thread and their ledger of feeling
        let mut onmind: Vec<String> = Vec::new();
        for (j, _) in w.ties(idx, true, 2).into_iter().chain(w.ties(idx, false, 2)) {
            onmind.push(w.agents[j].name.clone());
        }
        if ag.rival >= 0 { if let Some(r) = w.agents.get(ag.rival as usize) { onmind.push(r.name.clone()); } }
        onmind.dedup();
        for who in onmind.iter().take(4) {
            let cur = self.belief_of(&ag.name, who).ok().flatten();
            match cur {
                Some(b) => d.push_str(&format!("\n  Of {who}, they have believed: {b}")),
                None => d.push_str(&format!("\n  {who} is on their mind, though they have never set down what they make of them.")),
            }
        }
        if let Ok(ms) = self.memories_of(&ag.name, 4) {
            let lines: Vec<String> = ms.into_iter().map(|(who, m)| format!("  of {who}: {m}")).collect();
            if !lines.is_empty() { d.push_str(&format!("\nWhat others have lately left with them:\n{}", lines.join("\n"))); }
        }
        if let Ok(mut ts) = self.self_reflections(&ag.name, 6) {
            if !ts.is_empty() {
                ts.reverse();
                d.push_str(&format!("\nThe recent stream of their thinking (oldest first):\n{}", ts.iter().map(|t| format!("  · {t}")).collect::<Vec<_>>().join("\n")));
            }
        }
        if let Ok(es) = self.person_events(&ag.name, 5) {
            let lines: Vec<String> = es.into_iter().rev().map(|e| format!("  {} — {}", e.date, e.text)).collect();
            if !lines.is_empty() { d.push_str(&format!("\nWhat has lately befallen them:\n{}", lines.join("\n"))); }
        }
        // the occasions that still grip them — the episodic ground their self-reckoning must
        // account for. A repression appears only as a dread they cannot reach the cause of: a
        // self honestly taking stock must reckon with the part of itself it cannot face.
        {
            let lines: Vec<String> = w.carried(idx).into_iter().take(5).map(|m| engram_phrase(&w, m)).collect();
            if !lines.is_empty() {
                d.push_str(&format!("\nWhat they carry within themselves (let an honest reckoning take account of these):\n{}",
                    lines.iter().map(|l| format!("  · {l}")).collect::<Vec<_>>().join("\n")));
            }
        }
        if let Some(brief) = inquest_brief(&w, idx) {
            d.push_str(&format!("\n{brief}"));
        }
        Some(ReflectSubject { name: ag.name.clone(), dossier: d })
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

    /// A persistent calendar shift, in days, the driver adds to the real date to get the town's
    /// "today" — so the world can **jump forward** while still running at the real-life pace
    /// underneath. 0 = pure companion mode. Stored, so a jump endures across runs.
    pub fn day_offset(&self) -> i64 {
        self.conn.query_row("SELECT val FROM meta WHERE key='day_offset'", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Bump the calendar shift by `days` (jump forward); returns the new offset.
    pub fn jump(&mut self, days: i64) -> rusqlite::Result<i64> {
        let off = self.day_offset() + days.max(0);
        self.conn.execute("INSERT OR REPLACE INTO meta(key,val) VALUES('day_offset',?1)", params![off])?;
        Ok(off)
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

    /// New beats since the given per-table cursors, each tagged with its `voice` and the speaker's
    /// current seat, cast index (for their portrait), and sex — so the Discord feed can route each
    /// to its place-channel and post it under the townsperson's own name and face. Three voices:
    /// narration (narrated events), thought (a soul's reflections), speech (conversation lines).
    pub fn discord_beats(&self, since_e: i64, since_t: i64, since_d: i64, limit: i64)
        -> rusqlite::Result<Vec<DiscordBeat>>
    {
        let w = self.world_at(self.last_slot().max(0));
        let mut by_name: std::collections::HashMap<&str, (i32, String, i32)> = std::collections::HashMap::new();
        for (i, a) in w.agents.iter().enumerate() {
            by_name.insert(a.name.as_str(), (i as i32, a.seat.clone(), a.sex as i32));
        }
        let look = |name: &str| by_name.get(name).cloned().unwrap_or((-1, String::new(), -1));
        let mut out = Vec::new();

        // narration — the oracle's voiced chronicle events
        let mut s = self.conn.prepare(
            "SELECT e.id, e.kind, e.actor, n.text FROM events e JOIN narration n ON n.event_id = e.id
             WHERE e.id > ?1 ORDER BY e.id ASC LIMIT ?2")?;
        for row in s.query_map(params![since_e, limit], |r|
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
        {
            let (id, kind, actor, text) = row?;
            let (idx, seat, sex) = look(&actor);
            out.push(DiscordBeat { src: "e".into(), id, voice: "narration".into(), actor, idx, seat, sex, kind, text });
        }

        // thought — a soul's private reflection
        let mut s = self.conn.prepare(
            "SELECT id, subject, thought FROM reflections WHERE id > ?1 ORDER BY id ASC LIMIT ?2")?;
        for row in s.query_map(params![since_t, limit], |r|
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
        {
            let (id, actor, text) = row?;
            let (idx, seat, sex) = look(&actor);
            out.push(DiscordBeat { src: "t".into(), id, voice: "thought".into(), actor, idx, seat, sex, kind: "reflection".into(), text });
        }

        // speech — each line of a conversation, attributed to its speaker
        let mut s = self.conn.prepare(
            "SELECT id, transcript FROM dialogues WHERE id > ?1 ORDER BY id ASC LIMIT ?2")?;
        for row in s.query_map(params![since_d, limit], |r|
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        {
            let (id, transcript) = row?;
            for line in transcript.lines() {
                let line = line.trim();
                if let Some((who, said)) = line.split_once(": ") {
                    if let Some((idx, seat, sex)) = by_name.get(who).cloned() {
                        out.push(DiscordBeat { src: "d".into(), id, voice: "speech".into(),
                            actor: who.to_string(), idx, seat, sex, kind: "dialogue".into(), text: said.trim().to_string() });
                    }
                }
            }
        }
        Ok(out)
    }

    /// Where every living adult is this phase — for the Discord feed to keep each place-channel's
    /// topic showing who is there now. Reuses the dashboard's per-phase placement.
    pub fn presence(&self, today: Date, phase: Phase) -> rusqlite::Result<Vec<PresenceRow>> {
        let d = self.detail(today, phase)?;
        Ok(d.people.into_iter()
            .filter(|p| p.archetype != "child")
            .map(|p| PresenceRow { name: p.name, idx: p.idx as i32, location: p.location, seat: p.seat, doing: p.doing })
            .collect())
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

    /// What a soul carries on `date` — their live engrams, most gripping first, with the other
    /// soul (if any) named. This is folded state, not a recorded table: the autobiographical
    /// memory their stream of consciousness is grounded in, and the dashboard surfaces.
    /// Each entry: (kind, who_name_or_empty, valence, salience, day_it_happened).
    pub fn carried_by(&self, name: &str, date: Date) -> Vec<(String, String, i16, i16, i64)> {
        let world = self.world_on(date);
        let Some(idx) = world.idx(name) else { return Vec::new() };
        world.carried(idx).into_iter().map(|m| {
            let who = if m.who >= 0 { world.agents.get(m.who as usize).map(|a| a.name.clone()).unwrap_or_default() } else { String::new() };
            (m.kind.clone(), who, m.valence, m.salience, m.day)
        }).collect()
    }

    /// The shape of a soul's whole life — the defining moments consolidated into the lifelong store,
    /// oldest first. The autobiography carried always, distinct from what merely grips them now.
    /// Returns (kind, who, valence, salience, day).
    pub fn lifelong_of(&self, name: &str, date: Date) -> Vec<(String, String, i16, i16, i64)> {
        let world = self.world_on(date);
        let Some(idx) = world.idx(name) else { return Vec::new() };
        let mut life: Vec<&Memory> = world.agents[idx].lifelong.iter().collect();
        life.sort_by_key(|m| m.day);
        life.into_iter().map(|m| {
            let who = if m.who >= 0 { world.agents.get(m.who as usize).map(|a| a.name.clone()).unwrap_or_default() } else { String::new() };
            (m.kind.clone(), who, m.valence, m.salience, m.day)
        }).collect()
    }

    /// How a soul feels themselves seen by the parish on `date` (the recursive mirror), as the raw
    /// estimate and the phrasing of it — for the dashboard. May sit wide of their real standing.
    pub fn self_regard_of(&self, name: &str, date: Date) -> Option<(i16, String)> {
        let world = self.world_on(date);
        let idx = world.idx(name)?;
        let sa = world.agents[idx].seen_as;
        Some((sa, self_regard_phrase(sa).to_string()))
    }

    /// What is uppermost in a soul's mind on `date` (the global workspace) — the topic, its
    /// intensity, and the phrasing of it. None of the phrasing when their mind rests on the day's
    /// work. Returns (topic, intensity, phrase_or_none) for the dashboard.
    pub fn focus_of(&self, name: &str, date: Date) -> Option<(String, i16, Option<String>)> {
        let world = self.world_on(date);
        let idx = world.idx(name)?;
        let f = &world.agents[idx].focus;
        Some((f.topic.clone(), f.intensity, focus_phrase(&world, idx)))
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
                wants: want_phrase(&world, i),
                mood: mood_of(a).to_string(),
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

        let fear = world.inquest.as_ref().map(|inq| {
            let days = (target - inq.opened).max(0);
            let dread_word = match world.dread { d if d >= 80 => "the town in terror", d if d >= 60 => "fear thick in every lane", d if d >= 30 => "a wary dread", _ => "an uneasy quiet" };
            let bench = (inq.investigator >= 0).then(|| world.agents.get(inq.investigator as usize).map(|a| format!(" — {} on the bench", a.name))).flatten().unwrap_or_default();
            let level = format!("{dread_word}{bench}");
            if inq.closed {
                let who = (inq.accused >= 0).then(|| world.agents.get(inq.accused as usize).map(|a| a.name.clone())).flatten().unwrap_or_else(|| "someone".into());
                return format!("The murder of {} — {who} hanged for it {days}d on. {level}; no one quite sure justice was done.", inq.victim_name);
            }
            if inq.accused >= 0 {
                let acc = world.agents.get(inq.accused as usize).map(|a| a.name.clone()).unwrap_or_default();
                return format!("MURDER of {} ({days}d unsolved) — the parish has fixed on {acc}, and means to hang them. {level}.", inq.victim_name);
            }
            // name the soul suspicion falls on hardest
            let most = world.agents.iter().enumerate()
                .filter(|(i, a)| a.active() && a.archetype != "child" && *i != inq.victim)
                .max_by_key(|(_, a)| a.suspicion);
            match most {
                Some((_, a)) if a.suspicion >= 40 => format!("MURDER of {} — {days}d, killer unknown. Suspicion falls on {} (susp {}). {level}.", inq.victim_name, a.name, a.suspicion),
                _ => format!("MURDER of {} — {days}d, killer unknown, and no one yet named. {level}.", inq.victim_name),
            }
        });

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
            fear,
        })
    }

    fn pending(&self, today: Date, world: &World) -> Vec<String> {
        let mut p = Vec::new();
        // funerals the parish has yet to hold — the town's nearest great occasion
        let target = self.target_day(today).max(0);
        for f in &world.funerals {
            let days = (f.scheduled - target).max(0);
            let when = if days == 0 { "today".to_string() } else { format!("in {days}d") };
            p.push(format!("the funeral of {} — {when}", f.name));
        }
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

#[cfg(test)]
mod funeral_tests {
    use super::*;

    // A death schedules a funeral the parish holds some days on — a great occasion that fires
    // once, on its day, and is then laid to rest.
    #[test]
    fn a_death_is_buried_on_its_day() {
        let mut w = World::seed();
        let who = (0..w.agents.len()).find(|&i| w.agents[i].active() && w.agents[i].archetype != "child").unwrap();
        w.agents[who].death_day = Some(0);
        let name = w.agents[who].name.clone();
        w.funerals.push(Funeral { who, name: name.clone(), scheduled: 2, murdered: false });

        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();
        let mut held: Option<i64> = None;
        for day in 0..5i64 {
            let evs = life_tick(&mut w, day, date, 42);
            if evs.iter().any(|e| e.kind == "funeral" && e.text.contains(&name)) {
                held = Some(day);
            }
        }
        assert_eq!(held, Some(2), "the funeral is held on its scheduled day");
        assert!(w.funerals.is_empty(), "a held funeral is laid to rest, not held again");
    }
}

#[cfg(test)]
mod inquest_tests {
    use super::*;

    // An open killing hunts itself. With no recorded culprit, suspicion accretes onto the soul
    // the town already mistrusts — bad blood with the dead, an outsider's face, a desperate purse.
    // But the fold no longer CHARGES on its own: it presses the cloud past JUDGE_AT and there it
    // waits — the accusation is a ruling the magistrate (the oracle) must make. Here we make that
    // ruling "accuse" and confirm the downstream reckoning still hangs a friendless soul, the dread
    // breaking only when the town has its blood. The point: whether an innocent is charged at all
    // now turns on a mind weighing it, not a threshold.
    #[test]
    fn an_open_murder_presses_to_a_hanging() {
        let mut w = World::seed();
        let grown: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .collect();
        let (victim, suspect) = (grown[0], grown[1]);
        w.agents[suspect].birth_day = -40 * 365; // middle-aged, to keep death's hazard out of the test

        // the victim is dead; an inquest is open and the town gripped by dread
        w.agents[victim].death_day = Some(0);
        w.dread = 85;
        w.inquest = Some(Inquest {
            victim,
            victim_name: w.agents[victim].name.clone(),
            opened: 0,
            accused: -1,
            accused_since: 0,
            hanged: false,
            closed: false,
            investigator: -1,
            public_inquiry: false,
            held_until: 0,
            culprit: -1,
        });
        // the suspect is everything the parish fears: bad blood with the dead, an incomer, cornered,
        // and friendless and low — so doubt will not save them when the reckoning comes
        w.nudge_aff(suspect, victim, -90);
        w.nudge_aff(victim, suspect, -90);
        w.agents[suspect].origin = Some("parts unknown".into());
        w.agents[suspect].purse = -40;
        w.agents[suspect].standing = 12;

        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();
        let mut reached: Option<i64> = None;   // the day the cloud crossed the magistrate's threshold
        let mut charged = false;               // the day a ruling fixed the charge
        let mut accused_on: Option<i64> = None;
        let mut hanged: Option<(i64, String)> = None;
        for day in 1..60i64 {
            let mut evs = life_tick(&mut w, day, date, 42);
            // while the killer is unfound, the dread must not fade to calm
            if w.inquest.as_ref().is_some_and(|q| !q.closed) {
                assert!(w.dread >= 40, "dread should fester while the killing is unsolved (day {day}, dread {})", w.dread);
            }
            // the fold presses suspicion past bearing — but it must NOT charge of its own accord
            if reached.is_none() && w.agents[suspect].suspicion >= JUDGE_AT {
                reached = Some(day);
                assert!(w.inquest.as_ref().is_some_and(|q| q.accused < 0),
                    "the fold must not charge on its own — the accusation waits on the magistrate's ruling");
            }
            // once the cloud is past bearing, the magistrate rules to accuse (the oracle's role in
            // life): a `judgment` decree folds the charge in, exactly as the live `judge` job does
            if !charged && reached.is_some() {
                let mag = magistrate_idx(&w).expect("a magistrate sits over the inquiry");
                let ruling = Decree {
                    subject: w.agents[mag].name.clone(), kind: "judgment".into(),
                    target: w.agents[suspect].name.clone(), choice: "accuse".into(), text: String::new(),
                };
                evs.extend(apply_decrees(&mut w, day, date, std::slice::from_ref(&ruling)));
                charged = true;
            }
            if accused_on.is_none() && w.inquest.as_ref().is_some_and(|q| q.accused == suspect as i32) {
                accused_on = Some(day);
            }
            if let Some(e) = evs.iter().find(|e| e.kind == "murder" && e.text.contains("hanged")) {
                hanged = Some((day, e.text.clone()));
                break;
            }
        }

        assert!(reached.is_some(), "suspicion should mount past the magistrate's threshold");
        let acc = accused_on.expect("an `accuse` ruling should fix the charge on the suspect");
        let (day, text) = hanged.expect("a friendless charged suspect should hang within the window");
        assert!(day > acc, "the hanging follows the accusation, not precedes it");
        assert!(text.contains(&w.agents[suspect].name), "the suspect is the one hanged, got: {text}");
        assert!(!w.agents[suspect].active(), "the hanged soul leaves the cast");
        let q = w.inquest.as_ref().expect("the inquest record persists");
        assert!(q.closed && q.hanged, "a hanging closes the inquest");
        assert!(w.dread < 40, "the town's blood breaks the dread");
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    // A soul carries what happens to them, and acts on it. A bereavement deposits a charged
    // engram that fades slowly and holds the spirits down for weeks — grief is not shaken off
    // by the next Sunday — while an ordinary memory wears away in days and is let go.
    #[test]
    fn grief_is_carried_and_dampens_recovery() {
        let mut w = World::seed();
        let grown: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .collect();
        let (mourner, lost) = (grown[0], grown[1]);
        // make them kin so the death lands as grief, and keep the mourner clear of death's hazard
        w.agents[mourner].birth_day = -35 * 365;
        w.agents[mourner].parent = Some(lost);
        w.agents[lost].birth_day = -55 * 365;

        let date = Date::from_calendar_date(1934, Month::June, 3).unwrap(); // a Sunday in 1934
        assert_eq!(date.weekday(), Weekday::Sunday);

        // the kin dies; grief is laid down
        w.agents[lost].death_day = Some(0);
        for k in 0..w.agents.len() {
            if w.agents[k].active() && w.agents[k].parent == Some(lost) {
                nudge_mood(&mut w.agents[k], -35);
                w.remember(k, "grief", lost as i32, -80, 85, 0);
            }
        }
        let low = w.agents[mourner].mood;
        assert!(low < -20, "a fresh bereavement sinks the spirits");
        assert!(w.agents[mourner].memories.iter().any(|m| m.kind == "grief"), "the grief is carried as an engram");

        // a week on, the engram still grips and the spirits have barely lifted (recovery dampened)
        for day in 1..=7 { let _ = life_tick(&mut w, day, date, 7); }
        let g = w.agents[mourner].memories.iter().find(|m| m.kind == "grief").map(|m| m.salience).unwrap_or(0);
        assert!(g >= 70, "a charged grief is still gripping a week on, got salience {g}");
        assert!(w.agents[mourner].mood < low + 10, "grief holds the spirits down — no bouncing back by Sunday (low {low}, now {})", w.agents[mourner].mood);

        // an ordinary, uncharged memory wears away and is forgotten within a fortnight
        w.remember(mourner, "errand", -1, -10, 8, 100);
        for day in 100..114 { let _ = life_tick(&mut w, day, date, 7); }
        assert!(!w.agents[mourner].memories.iter().any(|m| m.kind == "errand"), "a faint memory is let go");
    }

    // A repressed engram does not fade and surfaces unbidden: a charged dread with no face to it,
    // laid down once, still grips long after and keeps pulling the spirits down with no occasion.
    #[test]
    fn a_haunting_does_not_fade_and_surfaces() {
        let mut w = World::seed();
        let who = (0..w.agents.len()).find(|&i| w.agents[i].active() && w.agents[i].archetype != "child").unwrap();
        w.agents[who].birth_day = -40 * 365;
        w.remember(who, "haunt", -1, -90, 90, 0);
        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();

        let mut dipped = false;
        let mut prev = w.agents[who].mood;
        for day in 1..40 {
            let _ = life_tick(&mut w, day, date, 99);
            if w.agents[who].mood < prev { dipped = true; }
            prev = w.agents[who].mood;
        }
        let h = w.agents[who].memories.iter().find(|m| m.kind == "haunt").map(|m| m.salience).unwrap_or(0);
        assert_eq!(h, 90, "the buried thing does not fade with time");
        assert!(dipped, "a repression surfaces unbidden, pulling the spirits down with no occasion");
    }

    // The working memory turns over within weeks, but a DEFINING moment is consolidated into the
    // lifelong store at full strength the instant it happens, and kept for the whole of the life —
    // the autobiography a continuous self rests on. A passing slight is not so kept.
    #[test]
    fn a_defining_moment_is_kept_for_life_though_the_working_memory_forgets_it() {
        let mut w = World::seed();
        let grown: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .collect();
        let (soul, lost) = (grown[0], grown[1]);

        // a bereavement — a defining moment — consolidates to the lifelong store at once
        w.remember(soul, "grief", lost as i32, -80, 85, 0);
        assert!(w.agents[soul].lifelong.iter().any(|m| m.kind == "grief" && m.who == lost as i32),
            "a defining moment is laid into the lifelong store the instant it happens");

        // the working store later forgets it, as it would once faded and crowded out over weeks
        w.agents[soul].memories.retain(|m| m.kind != "grief");
        assert!(!w.agents[soul].memories.iter().any(|m| m.kind == "grief"),
            "the bounded working memory has let the old grief go");

        // but the life remembers — the lifelong store still carries it, undimmed
        let kept = w.agents[soul].lifelong.iter().find(|m| m.kind == "grief" && m.who == lost as i32);
        assert!(kept.is_some_and(|m| m.salience == 85),
            "the lifelong store keeps the defining moment for life, at its full strength");

        // a passing slight is NOT kept for life — the autobiography holds only what truly formed them
        w.remember(soul, "snub", -1, -20, 40, 5);
        assert!(!w.agents[soul].lifelong.iter().any(|m| m.kind == "snub"),
            "an ordinary, low slight does not enter the lifelong store");
    }
}

#[cfg(test)]
mod expectation_tests {
    use super::*;

    // A soul under a mounting cloud is sure the parish will see they are no murderer. As suspicion
    // climbs the world falls ever further below that hope, and the surprise of being so wrong is
    // carried as a felt *wrong* — an injustice they cannot make answer to anything they have done.
    #[test]
    fn a_mounting_cloud_is_felt_as_a_wrong() {
        let mut w = World::seed();
        let grown: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        let (victim, soul) = (grown[0], grown[1]);
        w.agents[victim].death_day = Some(0);
        w.inquest = Some(Inquest {
            victim, victim_name: w.agents[victim].name.clone(), opened: 0, accused: -1,
            accused_since: 0, hanged: false, closed: false, investigator: -1, public_inquiry: false,
            held_until: 0, culprit: -1,
        });
        w.agents[soul].standing = 40;
        w.agents[soul].suspicion = 10;
        let mood0 = w.agents[soul].mood;

        // day 0: under a cloud, they form the expectation that they will come through it
        tend_expectations(&mut w, 0);
        assert!(w.agents[soul].expectations.iter().any(|e| e.topic == 1),
            "a soul under a cloud expects, with confidence, to be cleared");

        // the cloud mounts day by day — the world falls below the hope
        for day in 1..=EXPECT_AFTER {
            w.agents[soul].suspicion += 8;
            tend_expectations(&mut w, day);
        }
        assert!(w.agents[soul].memories.iter().any(|m| m.kind == "wronged"),
            "the betrayed hope is carried as a felt wrong");
        assert!(w.agents[soul].mood < mood0,
            "the injustice sinks their spirits (now {}, was {mood0})", w.agents[soul].mood);
    }

    // Surprise is scaled by confidence: a soul SURE of a friend's warmth, finding it turned cold,
    // feels a betrayal — a charged wound — where the same coldness from someone they'd never read
    // would barely register. A predictive self that can be wrong, and feel the wrongness.
    #[test]
    fn a_trusted_friend_turning_cold_is_a_betrayal() {
        let mut w = World::seed();
        let g: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        let (a, b) = (g[0], g[1]);
        w.nudge_aff(a, b, 70);
        w.nudge_aff(b, a, 70); // a is sure of b's warmth
        tend_expectations(&mut w, 0);
        assert!(w.agents[a].expectations.iter().any(|e| e.topic == 0 && e.about == b as i32),
            "a holds an expectation about how b regards them");
        let mood0 = w.agents[a].mood;

        // b turns cold; days pass until the expectation ripens and is read back
        w.affinity.insert((b as u32, a as u32), -30);
        for day in 1..=EXPECT_AFTER { tend_expectations(&mut w, day); }
        assert!(w.agents[a].memories.iter().any(|m| m.kind == "betrayed" && m.who == b as i32),
            "the friend's turn is carried as a betrayal, with their face on it");
        assert!(w.agents[a].mood < mood0, "the betrayal sinks their spirits");
    }
}

#[cfg(test)]
mod agency_tests {
    use super::*;

    // Endogenous aim: a carried wound, given time, hardens into a declared enmity — memory becoming
    // initiative. Nobody hands it to them; the grievance is their own and they take it up.
    #[test]
    fn a_carried_wound_hardens_into_enmity() {
        let mut w = World::seed();
        let g: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        let (soul, foe) = (g[0], g[1]);
        // they must be no kin — one does not take up arms against their own hearth
        w.agents[soul].spouse = None; w.agents[soul].parent = None;
        w.agents[foe].spouse = None; w.agents[foe].parent = None;
        w.agents[soul].rival = -1;
        w.remember(soul, "snub", foe as i32, -60, 75, 0);

        let mut declared = false;
        for day in 0..400 {
            if !w.agents[soul].memories.iter().any(|m| m.kind == "snub" && m.who == foe as i32) {
                w.remember(soul, "snub", foe as i32, -60, 75, day);
            }
            let _ = form_aims(&mut w, day, Date::from_calendar_date(1934, Month::June, 1).unwrap(), 7);
            if w.agents[soul].rival == foe as i32 { declared = true; break; }
        }
        assert!(declared, "a gripping grievance, given time, hardens into a declared enmity");
        assert_eq!(w.agents[soul].goal, 4, "the enmity becomes their consuming aim");
    }

    // The recursive mirror reflects a soul's circumstance — a low, suspected soul comes to feel
    // ill-regarded; a high, well-liked one feels well thought of — and it lags, no single day
    // snapping them to the truth (so a soul can live a while worse-regarded than they really are).
    #[test]
    fn the_mirror_reflects_a_cloud_and_lags() {
        let mut w = World::seed();
        let g: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        let (low, high) = (g[0], g[1]);
        w.agents[low].standing = 10;
        w.agents[low].suspicion = 60;
        w.agents[high].standing = 90;
        for &j in &g { if j != high { w.nudge_aff(j, high, 45); } }

        // one day's move is bounded — the mirror lags, it does not snap
        update_self_regard(&mut w, 1);
        assert!(w.agents[low].seen_as.abs() <= 12, "the mirror lags: one day moves it only so far");

        for day in 2..=18 { update_self_regard(&mut w, day); }
        assert!(w.agents[low].seen_as <= -30, "a low, suspected soul comes to feel ill-regarded, got {}", w.agents[low].seen_as);
        assert!(w.agents[high].seen_as >= 20, "a high, well-liked soul comes to feel well thought of, got {}", w.agents[high].seen_as);
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    // The global workspace broadcasts a single uppermost concern: a charged grief outweighs the
    // day's ordinary work and takes the mind; a soul with nothing pressing rests on their work.
    #[test]
    fn the_strongest_concern_takes_the_mind() {
        let mut w = World::seed();
        let g: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        let (grieving, easy) = (g[0], g[1]);
        w.remember(grieving, "grief", g[2] as i32, -80, 85, 0);

        compute_focus(&mut w, grieving);
        compute_focus(&mut w, easy);
        assert_eq!(w.agents[grieving].focus.topic, "grief", "a fresh grief takes the mind");
        assert!(w.agents[grieving].focus.intensity >= GRIP, "and grips it");
        assert_eq!(w.agents[easy].focus.topic, "work", "an unburdened soul's mind rests on the day's work");
        assert!(mind_occupied(&w.agents[grieving]) && !mind_occupied(&w.agents[easy]));
    }

    // The workspace, occupied: a soul whose mind is filled by grief cannot take up a fresh enmity,
    // though they carry a sharp grievance — there is no room in the mind for it. An easy mind can.
    #[test]
    fn an_occupied_mind_does_not_take_up_new_aims() {
        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();
        let setup = |occupied: bool| {
            let mut w = World::seed();
            let g: Vec<usize> = (0..w.agents.len())
                .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
            let (soul, foe) = (g[0], g[1]);
            w.agents[soul].spouse = None; w.agents[soul].parent = None;
            w.agents[foe].spouse = None; w.agents[foe].parent = None;
            w.agents[soul].rival = -1;
            w.remember(soul, "snub", foe as i32, -60, 75, 0);
            w.agents[soul].focus = if occupied {
                Preoccupation { topic: "grief".into(), target: -1, intensity: 90 }
            } else {
                Preoccupation { topic: "work".into(), target: -1, intensity: 18 }
            };
            let mut declared = false;
            for day in 0..300 {
                if !w.agents[soul].memories.iter().any(|m| m.kind == "snub" && m.who == foe as i32) {
                    w.remember(soul, "snub", foe as i32, -60, 75, day);
                }
                // hold the focus fixed (compute_focus isn't run in this isolated loop)
                let _ = form_aims(&mut w, day, date, 7);
                if w.agents[soul].rival == foe as i32 { declared = true; break; }
            }
            declared
        };
        assert!(!setup(true), "a mind filled with grief takes up no new enmity");
        assert!(setup(false), "an easy mind, carrying the same grievance, does take it up");
    }
}

#[cfg(test)]
mod act_tests {
    use super::*;

    // A soul's chosen action is no mere narration: it moves the world by a fixed, bounded amount,
    // and — crucially — it deposits a memory in the OTHER soul, so an action begets a reaction. A
    // confront leaves a slight the other now carries; a reconcile mends the tie and lets the grudge go.
    #[test]
    fn an_act_moves_the_world_and_is_carried_by_the_other() {
        let mut w = World::seed();
        let grown: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .collect();
        let (a, b) = (grown[0], grown[1]);
        let an = w.agents[a].name.clone();
        let bn = w.agents[b].name.clone();
        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();

        // they are at odds: a declared grudge, a soured tie, a remembered slight
        w.nudge_aff(a, b, -60);
        w.nudge_aff(b, a, -60);
        w.agents[a].rival = b as i32;
        w.remember(a, "snub", b as i32, -60, 70, 0);

        // a confront leaves a fresh slight the OTHER now carries — the reaction seed that may move them
        let confront = Decree { subject: an.clone(), kind: "act".into(), target: bn.clone(), choice: "confront".into(), text: "had words".into() };
        apply_decrees(&mut w, 1, date, std::slice::from_ref(&confront));
        assert!(w.agents[b].memories.iter().any(|m| m.kind == "snub" && m.who == a as i32),
            "the one confronted carries the slight — action begets reaction");

        // a reconcile mends the soured tie both ways and lets the grudge go
        let before = w.aff(a, b);
        let reconcile = Decree { subject: an.clone(), kind: "act".into(), target: bn.clone(), choice: "reconcile".into(), text: "made peace".into() };
        apply_decrees(&mut w, 2, date, std::slice::from_ref(&reconcile));
        assert!(w.aff(a, b) > before, "reconciling warms the soured tie");
        assert_eq!(w.agents[a].rival, -1, "the grudge is let go");
        assert!(!w.agents[a].memories.iter().any(|m| m.kind == "snub" && m.who == b as i32),
            "the old slights between them are released");
    }

    // An offer moves money within the giver's means — never beyond — and the taker is the better for it.
    #[test]
    fn an_offer_moves_money_within_means() {
        let mut w = World::seed();
        let grown: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .collect();
        let (a, b) = (grown[0], grown[1]);
        w.agents[a].purse = 40;
        w.agents[b].purse = 2;
        let (an, bn) = (w.agents[a].name.clone(), w.agents[b].name.clone());
        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();
        let (ga, gb) = (w.agents[a].purse, w.agents[b].purse);

        let offer = Decree { subject: an, kind: "act".into(), target: bn, choice: "offer".into(), text: "a helping hand".into() };
        apply_decrees(&mut w, 1, date, std::slice::from_ref(&offer));
        let given = ga - w.agents[a].purse;
        assert!(given > 0 && given <= 15, "the gift is real but bounded to the giver's means, got {given}");
        assert_eq!(w.agents[b].purse - gb, given, "what the giver loses, the taker gains");
        assert!(w.agents[a].purse >= 0, "a soul never gives themselves into the red");
    }

    // The gravest choice on the spine: a soul who chooses to GO leaves the cast for good — departed,
    // not dead (so no funeral) — and those who held them dear carry the loss like a living bereavement.
    #[test]
    fn a_departure_takes_a_soul_off_stage_and_is_mourned() {
        let mut w = World::seed();
        let grown: Vec<usize> = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child")
            .collect();
        let (leaver, dear) = (grown[0], grown[1]);
        w.nudge_aff(dear, leaver, 60); // someone holds them dear
        let ln = w.agents[leaver].name.clone();
        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();

        let go = Decree { subject: ln, kind: "depart".into(), target: String::new(), choice: "go".into(), text: "left for good".into() };
        apply_decrees(&mut w, 1, date, std::slice::from_ref(&go));
        assert!(!w.agents[leaver].active(), "a soul who goes leaves the cast");
        assert!(w.agents[leaver].death_day.is_none(), "they left alive — no death, and so no funeral");
        assert!(w.agents[dear].memories.iter().any(|m| m.kind == "grief" && m.who == leaver as i32),
            "one who held them dear carries the loss");
    }

    // The first two-sided decision: a ripe courtship comes to its question. An ACCEPT weds the two
    // (the answer joins their lives); a REFUSE breaks the suit and leaves the suitor stung.
    #[test]
    fn a_betrothal_accepted_weds_refused_breaks_the_suit() {
        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();

        // accept: subject = the courted (who answers), target = the suitor
        let mut w = World::seed();
        let g: Vec<usize> = (0..w.agents.len()).filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
        let (suitor, courted) = (g[0], g[1]);
        w.agents[suitor].spouse = None; w.agents[courted].spouse = None;
        w.agents[suitor].courting = courted as i32;
        w.agents[suitor].courtship = 40;
        w.nudge_aff(suitor, courted, 50);
        w.nudge_aff(courted, suitor, 40);
        let (sn, cn) = (w.agents[suitor].name.clone(), w.agents[courted].name.clone());
        let accept = Decree { subject: cn, kind: "betroth".into(), target: sn, choice: "accept".into(), text: "she took him".into() };
        apply_decrees(&mut w, 1, date, std::slice::from_ref(&accept));
        assert_eq!(w.agents[suitor].spouse, Some(courted), "the suitor is wed to the one who accepted");
        assert_eq!(w.agents[courted].spouse, Some(suitor), "and she to him");
        assert!(w.agents[courted].memories.iter().any(|m| m.kind == "wed" && m.who == suitor as i32),
            "the match is laid down as a charged memory");

        // refuse: a fresh pair — the suit is broken, no one wed
        let mut w2 = World::seed();
        let g2: Vec<usize> = (0..w2.agents.len()).filter(|&i| w2.agents[i].active() && w2.agents[i].archetype != "child").collect();
        let (s2, c2) = (g2[0], g2[1]);
        w2.agents[s2].spouse = None; w2.agents[c2].spouse = None;
        w2.agents[s2].courting = c2 as i32;
        w2.agents[s2].courtship = 40;
        let (sn2, cn2) = (w2.agents[s2].name.clone(), w2.agents[c2].name.clone());
        let refuse = Decree { subject: cn2, kind: "betroth".into(), target: sn2, choice: "refuse".into(), text: "she would not".into() };
        apply_decrees(&mut w2, 1, date, std::slice::from_ref(&refuse));
        assert_eq!(w2.agents[s2].spouse, None, "a refusal weds no one");
        assert_eq!(w2.agents[s2].courting, -1, "the broken suit is given up");
    }

    // A farmer's gamble on the land: `safe` is a sure small gain; `gamble` is a real swing — it
    // makes the year or sets them back — and the outcome is a fixed, replay-safe roll, never nothing.
    #[test]
    fn a_gamble_swings_the_purse_safe_is_a_sure_small_gain() {
        let mut w = World::seed();
        let f = (0..w.agents.len())
            .find(|&i| w.agents[i].active() && matches!(w.agents[i].archetype.as_str(), "hill_farmer" | "scheming_improver"))
            .expect("the parish has farmers");
        let nm = w.agents[f].name.clone();
        let date = Date::from_calendar_date(1934, Month::June, 1).unwrap();

        let p0 = w.agents[f].purse;
        let safe = Decree { subject: nm.clone(), kind: "gamble".into(), target: String::new(), choice: "safe".into(), text: "played it safe".into() };
        apply_decrees(&mut w, 1, date, std::slice::from_ref(&safe));
        assert_eq!(w.agents[f].purse - p0, 6, "the safe course is a sure small gain");

        let p1 = w.agents[f].purse;
        let gamble = Decree { subject: nm, kind: "gamble".into(), target: String::new(), choice: "gamble".into(), text: "chanced it".into() };
        apply_decrees(&mut w, 2, date, std::slice::from_ref(&gamble));
        let d = w.agents[f].purse - p1;
        assert!(d == 30 || d == -22, "a gamble makes the year or sets them back, got {d}");
    }
}

#[cfg(test)]
mod body_tests {
    use super::*;

    // A soul is not a disembodied mind: the day's labour tells on the flesh, the working folk
    // hardest, and the flesh tells on the spirits — a worn, ill body drags the mood down.
    #[test]
    fn the_body_tires_with_labour_and_the_flesh_tells_on_the_spirits() {
        let mut w = World::seed();
        let hand = (0..w.agents.len()).find(|&i| w.agents[i].active() && w.agents[i].archetype == "blunt_hand").unwrap();
        let gent = (0..w.agents.len()).find(|&i| w.agents[i].active() && w.agents[i].archetype == "genteel_status_seeker").unwrap();
        w.agents[hand].vigour = 60;
        w.agents[gent].vigour = 60;
        let date = Date::from_calendar_date(1934, Month::July, 2).unwrap(); // the Hay season, the backs broken

        for d in 1..15 { tend_body(&mut w, d, date, 7); }
        assert!(w.agents[hand].vigour < w.agents[gent].vigour,
            "a labouring hand in hay tires harder than the genteel ({} vs {})", w.agents[hand].vigour, w.agents[gent].vigour);
        assert!((0..=100).contains(&w.agents[hand].vigour), "vigour stays in its bounds");

        // a spent, ill body drags the spirits down
        let s = gent;
        w.agents[s].vigour = 6;
        w.agents[s].health = 20;
        let before = w.agents[s].mood;
        tend_body(&mut w, 200, date, 7);
        assert!(w.agents[s].mood <= before, "a worn, ailing body sinks the spirits");
    }
}
