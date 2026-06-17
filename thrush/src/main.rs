//! `thrush` — the console and live monitor for Thrushcombe.
//!
//!   thrush init [--start YYYY-MM-DD] [--seed N]   create a world (epoch = today by default)
//!   thrush tick                                   advance the log to today
//!   thrush status                                 print the town at a glance
//!   thrush watch                                  live TUI monitor (q to quit)

use std::error::Error;
use std::io;
use std::time::Duration;

use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event as CEvent, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use time::{Date, Month, OffsetDateTime};

use thrush_core::{Phase, Report, Sim, TownDetail};

mod wasm_engine;
use wasm_engine::WasmPolicies;

#[derive(Parser)]
#[command(name = "thrush", about = "Thrushcombe — a small society, on the real calendar")]
struct Cli {
    #[arg(long, default_value = "world.db")]
    db: String,
    /// Run the behaviour layer through the sandboxed wasm policy guests.
    #[arg(long)]
    wasm: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

/// If `--wasm`, swap the sim's engine to the wasmtime-backed one (falling back to native
/// if the guest can't be loaded). Path overridable via THRUSH_WASM.
fn apply_engine(sim: &mut Sim, wasm: bool) {
    if !wasm {
        return;
    }
    let path = std::env::var("THRUSH_WASM").unwrap_or_else(|_| "wasm/policies.wasm".into());
    match WasmPolicies::load(&path) {
        Ok(e) => {
            sim.set_engine(Box::new(e));
            eprintln!("behaviour engine: wasm ({path})");
        }
        Err(e) => eprintln!("wasm engine unavailable ({e}); using native"),
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new world. Epoch (day zero) defaults to today; backdate with --start.
    Init {
        #[arg(long)]
        start: Option<String>,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Advance the chronicle forward to today (catches up missed days).
    Tick,
    /// Jump the town forward N whole days (default 1), then catch up — the world lives each
    /// jumped day in full, just as if the time had passed. Real life still ticks underneath.
    Jump {
        #[arg(long, default_value_t = 1)]
        days: i64,
    },
    /// Render salient un-narrated events in voice via the local Qwen oracle.
    Narrate {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Fetch Sofia's real weather (forecast horizon) and record it for the days ahead.
    Weather,
    /// Now and then, let Qwen invent a surprising one-off happening (effect-free flavour).
    Wildcard {
        /// Force one regardless of the throttle/chance.
        #[arg(long)]
        force: bool,
    },
    /// Put a soul's turning point (a feud, a ruin, a match) to Qwen with their dossier.
    Hinge {
        /// Resolve one even if the throttle would skip it.
        #[arg(long)]
        force: bool,
    },
    /// Let two souls fall into conversation of their own accord; record what it leaves.
    Converse {
        /// Stage one even if the town has already talked today.
        #[arg(long)]
        force: bool,
        /// Force a specific pair, by their two names piped: --between "Bee|Mr Tranter".
        #[arg(long, default_value = "")]
        between: String,
        /// Set the scene the talk happens in: --setting "a busy evening at the Pelican…".
        #[arg(long, default_value = "")]
        setting: String,
    },
    /// Advance the inner life: the most-overdue souls each take a quiet hour and carry their
    /// stream of consciousness forward a beat — recording the thought (self-memory) and its residue.
    Reflect {
        /// How many souls' streams to advance this run (each the next most overdue).
        #[arg(long, default_value_t = 1)]
        count: i64,
    },
    /// Write the life the parish would tell of each soul — backstory, character, a defining turn.
    /// Works through those who lack one; injected into talk and reflection so souls know each other.
    Biography {
        /// How many souls (still lacking one) to write this run.
        #[arg(long, default_value_t = 3)]
        limit: i64,
    },
    /// Deepen the inner life: the most-overdue soul steps back from the hour-by-hour stream and
    /// consolidates — revising who they take themselves to be, updating what they believe of the
    /// souls who weigh on them (a tracked theory of mind), and facing any crack in their self-model.
    Introspect {
        /// How many souls to consolidate this run (each the next most overdue, unless --target).
        #[arg(long, default_value_t = 1)]
        count: i64,
        /// Consolidate one named soul instead of working through the most overdue.
        #[arg(long, default_value = "")]
        target: String,
    },
    /// Question the town under an open murder: the magistrate takes statements (alibi, and any
    /// blame cast), recorded and read out — they fold into suspicion. Works through the unquestioned.
    Interrogate {
        /// How many souls to question this run (each the next most-suspected, unless --target).
        #[arg(long, default_value_t = 1)]
        count: i64,
        /// Question one named soul instead of working through the most-suspected.
        #[arg(long, default_value = "")]
        target: String,
    },
    /// Put the open murder's reckoning to the magistrate: when suspicion has settled past bearing on
    /// one soul, the oracle rules in his voice — to ACCUSE (formal trial), HOLD (stay his hand for
    /// want of proof), or WIDEN (refuse the fixation, question the parish anew). The ruling drives the
    /// world: an accusation is a real charge, recorded and folded. No proof exists — only his judgement.
    Judge,
    /// Call an emergency town meeting over the open murder: the magistrate gives his account before
    /// a frightened parish and hears its fears voiced, and the oracle judges how the room comes away
    /// — CALMED (the dread breaks), INFLAMED (the mob demands a scapegoat NOW), or DIVIDED. The
    /// outcome drives the town's dread and the cloud over the hunted; the full account is kept.
    TownHall,
    /// Let the pressed souls act of their own accord: those whom something grips (a preoccupation
    /// that fills the mind, or a plan ripe for a move) take ONE plain townsperson's action of the
    /// oracle's choosing — call, confront, court, offer, reconcile, or withdraw — and it drives the
    /// world (warming or souring ties, money, a grudge let go), recorded and folded. A cooldown and
    /// a pressedness floor keep the town calm: most souls, most days, simply live the ordinary sim.
    Act {
        /// How many pressed souls to move this run (each the next most-pressed who has not lately acted).
        #[arg(long, default_value_t = 1)]
        count: i64,
    },
    /// Put the gravest choice to a soul at the end of their rope: when ruin or the parish's suspicion
    /// has driven them past bearing, the oracle decides — in their own character — whether they STAY
    /// and endure, or GO and leave Thrushcombe for good (off-stage, alive). Recorded and folded.
    Depart {
        /// How many souls at the brink to put to the choice this run (each the next most desperate).
        #[arg(long, default_value_t = 1)]
        count: i64,
    },
    /// Put a ripe courtship to its question: when a suit has been long and faithfully paid and the
    /// warmth is mutual, the COURTED soul rules — in their own heart and station — whether to ACCEPT
    /// the proposal or REFUSE it. The first two-sided decision; an acceptance weds them. Recorded, folded.
    Betroth {
        /// How many ripe courtships to put to the question this run (each the next most advanced).
        #[arg(long, default_value_t = 1)]
        count: i64,
    },
    /// Put a gamble on the land to a farmer: in a growing season the oracle decides — by that
    /// farmer's nerve and need — whether they GAMBLE on a bold, risky venture or play it SAFE. The
    /// season's fortune is a fixed, replay-safe roll. Recorded and folded.
    Gamble {
        /// How many farmers to put to the choice this run (each the next hungriest, once a season).
        #[arg(long, default_value_t = 1)]
        count: i64,
    },
    /// Print the town at a glance.
    Status,
    /// Live monitor (q / Esc to quit).
    Watch,
    /// Play the novelist: inject circumstance the town will react to.
    /// KIND = letter | loan | legacy | scandal | stranger | murder | appoint | investigate | inquiry | funeral | haunt | secret | bond | proclaim.
    /// haunt lays a buried, faceless dread on --target (a repression: never fades, surfaces unbidden, no public trace; --amount sets its grip 1-100).
    /// secret grounds a hidden private truth on --target via --note, fed only into their own inner life (never public). --amount 1 marks them the TRUE killer of the open murder.
    Providence {
        kind: String,
        #[arg(long, default_value = "")]
        target: String,
        #[arg(long, default_value_t = 0)]
        amount: i32,
        #[arg(long, default_value = "")]
        note: String,
    },
}

const SYSTEM_PROMPT: &str = "You are the chronicler of Thrushcombe St Mary, a West-Country market town in 1934. Render the given event as ONE plain sentence (two at the very most), in the register of interwar English provincial writing — dry, observed, lightly wry, the small dignity and the small humiliation borne with grace. \
Be STRICTLY FAITHFUL to the event you are given: use only the people, places, and facts named in it, and NEVER invent a name, a person, a surname, or a detail that is not there. If a soul is named, name them exactly; do not conjure new villagers. \
Note especially: to say two people have 'grown thick' or 'grown close' means they have become friends or intimates — it is NEVER about bodily size, weight, or a swelling figure. Read such phrases as friendship. \
Do not escalate into farce, do not pile embellishment upon embellishment, do not stretch a small thing into an absurdity. One wry touch is plenty. No preamble, no quotation marks, no lists.";

/// One call to the recorded oracle. Returns the rendered prose, or None on failure
/// (container down, timeout) so a batch degrades gracefully instead of dying.
fn narrate_one(_agent: &ureq::Agent, _host: &str, _model: &str, _date: &str, text: &str) -> Option<String> {
    claude_text(SYSTEM_PROMPT, text)
}

/// The town's calendar shift (days), loaded from the world at startup. Lets a `jump` carry the
/// town forward while real life still ticks one day per day underneath. `today()` adds it, so
/// every existing "now" stays correct with no further plumbing.
static DAY_OFFSET: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn today() -> Date {
    let base = OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .date();
    base + time::Duration::days(DAY_OFFSET.load(std::sync::atomic::Ordering::Relaxed))
}

const WILDCARD_KINDS: &[&str] = &["fire", "windfall", "fair", "blight", "scandal", "stranger", "foundling", "wonder"];

const WILDCARD_PROMPT: &str = "You invent ONE surprising in-world incident for the chronicle of Thrushcombe St Mary, a 1934 West-Country market town. Respond ONLY as JSON: {\"kind\": ..., \"target\": ..., \"text\": ...}. \"kind\" must be exactly one of: fire (a blaze that costs someone), windfall (good fortune for someone — a prize, a legacy, a found purse), fair (a travelling fair or feast that lifts the whole town), blight (crop or animal sickness on the farms), scandal (a damaging revelation about someone), stranger (a mysterious newcomer arrives), foundling (a baby left at a door), wonder (a marvel with no material effect — a comet, a strange light, a curiosity). \"target\" is one townsperson's name from the list given, or \"the town\". \"text\" is one or two warm, wry sentences in the register of interwar English provincial comedy describing the incident — no quotation marks.";

/// Ask Qwen to invent a wildcard: an effect-kind (from the fixed vocabulary), a target, and
/// the prose. Returns (kind, target, text), or None on failure.
fn wildcard_one(_agent: &ureq::Agent, _host: &str, _model: &str, date: &str, season: &str, names: &[&str]) -> Option<(String, String, String)> {
    let prompt = format!("It is {date}, in the season of {season}. The townsfolk include: {}. Invent the incident.", names.join(", "));
    let parsed = claude_json(WILDCARD_PROMPT, &prompt)?;
    let kind = parsed.get("kind")?.as_str()?.trim().to_lowercase();
    let kind = if WILDCARD_KINDS.contains(&kind.as_str()) { kind } else { "wonder".to_string() };
    let target = parsed.get("target").and_then(|v| v.as_str()).map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("the town").to_string();
    let text = parsed.get("text")?.as_str()?.trim().to_string();
    (!text.is_empty()).then_some((kind, target, text))
}

fn day_hash(day: i64) -> u64 {
    (day as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((day as u64) >> 7).wrapping_mul(0xD1B5_4A32_D192_ED03)
}

const HINGE_PROMPT: &str = "You decide, in character, what a soul of Thrushcombe St Mary — a 1934 West-Country market town — does at a genuine turning point in their life. You are given who they are, their situation, and their recent history. Weigh it as they would, in the register of interwar English provincial life. Choose EXACTLY ONE of the allowed options and write one or two sentences, in period voice and no quotation marks, of what they resolve to do and why. Respond ONLY as JSON: {\"choice\": one of the allowed options, \"reason\": the sentence}.";

const CONVERSE_PROMPT: &str = "You write a short, natural conversation between two souls of Thrushcombe St Mary, a 1934 West-Country market town, as they fall into talk. Stay wholly in period voice and in character, each in a distinct voice true to their station and feeling. Neither knows anything of the world beyond 1934 — no machines that think, no modern notions, nothing past their own time; they speak only of the world they live in, and the lettered among them know the wider news while the labouring folk keep to the parish. At most a brief greeting, then get to something real — the season's work, money owed, a marriage, a grievance, a piece of news, a scheme, a sly dig. They must NOT merely echo or restate each other; each line should answer in earnest — ask after something, share news, agree and build on it, reminisce, confide, tease, or, only where there is real cause, press or disagree — so the talk goes somewhere without straining to top the last line. Let warmth follow their regard: warm where they are fond, dry or barbed where there is a real grudge (never open abuse), civil where they feel little either way. Let rank tell, but in register not insult — the lesser defers and does not openly affront a clear superior, the greater is gracious or coolly condescending, not a brawler. Vary the phrasing and do not lean on stock fillers — avoid starting lines with 'I daresay' or 'I warrant'. Write 4 to 6 short lines in all, alternating, each one or two sentences, prefixed with the speaker's name and a colon. No narration, no stage directions, no preamble.";

fn converse_scene(_agent: &ureq::Agent, _host: &str, _model: &str, a_brief: &str, b_brief: &str, relation: &str) -> Option<String> {
    let prompt = format!("{a_brief}\n{b_brief}\n{relation}\nThey meet and fall into conversation. Write it.");
    let s = claude_text(CONVERSE_PROMPT, &prompt)?;
    // strip the filler tic per line, keeping each "Name:" prefix intact
    let s = s
        .lines()
        .map(|ln| match ln.split_once(':') {
            Some((name, said)) => format!("{name}: {}", thrush_core::strip_filler(said.trim())),
            None => thrush_core::strip_filler(ln),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!s.is_empty()).then_some(s)
}

/// Judge how a conversation left each of the two souls. Returns ((warmth,memory,sway) for a,
/// then for b), each validated to the vocabulary.
#[allow(clippy::type_complexity)]
fn assess_pair(_agent: &ureq::Agent, _host: &str, _model: &str, a: &str, b: &str, transcript: &str) -> Option<((String, String, String), (String, String, String))> {
    let sys = format!(
        "You judge how a conversation has affected each of two souls of a 1934 West-Country town. Respond ONLY as JSON: \
         {{\"a\":{{\"warmth\":one of [warmer,colder,unchanged],\"memory\":one short sentence in {a}'s own voice of what they now think of {b},\"sway\":one of [none,debt,rise,prosper,content,reconcile]}},\"b\":{{\"warmth\":...,\"memory\":in {b}'s voice of {a},\"sway\":...}}}}. \
         sway is whether the talk changed what the soul wants: debt=resolved to clear their debts, rise=spurred to rise in the world, prosper=talked into making a fortune, content=talked down to rest content, reconcile=moved to mend a quarrel, none=unchanged.",
    );
    let prompt = format!("The conversation:\n{transcript}\n\nHow did it leave {a}, and how {b}?");
    let parsed = claude_json(&sys, &prompt)?;
    let one = |o: &serde_json::Value| -> Option<(String, String, String)> {
        let warmth = o.get("warmth")?.as_str()?.trim().to_lowercase();
        let warmth = ["warmer", "colder", "unchanged"].into_iter().find(|w| warmth.contains(w)).unwrap_or("unchanged").to_string();
        let memory = o.get("memory")?.as_str()?.trim().to_string();
        let sway = o.get("sway").and_then(|s| s.as_str()).unwrap_or("none").trim().to_lowercase();
        let sway = ["debt", "rise", "prosper", "content", "reconcile"].into_iter().find(|s| sway.contains(s)).unwrap_or("none").to_string();
        (!memory.is_empty()).then_some((warmth, memory, sway))
    };
    Some((one(parsed.get("a")?)?, one(parsed.get("b")?)?))
}

const REFLECT_PROMPT: &str = "You voice the private reflection of one soul of Thrushcombe St Mary, a 1934 West-Country market town, in a quiet hour to themselves. You are given who they are, their station, their ties, their recent days, and how the parish stands. \
CRUCIAL — this is a CONTINUOUS STREAM OF CONSCIOUSNESS, not a fresh start each time. The lines under 'The thread of their recent thinking' are THEIR OWN inner monologue from recent hours, oldest first. Carry that train of thought forward where it left off: pick up what was unfinished, let an earlier worry deepen or ease or give way, follow a resolve to where it now stands, change their mind if the days have changed it — but NEVER simply restate a thought already in the thread. Each hour is the next moment of one unbroken inner life. \
ABOVE ALL — if the dossier names WHAT IS UPPERMOST IN THEIR MIND, that one thing must rule this hour. A soul gripped by a grief, by the dread of the killing, by a wrong they cannot answer, cannot think evenly of other matters: let it crowd the thought, intrude on whatever else they try to turn to, and colour the whole hour. Only when the dossier says their mind is easy does thought range freely in the proportions below. \
When the mind IS easy: most of an hour's thought — about seven parts in ten — turns INWARD, on themselves and their own life: who they are and who they have become, what they have made of their years and what they still want of the ones left to them, their regrets and small hopes, their faith and their failings, whether their work and their days amount to what they would wish. The plain inward reckoning of a life, as such a person would truly turn it over alone. Only the rest — about three parts in ten — turns outward: to one particular soul they cannot put from their mind, to the town, to the season's work. \
Stay wholly in period voice and true to their station and schooling: they know only the world of 1934 and their own parish — nothing of machines that think, nothing of times to come, no modern words or notions. One or two sentences of genuine, plain, unforced inward thought — no quotation marks, no preamble. Let it be honest: a real grief may sink them, a real hope lift them, a long grievance harden, an old fondness soften — do not flatten every hour into bland contentment, and do not manufacture drama where the soul would feel none. \
Then judge how the hour has left them, ONLY as JSON: {\"thought\": the inward sentence(s), \"mood\": one of [lifts, sinks, steadies], \"sway\": one of [none, debt, rise, prosper, content], \"toward\": the EXACT name (from the dossier) of the one soul they mused on if their thought turned to a particular person, else \"\", \"regard\": one of [none, warmer, colder] — whether the thought warmed or soured how they hold that soul, \"resolve\": one of [none, court, confront, mend] — and only rarely: court=resolved to pay court to them, confront=resolved to set themselves against them, mend=resolved to make peace with them, \"plan\": one of [none, fortune, rise, venture] — a DATED resolve they mean to pursue over weeks, distinct from a mere change of heart and set ONLY when a real, durable ambition takes hold: fortune=to mend their fortunes (clear what they owe, put money by), rise=to better their standing in the parish, venture=a bold scheme that may make them or ruin them, \"revise\": one of [keep, abandon, harder] — meaningful ONLY if the dossier says they ALREADY pursue a plan: keep=hold to it as before, abandon=think better of it and set it down, harder=renew the resolve and raise their sights; if they have no plan in train, leave it keep}. \
mood is whether the contemplation lifted, lowered, or merely steadied their spirits. sway is whether they talked themselves into a new aim: debt=to clear what they owe, rise=to better their standing, prosper=to make their fortune, content=to cease striving and rest content, none=unchanged. Use toward/regard/resolve ONLY when the thought genuinely turned to one named person; a purely inward hour leaves them \"\", none, none. Set plan only when a true ambition with a horizon forms — most hours, and any hour where they already pursue a plan, leave it none.";

/// Pull the first {...} JSON object out of a reply that may wrap it in prose or code fences.
fn extract_json(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| s[start..=end].to_string())
}

/// The town's one oracle. Every voice and every choice now runs through the `claude` CLI on this
/// machine (Sonnet by default, override with CLAUDE_MODEL), against the local Claude subscription —
/// no API key, no Qwen, no per-token ledger. The system prompt is appended; the dossier/instruction
/// is piped on stdin. A failed call returns None and the job simply no-ops (best-effort, as before).
fn claude_text(system: &str, user: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_else(|_| "sonnet".into());
    let bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()); // absolute path for headless/cron
    let mut child = Command::new(bin)
        .arg("-p").arg("--model").arg(&model).arg("--append-system-prompt").arg(system)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().ok()?;
    { let mut si = child.stdin.take()?; si.write_all(user.as_bytes()).ok()?; } // drop closes stdin → claude runs
    let out = child.wait_with_output().ok()?;
    out.status.success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// As `claude_text`, but pulling the first JSON object out of the reply.
fn claude_json(system: &str, user: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&extract_json(&claude_text(system, user)?)?).ok()
}

/// The day's Anthropic spend ledger — a "DATE\tUSD" sidecar beside the world db. Host-side ops
/// state, never folded into the world; it only governs whether to call Claude or fall to Qwen.
fn spend_file(db: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(db).parent().filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.join(".anthropic_spend.tsv")
}

/// Read today's spend from the ledger (0 if the file is for an earlier day or absent).
fn spent_today(path: &std::path::Path, today: &str) -> f64 {
    std::fs::read_to_string(path).ok().and_then(|s| {
        let mut it = s.split_whitespace();
        (it.next()? == today).then(|| it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0))
    }).unwrap_or(0.0)
}

/// One reflection from the local Qwen oracle — returns the raw JSON verdict object.
fn reflect_qwen(_agent: &ureq::Agent, _host: &str, _model: &str, dossier: &str) -> Option<serde_json::Value> {
    claude_json(REFLECT_PROMPT, &format!("{dossier}\n\nWrite their reflection."))
}

/// One reflection via the oracle (now the `claude` CLI). Returns the raw JSON verdict object.
fn reflect_claude(_agent: &ureq::Agent, _key: &str, dossier: &str, _spend: &std::path::Path, _today: &str) -> Option<serde_json::Value> {
    claude_json(REFLECT_PROMPT, &format!("{dossier}\n\nWrite their reflection."))
}

type Verdict = (String, String, String, String, String, String, String, String);

/// Validate a raw reflection verdict to its vocabularies; `toward` must name a living adult,
/// else the regard/resolve that lean on it are dropped. Returns (thought, mood, sway, toward,
/// regard, resolve, plan, revise), the thought stripped of stock fillers. None if thought empty.
fn parse_reflection(p: &serde_json::Value, names: &[String]) -> Option<Verdict> {
    let thought = thrush_core::strip_filler(p.get("thought")?.as_str()?.trim());
    if thought.trim().is_empty() {
        return None;
    }
    let pick = |key: &str, opts: &[&str], def: &str| -> String {
        let v = p.get(key).and_then(|v| v.as_str()).unwrap_or(def).trim().to_lowercase();
        opts.iter().find(|o| v.contains(*o)).map(|s| s.to_string()).unwrap_or_else(|| def.to_string())
    };
    let mood = pick("mood", &["lifts", "sinks", "steadies"], "steadies");
    let sway = pick("sway", &["debt", "rise", "prosper", "content"], "none");
    let mut regard = pick("regard", &["warmer", "colder"], "none");
    let mut resolve = pick("resolve", &["court", "confront", "mend"], "none");
    let plan = pick("plan", &["fortune", "rise", "venture"], "none");
    let revise = pick("revise", &["abandon", "harder"], "keep");
    let raw = p.get("toward").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let toward = if raw.is_empty() {
        String::new()
    } else {
        names.iter().find(|n| n.as_str() == raw || raw.contains(n.as_str()) || n.eq_ignore_ascii_case(&raw)).cloned().unwrap_or_default()
    };
    if toward.is_empty() {
        regard = "none".into();
        resolve = "none".into();
    }
    Some((thought, mood, sway, toward, regard, resolve, plan, revise))
}

const TESTIMONY_PROMPT: &str = "You voice one soul of Thrushcombe St Mary, a 1934 West-Country market town, giving a statement to the magistrate, who under public outcry is questioning the whole parish about the night Mr Quint was murdered. You are given who they are and how they stand. In their own period voice, true to their station and schooling, write what they say to the magistrate: where they were that night and what they were doing (their alibi), and — if they are frightened, or have an enemy, or are pressed hard — they may cast the suspicion onto another soul by name. A soul of standing answers calm and brief; a cornered or common soul may bluster, plead, or point a finger to save themselves. If the dossier states a KNOWN ALIBI as settled fact, their account MUST reflect it truthfully and fully. Two to four sentences, first person, no quotation marks, no preamble. \
Then judge it ONLY as JSON: {\"statement\": what they said to the magistrate, \"alibi\": one of [none, weak, strong] — strong ONLY if they were genuinely witnessed by someone OUTSIDE their own household, or it is otherwise proven; a spouse or a servant of their own house vouching alone is merely weak; weak if unsupported or only their own family can speak for them; none if they can give no account of themselves at all, \"accuses\": the EXACT name (from the dossier) of the one soul they cast suspicion on, else \"\"}.";

/// One statement to the magistrate from the local Qwen oracle — raw JSON {statement,alibi,accuses}.
fn testimony_qwen(_agent: &ureq::Agent, _host: &str, _model: &str, dossier: &str) -> Option<serde_json::Value> {
    claude_json(TESTIMONY_PROMPT, &format!("{dossier}\n\nGive their statement to the magistrate."))
}

/// One statement to the magistrate via the oracle (the `claude` CLI). Raw JSON {statement,alibi,accuses}.
fn testimony_claude(_agent: &ureq::Agent, _key: &str, dossier: &str, _spend: &std::path::Path, _today: &str) -> Option<serde_json::Value> {
    claude_json(TESTIMONY_PROMPT, &format!("{dossier}\n\nGive their statement to the magistrate."))
}

/// Validate a statement: (statement, alibi∈[none,weak,strong], accuses→a living name or ""). None if empty.
fn parse_testimony(p: &serde_json::Value, names: &[String]) -> Option<(String, String, String)> {
    let statement = p.get("statement")?.as_str()?.trim().to_string();
    if statement.is_empty() {
        return None;
    }
    let a = p.get("alibi").and_then(|v| v.as_str()).unwrap_or("weak").trim().to_lowercase();
    let alibi = ["strong", "weak", "none"].iter().find(|o| a.contains(*o)).map(|s| s.to_string()).unwrap_or_else(|| "weak".into());
    let raw = p.get("accuses").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let accuses = if raw.is_empty() { String::new() }
        else { names.iter().find(|n| n.as_str() == raw || raw.contains(n.as_str()) || n.eq_ignore_ascii_case(&raw)).cloned().unwrap_or_default() };
    Some((statement, alibi, accuses))
}

// ------------------------------------------------------------------ the magistrate's ruling
// The oracle stops being a narrator here and becomes a decision-maker: it rules, in the magistrate's
// own character, what is to be done with the soul suspicion has settled on — and that ruling drives
// the world (an `accuse` is a real charge). Recorded as a `judgment` decree, so replay holds exact.
const JUDGMENT_PROMPT: &str = "You rule as a magistrate of Thrushcombe St Mary, a 1934 West-Country market town, sitting over an unsolved murder while a frightened parish clamours for an answer. Suspicion has settled hardest on one soul — but there is NO proof against them, only the town's fear and its grudges. You are given who you are, who they are, and how things stand. Rule as THAT MAN truly would — by his station, his conscience, and whether he fears the mob or scorns it. A proud genteel magistrate may shield one of his own kind, or may give the town a friendless labourer to hang and be done with it; a careful or a just man may stay his hand, or widen the net, for want of proof. There is no right answer — only his. \
Give ONLY a JSON object: {\"account\": a record of his ruling and the reason for it, 1 to 3 sentences, third person, in the period chronicle voice, no quotation marks and no preamble; \"ruling\": EXACTLY one of [accuse, hold, widen] — accuse to bring them to formal trial (the town will likely hang them, guilty or not), hold to stay his hand for want of proof and wait, widen to refuse to fix on the one soul and turn the inquiry on the whole parish; \"reason\": a few plain words on why}.";

/// The magistrate's ruling from the local Qwen oracle — raw JSON {account,ruling,reason}.
fn judgment_qwen(_agent: &ureq::Agent, _host: &str, _model: &str, dossier: &str) -> Option<serde_json::Value> {
    claude_json(JUDGMENT_PROMPT, &format!("{dossier}\n\nRule on the matter before you."))
}

/// The magistrate's ruling via the oracle (the `claude` CLI). Raw JSON {account,ruling,reason}.
fn judgment_claude(_agent: &ureq::Agent, _key: &str, dossier: &str, _spend: &std::path::Path, _today: &str) -> Option<serde_json::Value> {
    claude_json(JUDGMENT_PROMPT, &format!("{dossier}\n\nRule on the matter before you."))
}

/// Validate a ruling: (account, ruling∈[accuse,hold,widen]). Defaults to the cautious `hold` if the
/// oracle names no clear ruling — the world never charges a soul on a malformed or ambiguous verdict.
fn parse_judgment(p: &serde_json::Value) -> Option<(String, String)> {
    let account = p.get("account").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if account.is_empty() {
        return None;
    }
    let r = p.get("ruling").and_then(|v| v.as_str()).unwrap_or("hold").trim().to_lowercase();
    let ruling = ["accuse", "widen", "hold"].iter().find(|o| r.contains(*o)).map(|s| s.to_string()).unwrap_or_else(|| "hold".into());
    Some((account, ruling))
}

// ------------------------------------------------------------------ the town meeting
// A set-piece: the magistrate before a frightened parish over an unsolved murder. The oracle renders
// the whole meeting and judges how the room turns — calmer, or inflamed toward a scapegoat, or split.
const TOWNHALL_PROMPT: &str = "You render an emergency town meeting in Thrushcombe St Mary, a 1934 West-Country market town, called by the magistrate over an unsolved murder that has the parish in terror. You are given who the magistrate is, where the inquiry truly stands, whom the fear has fixed on, and who is in the room. Render the meeting as it would ACTUALLY unfold — the magistrate's address to the assembled parish, the voices raised from the floor (name them where the dossier names them), the temper of the room and how it shifts as men speak. Stay wholly in the period chronicle voice, vivid and particular, true to a frightened 1934 market town where the gentlefolk and the labouring poor do not fear alike and do not trust the same men, and where a terrified parish wants a name to hang. \
Give ONLY a JSON object: {\"account\": the full rendered meeting, 6 to 12 sentences, third person, period chronicle voice, no quotation marks and no preamble; \"outcome\": EXACTLY one of [calmed, inflamed, divided] — calmed if the magistrate steadies them and they will let justice be done right and slow, inflamed if they come away more afraid and demanding a scapegoat be charged and hanged now, divided if the room splits with no common mind; \"reason\": a few plain words on why the room turned as it did}.";

/// Validate a town-meeting verdict: (account, outcome∈[calmed,inflamed,divided]). Defaults to divided.
fn parse_townhall(p: &serde_json::Value) -> Option<(String, String)> {
    let account = p.get("account").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if account.is_empty() {
        return None;
    }
    let o = p.get("outcome").and_then(|v| v.as_str()).unwrap_or("divided").trim().to_lowercase();
    let outcome = ["calmed", "inflamed", "divided"].iter().find(|x| o.contains(*x)).map(|s| s.to_string()).unwrap_or_else(|| "divided".into());
    Some((account, outcome))
}

// ------------------------------------------------------------------ a soul's own action
// The general lever: a pressed soul is given their whole inner state and a menu of plain acts, and
// the oracle chooses — in that soul's own character — what they DO. The choice drives the world
// (a recorded `act` decree, folded to a bounded consequence). Narrator becomes agent.
const ACT_PROMPT: &str = "You move one soul of Thrushcombe St Mary, a 1934 West-Country market town. You are given who they are, all that grips and weighs on them, and the souls in their life. Something has moved them today to act. Choose what THIS soul — as they truly are, of their station and temper and present feeling — would actually do: ONE plain action of the sort a townsperson takes, or none at all. Do not make them bold if they are timid, or generous if they are hard, or forgiving if the wound is fresh; let the action follow from the person and what presses on them. \
Give ONLY a JSON object: {\"account\": a record of what they did and why, 1 to 3 sentences, third person, in the period chronicle voice, no quotation marks and no preamble; \"act\": EXACTLY one of [call, confront, court, offer, reconcile, withdraw]; \"who\": the EXACT name (from those listed in the dossier) of the one soul they act upon, or \"\" if they withdraw}.";

/// One soul's chosen action from the local Qwen oracle — raw JSON {account,act,who}.
fn act_qwen(_agent: &ureq::Agent, _host: &str, _model: &str, dossier: &str) -> Option<serde_json::Value> {
    claude_json(ACT_PROMPT, &format!("{dossier}\n\nChoose what they do."))
}

/// One soul's chosen action via the oracle (the `claude` CLI). Raw JSON {account,act,who}.
fn act_claude(_agent: &ureq::Agent, _key: &str, dossier: &str, _spend: &std::path::Path, _today: &str) -> Option<serde_json::Value> {
    claude_json(ACT_PROMPT, &format!("{dossier}\n\nChoose what they do."))
}

/// Validate an action: (account, act∈menu, who→a living name or ""). A targeted act that names no
/// valid soul falls back to `withdraw` — the world never acts on a phantom. None if the account is empty.
fn parse_act(p: &serde_json::Value, names: &[String]) -> Option<(String, String, String)> {
    let account = p.get("account").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if account.is_empty() {
        return None;
    }
    let a = p.get("act").and_then(|v| v.as_str()).unwrap_or("withdraw").trim().to_lowercase();
    let mut act = ["confront", "reconcile", "withdraw", "call", "court", "offer"].iter()
        .find(|o| a.contains(*o)).map(|s| s.to_string()).unwrap_or_else(|| "withdraw".into());
    let raw = p.get("who").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let who = if raw.is_empty() { String::new() }
        else { names.iter().find(|n| n.as_str() == raw || raw.contains(n.as_str()) || n.eq_ignore_ascii_case(&raw)).cloned().unwrap_or_default() };
    // a directed act with no real soul to act upon is no act at all — keep the world honest
    if act != "withdraw" && who.is_empty() { act = "withdraw".into(); }
    Some((account, act, who))
}

// ------------------------------------------------------------------ leaving the parish
// The gravest decision on the same spine: a soul driven past bearing by ruin or suspicion rules on
// their own life — stay and endure, or go for good. `go` takes them off-stage (departed). The
// world never empties a soul out of the parish on a malformed verdict — it defaults to `stay`.
const DEPART_PROMPT: &str = "You decide, in their own character, whether one soul of Thrushcombe St Mary — a 1934 West-Country market town — leaves the parish for good. Things have come to a hard pass for them: ruin, or the town's suspicion under an open murder. You are given who they are and all that weighs on them. Weigh it exactly as THEY would, by their station, their ties, their temper — a soul does not give up the only world they know lightly, yet ruin or a frightened parish's suspicion has sent many a one to the railway station with a single case and no looking back. There is no right answer — only theirs. \
Give ONLY a JSON object: {\"account\": a record of what they resolve and why, 1 to 3 sentences, third person, period chronicle voice, no quotation marks and no preamble; \"choice\": EXACTLY one of [stay, go] — stay to remain in Thrushcombe and endure what they must, go to leave it for good}.";

/// Validate a departure verdict: (account, choice∈[stay,go]). Defaults to `stay` if the oracle names
/// no clear choice — a soul is never turned out of the parish on an ambiguous verdict. None if empty.
fn parse_departure(p: &serde_json::Value) -> Option<(String, String)> {
    let account = p.get("account").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if account.is_empty() {
        return None;
    }
    let c = p.get("choice").and_then(|v| v.as_str()).unwrap_or("stay").trim().to_lowercase();
    let choice = if c.contains("go") && !c.contains("stay") { "go" } else { "stay" }.to_string();
    Some((account, choice))
}

// ------------------------------------------------------------------ a proposal answered
// The first two-sided decision: a suitor's long pursuit (built in the fold) comes to its question,
// and the COURTED soul answers. accept weds them; refuse breaks the suit. Defaults to refuse — a
// soul is never married off on an ambiguous verdict.
const BETROTH_PROMPT: &str = "You answer, in the courted soul's own heart and character, a proposal of marriage in Thrushcombe St Mary — a 1934 West-Country market town. A suitor has paid them long and faithful court, and now asks for their hand. You are given who the courted soul is, all that weighs on them, and who the suitor is. Weigh it exactly as THEY would, by their own feeling, their station, their prospects, what their kin and the parish would say — a match is not made on warmth alone, nor refused lightly when a life hangs on it; yet a soul may refuse a suit they cannot return, or one their family would never countenance. There is no right answer — only theirs. \
Give ONLY a JSON object: {\"account\": a record of how they answer and why, 1 to 3 sentences, third person, period chronicle voice, no quotation marks and no preamble; \"choice\": EXACTLY one of [accept, refuse] — accept to take the suitor and be married, refuse to decline the suit}.";

/// Validate a betrothal answer: (account, choice∈[accept,refuse]). Defaults to refuse on ambiguity.
fn parse_betrothal(p: &serde_json::Value) -> Option<(String, String)> {
    let account = p.get("account").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if account.is_empty() {
        return None;
    }
    let c = p.get("choice").and_then(|v| v.as_str()).unwrap_or("refuse").trim().to_lowercase();
    let choice = if c.contains("accept") && !c.contains("refuse") { "accept" } else { "refuse" }.to_string();
    Some((account, choice))
}

// ------------------------------------------------------------------ a gamble on the land
// A farmer weighs a bold risk on the land against the sure return of honest husbandry. The decision
// is the oracle's; the season's fortune is a fixed roll in the fold. Defaults to the cautious `safe`.
const GAMBLE_PROMPT: &str = "You decide, in a farmer's own character, whether they gamble on the land this season in Thrushcombe St Mary — a 1934 West-Country market town. A chance has come to sink what they have into a bold, risky venture that may make their year or ruin it, or to play it safe and take the small, sure return. You are given who they are and how they stand. Weigh it exactly as THIS farmer would — by their nerve, their debts, their need, what they can bear to lose; a desperate man may chance everything, a careful one never will. There is no right answer — only theirs. \
Give ONLY a JSON object: {\"account\": a record of what they resolve and why, 1 to 3 sentences, third person, period chronicle voice, no quotation marks and no preamble; \"choice\": EXACTLY one of [gamble, safe] — gamble to chance the risky venture, safe to take the sure return}.";

/// Validate a gamble verdict: (account, choice∈[gamble,safe]). Defaults to safe on ambiguity.
fn parse_gamble(p: &serde_json::Value) -> Option<(String, String)> {
    let account = p.get("account").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if account.is_empty() {
        return None;
    }
    let c = p.get("choice").and_then(|v| v.as_str()).unwrap_or("safe").trim().to_lowercase();
    let choice = if c.contains("gamble") && !c.contains("safe") { "gamble" } else { "safe" }.to_string();
    Some((account, choice))
}

const INTROSPECT_PROMPT: &str = "You voice the deepest hour of one soul of Thrushcombe St Mary, a 1934 West-Country market town — the hour they step back from the day's passing thoughts and take stock of their whole self. You are given who they are, who they have so far taken themselves to be, the recent run of their thinking, what has befallen them, and the souls who weigh on them with whatever they have believed of each. Wholly in their period voice and true to their station and schooling (they know only the world of 1934 and their own parish), do three things: \
1. SELF — set down their settled, revised sense of who they are NOW: not the mood of an hour but the durable self-concept they carry — the kind of person they take themselves to be, what they have made of their life and what they still want of it, the truths they own and the lies they tell themselves. Carry forward what they believed of themselves before, and let it shift only where the recent days have genuinely moved it. A few plain sentences, first person. \
2. BELIEFS — for one or two of the named souls who weigh on them, set down what they NOW privately believe about that person: their read of that soul's character and intentions, updated by what has lately passed between them. Particular and honest, in their own voice. \
3. FRACTURE — judge whether there is a real crack between who they believe themselves to be and what is actually so (or what they have done): none = no true contradiction; reckoning = they face it and let it change how they understand themselves, at a cost; denial = the contradiction is more than they can bear, and they push it down, refuse it, harden the old self-image against the evidence. Most hours are none; reserve reckoning and denial for a genuine collision. \
Respond ONLY as JSON: {\"self\": the revised self-concept in the first person, \"beliefs\": [{\"about\": EXACT name from the dossier, \"belief\": what they now believe of that soul}], \"fracture\": one of [none, reckoning, denial]}. No preamble, no quotation marks inside the strings.";

/// One consolidation of the inner life from the local Qwen oracle — raw JSON {self,beliefs,fracture}.
fn introspect_qwen(_agent: &ureq::Agent, _host: &str, _model: &str, dossier: &str) -> Option<serde_json::Value> {
    claude_json(INTROSPECT_PROMPT, &format!("{dossier}\n\nTake stock of themselves."))
}

/// One consolidation of the inner life via the oracle (the `claude` CLI). Raw JSON {self,beliefs,fracture}.
fn introspect_claude(_agent: &ureq::Agent, _key: &str, dossier: &str, _spend: &std::path::Path, _today: &str) -> Option<serde_json::Value> {
    claude_json(INTROSPECT_PROMPT, &format!("{dossier}\n\nTake stock of themselves."))
}

/// Validate a consolidation: (self_concept, beliefs as (name,text) for living adults, fracture∈
/// [none,reckoning,denial]). None if there is nothing usable (no self and no beliefs).
fn parse_introspect(p: &serde_json::Value, names: &[String]) -> Option<(String, Vec<(String, String)>, String)> {
    let self_concept = p.get("self").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let resolve_name = |raw: &str| names.iter().find(|n| n.as_str() == raw || raw.contains(n.as_str()) || n.eq_ignore_ascii_case(raw)).cloned();
    let mut beliefs = Vec::new();
    if let Some(arr) = p.get("beliefs").and_then(|v| v.as_array()) {
        for b in arr {
            let about = b.get("about").and_then(|v| v.as_str()).unwrap_or("").trim();
            let text = b.get("belief").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if let (Some(nm), false) = (resolve_name(about), text.is_empty()) {
                beliefs.push((nm, text));
            }
        }
    }
    let f = p.get("fracture").and_then(|v| v.as_str()).unwrap_or("none").trim().to_lowercase();
    let fracture = ["reckoning", "denial"].iter().find(|o| f.contains(*o)).map(|s| s.to_string()).unwrap_or_else(|| "none".into());
    (!self_concept.is_empty() || !beliefs.is_empty()).then_some((self_concept, beliefs, fracture))
}

const BIO_PROMPT: &str = "You write the biography of one soul of Thrushcombe St Mary, a 1934 West-Country market town — the life the parish would tell of them. You are given their settled facts: name, station, household, age, family, where they came from. Invent a warm, particular life that fits those facts exactly: where they were born and how they came to their place in the town, the shape of their character, a defining turn or two of their life, what they are known (or whispered) for in the parish, and a private hope or an old wound. Stay wholly in period and in keeping with their station — a labourer's life is not a gentlewoman's, and the lettered and the unlettered came to their lot by different roads. Three to five sentences, plain and vivid, in the third person, no quotation marks, no preamble, no lists. This is the story the parish tells of them.";

/// Write one soul's biography via Claude (records token cost). None on failure → Qwen fallback.
fn bio_claude(_agent: &ureq::Agent, _key: &str, facts: &str, _spend: &std::path::Path, _today: &str) -> Option<String> {
    claude_text(BIO_PROMPT, &format!("The facts: {facts}\n\nWrite their biography."))
}

/// Tidy a biography: drop a leading markdown heading line the model sometimes prepends
/// (e.g. "# Mrs Pelham"), and trim. The body is left as the prose it is.
fn clean_bio(s: &str) -> String {
    let s = s.trim();
    match s.strip_prefix('#') {
        Some(_) => s.splitn(2, '\n').nth(1).unwrap_or("").trim().to_string(),
        None => s.to_string(),
    }
}

/// Write one soul's biography via the local Qwen oracle. None on failure.
fn bio_qwen(_agent: &ureq::Agent, _host: &str, _model: &str, facts: &str) -> Option<String> {
    claude_text(BIO_PROMPT, &format!("The facts: {facts}\n\nWrite their biography."))
}

/// Put a soul's dilemma to the oracle (the `claude` CLI). Returns (choice, prose), choice validated.
fn hinge_one(_agent: &ureq::Agent, _host: &str, _model: &str, dossier: &str, options: &[String]) -> Option<(String, String)> {
    let prompt = format!("{dossier}\n\nAllowed choices: {}. Decide, in character.", options.join(", "));
    let parsed = claude_json(HINGE_PROMPT, &prompt)?;
    let choice = parsed.get("choice")?.as_str()?.trim().to_lowercase();
    let choice = options.iter().find(|o| choice.contains(o.as_str())).cloned()?;
    let reason = parsed.get("reason")?.as_str()?.trim().to_string();
    (!reason.is_empty()).then_some((choice, reason))
}

/// Fetch Sofia's daily weather (recent + forecast) from open-meteo and record it for days
/// not yet folded. Free, no key; recorded so the fold stays deterministic.
fn fetch_sofia_weather(sim: &mut Sim) -> Result<u32, Box<dyn Error>> {
    let url = "https://api.open-meteo.com/v1/forecast?latitude=42.6975&longitude=23.3242\
               &daily=precipitation_sum,temperature_2m_max,temperature_2m_min&timezone=Europe%2FSofia\
               &past_days=7&forecast_days=16";
    let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(30)).build();
    let resp: serde_json::Value = agent.get(url).call()?.into_json()?;
    let d = &resp["daily"];
    let empty = vec![];
    let times = d["time"].as_array().unwrap_or(&empty);
    let pr = d["precipitation_sum"].as_array().unwrap_or(&empty);
    let tx = d["temperature_2m_max"].as_array().unwrap_or(&empty);
    let tn = d["temperature_2m_min"].as_array().unwrap_or(&empty);
    let mut stored = 0;
    for i in 0..times.len() {
        let Some(ds) = times[i].as_str() else { continue };
        let date = parse_date(ds);
        let precip = pr.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let tmax = tx.get(i).and_then(|v| v.as_f64()).unwrap_or(15.0);
        let tmin = tn.get(i).and_then(|v| v.as_f64()).unwrap_or(8.0);
        if sim.record_weather(date, precip, tmax, tmin)? {
            stored += 1;
        }
    }
    Ok(stored)
}

fn parse_date(s: &str) -> Date {
    let p: Vec<&str> = s.split('-').collect();
    let y: i32 = p[0].parse().expect("year");
    let m: u8 = p[1].parse().expect("month");
    let d: u8 = p[2].parse().expect("day");
    Date::from_calendar_date(y, Month::try_from(m).expect("month 1-12"), d).expect("valid date")
}

/// The day's phase, read straight off the real clock — in companion mode the town's
/// phase *is* yours.
fn phase_now() -> &'static str {
    let h = OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .hour();
    match h {
        5..=7 => "dawn",
        8..=11 => "forenoon",
        12..=16 => "afternoon",
        17..=21 => "evening",
        _ => "night",
    }
}

fn bar(v: i32) -> String {
    let v = v.clamp(0, 100) as usize;
    let n = v / 10;
    format!("{}{}", "\u{2588}".repeat(n), "\u{00b7}".repeat(10 - n))
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    // load the world's standing calendar shift so today() reflects any prior jump
    if let Ok(sim) = Sim::open(&cli.db) {
        DAY_OFFSET.store(sim.day_offset(), std::sync::atomic::Ordering::Relaxed);
    }
    match cli.cmd {
        Cmd::Init { start, seed } => {
            let epoch = start.as_deref().map(parse_date).unwrap_or_else(today);
            let mut sim = Sim::init(&cli.db, epoch, seed)?;
            apply_engine(&mut sim, cli.wasm);
            let added = sim.catch_up(today(), cur_phase())?;
            println!(
                "Thrushcombe founded. epoch={epoch}  seed={seed}  logged {added} event(s) catching up to {}.",
                today()
            );
            print_status(&sim.report(today())?);
        }
        Cmd::Tick => {
            let mut sim = Sim::open(&cli.db)?;
            apply_engine(&mut sim, cli.wasm);
            let added = sim.catch_up(today(), cur_phase())?;
            println!("Advanced to {}. {added} new event(s) logged.", today());
        }
        Cmd::Jump { days } => {
            let mut sim = Sim::open(&cli.db)?;
            apply_engine(&mut sim, cli.wasm);
            let n = days.max(1);
            let from = today();
            let off = sim.jump(n)?;
            DAY_OFFSET.store(off, std::sync::atomic::Ordering::Relaxed); // so today() now reflects the jump
            // fold every jumped day in full, through its night — the town lives them exactly as
            // it would have at the real-life pace (deaths, feuds, the inquest, funerals, all of it)
            let added = sim.catch_up(today(), Phase::Night)?;
            println!("Jumped {n} day(s): {from} → {}. {added} new event(s) — the town lived them in full.", today());
            print_status(&sim.report(today())?);
        }
        Cmd::Narrate { limit } => {
            let sim = Sim::open(&cli.db)?;
            let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
            let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
            let agent = ureq::AgentBuilder::new()
                .timeout_read(Duration::from_secs(180)) // survive a cold model load
                .build();
            let items = sim.unnarrated_salient(limit)?;
            if items.is_empty() {
                println!("Nothing to narrate — the chronicle is current.");
            }
            let mut done = 0;
            for (id, date, text) in items {
                match narrate_one(&agent, &host, &model, &date, &text) {
                    Some(prose) => {
                        sim.save_narration(id, &prose)?;
                        println!("  {date}  {prose}");
                        done += 1;
                    }
                    None => {
                        eprintln!("oracle unavailable (is {host} up?) — stopping after {done} rendered.");
                        break;
                    }
                }
            }
        }
        Cmd::Weather => {
            let mut sim = Sim::open(&cli.db)?;
            let stored = fetch_sofia_weather(&mut sim)?;
            println!("Recorded {stored} day(s) of Sofia weather over the days ahead.");
        }
        Cmd::Wildcard { force } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let day = sim.target_day(t).max(0);
            // throttle: at most one every ~3 days, and only ~28% of eligible days
            let recent = sim.last_wildcard_day()?.is_some_and(|last| day - last < 3);
            if !force && (recent || day_hash(day) % 100 >= 28) {
                return Ok(());
            }
            let season = thrush_core::Season::of(t).name();
            let names = sim.grown_names(t);
            let pick: Vec<&str> = if names.is_empty() {
                vec![]
            } else {
                let n = names.len();
                let h = day_hash(day) as usize;
                (0..3).map(|k| names[h.wrapping_add(k * 7) % n].as_str()).collect()
            };
            let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
            let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(180)).build();
            match wildcard_one(&agent, &host, &model, &t.to_string(), season, &pick) {
                Some((kind, target, text)) => {
                    sim.record_wildcard(t, &kind, &target, &text)?;
                    sim.catch_up(today(), cur_phase())?; // re-fold the day so the effect lands
                    println!("Wildcard [{kind} · {target}]: {text}");
                }
                None => eprintln!("oracle unavailable ({host}) — no wildcard this time."),
            }
        }
        Cmd::Hinge { force } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let day = sim.target_day(t).max(0);
            // throttle: a turning point is a rare thing — at most one every ~2 days
            if !force && day_hash(day.wrapping_add(7)) % 100 >= 45 {
                return Ok(());
            }
            let Some(h) = sim.pending_hinge(t) else {
                if force {
                    println!("No soul is at a turning point just now.");
                }
                return Ok(());
            };
            // assemble the dossier: who they are + their recent history
            let a = &sim.world_snapshot(t).agents[h.subject];
            let recent: Vec<String> = sim.person_events(&h.subject_name, 6).map(|es| es.into_iter().map(|e| format!("  {} — {}", e.date, e.text)).collect()).unwrap_or_default();
            let dossier = format!(
                "{name}, {role}, of {seat}, aged {age}. Standing {standing}, purse {purse}£, presently {mood}.\nThe situation: {sit}\nRecent days for {name}:\n{hist}",
                name = h.subject_name,
                role = a.trade.clone().unwrap_or_else(|| arch_tag(&a.archetype).to_string()),
                seat = a.seat,
                age = a.age(day),
                standing = a.standing,
                purse = a.purse,
                mood = thrush_core::mood_word(a.mood),
                sit = h.situation,
                hist = if recent.is_empty() { "  (little to remark on of late)".to_string() } else { recent.join("\n") },
            );
            let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
            let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            match hinge_one(&agent, &host, &model, &dossier, &h.options) {
                Some((choice, reason)) => {
                    sim.record_decree(t, &h.subject_name, &h.kind, &h.target_name, &choice, &reason)?;
                    sim.catch_up(today(), cur_phase())?; // re-fold the day so the verdict lands
                    println!("Hinge [{} · {} → {choice}]: {reason}", h.kind, h.subject_name);
                }
                None => eprintln!("oracle unavailable ({host}) — the decision waits."),
            }
        }
        Cmd::Converse { force, between, setting } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let day = sim.target_day(t).max(0);
            // a forced pair (--between "A|B") always stages, bypassing the once-a-day cooldown
            let forced = (!between.is_empty()).then(|| between.split_once('|')).flatten();
            if forced.is_none() && !force && sim.last_dialogue_day()?.is_some_and(|last| last >= day) {
                return Ok(());
            }
            let pair = match forced {
                Some((a, b)) => sim.converse_pair_between(t, a.trim(), b.trim(), setting.trim()),
                None => sim.converse_pair(t),
            };
            let Some(p) = pair else {
                eprintln!("no such pair to set talking.");
                return Ok(());
            };
            let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
            let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            let Some(scene) = converse_scene(&agent, &host, &model, &p.a_brief, &p.b_brief, &p.relation) else {
                eprintln!("oracle unavailable ({host}) — no conversation today.");
                return Ok(());
            };
            match assess_pair(&agent, &host, &model, &p.a_name, &p.b_name, &scene) {
                Some(((wa, ma, sa), (wb, mb, sb))) => {
                    // record each soul's residue: record_dialogue(source, target) keeps target's memory of source
                    sim.record_dialogue(t, &p.a_name, &p.b_name, &scene, &mb, &wb, &sb)?; // b's memory of a
                    sim.record_dialogue(t, &p.b_name, &p.a_name, &scene, &ma, &wa, &sa)?; // a's memory of b
                    sim.catch_up(today(), cur_phase())?;
                    println!("Conversation [{} & {}]: {} came away {wa}; {} came away {wb}.", p.a_name, p.b_name, p.a_name, p.b_name);
                }
                None => eprintln!("oracle unavailable ({host}) — conversation unrecorded."),
            }
        }
        Cmd::Reflect { count } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let names: Vec<String> = sim.world_snapshot(t).agents.iter()
                .filter(|a| a.active() && a.archetype != "child").map(|a| a.name.clone()).collect();
            // prefer Claude (Haiku) for a sharper inner voice — but never past the day's Anthropic
            // cap (default $1; set ANTHROPIC_DAILY_USD). Beyond it, reflect on the free local Qwen.
            let cap: f64 = std::env::var("ANTHROPIC_DAILY_USD").ok().and_then(|x| x.parse().ok()).unwrap_or(1.0);
            let spend = spend_file(&cli.db);
            let today_str = t.to_string();
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            // the wall-clock hour only breaks ties among the equally-overdue; who reflects is decided
            // by who has gone longest without. Advance `count` souls' streams a beat each — after each
            // is recorded and re-folded, the next-most-overdue is a different soul.
            let salt = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()).hour() as u64;
            for _ in 0..count.max(1) {
                let Some(r) = sim.reflect_subject(t, salt) else { break };
                let claude_ok = cap - spent_today(&spend, &today_str) > 0.01;
                let raw = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).filter(|_| claude_ok)
                    .and_then(|key| reflect_claude(&agent, &key, &r.dossier, &spend, &today_str))
                    .or_else(|| {
                        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
                        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
                        reflect_qwen(&agent, &host, &model, &r.dossier)
                    });
                match raw.as_ref().and_then(|v| parse_reflection(v, &names)) {
                    Some((thought, mood, sway, toward, regard, resolve, plan, revise)) => {
                        sim.record_reflection(t, &r.name, &thought, &mood, &sway, &toward, &regard, &resolve, &plan, &revise)?;
                        sim.catch_up(today(), cur_phase())?; // re-fold so the residue lands + next pick differs
                        let tail = if revise != "keep" { format!(" · plan {revise}") }
                            else if plan != "none" { format!(" · set on {plan}") }
                            else if resolve != "none" { format!(" · resolved to {resolve} {toward}") }
                            else if regard != "none" { format!(" · {regard} toward {toward}") }
                            else { String::new() };
                        println!("Reflection [{} · {mood}/{sway}{tail}]: {thought}", r.name);
                    }
                    None => {
                        eprintln!("oracle unavailable — the stream stalls this beat.");
                        break;
                    }
                }
            }
        }
        Cmd::Introspect { count, target } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let names: Vec<String> = sim.world_snapshot(t).agents.iter()
                .filter(|a| a.active() && a.archetype != "child").map(|a| a.name.clone()).collect();
            let cap: f64 = std::env::var("ANTHROPIC_DAILY_USD").ok().and_then(|x| x.parse().ok()).unwrap_or(1.0);
            let spend = spend_file(&cli.db);
            let today_str = t.to_string();
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            let tgt = (!target.is_empty()).then_some(target.as_str());
            for _ in 0..count.max(1) {
                let Some(r) = sim.psyche_subject(t, tgt) else {
                    println!("No soul to consolidate.");
                    break;
                };
                let claude_ok = cap - spent_today(&spend, &today_str) > 0.01;
                let raw = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).filter(|_| claude_ok)
                    .and_then(|key| introspect_claude(&agent, &key, &r.dossier, &spend, &today_str))
                    .or_else(|| {
                        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
                        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
                        introspect_qwen(&agent, &host, &model, &r.dossier)
                    });
                match raw.as_ref().and_then(|v| parse_introspect(v, &names)) {
                    Some((self_concept, beliefs, fracture)) => {
                        sim.record_psyche(t, &r.name, &self_concept, &beliefs, &fracture)?;
                        sim.catch_up(today(), cur_phase())?; // fold any fracture residue + next pick differs
                        let bl = beliefs.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(", ");
                        let ftail = if fracture != "none" { format!(" · FRACTURE: {fracture}") } else { String::new() };
                        let btail = if bl.is_empty() { String::new() } else { format!(" · reads {bl}") };
                        println!("Introspection [{}{btail}{ftail}]: {}", r.name, self_concept);
                        if tgt.is_some() { break; }
                    }
                    None => {
                        eprintln!("oracle unavailable — the soul cannot gather itself this hour.");
                        break;
                    }
                }
            }
        }
        Cmd::Interrogate { count, target } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let names: Vec<String> = sim.world_snapshot(t).agents.iter()
                .filter(|a| a.active() && a.archetype != "child").map(|a| a.name.clone()).collect();
            let cap: f64 = std::env::var("ANTHROPIC_DAILY_USD").ok().and_then(|x| x.parse().ok()).unwrap_or(1.0);
            let spend = spend_file(&cli.db);
            let today_str = t.to_string();
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            let tgt = (!target.is_empty()).then_some(target.as_str());
            for _ in 0..count.max(1) {
                let Some(r) = sim.testimony_subject(t, tgt) else {
                    println!("No one left for the magistrate to question.");
                    break;
                };
                let claude_ok = cap - spent_today(&spend, &today_str) > 0.01;
                let raw = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).filter(|_| claude_ok)
                    .and_then(|key| testimony_claude(&agent, &key, &r.dossier, &spend, &today_str))
                    .or_else(|| {
                        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
                        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
                        testimony_qwen(&agent, &host, &model, &r.dossier)
                    });
                match raw.as_ref().and_then(|v| parse_testimony(v, &names)) {
                    Some((statement, mut alibi, mut accuses)) => {
                        // Pete Peckers' alibi is a settled fact — he was mending Mr Sunter's tractor; it holds.
                        if r.name == "Mr Pete Peckers" { alibi = "strong".into(); accuses = String::new(); }
                        // the magistrate reads out the telling statements — a clearing, a poor account, a finger pointed
                        let public = !accuses.is_empty() || alibi != "weak" || tgt.is_some();
                        sim.record_testimony(t, &r.name, &alibi, &accuses, public, &statement)?;
                        sim.catch_up(today(), cur_phase())?; // fold the effect on suspicion + next pick differs
                        let tail = if !accuses.is_empty() { format!(" · names {accuses}") } else { String::new() };
                        let vis = if public { "read out" } else { "in private" };
                        println!("Statement [{} · alibi {alibi}{tail} · {vis}]: {statement}", r.name);
                        if tgt.is_some() { break; } // a named questioning is a single act
                    }
                    None => {
                        eprintln!("oracle unavailable — the questioning halts.");
                        break;
                    }
                }
            }
        }
        Cmd::Judge => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            match sim.pending_judgment(t) {
                None => println!("No ruling is before the magistrate — either the cloud has not settled past bearing on any one soul, the bench is in a cooling after a recent ruling, or the case is already decided."),
                Some((mag, suspect, dossier)) => {
                    let cap: f64 = std::env::var("ANTHROPIC_DAILY_USD").ok().and_then(|x| x.parse().ok()).unwrap_or(1.0);
                    let spend = spend_file(&cli.db);
                    let today_str = t.to_string();
                    let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
                    let claude_ok = cap - spent_today(&spend, &today_str) > 0.01;
                    let raw = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).filter(|_| claude_ok)
                        .and_then(|key| judgment_claude(&agent, &key, &dossier, &spend, &today_str))
                        .or_else(|| {
                            let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
                            let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
                            judgment_qwen(&agent, &host, &model, &dossier)
                        });
                    match raw.as_ref().and_then(parse_judgment) {
                        Some((account, ruling)) => {
                            sim.record_judgment(t, &mag, &suspect, &ruling, &account)?;
                            sim.catch_up(today(), cur_phase())?; // fold the ruling — a charge, or a stay
                            println!("Ruling [{mag} · {ruling} · re {suspect}]: {account}");
                        }
                        None => eprintln!("oracle unavailable — the magistrate reserves his judgement this day."),
                    }
                }
            }
        }
        Cmd::TownHall => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            match sim.townhall_brief(t) {
                None => println!("No murder is open — there is nothing to call the parish together over."),
                Some((mag, dossier)) => {
                    let raw = claude_json(TOWNHALL_PROMPT, &format!("{dossier}\n\nRender the meeting and judge how the parish comes away."));
                    match raw.as_ref().and_then(parse_townhall) {
                        Some((account, outcome)) => {
                            sim.record_townhall(t, &mag, &outcome, &account)?;
                            sim.catch_up(today(), cur_phase())?; // fold the outcome — the dread breaks, or boils over
                            println!("Town meeting [{mag} · the parish came away {outcome}]:\n{account}");
                        }
                        None => eprintln!("oracle unavailable — the meeting goes unrecorded."),
                    }
                }
            }
        }
        Cmd::Act { count } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let names: Vec<String> = sim.world_snapshot(t).agents.iter()
                .filter(|a| a.active() && a.archetype != "child").map(|a| a.name.clone()).collect();
            let cap: f64 = std::env::var("ANTHROPIC_DAILY_USD").ok().and_then(|x| x.parse().ok()).unwrap_or(1.0);
            let spend = spend_file(&cli.db);
            let today_str = t.to_string();
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            for _ in 0..count.max(1) {
                let Some((actor, dossier)) = sim.action_subject(t) else {
                    println!("No soul is pressed to act just now — the town is at its ordinary business.");
                    break;
                };
                let claude_ok = cap - spent_today(&spend, &today_str) > 0.01;
                let raw = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).filter(|_| claude_ok)
                    .and_then(|key| act_claude(&agent, &key, &dossier, &spend, &today_str))
                    .or_else(|| {
                        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
                        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
                        act_qwen(&agent, &host, &model, &dossier)
                    });
                match raw.as_ref().and_then(|v| parse_act(v, &names)) {
                    Some((account, act, who)) => {
                        sim.record_act(t, &actor, &act, &who, &account)?;
                        sim.catch_up(today(), cur_phase())?; // fold the act — its real effect, and the next pick differs
                        let tail = if who.is_empty() { String::new() } else { format!(" → {who}") };
                        println!("Act [{actor} · {act}{tail}]: {account}");
                    }
                    None => {
                        eprintln!("oracle unavailable — the soul does not stir this hour.");
                        break;
                    }
                }
            }
        }
        Cmd::Depart { count } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            for _ in 0..count.max(1) {
                let Some((actor, dossier)) = sim.pending_departure(t) else {
                    println!("No soul is at the brink of leaving — the parish holds them yet.");
                    break;
                };
                let raw = claude_json(DEPART_PROMPT, &format!("{dossier}\n\nDecide: do they stay, or go?"));
                match raw.as_ref().and_then(parse_departure) {
                    Some((account, choice)) => {
                        sim.record_departure(t, &actor, &choice, &account)?;
                        sim.catch_up(today(), cur_phase())?; // fold it — a leaving is for good
                        println!("Departure [{actor} · {choice}]: {account}");
                    }
                    None => {
                        eprintln!("oracle unavailable — the soul cannot bring themselves to decide.");
                        break;
                    }
                }
            }
        }
        Cmd::Betroth { count } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            for _ in 0..count.max(1) {
                let Some((courted, suitor, dossier)) = sim.pending_betrothal(t) else {
                    println!("No courtship has yet ripened to its question.");
                    break;
                };
                let raw = claude_json(BETROTH_PROMPT, &format!("{dossier}\n\nThe suitor is {suitor}. How do they answer?"));
                match raw.as_ref().and_then(parse_betrothal) {
                    Some((account, choice)) => {
                        sim.record_betrothal(t, &courted, &suitor, &choice, &account)?;
                        sim.catch_up(today(), cur_phase())?; // fold it — an acceptance weds them
                        println!("Betrothal [{courted} · {choice} · {suitor}]: {account}");
                    }
                    None => {
                        eprintln!("oracle unavailable — the answer is not given this day.");
                        break;
                    }
                }
            }
        }
        Cmd::Gamble { count } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            for _ in 0..count.max(1) {
                let Some((farmer, dossier)) = sim.pending_gamble(t) else {
                    println!("No farmer is at a gamble on the land just now.");
                    break;
                };
                let raw = claude_json(GAMBLE_PROMPT, &format!("{dossier}\n\nDo they gamble, or play it safe?"));
                match raw.as_ref().and_then(parse_gamble) {
                    Some((account, choice)) => {
                        sim.record_gamble(t, &farmer, &choice, &account)?;
                        sim.catch_up(today(), cur_phase())?; // fold it — the season's fortune is rolled
                        println!("Gamble [{farmer} · {choice}]: {account}");
                    }
                    None => {
                        eprintln!("oracle unavailable — the farmer holds off deciding.");
                        break;
                    }
                }
            }
        }
        Cmd::Biography { limit } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let todo = sim.souls_without_bio(t);
            if todo.is_empty() {
                println!("Every soul has a biography.");
                return Ok(());
            }
            let cap: f64 = std::env::var("ANTHROPIC_DAILY_USD").ok().and_then(|x| x.parse().ok()).unwrap_or(1.0);
            let spend = spend_file(&cli.db);
            let today_str = t.to_string();
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            let mut done = 0;
            for name in todo.into_iter().take(limit.max(0) as usize) {
                let Some(facts) = sim.bio_facts(&name, t) else { continue };
                let claude_ok = cap - spent_today(&spend, &today_str) > 0.01;
                let bio = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).filter(|_| claude_ok)
                    .and_then(|key| bio_claude(&agent, &key, &facts, &spend, &today_str))
                    .or_else(|| {
                        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
                        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
                        bio_qwen(&agent, &host, &model, &facts)
                    });
                match bio {
                    Some(b) => {
                        let b = clean_bio(&b);
                        done += 1;
                        println!("Biography [{name}]: {}…", b.chars().take(90).collect::<String>());
                        sim.record_biography(&name, &b)?;
                    }
                    None => eprintln!("oracle unavailable — {name}'s life goes unwritten this run."),
                }
            }
            println!("{done} written; {} still want one.", sim.souls_without_bio(t).len());
        }
        Cmd::Status => {
            let mut sim = Sim::open(&cli.db)?;
            apply_engine(&mut sim, cli.wasm);
            sim.catch_up(today(), cur_phase())?;
            print_status(&sim.report(today())?);
        }
        Cmd::Watch => {
            let mut sim = Sim::open(&cli.db)?;
            apply_engine(&mut sim, cli.wasm);
            run_watch(&mut sim)?;
        }
        Cmd::Providence { kind, target, amount, note } => {
            let mut sim = Sim::open(&cli.db)?;
            apply_engine(&mut sim, cli.wasm);
            sim.providence(today(), &kind, &target, amount, &note)?;
            let added = sim.catch_up(today(), cur_phase())?;
            println!("Providence — {kind} upon {target}. {added} event(s) re-folded; the town will make of it what it will.");
            print_status(&sim.report(today())?);
        }
    }
    Ok(())
}

