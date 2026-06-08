//! Thrushcombe — the reader. A small read-only web view over the SQLite chronicle and
//! the folded world: the daily record, the cast, and each soul's life as the town set it
//! down. Browse the history a 50-year run generates.
//!
//!   thrush-web [world.db]        # serves http://127.0.0.1:8717

use thrush_core::{Agent, Season, Sim};
use tiny_http::{Header, Response, Server};
use time::{Date, OffsetDateTime};

fn today() -> Date {
    OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()).date()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

const CSS: &str = "
:root{--ink:#2b2620;--faint:#8a7f70;--rule:#e2d9c8;--bg:#f5f0e6;--link:#7a4a2b}
*{box-sizing:border-box}
body{background:var(--bg);color:var(--ink);font:17px/1.6 Georgia,'Iowan Old Style',serif;margin:0}
.wrap{max-width:760px;margin:0 auto;padding:2.4rem 1.4rem 5rem}
a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
h1{font-size:2rem;margin:0;letter-spacing:.02em}
h2{font-size:1.15rem;margin:2.2rem 0 .6rem;border-bottom:1px solid var(--rule);padding-bottom:.3rem}
.sub{color:var(--faint);font-style:italic;margin:.2rem 0 1.4rem}
.nav{margin:.6rem 0 2rem;font-variant:small-caps;letter-spacing:.04em}
.nav a{margin-right:1.1rem}
.entry{margin:.5rem 0;padding-left:.2rem}
.date{color:var(--faint);font-size:.82rem;font-variant:small-caps;letter-spacing:.03em}
.who{display:flex;justify-content:space-between;align-items:baseline;border-bottom:1px solid var(--rule);padding:.45rem 0}
.who .meta{color:var(--faint);font-size:.85rem}
.bars{font-family:ui-monospace,monospace;color:var(--faint);font-size:.8rem}
.gone{opacity:.55}
.tag{display:inline-block;font-size:.7rem;color:var(--faint);font-variant:small-caps;border:1px solid var(--rule);border-radius:3px;padding:0 .35rem;margin-left:.4rem}
.card{border:1px solid var(--rule);border-radius:6px;padding:1rem 1.2rem;margin:1rem 0;background:#fbf8f1}
.kin a{margin-right:.8rem}
";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>{}</title><style>{}</style></head><body><div class=wrap>\
         <div class=nav><a href=/>Chronicle</a><a href=/folk>The Town</a></div>{}</div></body></html>",
        esc(title), CSS, body
    )
}

fn bar(v: i32) -> String {
    let n = (v.clamp(0, 100) / 10) as usize;
    format!("{}{}", "\u{2588}".repeat(n), "\u{00b7}".repeat(10 - n))
}

fn status_label(sim: &Sim, a: &Agent) -> Option<String> {
    if let Some(d) = a.death_day {
        Some(format!("died {}", sim.day_to_date(d)))
    } else if a.departed {
        Some("left Thrushcombe".into())
    } else {
        None
    }
}

fn index(sim: &Sim) -> String {
    let t = today();
    let world = sim.world_snapshot(t);
    let pop = world.agents.iter().filter(|a| a.active()).count();
    let season = Season::of(t).name();
    let mut body = format!(
        "<h1>Thrushcombe St Mary</h1><div class=sub>{}, {} &middot; {} souls &middot; {}</div>",
        t.weekday(), esc(&t.to_string()), pop, season
    );
    body.push_str("<h2>The Chronicle</h2>");
    match sim.chronicle(60) {
        Ok(entries) => {
            for e in entries {
                body.push_str(&format!(
                    "<div class=entry><span class=date>{} &middot; {}</span><br>{}</div>",
                    esc(&e.date), esc(&e.actor), esc(&e.text)
                ));
            }
        }
        Err(e) => body.push_str(&format!("<p>(chronicle unavailable: {})</p>", esc(&e.to_string()))),
    }
    page("Thrushcombe — Chronicle", &body)
}

fn folk(sim: &Sim) -> String {
    let t = today();
    let day = sim.target_day(t).max(0);
    let world = sim.world_snapshot(t);
    let mut living: Vec<usize> = (0..world.agents.len()).filter(|&i| world.agents[i].active() && world.agents[i].archetype != "child").collect();
    living.sort_by(|&x, &y| world.agents[y].standing.cmp(&world.agents[x].standing));
    let children: Vec<usize> = (0..world.agents.len()).filter(|&i| world.agents[i].active() && world.agents[i].archetype == "child").collect();
    let gone: Vec<usize> = (0..world.agents.len()).filter(|&i| !world.agents[i].active()).collect();

    let mut body = String::from("<h1>The Town</h1><div class=sub>every soul Thrushcombe holds, and held</div>");
    let row = |i: usize, a: &Agent, dim: bool| {
        let label = status_label(sim, a).map(|s| format!("<span class=tag>{}</span>", esc(&s))).unwrap_or_default();
        format!(
            "<div class='who{}'><span><a href=/folk/{}>{}</a>{}</span><span class=meta>{} &middot; {}y &middot; <span class=bars>{}</span></span></div>",
            if dim { " gone" } else { "" }, i, esc(&a.name), label,
            esc(&pretty_arch(&a.archetype)), a.age(day), bar(a.standing)
        )
    };
    body.push_str("<h2>The grown folk</h2>");
    for i in living { body.push_str(&row(i, &world.agents[i], false)); }
    if !children.is_empty() {
        body.push_str("<h2>The children</h2>");
        for i in children { body.push_str(&row(i, &world.agents[i], false)); }
    }
    if !gone.is_empty() {
        body.push_str("<h2>Gone before &amp; gone away</h2>");
        for i in gone { body.push_str(&row(i, &world.agents[i], true)); }
    }
    page("Thrushcombe — The Town", &body)
}

