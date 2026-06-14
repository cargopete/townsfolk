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
    },
    /// Let the soul most overdue take a quiet hour to themselves and reflect — on their life,
    /// their work, the town, their hopes. Records the thought (self-memory) and its residue.
    Reflect,
    /// Write the life the parish would tell of each soul — backstory, character, a defining turn.
    /// Works through those who lack one; injected into talk and reflection so souls know each other.
    Biography {
        /// How many souls (still lacking one) to write this run.
        #[arg(long, default_value_t = 3)]
        limit: i64,
    },
    /// Print the town at a glance.
    Status,
    /// Live monitor (q / Esc to quit).
    Watch,
    /// Play the novelist: inject circumstance the town will react to.
    /// KIND = letter | loan | legacy | scandal | stranger.
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

const SYSTEM_PROMPT: &str = "You are the chronicler of Thrushcombe St Mary, a small West-Country market town in 1934. Render the given event as a single short, warm, wry diary-style sentence or two, in the register of interwar English provincial comedy — gentle misfortune borne with dignity, the small humiliation, the understated joke. Never melodrama. No preamble, no quotation marks.";

/// One call to the recorded oracle. Returns the rendered prose, or None on failure
/// (container down, timeout) so a batch degrades gracefully instead of dying.
fn narrate_one(agent: &ureq::Agent, host: &str, model: &str, _date: &str, text: &str) -> Option<String> {
    let body = serde_json::json!({
        "model": model,
        "system": SYSTEM_PROMPT,
        "prompt": text,
        "think": false,
        "stream": false,
        "options": { "num_ctx": 4096, "temperature": 0.8 },
    });
    let resp: serde_json::Value = agent
        .post(&format!("{host}/api/generate"))
        .send_json(body)
        .ok()?
        .into_json()
        .ok()?;
    let s = resp.get("response")?.as_str()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn today() -> Date {
    OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .date()
}

const WILDCARD_KINDS: &[&str] = &["fire", "windfall", "fair", "blight", "scandal", "stranger", "foundling", "wonder"];

const WILDCARD_PROMPT: &str = "You invent ONE surprising in-world incident for the chronicle of Thrushcombe St Mary, a 1934 West-Country market town. Respond ONLY as JSON: {\"kind\": ..., \"target\": ..., \"text\": ...}. \"kind\" must be exactly one of: fire (a blaze that costs someone), windfall (good fortune for someone — a prize, a legacy, a found purse), fair (a travelling fair or feast that lifts the whole town), blight (crop or animal sickness on the farms), scandal (a damaging revelation about someone), stranger (a mysterious newcomer arrives), foundling (a baby left at a door), wonder (a marvel with no material effect — a comet, a strange light, a curiosity). \"target\" is one townsperson's name from the list given, or \"the town\". \"text\" is one or two warm, wry sentences in the register of interwar English provincial comedy describing the incident — no quotation marks.";

/// Ask Qwen to invent a wildcard: an effect-kind (from the fixed vocabulary), a target, and
/// the prose. Returns (kind, target, text), or None on failure.
fn wildcard_one(agent: &ureq::Agent, host: &str, model: &str, date: &str, season: &str, names: &[&str]) -> Option<(String, String, String)> {
    let prompt = format!("It is {date}, in the season of {season}. The townsfolk include: {}. Invent the incident.", names.join(", "));
    let body = serde_json::json!({
        "model": model, "system": WILDCARD_PROMPT, "prompt": prompt,
        "think": false, "stream": false, "format": "json", "options": { "num_ctx": 4096, "temperature": 0.95 },
    });
    let resp: serde_json::Value = agent.post(&format!("{host}/api/generate")).send_json(body).ok()?.into_json().ok()?;
    let parsed: serde_json::Value = serde_json::from_str(resp.get("response")?.as_str()?).ok()?;
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

fn converse_scene(agent: &ureq::Agent, host: &str, model: &str, a_brief: &str, b_brief: &str, relation: &str) -> Option<String> {
    let prompt = format!("{a_brief}\n{b_brief}\n{relation}\nThey meet and fall into conversation. Write it.");
    let body = serde_json::json!({
        "model": model, "system": CONVERSE_PROMPT, "prompt": prompt, "think": false, "stream": false,
        "options": { "num_ctx": 8192, "temperature": 0.9 },
    });
    let resp: serde_json::Value = agent.post(&format!("{host}/api/generate")).send_json(body).ok()?.into_json().ok()?;
    let s = resp.get("response")?.as_str()?.trim().to_string();
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
fn assess_pair(agent: &ureq::Agent, host: &str, model: &str, a: &str, b: &str, transcript: &str) -> Option<((String, String, String), (String, String, String))> {
    let sys = format!(
        "You judge how a conversation has affected each of two souls of a 1934 West-Country town. Respond ONLY as JSON: \
         {{\"a\":{{\"warmth\":one of [warmer,colder,unchanged],\"memory\":one short sentence in {a}'s own voice of what they now think of {b},\"sway\":one of [none,debt,rise,prosper,content,reconcile]}},\"b\":{{\"warmth\":...,\"memory\":in {b}'s voice of {a},\"sway\":...}}}}. \
         sway is whether the talk changed what the soul wants: debt=resolved to clear their debts, rise=spurred to rise in the world, prosper=talked into making a fortune, content=talked down to rest content, reconcile=moved to mend a quarrel, none=unchanged.",
    );
    let prompt = format!("The conversation:\n{transcript}\n\nHow did it leave {a}, and how {b}?");
    let body = serde_json::json!({
        "model": model, "system": sys, "prompt": prompt, "think": false, "stream": false, "format": "json",
        "options": { "num_ctx": 8192, "temperature": 0.5 },
    });
    let resp: serde_json::Value = agent.post(&format!("{host}/api/generate")).send_json(body).ok()?.into_json().ok()?;
    let parsed: serde_json::Value = serde_json::from_str(resp.get("response")?.as_str()?).ok()?;
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
Most of an hour's thought — about seven parts in ten — turns INWARD, on themselves and their own life: who they are and who they have become, what they have made of their years and what they still want of the ones left to them, their regrets and small hopes, their faith and their failings, whether their work and their days amount to what they would wish. The plain inward reckoning of a life, as such a person would truly turn it over alone. Only the rest — about three parts in ten — turns outward: to one particular soul they cannot put from their mind, to the town, to the season's work. \
Stay wholly in period voice and true to their station and schooling: they know only the world of 1934 and their own parish — nothing of machines that think, nothing of times to come, no modern words or notions. One or two sentences of genuine, plain, unforced inward thought — no quotation marks, no preamble. Let it be honest: a real grief may sink them, a real hope lift them, a long grievance harden, an old fondness soften — do not flatten every hour into bland contentment, and do not manufacture drama where the soul would feel none. \
Then judge how the hour has left them, ONLY as JSON: {\"thought\": the inward sentence(s), \"mood\": one of [lifts, sinks, steadies], \"sway\": one of [none, debt, rise, prosper, content], \"toward\": the EXACT name (from the dossier) of the one soul they mused on if their thought turned to a particular person, else \"\", \"regard\": one of [none, warmer, colder] — whether the thought warmed or soured how they hold that soul, \"resolve\": one of [none, court, confront, mend] — and only rarely: court=resolved to pay court to them, confront=resolved to set themselves against them, mend=resolved to make peace with them, \"plan\": one of [none, fortune, rise, venture] — a DATED resolve they mean to pursue over weeks, distinct from a mere change of heart and set ONLY when a real, durable ambition takes hold: fortune=to mend their fortunes (clear what they owe, put money by), rise=to better their standing in the parish, venture=a bold scheme that may make them or ruin them}. \
mood is whether the contemplation lifted, lowered, or merely steadied their spirits. sway is whether they talked themselves into a new aim: debt=to clear what they owe, rise=to better their standing, prosper=to make their fortune, content=to cease striving and rest content, none=unchanged. Use toward/regard/resolve ONLY when the thought genuinely turned to one named person; a purely inward hour leaves them \"\", none, none. Set plan only when a true ambition with a horizon forms — most hours, and any hour where they already pursue a plan, leave it none.";

/// Pull the first {...} JSON object out of a reply that may wrap it in prose or code fences.
fn extract_json(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| s[start..=end].to_string())
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

/// Add a call's cost to today's ledger, carrying forward (or resetting on a new day).
fn add_spend(path: &std::path::Path, today: &str, cost: f64) {
    let total = spent_today(path, today) + cost;
    let _ = std::fs::write(path, format!("{today}\t{total:.6}"));
}

/// One reflection from the local Qwen oracle — returns the raw JSON verdict object.
fn reflect_qwen(agent: &ureq::Agent, host: &str, model: &str, dossier: &str) -> Option<serde_json::Value> {
    let body = serde_json::json!({
        "model": model, "system": REFLECT_PROMPT,
        "prompt": format!("{dossier}\n\nWrite their reflection."),
        "think": false, "stream": false, "format": "json", "options": { "num_ctx": 8192, "temperature": 0.9 },
    });
    let resp: serde_json::Value = agent.post(&format!("{host}/api/generate")).send_json(body).ok()?.into_json().ok()?;
    serde_json::from_str(resp.get("response")?.as_str()?).ok()
}

/// One reflection from Claude (Haiku by default) over raw HTTP — a sharper inner voice. Records
/// the call's real token cost to the daily ledger. Returns the raw JSON verdict object; None on
/// any failure, so the caller can fall back to local Qwen.
fn reflect_claude(agent: &ureq::Agent, key: &str, dossier: &str, spend: &std::path::Path, today: &str) -> Option<serde_json::Value> {
    let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".into());
    let body = serde_json::json!({
        "model": model, "max_tokens": 512, "system": REFLECT_PROMPT,
        "messages": [{ "role": "user", "content": format!("{dossier}\n\nWrite their reflection. Respond only with the JSON object.") }],
    });
    let resp: serde_json::Value = agent
        .post("https://api.anthropic.com/v1/messages")
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_json(body).ok()?.into_json().ok()?;
    // tally the spend — Haiku 4.5 rates ($1/MTok in, $5/MTok out); a costlier override under-counts.
    let usage = resp.get("usage");
    let tok = |k: &str| usage.and_then(|u| u.get(k)).and_then(|v| v.as_f64()).unwrap_or(0.0);
    add_spend(spend, today, tok("input_tokens") / 1_000_000.0 + tok("output_tokens") * 5.0 / 1_000_000.0);
    let text = resp.get("content")?.as_array()?.iter()
        .filter_map(|b| (b.get("type").and_then(|t| t.as_str()) == Some("text")).then(|| b.get("text").and_then(|t| t.as_str())).flatten())
        .next()?;
    serde_json::from_str(&extract_json(text)?).ok()
}

type Verdict = (String, String, String, String, String, String, String);

/// Validate a raw reflection verdict to its vocabularies; `toward` must name a living adult,
/// else the regard/resolve that lean on it are dropped. Returns (thought, mood, sway, toward,
/// regard, resolve, plan), the thought stripped of stock fillers. None if the thought is empty.
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
    Some((thought, mood, sway, toward, regard, resolve, plan))
}

const BIO_PROMPT: &str = "You write the biography of one soul of Thrushcombe St Mary, a 1934 West-Country market town — the life the parish would tell of them. You are given their settled facts: name, station, household, age, family, where they came from. Invent a warm, particular life that fits those facts exactly: where they were born and how they came to their place in the town, the shape of their character, a defining turn or two of their life, what they are known (or whispered) for in the parish, and a private hope or an old wound. Stay wholly in period and in keeping with their station — a labourer's life is not a gentlewoman's, and the lettered and the unlettered came to their lot by different roads. Three to five sentences, plain and vivid, in the third person, no quotation marks, no preamble, no lists. This is the story the parish tells of them.";

/// Write one soul's biography via Claude (records token cost). None on failure → Qwen fallback.
fn bio_claude(agent: &ureq::Agent, key: &str, facts: &str, spend: &std::path::Path, today: &str) -> Option<String> {
    let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".into());
    let body = serde_json::json!({
        "model": model, "max_tokens": 400, "system": BIO_PROMPT,
        "messages": [{ "role": "user", "content": format!("The facts: {facts}\n\nWrite their biography.") }],
    });
    let resp: serde_json::Value = agent
        .post("https://api.anthropic.com/v1/messages")
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_json(body).ok()?.into_json().ok()?;
    let usage = resp.get("usage");
    let tok = |k: &str| usage.and_then(|u| u.get(k)).and_then(|v| v.as_f64()).unwrap_or(0.0);
    add_spend(spend, today, tok("input_tokens") / 1_000_000.0 + tok("output_tokens") * 5.0 / 1_000_000.0);
    let text = resp.get("content")?.as_array()?.iter()
        .filter_map(|b| (b.get("type").and_then(|t| t.as_str()) == Some("text")).then(|| b.get("text").and_then(|t| t.as_str())).flatten())
        .next()?.trim().to_string();
    (!text.is_empty()).then_some(text)
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
fn bio_qwen(agent: &ureq::Agent, host: &str, model: &str, facts: &str) -> Option<String> {
    let body = serde_json::json!({
        "model": model, "system": BIO_PROMPT, "prompt": format!("The facts: {facts}\n\nWrite their biography."),
        "think": false, "stream": false, "options": { "num_ctx": 4096, "temperature": 0.9 },
    });
    let resp: serde_json::Value = agent.post(&format!("{host}/api/generate")).send_json(body).ok()?.into_json().ok()?;
    let s = resp.get("response")?.as_str()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Put a soul's dilemma to Qwen. Returns (choice, prose), choice validated to the options.
fn hinge_one(agent: &ureq::Agent, host: &str, model: &str, dossier: &str, options: &[String]) -> Option<(String, String)> {
    let prompt = format!("{dossier}\n\nAllowed choices: {}. Decide, in character.", options.join(", "));
    let body = serde_json::json!({
        "model": model, "system": HINGE_PROMPT, "prompt": prompt,
        "think": false, "stream": false, "format": "json", "options": { "num_ctx": 8192, "temperature": 0.8 },
    });
    let resp: serde_json::Value = agent.post(&format!("{host}/api/generate")).send_json(body).ok()?.into_json().ok()?;
    let parsed: serde_json::Value = serde_json::from_str(resp.get("response")?.as_str()?).ok()?;
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
        Cmd::Converse { force } => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            let day = sim.target_day(t).max(0);
            // the town has at most one conversation of its own accord a day
            if !force && sim.last_dialogue_day()?.is_some_and(|last| last >= day) {
                return Ok(());
            }
            let Some(p) = sim.converse_pair(t) else { return Ok(()) };
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
        Cmd::Reflect => {
            let mut sim = Sim::open(&cli.db)?;
            sim.catch_up(today(), cur_phase())?;
            let t = today();
            // the wall-clock hour only breaks ties among the equally-overdue — who reflects is
            // decided by who has gone longest without; what they think is recorded and replayed.
            let salt = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()).hour() as u64;
            let Some(r) = sim.reflect_subject(t, salt) else { return Ok(()) };
            let names: Vec<String> = sim.world_snapshot(t).agents.iter()
                .filter(|a| a.active() && a.archetype != "child").map(|a| a.name.clone()).collect();
            let agent = ureq::AgentBuilder::new().timeout_read(Duration::from_secs(240)).build();
            // prefer Claude (Haiku) for a sharper inner voice — but never past the day's Anthropic
            // cap (default $1; set ANTHROPIC_DAILY_USD). Beyond it, reflect on the free local Qwen.
            let cap: f64 = std::env::var("ANTHROPIC_DAILY_USD").ok().and_then(|x| x.parse().ok()).unwrap_or(1.0);
            let spend = spend_file(&cli.db);
            let today_str = t.to_string();
            let claude_ok = cap - spent_today(&spend, &today_str) > 0.01;
            let raw = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).filter(|_| claude_ok)
                .and_then(|key| reflect_claude(&agent, &key, &r.dossier, &spend, &today_str))
                .or_else(|| {
                    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11435".into());
                    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
                    reflect_qwen(&agent, &host, &model, &r.dossier)
                });
            match raw.as_ref().and_then(|v| parse_reflection(v, &names)) {
                Some((thought, mood, sway, toward, regard, resolve, plan)) => {
                    sim.record_reflection(t, &r.name, &thought, &mood, &sway, &toward, &regard, &resolve, &plan)?;
                    sim.catch_up(today(), cur_phase())?; // re-fold the day so the residue lands
                    let tail = if plan != "none" { format!(" · set on {plan}") }
                        else if resolve != "none" { format!(" · resolved to {resolve} {toward}") }
                        else if regard != "none" { format!(" · {regard} toward {toward}") }
                        else { String::new() };
                    println!("Reflection [{} · {mood}/{sway}{tail}]: {thought}", r.name);
                }
                None => eprintln!("oracle unavailable — no reflection this hour."),
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