fn print_status(r: &Report) {
    println!("\n  THRUSHCOMBE ST MARY        {}  {}   ·   day {}", r.weekday, r.date, r.day);
    println!("  {} — {}   ({})\n", r.season, phase_now(), r.armed);
    if let Some(fear) = &r.fear {
        println!("  ☠  {fear}\n");
    }
    println!("  The town");
    for a in &r.agents {
        println!(
            "    {:<22} {} {:>3}y  standing {}  purse £{}",
            a.name,
            short_arch(&a.archetype),
            a.age(r.day),
            bar(a.standing),
            a.purse
        );
    }
    println!("\n  Stock");
    for an in &r.animals {
        let gest = if an.gest > 0 { format!("in calf {}d", an.gest) } else { "—".into() };
        println!("    {:<12} ({:<18}) health {}  £{}  {}", an.name, an.owner, bar(an.health), an.value, gest);
    }
    if !r.news.is_empty() {
        println!("\n  News in flight");
        for nws in &r.news {
            println!("    ~ {nws}");
        }
    }
    if !r.pending.is_empty() {
        println!("\n  On the calendar");
        for p in &r.pending {
            println!("    · {p}");
        }
    }
    println!("\n  Chronicle");
    for c in &r.chronicle {
        println!("    {c}");
    }
    println!();
}

