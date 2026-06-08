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
            let added = sim.catch_up(today())?;
            println!(
                "Thrushcombe founded. epoch={epoch}  seed={seed}  logged {added} event(s) catching up to {}.",
                today()
            );
            print_status(&sim.report(today())?);
        }
        Cmd::Tick => {
            let mut sim = Sim::open(&cli.db)?;
            apply_engine(&mut sim, cli.wasm);
            let added = sim.catch_up(today())?;
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
        Cmd::Status => {
            let mut sim = Sim::open(&cli.db)?;
            apply_engine(&mut sim, cli.wasm);
            sim.catch_up(today())?;
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
            let added = sim.catch_up(today())?;
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
        sim.catch_up(today())?;
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
        Line::from(Span::styled(format!("{} — armed: {}", d.season, d.armed), Style::default().fg(Color::Green))),
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
        mid.push(Line::from(format!("{}, {}y · of {}", arch_tag(&p.archetype), p.age, p.seat)));
        mid.push(Line::from(format!("standing {} {}", bar(p.standing), p.standing)));
        mid.push(Line::from(format!("purse    £{}", p.purse)));
        mid.push(Line::from(vec![Span::raw("now:  "), Span::styled(format!("{} · {}", p.location, p.doing), Style::default().fg(Color::Green))]));
        mid.push(Line::from(vec![Span::raw("next: "), Span::styled(p.next.clone(), Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))]));
        let mut kin = String::new();
        if let Some(s) = &p.spouse { kin.push_str(&format!("⚭ {}  ", s)); }
        if let Some(par) = &p.parent { kin.push_str(&format!("↑ {}  ", par)); }
        if !p.children.is_empty() { kin.push_str(&format!("↓ {}", p.children.join(", "))); }
        if !kin.is_empty() { mid.push(Line::from(Span::styled(kin, Style::default().fg(Color::Magenta)))); }
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
