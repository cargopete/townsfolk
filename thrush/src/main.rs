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
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use time::{Date, Month, OffsetDateTime};

use thrush_core::{Report, Sim};

#[derive(Parser)]
#[command(name = "thrush", about = "Thrushcombe — a small society, on the real calendar")]
struct Cli {
    #[arg(long, default_value = "world.db")]
    db: String,
    #[command(subcommand)]
    cmd: Cmd,
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

/// Map a purse (pounds, roughly -50..+50 of interest) to a 0..100 solvency reading.
fn solvency(purse: i32) -> i32 {
    (purse + 50).clamp(0, 100)
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { start, seed } => {
            let epoch = start.as_deref().map(parse_date).unwrap_or_else(today);
            let mut sim = Sim::init(&cli.db, epoch, seed)?;
            let added = sim.catch_up(today())?;
            println!(
                "Thrushcombe founded. epoch={epoch}  seed={seed}  logged {added} event(s) catching up to {}.",
                today()
            );
            print_status(&sim.report(today())?);
        }
        Cmd::Tick => {
            let mut sim = Sim::open(&cli.db)?;
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
            sim.catch_up(today())?;
            print_status(&sim.report(today())?);
        }
        Cmd::Watch => {
            let mut sim = Sim::open(&cli.db)?;
            run_watch(&mut sim)?;
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

fn watch_loop<B: ratatui::backend::Backend>(
    sim: &mut Sim,
    terminal: &mut Terminal<B>,
) -> Result<(), Box<dyn Error>> {
    loop {
        sim.catch_up(today())?;
        let r = sim.report(today())?;
        terminal.draw(|f| draw(f, &r))?;
        if event::poll(Duration::from_millis(1000))? {
            if let CEvent::Key(k) = event::read()? {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn draw(f: &mut Frame, r: &Report) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(f.area());

    // --- header ---
    let real = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let head = vec![
        Line::from(vec![
            Span::styled("THRUSHCOMBE ST MARY", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(format!("   sim {} {}  ·  day {}   ", r.weekday, r.date, r.day)),
            Span::styled(format!("[{}]", phase_now()), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(Span::styled(
            format!("{} — armed: {}", r.season, r.armed),
            Style::default().fg(Color::Green),
        )),
        Line::from(Span::styled(
            format!("real {:04}-{:02}-{:02} {:02}:{:02}   ·   sim-date = real-date (companion lock)   ·   q to quit",
                real.year(), real.month() as u8, real.day(), real.hour(), real.minute()),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(head).block(Block::default().borders(Borders::BOTTOM)), root[0]);

    // --- body: three columns ---
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(30), Constraint::Percentage(32)])
        .split(root[1]);

    // left: the town
    let mut town: Vec<Line> = Vec::new();
    for a in &r.agents {
        town.push(Line::from(Span::styled(
            format!("{} ({}y)", a.name, a.age(r.day)),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        town.push(Line::from(format!("  {} {}  £{}", short_arch(&a.archetype), bar(a.standing), a.purse)));
    }
    f.render_widget(
        Paragraph::new(town).block(Block::default().borders(Borders::ALL).title(" The town (standing) ")),
        cols[0],
    );

    // middle: tensions, stock, calendar
    let mut mid: Vec<Line> = Vec::new();
    mid.push(Line::from(Span::styled("Tensions", Style::default().fg(Color::Magenta))));
    if let Some(c) = r.agents.iter().find(|a| a.name.contains("Cynthia")) {
        mid.push(Line::from(format!("  Cynthia  solvency {}", bar(solvency(c.purse)))));
        mid.push(Line::from(format!("           face     {}", bar(c.standing))));
    }
    if let Some(rp) = r.agents.iter().find(|a| a.name.contains("Rupert")) {
        mid.push(Line::from(format!("  Rupert   modern   {}", bar(70))));
        mid.push(Line::from(format!("           respect  {}", bar(rp.standing))));
    }
    mid.push(Line::from(""));
    mid.push(Line::from(Span::styled("News in flight", Style::default().fg(Color::Magenta))));
    if r.news.is_empty() {
        mid.push(Line::from("  (the town is quiet)"));
    }
    for nws in &r.news {
        mid.push(Line::from(format!("  ~ {nws}")));
    }
    mid.push(Line::from(""));
    mid.push(Line::from(Span::styled("Stock", Style::default().fg(Color::Magenta))));
    for an in &r.animals {
        let g = if an.gest > 0 { format!(" calf {}d", an.gest) } else { String::new() };
        mid.push(Line::from(format!("  {:<10} {}{}", an.name, bar(an.health), g)));
    }
    mid.push(Line::from(""));
    mid.push(Line::from(Span::styled("On the calendar", Style::default().fg(Color::Magenta))));
    for p in &r.pending {
        mid.push(Line::from(format!("  · {p}")));
    }
    f.render_widget(
        Paragraph::new(mid).block(Block::default().borders(Borders::ALL).title(" State ")).wrap(Wrap { trim: true }),
        cols[1],
    );

    // right: chronicle
    let chron: Vec<Line> = r.chronicle.iter().map(|c| Line::from(c.clone())).collect();
    f.render_widget(
        Paragraph::new(chron).block(Block::default().borders(Borders::ALL).title(" Chronicle ")).wrap(Wrap { trim: true }),
        cols[2],
    );
}