fn person(sim: &Sim, idx: usize) -> String {
    let t = today();
    let day = sim.target_day(t).max(0);
    let world = sim.world_snapshot(t);
    let Some(a) = world.agents.get(idx) else {
        return page("Not found", "<h1>No such soul</h1><p><a href=/folk>Back to the town</a></p>");
    };
    let name = |i: usize| esc(&world.agents[i].name);
    let link = |i: usize| format!("<a href=/folk/{}>{}</a>", i, name(i));

    let status = status_label(sim, a).map(|s| format!(" <span class=tag>{}</span>", esc(&s))).unwrap_or_default();
    let mut body = format!(
        "<h1>{}{}</h1><div class=sub>{} of {} &middot; {} years</div>",
        esc(&a.name), status, esc(&pretty_arch(&a.archetype)), esc(&a.seat), a.age(day)
    );

    // standing / purse
    body.push_str(&format!(
        "<div class=card>standing <span class=bars>{}</span> {} &middot; purse £{}</div>",
        bar(a.standing), a.standing, a.purse
    ));

    // kin
    let mut kin = String::new();
    if let Some(s) = a.spouse { kin.push_str(&format!("married to {} &middot; ", link(s))); }
    if let Some(p) = a.parent { kin.push_str(&format!("child of {} &middot; ", link(p))); }
    let kids: Vec<usize> = (0..world.agents.len()).filter(|&i| world.agents[i].parent == Some(idx)).collect();
    if !kids.is_empty() {
        kin.push_str("children: ");
        kin.push_str(&kids.iter().map(|&i| link(i)).collect::<Vec<_>>().join(", "));
    }
    if !kin.is_empty() {
        body.push_str(&format!("<div class=kin><h2>Family</h2>{}</div>", kin.trim_end_matches("&middot; ")));
    }

    // their life as the town recorded it
    body.push_str("<h2>Their record</h2>");
    match sim.person_events(&a.name, 200) {
        Ok(entries) if !entries.is_empty() => {
            for e in entries {
                body.push_str(&format!(
                    "<div class=entry><span class=date>{}</span><br>{}</div>",
                    esc(&e.date), esc(&e.text)
                ));
            }
        }
        Ok(_) => body.push_str("<p class=sub>The town has yet to remark on them.</p>"),
        Err(e) => body.push_str(&format!("<p>({})</p>", esc(&e.to_string()))),
    }
    body.push_str("<p style='margin-top:2rem'><a href=/folk>&larr; The Town</a></p>");
    page(&format!("Thrushcombe — {}", a.name), &body)
}

fn pretty_arch(a: &str) -> String {
    match a {
        "genteel_status_seeker" => "gentlefolk",
        "hill_farmer" => "hill farmer",
        "practitioner" => "of the practice",
        "scheming_improver" => "improver",
        "blunt_hand" => "working folk",
        "official" => "of the parish & the law",
        "child" => "child",
        _ => "—",
    }
    .into()
}

fn route(sim: &Sim, url: &str) -> String {
    let path = url.split('?').next().unwrap_or("/");
    if path == "/" {
        index(sim)
    } else if path == "/folk" {
        folk(sim)
    } else if let Some(rest) = path.strip_prefix("/folk/") {
        match rest.parse::<usize>() {
            Ok(i) => person(sim, i),
            Err(_) => page("Not found", "<h1>Not found</h1>"),
        }
    } else {
        page("Not found", "<h1>Not found</h1><p><a href=/>Home</a></p>")
    }
}

fn main() {
    let db = std::env::args().nth(1).unwrap_or_else(|| "world.db".into());
    let addr = std::env::var("THRUSH_WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8717".into());

    let mut sim = Sim::open(&db).unwrap_or_else(|e| {
        eprintln!("could not open {db}: {e}");
        std::process::exit(1);
    });
    let _ = sim.catch_up(today()); // align the chronicle to today before serving

    let server = Server::http(&addr).unwrap_or_else(|e| {
        eprintln!("could not bind {addr}: {e}");
        std::process::exit(1);
    });
    println!("Thrushcombe reader on http://{addr}  (db: {db})");
    for req in server.incoming_requests() {
        let html = route(&sim, req.url());
        let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
        let _ = req.respond(Response::from_string(html).with_header(header));
    }
}