fn short_arch(a: &str) -> &'static str {
    match a {
        "genteel_status_seeker" => "[genteel]",
        "hill_farmer" => "[farmer] ",
        "practitioner" => "[vet]    ",
        "scheming_improver" => "[improver]",
        "blunt_hand" => "[hand]   ",
        "official" => "[parson] ",
        "child" => "[child]  ",
        _ => "[—]      ",
    }
}

// ----------------------------------------------------------------------------- TUI

fn run_watch(sim: &mut Sim) -> Result<(), Box<dyn Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = watch_loop(sim, &mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn cur_phase() -> Phase {
    let h = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()).hour();
    Phase::from_hour(h)
}

fn watch_loop<B: ratatui::backend::Backend>(
    sim: &mut Sim,
    terminal: &mut Terminal<B>,
) -> Result<(), Box<dyn Error>> {
    let mut state = ListState::default();
    state.select(Some(0));
    loop {
        sim.catch_up(today(), cur_phase())?;
        let d = sim.detail(today(), cur_phase())?;
        let n = d.people.len().max(1);
        let sel = state.selected().unwrap_or(0).min(n - 1);
        state.select(Some(sel));
        terminal.draw(|f| draw(f, &d, &mut state))?;
        if event::poll(Duration::from_millis(1000))? {
            if let CEvent::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => state.select(Some((sel + 1).min(n - 1))),
                    KeyCode::Up | KeyCode::Char('k') => state.select(Some(sel.saturating_sub(1))),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn arch_tag(a: &str) -> &'static str {
    match a {
        "genteel_status_seeker" => "genteel",
        "hill_farmer" => "farmer",
        "practitioner" => "practice",
        "scheming_improver" => "improver",
        "blunt_hand" => "working",
        "official" => "parish",
        "child" => "child",
        _ => "—",
    }
}

fn draw(f: &mut Frame, d: &TownDetail, state: &mut ListState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(f.area());

    // --- header ---
    let real = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let head = vec![
        Line::from(vec![
            Span::styled("THRUSHCOMBE ST MARY", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(format!("   {} {}  ·  ", d.weekday, d.date)),
            Span::styled(format!("[{}]", d.phase), Style::default().fg(Color::Cyan)),
            Span::raw(format!("  ·  {} souls", d.population)),
        ]),
        Line::from(vec![
            Span::styled(format!("{} — armed: {}", d.season, d.armed), Style::default().fg(Color::Green)),
            Span::styled(d.weather.as_ref().map(|w| format!("   ☁ {w}")).unwrap_or_default(), Style::default().fg(Color::Blue)),
        ]),
        Line::from(Span::styled(
            format!("real {:02}:{:02}  ·  sim-date = real-date  ·  ↑/↓ select a soul  ·  q to quit", real.hour(), real.minute()),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(head).block(Block::default().borders(Borders::BOTTOM)), root[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(30), Constraint::Percentage(28)])
        .split(root[1]);

    // left: selectable town board — name, where, doing
    let items: Vec<ListItem> = d.people.iter().map(|p| {
        ListItem::new(vec![
            Line::from(vec![
                Span::styled(format!("{:<22}", p.name), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {:>3}y {:<9}", p.age, arch_tag(&p.archetype)), Style::default().fg(Color::DarkGray)),
                Span::styled(bar(p.standing), Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::styled(format!("  {:<26}", trunc(&p.location, 26)), Style::default().fg(Color::Cyan)),
                Span::styled(p.doing.clone(), Style::default().fg(Color::Green)),
            ]),
        ])
    }).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(" The town, this {} ", d.phase)))
        .highlight_style(Style::default().bg(Color::Rgb(60, 50, 40)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, cols[0], state);

    // middle: the selected soul, then the day's events / gossip / calendar
    let sel = state.selected().unwrap_or(0).min(d.people.len().saturating_sub(1));
    let mut mid: Vec<Line> = Vec::new();
    if let Some(p) = d.people.get(sel) {
        mid.push(Line::from(Span::styled(p.name.clone(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
        let role = p.trade.clone().unwrap_or_else(|| arch_tag(&p.archetype).to_string());
        mid.push(Line::from(format!("{}, {}y · of {}", role, p.age, p.seat)));
        mid.push(Line::from(vec![
            Span::raw("wants "),
            Span::styled(p.wants.clone(), Style::default().fg(Color::Yellow)),
            Span::styled(format!("  · {}", p.mood), Style::default().fg(Color::DarkGray)),
        ]));
        if let Some(o) = &p.origin {
            mid.push(Line::from(Span::styled(format!("came from {o}"), Style::default().fg(Color::DarkGray))));
        }
        mid.push(Line::from(format!("standing {} {}", bar(p.standing), p.standing)));
        mid.push(Line::from(format!("purse    £{}", p.purse)));
        mid.push(Line::from(vec![Span::raw("now:  "), Span::styled(format!("{} · {}", p.location, p.doing), Style::default().fg(Color::Green))]));
        mid.push(Line::from(vec![Span::raw("next: "), Span::styled(p.next.clone(), Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))]));
        let mut kin = String::new();
        if let Some(s) = &p.spouse { kin.push_str(&format!("⚭ {}  ", s)); }
        if let Some(par) = &p.parent { kin.push_str(&format!("↑ {}  ", par)); }
        if !p.children.is_empty() { kin.push_str(&format!("↓ {}", p.children.join(", "))); }
        if !kin.is_empty() { mid.push(Line::from(Span::styled(kin, Style::default().fg(Color::Magenta)))); }
        if !p.friends.is_empty() {
            mid.push(Line::from(vec![Span::raw("friends: "), Span::styled(p.friends.join(", "), Style::default().fg(Color::Green))]));
        }
        if !p.rivals.is_empty() {
            mid.push(Line::from(vec![Span::raw("at odds: "), Span::styled(p.rivals.join(", "), Style::default().fg(Color::Red))]));
        }
        if !p.recent.is_empty() {
            mid.push(Line::from(""));
            mid.push(Line::from(Span::styled("their record", Style::default().fg(Color::DarkGray))));
            for e in &p.recent {
                mid.push(Line::from(format!("· {} {}", e.date, e.text)));
            }
        }
    }
    mid.push(Line::from(""));
    section(&mut mid, "Today in Thrushcombe", &d.global_today);
    section(&mut mid, "News in flight", &d.gossip);
    section(&mut mid, "On the calendar", &d.upcoming);
    f.render_widget(
        Paragraph::new(mid).block(Block::default().borders(Borders::ALL).title(" The soul & the day ")).wrap(Wrap { trim: true }),
        cols[1],
    );

    // right: chronicle
    let chron: Vec<Line> = d.recent.iter().map(|e| Line::from(format!("{}  {}", e.date, e.text))).collect();
    f.render_widget(
        Paragraph::new(chron).block(Block::default().borders(Borders::ALL).title(" Chronicle ")).wrap(Wrap { trim: true }),
        cols[2],
    );
}

fn section(out: &mut Vec<Line>, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push(Line::from(Span::styled(title.to_string(), Style::default().fg(Color::Magenta))));
    for i in items {
        out.push(Line::from(format!("  ~ {i}")));
    }
    out.push(Line::from(""));
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}
