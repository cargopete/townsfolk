//! Thrushcombe — the reader & dashboard. A read-only web view over the chronicle and
//! the folded world: a detailed live board of where everyone is and what they're about,
//! the cast and their lineage, and each soul's whole record.
//!
//!   thrush-web [world.db]        # serves http://127.0.0.1:8717

use base64::Engine as _;
use thrush_core::{Agent, Phase, Sim};
use tiny_http::{Header, Response, Server};
use time::{Date, OffsetDateTime};

/// The town's calendar shift (days) — refreshed from the world each request so the dashboard
/// reflects any `jump` the CLI has made. `today()` adds it, so every reader stays correct.
static DAY_OFFSET: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn now() -> OffsetDateTime {
    OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc())
}
fn today() -> Date {
    now().date() + time::Duration::days(DAY_OFFSET.load(std::sync::atomic::Ordering::Relaxed))
}
fn phase_now() -> Phase {
    Phase::from_hour(now().hour())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn parse_date(s: &str) -> Option<Date> {
    let p: Vec<&str> = s.split('-').collect();
    if p.len() != 3 {
        return None;
    }
    let m: u8 = p[1].parse().ok()?;
    Date::from_calendar_date(p[0].parse().ok()?, time::Month::try_from(m).ok()?, p[2].parse().ok()?).ok()
}

fn qparam(url: &str, key: &str) -> Option<String> {
    url.split('?').nth(1)?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

const CSS: &str = "
:root{--ink:#2b2620;--soft:#5a5046;--faint:#8a7f70;--rule:#e2d9c8;--hair:#efe7d8;--bg:#f5f0e6;--link:#7a4a2b;--card:#fbf8f1;--green:#3f6b52;--red:#9a3b2b;--r:9px;--r-sm:5px}
*{box-sizing:border-box}
html{scroll-behavior:smooth;-webkit-text-size-adjust:100%}
body{background:var(--bg);color:var(--ink);font:17px/1.62 Georgia,'Iowan Old Style','Palatino Linotype',serif;margin:0;text-rendering:optimizeLegibility;-webkit-font-smoothing:antialiased}
.wrap{max-width:1000px;margin:0 auto;padding:1.4rem 1.05rem 4rem}
@media(min-width:760px){.wrap{padding:2.4rem 1.6rem 6rem}}
a{color:var(--link);text-decoration:none}
a:hover{text-decoration:underline}
:focus-visible{outline:2px solid var(--link);outline-offset:2px;border-radius:3px}
h1{font-size:clamp(1.7rem,5.2vw,2.15rem);line-height:1.12;margin:0;letter-spacing:-.012em;text-wrap:balance}
h2{font-size:1.02rem;margin:2.4rem 0 .6rem;border-bottom:1px solid var(--rule);padding-bottom:.35rem;font-variant:small-caps;letter-spacing:.06em;color:var(--soft)}
p{text-wrap:pretty}
.sub{color:var(--faint);font-style:italic;margin:.35rem 0 1.4rem;text-wrap:pretty}
.nav{display:flex;flex-wrap:wrap;gap:.1rem;margin:.1rem -.55rem 1.8rem;font-variant:small-caps;letter-spacing:.05em;font-size:.95rem}
.nav a{color:var(--ink);opacity:.72;padding:.5rem .7rem;border-radius:var(--r-sm);transition:opacity .15s,background .15s}
.nav a:hover{opacity:1;background:var(--card);text-decoration:none}
.entry{margin:.5rem 0}
.date{color:var(--faint);font-size:.78rem;font-variant:small-caps;letter-spacing:.03em;font-variant-numeric:tabular-nums}
.bars{font-family:ui-monospace,'SF Mono',monospace;color:var(--faint);font-size:.76rem;letter-spacing:-.5px;font-variant-numeric:tabular-nums}
.num{font-variant-numeric:tabular-nums}
.tag{display:inline-block;font-size:.66rem;color:var(--faint);font-variant:small-caps;letter-spacing:.03em;border:1px solid var(--rule);border-radius:var(--r-sm);padding:.05rem .4rem;margin-left:.4rem;vertical-align:middle}
.gone td{opacity:.5}
.card{border:1px solid var(--rule);border-radius:var(--r);padding:.85rem 1.1rem;margin:.9rem 0;background:var(--card)}
.cols{display:grid;grid-template-columns:1fr 1fr;gap:1.1rem}
.cols ul{margin:.2rem 0;padding-left:1.1rem}.cols li{margin:.2rem 0}
table{width:100%;border-collapse:collapse;font-size:.93rem}
th{text-align:left;color:var(--faint);font-weight:normal;font-variant:small-caps;font-size:.76rem;letter-spacing:.04em;border-bottom:1px solid var(--rule);padding:.4rem .45rem}
td{padding:.42rem .45rem;border-bottom:1px solid var(--hair);vertical-align:top}
tr:hover td{background:rgba(122,74,43,.045)}
.doing{color:var(--green)}.next{color:var(--faint);font-style:italic}
.where{font-variant:small-caps;letter-spacing:.02em}
.life{font-size:1.06rem;line-height:1.75;color:#3a342c;border-left:3px solid var(--rule);padding-left:1.1rem;margin:1.1rem 0;text-wrap:pretty}
.think{font-style:italic;color:#4a4036}
.think::before{content:'\\201C'}.think::after{content:'\\201D'}
.who{font-variant:small-caps;letter-spacing:.03em}
.fear{background:linear-gradient(180deg,#3f2a25,#34211d);color:#f3e9dd;border:1px solid #5a3a30;border-radius:var(--r);padding:.9rem 1.15rem;margin:0 0 1.6rem;font-style:italic;line-height:1.55}
.fear b{font-style:normal;font-variant:small-caps;letter-spacing:.06em;color:#e8b9a0}
.fear a{color:#e8b9a0;text-decoration:underline}
.suspect{color:var(--red);font-variant:small-caps;letter-spacing:.04em;font-size:.82rem}
select,button,input{font:inherit;color:var(--ink);background:var(--card);border:1px solid var(--rule);border-radius:var(--r-sm);padding:.5rem .65rem}
button{cursor:pointer;transition:background .15s,transform .05s}
button:hover{background:#f3ecda}button:active{transform:translateY(1px)}
@media(max-width:640px){
  body{font-size:16px}
  .cols{grid-template-columns:1fr;gap:.6rem}
  .hidesm{display:none}
  .board thead,.roster thead{display:none}
  .board tr,.roster tr{display:block;border:1px solid var(--rule);border-radius:var(--r);background:var(--card);padding:.55rem .85rem;margin:.65rem 0}
  .board tr:hover td,.roster tr:hover td{background:transparent}
  .board td,.roster td{display:flex;justify-content:space-between;align-items:baseline;gap:1.2rem;border:0;padding:.22rem 0;text-align:right}
  .board td::before,.roster td::before{content:attr(data-label);color:var(--faint);font-variant:small-caps;letter-spacing:.04em;font-size:.72rem;text-align:left;white-space:nowrap}
  .board .who-cell,.roster .who-cell{display:block;text-align:left;font-size:1.08rem;border-bottom:1px solid var(--hair);padding-bottom:.4rem;margin-bottom:.35rem}
  .board .who-cell::before,.roster .who-cell::before{content:none}
  .gone td{opacity:1}.gone .who-cell{opacity:.55}
}
";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>{}</title><style>{}</style></head><body><div class=wrap>\
         <div class=nav><a href=/>Dashboard</a><a href=/folk>The Town</a><a href=/map>Map</a><a href=/graph>Kinship</a><a href=/day>History</a><a href=/thoughts>Thoughts</a><a href=/inquiry>The Inquiry</a><a href=/talk>A word…</a></div>{}</div></body></html>",
        esc(title), CSS, body
    )
}

/// The town-wide pall of an open (or freshly-closed) killing, for the top of every page.
fn fear_banner(sim: &Sim) -> String {
    let w = sim.world_snapshot(today());
    let Some(inq) = &w.inquest else { return String::new() };
    if inq.closed && w.dread <= 0 { return String::new(); }
    let vn = esc(&inq.victim_name);
    let body = if inq.closed {
        let who = (inq.accused >= 0).then(|| w.agents.get(inq.accused as usize).map(|a| esc(&a.name))).flatten().unwrap_or_else(|| "a soul".into());
        format!("<b>after the hanging</b> &nbsp; The town is still raw from the hanging of {who} for the murder of {vn}. No one is quite sure the right neck was stretched, and the unease has not lifted.")
    } else if inq.accused >= 0 {
        let acc = w.agents.get(inq.accused as usize).map(|a| esc(&a.name)).unwrap_or_default();
        format!("<b>murder &middot; the accused</b> &nbsp; {vn} was murdered, and the parish has fixed on <span class=suspect>{acc}</span> — the talk is openly of hanging.")
    } else {
        let most = (0..w.agents.len())
            .filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child" && i != inq.victim)
            .max_by_key(|&i| w.agents[i].suspicion);
        let tail = match most {
            Some(m) if w.agents[m].suspicion >= 40 => format!(" Suspicion is settling on <span class=suspect>{}</span> — though whether justly, who can say.", esc(&w.agents[m].name)),
            _ => " No one is yet named.".into(),
        };
        format!("<b>murder &middot; killer unknown</b> &nbsp; {vn} was found dead — murder, by one of the town's own. Fear walks the lanes; every soul weighs every other.{tail}")
    };
    let bench = (!inq.closed && inq.investigator >= 0)
        .then(|| w.agents.get(inq.investigator as usize).map(|a| format!(" <span class=suspect>{} sits as magistrate</span> — and his questions fall on the working folk and the strangers, never on his own kind.", esc(&a.name))))
        .flatten().unwrap_or_default();
    let inquiry = (!inq.closed && inq.public_inquiry)
        .then_some(" The whole town is being questioned — <a href=/inquiry style='color:#e8b9a0'>read the transcripts &rarr;</a>")
        .unwrap_or_default();
    format!("<div class=fear>{body}{bench}{inquiry}</div>")
}

fn bar(v: i32) -> String {
    let n = (v.clamp(0, 100) / 10) as usize;
    format!("{}{}", "\u{2588}".repeat(n), "\u{00b7}".repeat(10 - n))
}

fn list(title: &str, items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let lis: String = items.iter().map(|i| format!("<li>{}</li>", esc(i))).collect();
    format!("<div><h2>{}</h2><ul>{}</ul></div>", esc(title), lis)
}

/// The detailed live board.
fn dashboard(sim: &Sim) -> String {
    let d = match sim.detail(today(), phase_now()) {
        Ok(d) => d,
        Err(e) => return page("error", &format!("<h1>err</h1><p>{}</p>", esc(&e.to_string()))),
    };
    let wline = d.weather.as_ref().map(|w| format!(" &middot; {}", esc(w))).unwrap_or_default();
    let mut body = format!(
        "<h1>Thrushcombe St Mary</h1><div class=sub>{}, {} &middot; {} ({}) &middot; {} souls{}<br><span class=date>armed this season: {}</span></div>",
        esc(&d.weekday), esc(&d.date), esc(&d.season), esc(&d.phase), d.population, wline, esc(&d.armed)
    );

    // two columns: today's global events + news in flight, then upcoming
    body.push_str("<div class=cols>");
    body.push_str(&list("Today in Thrushcombe", &d.global_today));
    body.push_str(&list("News in flight", &d.gossip));
    body.push_str("</div>");
    body.push_str(&list("On the calendar", &d.upcoming));

    // the detailed town board: where everyone is, what they're about, and next
    body.push_str("<h2>The town, this ");
    body.push_str(&esc(&d.phase));
    body.push_str("</h2><table class=board><tr><th>Soul</th><th>Where</th><th>Doing now</th><th>Wants</th><th>Standing</th><th>£</th></tr>");
    for p in &d.people {
        body.push_str(&format!(
            "<tr><td class=who-cell><a href=/folk/{}>{}</a> <span class=date>{}y &middot; {}</span></td>\
             <td class=where data-label=Where>{}</td><td class=doing data-label='Doing now'>{}</td><td class=next data-label=Wants>{}</td>\
             <td data-label=Standing><span class=bars>{}</span></td><td class=num data-label=Purse>{}</td></tr>",
            p.idx, esc(&p.name), p.age, esc(&p.mood), esc(&p.location), esc(&p.doing), esc(&p.wants), bar(p.standing), p.purse
        ));
    }
    body.push_str("</table>");

    // the chronicle
    body.push_str("<h2>The chronicle</h2>");
    for e in &d.recent {
        body.push_str(&format!(
            "<div class=entry><span class=date>{} &middot; {}</span><br>{}</div>",
            esc(&e.date), esc(&e.actor), esc(&e.text)
        ));
    }
    page("Thrushcombe — Dashboard", &format!("{}{}", fear_banner(sim), body))
}

/// A person's station: their specific trade if they have one, else their stratum.
fn station(a: &Agent) -> String {
    a.trade.clone().unwrap_or_else(|| pretty_arch(&a.archetype).to_string())
}

fn pretty_arch(a: &str) -> &'static str {
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

fn folk(sim: &Sim) -> String {
    let t = today();
    let day = sim.target_day(t).max(0);
    let world = sim.world_snapshot(t);
    let grown: Vec<usize> = (0..world.agents.len()).filter(|&i| world.agents[i].active() && world.agents[i].archetype != "child").collect();
    let mut grown = grown;
    grown.sort_by(|&x, &y| world.agents[y].standing.cmp(&world.agents[x].standing));
    let children: Vec<usize> = (0..world.agents.len()).filter(|&i| world.agents[i].active() && world.agents[i].archetype == "child").collect();
    let gone: Vec<usize> = (0..world.agents.len()).filter(|&i| !world.agents[i].active()).collect();

    let mut body = String::from("<h1>The Town</h1><div class=sub>every soul Thrushcombe holds, and held</div>");
    let section = |title: &str, ids: &[usize], dim: bool| -> String {
        if ids.is_empty() {
            return String::new();
        }
        let rows: String = ids.iter().map(|&i| {
            let a = &world.agents[i];
            let label = status_label(sim, a).map(|s| format!("<span class=tag>{}</span>", esc(&s))).unwrap_or_default();
            let from = a.origin.as_ref().map(|o| format!("<span class=tag>of {}</span>", esc(o))).unwrap_or_default();
            format!(
                "<tr class='{}'><td class=who-cell><a href=/folk/{}>{}</a>{}{}</td><td data-label=Station>{}</td><td class=num data-label=Age>{}y</td><td data-label=Standing><span class=bars>{}</span></td></tr>",
                if dim { "gone" } else { "" }, i, esc(&a.name), label, from, esc(&station(a)), a.age(day), bar(a.standing)
            )
        }).collect();
        format!("<h2>{}</h2><table class=roster><tr><th>Name</th><th>Station</th><th>Age</th><th>Standing</th></tr>{}</table>", esc(title), rows)
    };
    body.push_str(&section("The grown folk", &grown, false));
    body.push_str(&section("The children", &children, false));
    body.push_str(&section("Gone before & gone away", &gone, true));
    page("Thrushcombe — The Town", &format!("{}{}", fear_banner(sim), body))
}

/// The town's inner life — the 20 most recent reflections town-wide, or, when a soul is chosen
/// from the dropdown, that one soul's last 50 thoughts. The quiet hours laid bare.
fn thoughts(sim: &Sim, url: &str) -> String {
    let world = sim.world_snapshot(today());
    let idx_of = |name: &str| world.agents.iter().position(|a| a.name == name);
    let sel: Option<usize> = qparam(url, "soul").and_then(|v| v.parse().ok()).filter(|&i| world.agents.get(i).is_some());

    let mut body = String::from(
        "<h1>The town, thinking</h1>\
         <div class=sub>each hour the soul most overdue takes a quiet turn of thought — on their work, \
         their place, those about them, life as they find it. Newest first.</div>",
    );

    // a dropdown to read one soul's running stream, else the whole town's latest
    let mut opts = format!("<option value=''{}>— the whole town —</option>", if sel.is_none() { " selected" } else { "" });
    let mut souls: Vec<usize> = (0..world.agents.len())
        .filter(|&i| world.agents[i].active() && world.agents[i].archetype != "child")
        .collect();
    souls.sort_by(|&a, &b| world.agents[a].name.cmp(&world.agents[b].name));
    for i in souls {
        opts.push_str(&format!(
            "<option value={}{}>{}</option>",
            i, if sel == Some(i) { " selected" } else { "" }, esc(&world.agents[i].name)
        ));
    }
    body.push_str(&format!(
        "<form method=get action=/thoughts style='margin:1rem 0 1.6rem'>\
         <select name=soul onchange='this.form.submit()' style='font:inherit;padding:.3rem .4rem'>{}</select> \
         <noscript><button type=submit>Read</button></noscript></form>",
        opts
    ));

    match sel {
        Some(i) => {
            let who = world.agents[i].name.clone();
            body.push_str(&format!(
                "<div class=sub>The running stream of <a href=/folk/{i} class=who>{}</a> — up to fifty thoughts back, newest first.</div>",
                esc(&who)
            ));
            match sim.reflections_of(&who, 50) {
                Ok(rs) if !rs.is_empty() => {
                    for (day, thought) in rs {
                        body.push_str(&format!(
                            "<div class=entry style='margin:.9rem 0'><span class=date>{}</span>\
                             <br><span class=think>{}</span></div>",
                            esc(&sim.day_to_date(day)), esc(&thought)
                        ));
                    }
                }
                Ok(_) => body.push_str("<p class=sub>They have not had a quiet hour to themselves yet.</p>"),
                Err(e) => body.push_str(&format!("<p>({})</p>", esc(&e.to_string()))),
            }
        }
        None => match sim.recent_reflections(20) {
            Ok(rs) if !rs.is_empty() => {
                for (day, who, thought) in rs {
                    let when = sim.day_to_date(day);
                    let nm = match idx_of(&who) {
                        Some(i) => format!("<a href=/folk/{i} class=who>{}</a>", esc(&who)),
                        None => format!("<span class=who>{}</span>", esc(&who)),
                    };
                    body.push_str(&format!(
                        "<div class=entry style='margin:.9rem 0'>{} <span class=date>&middot; {}</span>\
                         <br><span class=think>{}</span></div>",
                        nm, esc(&when), esc(&thought)
                    ));
                }
            }
            Ok(_) => body.push_str(
                "<p class=sub>No one has had a quiet hour to themselves just yet — give the town an hour or two.</p>",
            ),
            Err(e) => body.push_str(&format!("<p>({})</p>", esc(&e.to_string()))),
        },
    }
    body.push_str("<p style='margin-top:2rem'><a href=/>&larr; Dashboard</a></p>");
    page("Thrushcombe — the town, thinking", &format!("{}{}", fear_banner(sim), body))
}

/// The released transcripts of the magistrate's inquiry — what each questioned soul claimed,
/// their alibi, and any name they cast the blame upon. Public knowledge the whole town turns over.
fn inquiry(sim: &Sim) -> String {
    let world = sim.world_snapshot(today());
    let idx_of = |name: &str| world.agents.iter().position(|a| a.name == name);
    let open = world.inquest.as_ref().filter(|q| !q.closed);
    let mut body = String::from("<h1>The Inquiry</h1>");
    match open {
        Some(inq) => {
            let mag = world.agents.get(inq.investigator as usize).map(|a| esc(&a.name)).unwrap_or_else(|| "the magistrate".into());
            body.push_str(&format!(
                "<div class=sub>{mag} questions the parish over the murder of {}. \
                 The statements read out in the open, newest first — alibis given, fingers pointed. \
                 The town hears each, and weighs it.</div>",
                esc(&inq.victim_name)
            ));
        }
        None => body.push_str("<div class=sub>No inquiry sits at present.</div>"),
    }
    let link = |who: &str| match idx_of(who) {
        Some(i) => format!("<a href=/folk/{i} class=who>{}</a>", esc(who)),
        None => format!("<span class=who>{}</span>", esc(who)),
    };
    match sim.public_testimony(60) {
        Ok(ts) if !ts.is_empty() => {
            for (day, who, alibi, accuses, text) in ts {
                let badge = match alibi.as_str() {
                    "strong" => "<span class=tag style='color:#2f6b3f;border-color:#2f6b3f'>alibi holds</span>",
                    "none" => "<span class=tag style='color:#9a3b2b;border-color:#9a3b2b'>no account</span>",
                    _ => "<span class=tag>thin account</span>",
                };
                let named = if accuses.is_empty() { String::new() }
                    else { format!(" <span class=suspect>points at {}</span>", link(&accuses)) };
                body.push_str(&format!(
                    "<div class=card><div>{} {} <span class=date>&middot; {}</span>{}</div>\
                     <div class=think style='margin-top:.4rem'>{}</div></div>",
                    link(&who), badge, esc(&sim.day_to_date(day)), named, esc(&text)
                ));
            }
        }
        Ok(_) => body.push_str("<p class=sub>The magistrate has read out no statements yet.</p>"),
        Err(e) => body.push_str(&format!("<p>({})</p>", esc(&e.to_string()))),
    }
    // emergency town meetings — the full account of each assembly, and how the parish came away
    if let Ok(halls) = sim.town_halls(10) {
        if !halls.is_empty() {
            body.push_str("<h2 style='margin-top:2.4rem'>The town meetings</h2>");
            for (day, outcome, text) in halls {
                let (label, col) = match outcome.as_str() {
                    "calmed"   => ("the parish came away calmer", "#2f6b3f"),
                    "inflamed" => ("the parish came away inflamed — crying for a name", "#9a3b2b"),
                    _           => ("the parish came away divided", "#7a5a2e"),
                };
                body.push_str(&format!(
                    "<div class=card><div><span class=tag style='color:{col};border-color:{col}'>{label}</span> <span class=date>&middot; {}</span></div>\
                     <div class=think style='margin-top:.5rem;white-space:pre-line'>{}</div></div>",
                    esc(&sim.day_to_date(day)), esc(&text)
                ));
            }
        }
    }
    // the full case file — every statement taken, whether read out or given in private
    match sim.all_testimony(200) {
        Ok(ts) if !ts.is_empty() => {
            body.push_str(&format!(
                "<h2 style='margin-top:2.4rem'>All statements taken <span class=sub style='font-weight:normal'>&middot; {} in all</span></h2>\
                 <div class=sub>Every soul's account to the magistrate, newest first — those read out in the open, and those taken in private.</div>",
                ts.len()
            ));
            for (day, who, alibi, accuses, public, text) in ts {
                let badge = match alibi.as_str() {
                    "strong" => "<span class=tag style='color:#2f6b3f;border-color:#2f6b3f'>alibi holds</span>",
                    "none" => "<span class=tag style='color:#9a3b2b;border-color:#9a3b2b'>no account</span>",
                    _ => "<span class=tag>thin account</span>",
                };
                let heard = if public { "<span class=tag style='color:#7a5a2e;border-color:#7a5a2e'>read out</span>" }
                    else { "<span class=tag style='color:#777;border-color:#aaa'>in private</span>" };
                let named = if accuses.is_empty() { String::new() }
                    else { format!(" <span class=suspect>points at {}</span>", link(&accuses)) };
                body.push_str(&format!(
                    "<div class=card><div>{} {} {} <span class=date>&middot; {}</span>{}</div>\
                     <div class=think style='margin-top:.4rem'>{}</div></div>",
                    link(&who), badge, heard, esc(&sim.day_to_date(day)), named, esc(&text)
                ));
            }
        }
        Ok(_) => {}
        Err(e) => body.push_str(&format!("<p>({})</p>", esc(&e.to_string()))),
    }
    body.push_str("<p style='margin-top:2rem'><a href=/>&larr; Dashboard</a></p>");
    page("Thrushcombe — the inquiry", &format!("{}{}", fear_banner(sim), body))
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
    let idx_of = |nm: &str| world.agents.iter().position(|x| x.name == nm);

    let status = status_label(sim, a).map(|s| format!(" <span class=tag>{}</span>", esc(&s))).unwrap_or_default();
    let origin = a.origin.as_ref().map(|o| format!(" &middot; came from {}", esc(o))).unwrap_or_default();
    let speak = if a.active() {
        format!(" <a href='/talk?to={}' class=tag>speak to me directly →</a>", idx)
    } else {
        String::new()
    };
    // a sepia portrait if one has been made, else a period likeness-card with their initials
    let initials: String = a.name.split_whitespace()
        .filter(|w| w.chars().next().is_some_and(|c| c.is_uppercase()))
        .filter_map(|w| w.chars().next()).take(2).collect();
    let portrait = format!(
        "<div style='float:right;width:148px;margin:.2rem 0 .8rem 1.2rem;text-align:center'>\
           <img src='/portraits/{idx}.jpg' alt='' style='width:148px;height:182px;object-fit:cover;border-radius:6px;border:3px solid #8a7654;box-shadow:0 2px 6px rgba(80,60,30,.3);filter:sepia(.3)' \
             onerror=\"this.style.display='none';document.getElementById('ph{idx}').style.display='flex'\">\
           <div id='ph{idx}' style='display:none;width:148px;height:182px;border-radius:6px;border:3px solid #8a7654;box-shadow:0 2px 6px rgba(80,60,30,.3);background:linear-gradient(160deg,#d8cba6,#b9a77f);color:#5b4d39;align-items:center;justify-content:center;font:italic 2.6rem Georgia'>{initials}</div>\
           <div style='font-size:.74rem;color:#7a6a4e;margin-top:.25rem;font-style:italic'>a likeness</div>\
         </div>"
    );
    let mut body = format!(
        "{portrait}<h1>{}{}{}</h1><div class=sub>{} of {} &middot; {} years{}</div>",
        esc(&a.name), status, speak, esc(&station(a)), esc(&a.seat), a.age(day), origin
    );

    // the life the parish tells of them
    if let Ok(Some(bio)) = sim.biography(&a.name) {
        body.push_str(&format!("<p class=life>{}</p>", esc(&bio)));
    }

    body.push_str(&format!(
        "<div class=card>standing <span class=bars>{}</span> {} &middot; purse £{}",
        bar(a.standing), a.standing, a.purse
    ));
    // the body — embodiment: how worn or hale they are, and whether they ail
    {
        let vig = if a.vigour <= 22 { "worn to the bone" } else if a.vigour <= 42 { "tired" }
            else if a.vigour >= 82 { "hale and rested" } else { "middling in body" };
        let ail = if a.health <= 38 { ", and ill" } else if a.health <= 64 { ", a little poorly" } else { "" };
        body.push_str(&format!(" &middot; <span class=doing>{vig}{ail}</span>"));
    }
    // live placement/doings and ties for the present cast
    let mut ties_html = String::new();
    if a.active() {
        if let Ok(d) = sim.detail(t, phase_now()) {
            if let Some(p) = d.people.iter().find(|p| p.idx == idx) {
                body.push_str(&format!(
                    "<br><span class=where>{}</span> &middot; <span class=doing>{}</span> &middot; next: <span class=next>{}</span>\
                     <br>wants <b>{}</b> &middot; {}",
                    esc(&p.location), esc(&p.doing), esc(&p.next), esc(&p.wants), esc(&p.mood)
                ));
                if !p.friends.is_empty() {
                    ties_html.push_str(&format!("<div class=doing>thick with {}</div>", esc(&p.friends.join(", "))));
                }
                if !p.rivals.is_empty() {
                    ties_html.push_str(&format!("<div style='color:#c0392b'>at odds with {}</div>", esc(&p.rivals.join(", "))));
                }
            }
        }
    }
    body.push_str("</div>");
    if !ties_html.is_empty() {
        body.push_str(&format!("<h2>Where they stand</h2>{}", ties_html));
    }

    // a plan they set themselves and are carrying toward its reckoning
    if a.active() && a.intent != 0 {
        let what = match a.intent { 1 => "to mend their fortunes", 2 => "to better their station", _ => "a bold venture" };
        body.push_str(&format!(
            "<h2>Pursuing</h2><div class=doing>{} <span class=date>— resolved {} days since, not yet come to its head</span></div>",
            esc(what), a.intent_age
        ));
    }

    // where they stand in an open murder inquiry — suspicion, a clearing, and their own statement
    if let Some(inq) = world.inquest.as_ref().filter(|q| !q.closed) {
        if idx != inq.victim {
            let standing = if a.cleared {
                "<span class=tag style='color:#2f6b3f;border-color:#2f6b3f'>alibi holds — cleared</span>".to_string()
            } else if inq.accused == idx as i32 {
                "<span class=suspect>stands accused of the murder</span>".to_string()
            } else if a.suspicion >= 60 {
                format!("<span class=suspect>heavily suspected</span> <span class=date>&middot; suspicion {}</span>", a.suspicion)
            } else if a.suspicion >= 30 {
                format!("<span class=suspect>under a cloud</span> <span class=date>&middot; suspicion {}</span>", a.suspicion)
            } else {
                "little suspected".to_string()
            };
            body.push_str(&format!("<h2>The inquiry</h2><div class=doing>{}</div>", standing));
            if let Ok(Some((alibi, accuses, text))) = sim.testimony_of(&a.name) {
                let named = if accuses.is_empty() { String::new() } else { format!(" <span class=suspect>They named {}.</span>", esc(&accuses)) };
                body.push_str(&format!(
                    "<div class=card><div class=date>Before the magistrate &middot; alibi {}</div>\
                     <div class=think style='margin-top:.3rem'>{}</div>{}</div>",
                    esc(&alibi), esc(&text), named
                ));
            }
        }
    }

    // what is uppermost in their mind — the global workspace, the single thing that fills it now.
    // The integration of all the rest: whichever concern won the day's contention for their focus.
    if a.active() {
        if let Some((topic, intensity, phrase)) = sim.focus_of(&a.name, t) {
            match phrase {
                Some(p) => {
                    let heavy = matches!(topic.as_str(), "dread" | "grief" | "haunt" | "betrayal" | "wrong");
                    let tone = if heavy { "#7a2e2e" } else { "#3a4a6a" };
                    let grip = if intensity >= 55 { " <span class=date>— it fills their mind, and crowds out the rest</span>" } else { "" };
                    body.push_str(&format!("<h2>Uppermost in their mind</h2><p class=life style='color:{tone}'>{}{}.</p>", esc(&p), grip));
                }
                None => body.push_str("<h2>Uppermost in their mind</h2><p class=life><span class=date>Their mind is easy just now, on the day's ordinary work — no one thing crowds it.</span></p>"),
            }
        }
    }
    // how they have come to see themselves — the evolving self-concept they reason from
    if let Ok(Some(sc)) = sim.self_model(&a.name) {
        body.push_str(&format!("<h2>How they see themselves</h2><p class=life>{}</p>", esc(&sc)));
    }
    // the recursive mirror — how they imagine the parish regards them, which may sit wide of the
    // truth. When the gap with their real standing is stark, that gap is itself the story.
    if let Some((sa, phrase)) = sim.self_regard_of(&a.name, t) {
        let tone = if sa <= -30 { "#7a2e2e" } else if sa >= 45 { "#3a5a3a" } else { "#5a5446" };
        let gap = if sa <= -45 && a.standing as i16 - 50 > sa + 30 {
            " <span class=date>— though the parish does not, in truth, hold them so low as they fear</span>"
        } else { "" };
        body.push_str(&format!("<h2>How they feel themselves seen</h2><p class=life style='color:{tone}'>{}{}.</p>", esc(&phrase), gap));
    }
    // the particular occasions still gripping them — their episodic memory, what they carry and
    // act on. A repressed engram shows only as a nameless dread; its cause is never named.
    let carried = sim.carried_by(&a.name, t);
    if !carried.is_empty() {
        body.push_str("<h2>What they carry</h2>");
        for (kind, who, valence, salience, _day) in carried {
            let grip = if salience >= 70 { "still raw" } else if salience >= 40 { "not yet settled" } else { "fading now" };
            let whol = if who.is_empty() { who.clone() } else {
                idx_of(&who).map(|i| format!("<a href=/folk/{i} class=who>{}</a>", esc(&who))).unwrap_or_else(|| esc(&who))
            };
            let phrase = match kind.as_str() {
                "grief"   => format!("A grief — the loss of {whol}"),
                "accused" => "The terror of having stood named for murder before the whole parish".to_string(),
                "cleared" => "The relief of having been believed, and cleared".to_string(),
                "snub"    => format!("A slight from {whol}, not forgiven"),
                "wed"     => format!("The joy of their match with {whol}"),
                "haunt"   => "A dread that rises with no cause they can name — leaving them adrift, floating, strange to themselves".to_string(),
                "betrayed"   => format!("The sting of {whol} turning cold, where they had been so sure of warmth"),
                "reprieve"   => format!("Warmth from {whol} where they had given up hoping for it"),
                "wronged"    => "The parish turning against them for no thing they have done — a wrong they cannot answer".to_string(),
                "vindicated" => "Having come through the suspicion they so feared".to_string(),
                other     => esc(other),
            };
            let tone = if valence < 0 { "#7a2e2e" } else { "#3a5a3a" };
            body.push_str(&format!(
                "<div class=entry><span class=think style='color:{tone}'>{phrase}</span> <span class=date>· {grip}</span></div>"
            ));
        }
    }
    // the shape of their whole life — the defining moments kept always, oldest first. The deep,
    // continuous self: bereavements, matches, reckonings, the buried thing, carried across the years.
    let life = sim.lifelong_of(&a.name, t);
    if !life.is_empty() {
        body.push_str("<h2>The shape of their life</h2>");
        for (kind, who, valence, _salience, day) in life {
            let whol = if who.is_empty() { who.clone() } else {
                idx_of(&who).map(|i| format!("<a href=/folk/{i} class=who>{}</a>", esc(&who))).unwrap_or_else(|| esc(&who))
            };
            let phrase = match kind.as_str() {
                "grief"   => format!("The loss of {whol}"),
                "accused" => "The day they stood named for murder before the whole parish".to_string(),
                "cleared" => "The day they were believed, and cleared".to_string(),
                "wed"     => format!("Their match with {whol}"),
                "haunt"   => "A dread with no cause they can name — a thing buried past their own reach".to_string(),
                "betrayed"   => format!("{whol} turning cold, where they had been sure of warmth"),
                "reprieve"   => format!("Warmth from {whol} where they had given up hope of it"),
                "wronged"    => "The parish turning on them for nothing they had done".to_string(),
                "vindicated" => "Coming through the suspicion they so feared".to_string(),
                "snub"    => format!("A lasting hurt from {whol}"),
                other     => esc(other),
            };
            let when = sim.day_to_date(day);
            let tone = if valence < 0 { "#6a4a4a" } else { "#4a5a4a" };
            body.push_str(&format!(
                "<div class=entry><span class=date>{when}</span> &nbsp; <span class=think style='color:{tone}'>{phrase}</span></div>"
            ));
        }
    }
    // their theory of the souls who weigh on them — what they privately make of each
    if let Ok(beliefs) = sim.beliefs_held_by(&a.name, 8) {
        if !beliefs.is_empty() {
            body.push_str("<h2>What they make of others</h2>");
            for (who, t) in beliefs {
                let lk = idx_of(&who).map(|i| format!("<a href=/folk/{i} class=who>{}</a>", esc(&who))).unwrap_or_else(|| esc(&who));
                body.push_str(&format!("<div class=entry><span class=date>of {}</span><br><span class=think>{}</span></div>", lk, esc(&t)));
            }
        }
    }

    // what has lately been on their own mind — the inner life, hour by hour
    if let Ok(thoughts) = sim.self_reflections(&a.name, 3) {
        if !thoughts.is_empty() {
            body.push_str("<h2>On their mind</h2>");
            for (k, th) in thoughts.iter().enumerate() {
                let when = if k == 0 { "of late" } else { "earlier" };
                body.push_str(&format!(
                    "<div class=entry><span class=date>{}</span><br><span class=think>{}</span></div>",
                    when, esc(th)
                ));
            }
        }
    }

    // their day, phase by phase — recorded beats slotted into the routine
    if a.active() {
        if let Ok(lines) = sim.person_day(idx, t) {
            let cur = phase_now().name();
            body.push_str("<h2>Their day</h2><table><tr><th>Phase</th><th>Where</th><th>Doing</th></tr>");
            for l in lines {
                let hi = if l.phase == cur { " style='background:#f3ecda'" } else { "" };
                let doing = if l.beat {
                    format!("<b>{}</b>", esc(&l.doing))
                } else {
                    format!("<span class=next>{}</span>", esc(&l.doing))
                };
                body.push_str(&format!(
                    "<tr{hi}><td class=where>{}</td><td class=where>{}</td><td>{}</td></tr>",
                    esc(&l.phase), esc(&l.location), doing
                ));
            }
            body.push_str("</table>");
        }
    }

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
        body.push_str(&format!("<h2>Family</h2><p>{}</p>", kin.trim_end_matches("&middot; ")));
    }

    // what they've come to remember of others, from conversations had
    if let Ok(mems) = sim.memories_of(&a.name, 12) {
        if !mems.is_empty() {
            body.push_str("<h2>What they remember</h2>");
            for (who, m) in mems {
                body.push_str(&format!("<div class=entry><span class=date>of {}</span><br><span class=doing>{}</span></div>", esc(&who), esc(&m)));
            }
        }
    }

    body.push_str("<h2>Their record</h2>");
    match sim.person_events(&a.name, 300) {
        Ok(entries) if !entries.is_empty() => {
            for e in entries {
                body.push_str(&format!("<div class=entry><span class=date>{}</span><br>{}</div>", esc(&e.date), esc(&e.text)));
            }
        }
        Ok(_) => body.push_str("<p class=sub>The town has yet to remark on them.</p>"),
        Err(e) => body.push_str(&format!("<p>({})</p>", esc(&e.to_string()))),
    }
    body.push_str("<p style='margin-top:2rem'><a href=/folk>&larr; The Town</a></p>");
    page(&format!("Thrushcombe — {}", a.name), &format!("{}{}", fear_banner(sim), body))
}

fn jstr(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The kinship graph — marriages (dashed) and descent (arrows), laid out by vis-network.
fn graph(sim: &Sim) -> String {
    let world = sim.world_snapshot(today());
    let mut nodes = String::new();
    let mut edges = String::new();
    for i in 0..world.agents.len() {
        let a = &world.agents[i];
        if !a.active() {
            continue;
        }
        nodes.push_str(&format!("{{id:{},label:{},group:{}}},", i, jstr(&a.name), jstr(pretty_arch(&a.archetype))));
        if let Some(s) = a.spouse {
            if i < s && world.agents.get(s).map(|x| x.active()).unwrap_or(false) {
                edges.push_str(&format!("{{from:{},to:{},dashes:true,color:{{color:'#c0392b'}},width:2}},", i, s));
            }
        }
        if let Some(p) = a.parent {
            if world.agents.get(p).map(|x| x.active()).unwrap_or(false) {
                edges.push_str(&format!("{{from:{},to:{},arrows:'to',color:{{color:'#8a7f70'}}}},", p, i));
            }
        }
    }
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Thrushcombe — Kinship</title><style>{}\
         body{{margin:0}}#net{{height:88vh;border:1px solid var(--rule)}}.legend{{color:var(--faint);font-size:.85rem}}</style>\
         <script src='https://unpkg.com/vis-network/standalone/umd/vis-network.min.js'></script></head>\
         <body><div class=wrap style='max-width:1100px'>\
         <div class=nav><a href=/>Dashboard</a><a href=/folk>The Town</a><a href=/graph>Kinship</a><a href=/day>History</a><a href=/thoughts>Thoughts</a><a href=/talk>A word…</a></div>\
         <h1>Kinship</h1><div class=legend>marriages dashed &middot; descent arrowed &middot; drag to explore, click a soul to open them</div>\
         <div id=net></div>\
         <script>\
         var nodes=new vis.DataSet([{}]);var edges=new vis.DataSet([{}]);\
         var net=new vis.Network(document.getElementById('net'),{{nodes:nodes,edges:edges}},\
           {{nodes:{{shape:'dot',size:12,font:{{face:'Georgia',size:15}}}},physics:{{stabilization:true,barnesHut:{{springLength:120}}}}}});\
         net.on('click',function(p){{if(p.nodes.length)location.href='/folk/'+p.nodes[0];}});\
         </script></div></body></html>",
        CSS, nodes, edges
    )
}

const PHASE_NAMES: [&str; 5] = ["dawn", "forenoon", "afternoon", "evening", "night"];

/// Time-travel: the chronicle of a chosen date, grouped by phase, with prev/next.
fn day(sim: &Sim, url: &str) -> String {
    let date = qparam(url, "d").as_deref().and_then(parse_date).unwrap_or_else(today);
    let ds = date.to_string();
    let prev = date.previous_day().map(|d| d.to_string()).unwrap_or_else(|| ds.clone());
    let next = date.next_day().map(|d| d.to_string()).unwrap_or_else(|| ds.clone());
    let mut body = format!(
        "<h1>{}, {}</h1>\
         <div class=sub><a href=/day?d={}>&larr; the day before</a> &middot; \
         <form style='display:inline' method=get action=/day><input type=date name=d value={}> <button>go</button></form> \
         &middot; <a href=/day?d={}>the day after &rarr;</a></div>",
        date.weekday(), esc(&ds), esc(&prev), esc(&ds), esc(&next)
    );
    match sim.events_on(&ds, 800) {
        Ok(es) if !es.is_empty() => {
            for (ph, label) in PHASE_NAMES.iter().enumerate() {
                let slot: Vec<&_> = es.iter().filter(|e| e.phase == ph as i64).collect();
                if slot.is_empty() {
                    continue;
                }
                body.push_str(&format!("<h2>{}</h2>", esc(label)));
                for e in slot {
                    body.push_str(&format!(
                        "<div class=entry><span class=date>{}</span> {}</div>",
                        esc(&e.actor), esc(&e.text)
                    ));
                }
            }
        }
        Ok(_) => body.push_str("<p class=sub>Nothing the town saw fit to record that day.</p>"),
        Err(e) => body.push_str(&format!("<p>({})</p>", esc(&e.to_string()))),
    }
    page(&format!("Thrushcombe — {ds}"), &body)
}

// ------------------------------------------------------------------------------- dialogue
//
// Speak to a soul. You adopt a *source* soul's voice; Qwen answers as the *target* soul, in
// character, mindful of who's addressing them and what they remember. The transcript is live
// and non-deterministic; only its recorded residue (a warming or cooling, a memory kept)
// enters the fold, so the world stays exact.

/// Build the in-character system prompt for `target` speaking with `source`.
/// A short hint at how an archetype speaks, so a vet does not sound like a vicar.
fn voice_of(arch: &str) -> &'static str {
    match arch {
        "genteel" | "genteel_status_seeker" => "genteel and keenly class-conscious, given to delicate courtesies and barbs wrapped in politeness",
        "parson" => "measured and a touch moralising, never far from scripture or the parish's good order",
        "vet" | "practitioner" => "practical, dry and plain-spoken, apt to reach for a remark about beasts or weather",
        "farmer" | "hill_farmer" => "blunt, weather-wise and sparing with words, suspicious of fine talk",
        "improver" | "scheming_improver" => "restless and full of schemes and modern notions, impatient with old ways",
        "hand" | "blunt_hand" => "rough and plain, deferential to your betters but shrewd underneath",
        _ => "plain-spoken and of the country",
    }
}

/// How wide a soul's grasp of the wider world runs — soft, by station and trade. The lettered
/// read the papers and hold provincial opinions on national affairs; the rest keep to the parish.
fn learning_of(arch: &str, standing: i32) -> &'static str {
    match arch {
        "genteel" | "genteel_status_seeker" | "parson" | "improver" | "scheming_improver"
        | "vet" | "practitioner" => {
            "You are lettered and read the newspapers: you hold your own provincial opinions on the \
             wider world — the slump and the dole, the government, the troubles in Europe, the King \
             and his doings — though you weigh them from a comfortable distance."
        }
        _ if standing >= 45 => {
            "You read a little and follow the bigger news as it reaches the market, though you set \
             more store by what you can see with your own eyes than by what the papers say."
        }
        _ => {
            "You are not much for letters: your world is the parish — the beasts, the weather, the \
             market price, the chapel and the talk of the lane. Of national affairs you know only \
             what you overhear or what the vicar and the gentry let fall, and on such matters you \
             defer to your betters or turn the talk back to something nearer home."
        }
    }
}

fn persona(sim: &Sim, source: usize, target: usize) -> Option<String> {
    let w = sim.world_snapshot(today());
    let t = w.agents.get(target)?;
    let s = w.agents.get(source)?;
    let day = sim.target_day(today()).max(0);
    let role = t.trade.clone().unwrap_or_else(|| pretty_arch(&t.archetype).to_string());
    let srole = s.trade.clone().unwrap_or_else(|| pretty_arch(&s.archetype).to_string());

    // where the other stands relative to you — the engine of provincial manners
    let gap = s.standing - t.standing;
    let station = if gap >= 12 {
        format!("{} is your social superior, and you mind your manners accordingly", s.name)
    } else if gap <= -12 {
        format!("{} ranks below you, and you are quietly aware of it", s.name)
    } else {
        format!("{} is roughly your equal in the town", s.name)
    };
    // how you feel about them, from the affinity ledger
    let feeling = match w.aff(target, source) {
        f if f >= 35 => format!("You are genuinely fond of {}.", s.name),
        f if f >= 12 => format!("You think well enough of {}.", s.name),
        f if f <= -35 => format!("You bear {} a real grudge, and it colours every word.", s.name),
        f if f <= -12 => format!("You have little love for {}, and are guarded with them.", s.name),
        _ => format!("You have no strong feeling about {} either way.", s.name),
    };
    let want = thrush_core::want_phrase(&w, target);
    // a confined soul is a prisoner: their location is the gaol, and they speak from inside it —
    // never the easy small-talk of a free villager. This directive overrides the free-man framing.
    let seat_str: String = t.confined.clone().unwrap_or_else(|| t.seat.clone());
    let confinement = t.confined.as_ref().map(|place| format!(
        " IMPORTANT — YOU ARE A PRISONER. You are held in {place} and you cannot leave; you have been shut behind a locked door for weeks, cut off from your home, your work, the lanes and the harvest, and {sname} has come to speak with you through your confinement. Speak only as one imprisoned — of the cold and the stone, the long hours, what little you can see or hear, how you came to be here and what you dread is coming. You do NOT make the easy small talk of a free man, and you do not speak of your work or the harvest as though you were at large, because you are not. Anyone who speaks with you knows full well you are a prisoner.",
        sname = s.name,
    )).unwrap_or_default();

    // A bespoke character (Aldric Fynch and the like) speaks in their own prompted voice, with only
    // the live world grounding and the strict 1934 guard appended — not the generic villager scaffolding.
    if let Ok(Some(custom)) = sim.custom_persona(&t.name) {
        let mut p = custom;
        p.push_str(&format!(
            "\n\n--- WHERE YOU ARE NOW ---\nIt is the year 1934, and you are in the West-Country market town of Thrushcombe St Mary, where you have lately come to stay. You are presently {mood}. You are speaking with {sname}, {srole}. {station}. {feeling} \
             You know only what a man of your learning could know in 1934 — nothing of any matter after this year, no machine that thinks, no notion not yet born; should a stranger speak of such things you do not take their meaning, or mishear it for something of your own world, and you never break the year. Never mention being an AI or a model; never narrate stage directions or describe your own tone.",
            mood = thrush_core::mood_of(t), sname = s.name, srole = srole, station = station, feeling = feeling,
        ));
        p.push_str(&format!(" {}", thrush_core::relationships_brief(&w, target, day)));
        if let Some(rel) = thrush_core::pair_relation(&w, target, source) {
            p.push_str(&format!(" As to the one before you: {rel}."));
        }
        if let Some(brief) = sim.murder_brief(today(), &t.name) {
            p.push_str(&format!(" The shadow over the parish you have walked into: {brief}"));
        }
        if !t.secret.is_empty() {
            p.push_str(&format!(" A private matter you keep close: {}.", t.secret));
        }
        p.push_str(&format!(" The season is {}.", thrush_core::Season::of(today()).name()));
        if let Ok(recent) = sim.chronicle(5) {
            let happ: Vec<String> = recent.into_iter().rev().map(|e| e.text).collect();
            if !happ.is_empty() { p.push_str(&format!(" Lately about the parish: {}", happ.join(" "))); }
        }
        p.push_str(&confinement);
        return Some(p);
    }

    let mut p = format!(
        "You are {name}, {role} of {seat}, aged {age}, in the West-Country market town of Thrushcombe St Mary in the year 1934. \
         You are {voice}. Your standing is {standing} of a hundred and you are presently {mood}. What you want of life: {want}. \
         You are speaking with {sname}, {srole}. {station}. {feeling} \
         Speak as {name} would, and let your regard and your station set your warmth: where you are fond, be warm; where there is real coldness or a grudge, let it tell in dry reserve or a barb wrapped in courtesy, not open abuse; where you feel little either way, be civil and easy. \
         Keep your place — to a clear superior you stay courteous and a touch deferential even when you privately bridle, and you do not openly insult your betters; to those beneath you, be gracious or coolly condescending, never a brawler. \
         Be particular and true to yourself, never blandly agreeable, but never manufacture a quarrel where there is no cause. \
         Vary your phrasing; do not lean on stock fillers — avoid beginning successive lines with 'I daresay', 'I warrant' or the like. \
         You know only what a soul of your station and schooling could know in the year 1934: you have never heard of machines that think, of computers or simulations, of flight to the moon, nor of any matter after this year. \
         Should a stranger speak of such things the words carry no meaning for you — you mishear them for something from your own world, or say plainly you do not take their meaning, and you never explain a notion you could not have had. About your own world, though, you are nobody's fool. \
         Never mention being an AI or a model; never break character; never narrate stage directions or describe your own tone.",
        name = t.name, role = role, seat = seat_str, age = t.age(day),
        voice = voice_of(&t.archetype), standing = t.standing, mood = thrush_core::mood_of(t),
        want = want, sname = s.name, srole = srole, station = station, feeling = feeling,
    );
    p.push_str(&confinement);
    // the truth of who they are bound to — so they never invent a spouse or forget a child,
    // and the bond between the two speakers is named plainly (kin, marriage, or a suit).
    p.push_str(&format!(" {}", thrush_core::relationships_brief(&w, target, day)));
    if let Some(rel) = thrush_core::pair_relation(&w, target, source) {
        p.push_str(&format!(" As to the one you speak with: {rel}."));
    }
    // how wide their knowledge of the world runs, by station and trade
    p.push(' ');
    p.push_str(learning_of(&t.archetype, t.standing));
    // a single illustration of the move — deflect the anachronism, don't lecture in its terms
    p.push_str(
        " By way of example only, never to be repeated word for word: were someone to ask after a \
         'computer', or whether the world is but a 'simulation', you would frown at the strange word, \
         take it perhaps for some contraption or play-acting you have not seen, and turn the talk back \
         to what is real to you — never philosophising in their terms.",
    );
    if let Ok(mems) = sim.memories_of(&t.name, 8) {
        let about: Vec<String> = mems.into_iter().filter(|(who, _)| who == &s.name).map(|(_, m)| m).collect();
        if !about.is_empty() {
            p.push_str(&format!(" What you already remember of {}: {}.", s.name, about.join("; ")));
        }
    }
    // your settled, private read of the one you are speaking with — your theory of them
    if let Ok(Some(b)) = sim.belief_of(&t.name, &s.name) {
        p.push_str(&format!(" What you have privately come to believe of {}: {}", s.name, b));
    }
    // your own life, and what the parish knows of the one addressing you — the histories you both carry
    if let Ok(Some(bio)) = sim.biography(&t.name) {
        p.push_str(&format!(" Your own life, as the parish tells it: {bio}"));
    }
    if let Ok(Some(bio)) = sim.biography(&s.name) {
        p.push_str(&format!(" What is known of {}: {bio}", s.name));
    }
    // what has lately been on your own mind, so you bring a present, thinking self to the talk
    if let Ok(thoughts) = sim.self_reflections(&t.name, 2) {
        if !thoughts.is_empty() {
            p.push_str(&format!(" Of late you have been turning over in your mind: {}.", thoughts.join("; ")));
        }
    }
    // the killing hanging over the town, as THIS soul truly knows and feels it — so they speak of
    // it in character, and an open murder is never mistaken for a closed one (the magistrate above
    // all: a hold or a widening is not a conclusion while the killer walks free)
    if let Some(brief) = sim.murder_brief(today(), &t.name) {
        p.push_str(&format!(" {brief}"));
    }
    // a grounded private truth you carry — fed only to YOU, kept consistent. The true killer's is
    // repressed and must NEVER be confessed, even here, even pressed; it leaks only as unease. An
    // ordinary secret is simply kept close and the talk turned aside from it.
    if !t.secret.is_empty() {
        let repressed = w.inquest.as_ref().is_some_and(|q| q.culprit == target as i32)
            || t.memories.iter().any(|m| m.kind == "haunt");
        if repressed {
            p.push_str(&format!(
                " There is a thing you have buried so deep you can scarcely know it for what it is, and you will NEVER speak it — not pressed, not cornered, not to save another: {}. Should the talk come near it you grow vague, you flinch, you turn it aside; it shows only as an unease you cannot account for, never as a word.",
                t.secret
            ));
        } else {
            p.push_str(&format!(
                " You carry a private truth you will tell no one, and you keep the talk away from it: {}.",
                t.secret
            ));
        }
    }
    // the town as it actually stands, so the talk can touch real goings-on
    p.push_str(&format!(" The season is {}.", thrush_core::Season::of(today()).name()));
    if let Ok(recent) = sim.chronicle(5) {
        let happenings: Vec<String> = recent.into_iter().rev().map(|e| e.text).collect();
        if !happenings.is_empty() {
            p.push_str(&format!(" Lately about the parish: {}", happenings.join(" ")));
        }
    }
    Some(p)
}

/// Resolve the `claude` CLI binary: CLAUDE_BIN if set, else the usual ~/.local/bin path (so a
/// headless --user service finds it without a full PATH), else bare "claude" on PATH.
fn claude_bin() -> String {
    if let Ok(b) = std::env::var("CLAUDE_BIN") {
        if !b.is_empty() { return b; }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let p = format!("{home}/.local/bin/claude");
    if std::path::Path::new(&p).exists() { return p; }
    "claude".into()
}

/// The web's oracle, same as the town's: one shot through `claude -p --model sonnet` (the local
/// subscription). System prompt appended, the conversation piped on stdin; the reply is its text.
fn claude_text(system: &str, user: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_else(|_| "sonnet".into());
    let mut child = Command::new(claude_bin())
        .arg("-p").arg("--model").arg(&model).arg("--append-system-prompt").arg(system)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().ok()?;
    { let mut si = child.stdin.take()?; si.write_all(user.as_bytes()).ok()?; }
    let out = child.wait_with_output().ok()?;
    out.status.success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// As `claude_text`, but pulling the first JSON object out of the reply.
fn claude_json(system: &str, user: &str) -> Option<serde_json::Value> {
    let t = claude_text(system, user)?;
    let (start, end) = (t.find('{')?, t.rfind('}')?);
    (end > start).then(|| serde_json::from_str::<serde_json::Value>(&t[start..=end]).ok()).flatten()
}

/// One turn of conversation through the oracle (the `claude` CLI, Sonnet). `history` is the prior
/// (role, content) turns — assistant = this speaker, user = the other — laid out as a transcript so
/// the reply builds on the talk and never loops. The system prompt carries voice + the anti-repeat rules.
fn chat_reply(system: &str, history: &[(String, String)], message: &str) -> Option<String> {
    let mut convo = String::new();
    for (role, content) in history {
        let who = if role == "assistant" { "You" } else { "They" };
        convo.push_str(&format!("{who}: {content}\n"));
    }
    let user = if convo.is_empty() {
        message.to_string() // the opener: a seed stage-direction, answered fresh
    } else {
        format!("{convo}They: {message}\n\nYour reply — your next turn only, in your own voice, advancing the talk and never repeating anything already said above:")
    };
    chat_reply_raw(system, &user)
}

/// The bare CLI call behind a turn, with the filler tic stripped per line.
fn chat_reply_raw(system: &str, user: &str) -> Option<String> {
    let s = claude_text(system, user)?;
    // a reply may come back with a leading "You:"/"Name:" label the model echoes from the transcript
    let s = s.strip_prefix("You:").unwrap_or(&s).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// At the end of a conversation, have the oracle judge its residue: did the target warm or
/// cool toward the source, and what one line do they keep of it?
fn assess_dialogue(target_name: &str, source_name: &str, transcript: &str) -> Option<(String, String, String)> {
    let sys = format!(
        "You judge how a conversation has left {target}, a soul of a 1934 West-Country town, feeling about {source}. \
         Respond ONLY as JSON: {{\"warmth\": one of [warmer, colder, unchanged], \"memory\": one short sentence in {target}'s own voice of what they now think of {source}, \"sway\": one of [none, debt, rise, prosper, content, reconcile]}}. \
         Default to 'unchanged' — most talk leaves regard where it was. Reserve 'warmer' for a genuinely warming exchange and 'colder' for real friction or a slight; mere courtesy, or plain civility across a difference of station, is 'unchanged', not warmer. \
         Let the memory keep {target}'s own station and regard — a plain soul speaks of a social better with deference, a better of a lesser from a height, not as easy equals. \
         sway is whether the talk changed what {target} wants: debt=resolved to clear their debts, rise=spurred to rise, prosper=to make a fortune, content=to rest content, reconcile=to mend a quarrel, none=unchanged.",
        target = target_name, source = source_name,
    );
    let prompt = format!("The conversation:\n{transcript}\n\nHow has it left {target_name} toward {source_name}?");
    let parsed = claude_json(&sys, &prompt)?;
    let warmth = parsed.get("warmth")?.as_str()?.trim().to_lowercase();
    let warmth = ["warmer", "colder", "unchanged"].into_iter().find(|w| warmth.contains(w)).unwrap_or("unchanged").to_string();
    let sway = parsed.get("sway").and_then(|s| s.as_str()).unwrap_or("none").trim().to_lowercase();
    let sway = ["debt", "rise", "prosper", "content", "reconcile"].into_iter().find(|s| sway.contains(s)).unwrap_or("none").to_string();
    let memory = parsed.get("memory")?.as_str()?.trim().to_string();
    (!memory.is_empty()).then_some((warmth, memory, sway))
}

/// The conversation page: pick a source and a target, then talk.
fn talk_page(sim: &Sim, url: &str) -> String {
    let w = sim.world_snapshot(today());
    let live: Vec<usize> = (0..w.agents.len()).filter(|&i| w.agents[i].active() && w.agents[i].archetype != "child").collect();
    let preset_to: i64 = qparam(url, "to").and_then(|s| s.parse().ok()).unwrap_or(-1);
    let opts = |sel: i64| -> String {
        live.iter()
            .map(|&i| format!("<option value={}{}>{}</option>", i, if i as i64 == sel { " selected" } else { "" }, esc(&w.agents[i].name)))
            .collect()
    };
    let body = format!(
        "<h1>A word with…</h1>\
         <div class=sub>Adopt one soul's voice, and speak to another. They answer in character — and remember.</div>\
         <div class=card>\
           <label>You speak as <select id=src><option value=-1>— pick a soul —</option>{src}</select></label> &nbsp; \
           <label>to <select id=tgt><option value=-1>— pick a soul —</option>{tgt}</select></label>\
         </div>\
         <div id=log></div>\
         <div class=card><input id=msg placeholder='Say something…' style='width:78%' autocomplete=off> \
           <button onclick=say()>Say</button> <button onclick=conclude() style='float:right'>End &amp; record</button></div>\
         <div id=out class=sub></div>\
         <h1 style='margin-top:1.4em'>…or set two souls talking</h1>\
         <div class=sub>Pick two, and watch them fall into conversation of their own accord. What passes between them stays with them — and the town hears of it.</div>\
         <div class=card>\
           <label><select id=pa><option value=-1>— a soul —</option>{pa}</select></label> &nbsp;and&nbsp; \
           <label><select id=pb><option value=-1>— a soul —</option>{pb}</select></label> &nbsp; \
           <button id=bbtn onclick=between()>Let them talk →</button>\
         </div>\
         <div id=blog></div>\
         <div id=bout class=sub></div>\
         <script>\
         var hist=[];\
         var busy=false;\
         function blin(who,txt){{var d=document.getElementById('blog');d.innerHTML+='<div class=entry><span class=date>'+who+'</span><br><span class=where>'+txt+'</span></div>';window.scrollTo(0,document.body.scrollHeight);}}\
         async function between(){{\
           if(busy)return;var pa=document.getElementById('pa'),pb=document.getElementById('pb');\
           if(pa.value<0||pb.value<0||pa.value===pb.value){{document.getElementById('bout').textContent='Pick two different souls.';return;}}\
           busy=true;var btn=document.getElementById('bbtn');btn.disabled=true;\
           var a=+pa.value,b=+pb.value,h=[],blog=document.getElementById('blog');blog.innerHTML='';document.getElementById('bout').textContent='…';\
           for(var k=0;k<8;k++){{\
             var r=await fetch('/talk/between',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{a:a,b:b,history:h}})}});\
             var j=await r.json();if(!j.line){{break;}}\
             h.push([j.speaker,j.line]);blin(j.name,j.line);\
             if(j.done)break;await new Promise(function(res){{setTimeout(res,1000);}});\
           }}\
           document.getElementById('bout').textContent='recording…';\
           var e=await fetch('/talk/between/end',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{a:a,b:b,history:h}})}});\
           var ej=await e.json();\
           var notes=(ej.notes||[]).join('<br>');var stir=notes.indexOf('away warmer')>=0||notes.indexOf('away colder')>=0;\
           document.getElementById('bout').innerHTML=notes+(stir?'<br><i>The town will hear of it.</i>':'<br><i>It passed without remark.</i>');\
           busy=false;btn.disabled=false;\
         }}\
         function add(who,txt,cls){{var d=document.getElementById('log');d.innerHTML+='<div class=entry><span class=date>'+who+'</span><br><span class='+cls+'>'+txt+'</span></div>';window.scrollTo(0,document.body.scrollHeight);}}\
         function nm(s){{return s.options[s.selectedIndex].text;}}\
         async function say(){{\
           var src=document.getElementById('src'),tgt=document.getElementById('tgt'),m=document.getElementById('msg');\
           if(src.value<0||tgt.value<0||!m.value.trim())return;\
           add(nm(src),m.value,'doing');var msg=m.value;m.value='';\
           var r=await fetch('/talk/say',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{source:+src.value,target:+tgt.value,history:hist,message:msg}})}});\
           var j=await r.json();hist.push(['user',msg]);hist.push(['assistant',j.reply||'(no answer)']);add(nm(tgt),j.reply||'(silence)','where');\
         }}\
         async function conclude(){{\
           var src=document.getElementById('src'),tgt=document.getElementById('tgt');if(src.value<0||tgt.value<0||!hist.length)return;\
           document.getElementById('out').textContent='recording…';\
           var r=await fetch('/talk/end',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{source:+src.value,target:+tgt.value,history:hist}})}});\
           var j=await r.json();document.getElementById('out').textContent=nm(tgt)+' came away '+(j.warmth||'unchanged')+'. They will remember: \"'+(j.memory||'')+'\"';hist=[];\
         }}\
         document.getElementById('msg').addEventListener('keydown',function(e){{if(e.key==='Enter')say();}});\
         </script>",
        src = opts(-1), tgt = opts(preset_to), pa = opts(-1), pb = opts(-1)
    );
    page("Thrushcombe — A word with…", &body)
}

/// A map of Thrushcombe St Mary — the parish laid out by its places, every living soul a little
/// portrait-dot at their own door. Clean HTML/CSS (no fragile inline SVG); a generated map
/// illustration drops in behind it as the background if one is placed at portraits/map.<ext>.
fn map_page(sim: &Sim) -> String {
    let w = sim.world_snapshot(today());
    // the parish geography as percentages of the map panel: (seat-key, label, x%, y%)
    let places: &[(&str, &str, i32, i32)] = &[
        ("high foldside", "High Foldside", 13, 12),
        ("the laurels", "The Laurels", 16, 28),
        ("crale court", "Crale Court", 86, 14),
        ("the crale estate", "Crale Estate", 90, 24),
        ("five elms", "Five Elms", 84, 29),
        ("the vicarage", "The Vicarage", 44, 22),
        ("the school", "The School", 36, 33),
        ("the post office", "The Post Office", 54, 34),
        ("the bank house", "The Bank House", 64, 30),
        ("church row", "Church Row", 33, 44),
        ("the churchyard", "The Churchyard", 50, 50),
        ("the constabulary", "The Constabulary", 60, 45),
        ("the committee", "The Vestry", 41, 40),
        ("the draper's", "The Draper's", 56, 40),
        ("the shop", "The Shop", 66, 42),
        ("the shambles", "The Shambles", 45, 55),
        ("the mill", "The Mill", 26, 50),
        ("beck house", "Beck House", 22, 60),
        ("springs house", "Springs House", 12, 44),
        ("ivy cottage", "Ivy Cottage", 76, 50),
        ("the pelican", "The Pelican", 64, 62),
        ("the forge", "The Forge", 40, 68),
        ("the bakehouse", "The Bakehouse", 70, 70),
        ("the empty cottage", "Widcombe Lane", 52, 80),
        ("home farm", "Home Farm", 16, 76),
        ("gunnerside", "Gunnerside", 26, 90),
        ("the station", "The Station", 82, 80),
        ("the carrier's yard", "Carrier's Yard", 68, 88),
        ("the knacker's yard", "Knacker's Yard", 92, 92),
        ("the docks at plymouth", "Away - Plymouth", 90, 7),
    ];

    // place each soul by which location their seat names — substring match, so "lodgings at the
    // Pelican" lands at the Pelican and "…at the Bank House of Church Row" at the Bank House.
    let mut by_seat: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, a) in w.agents.iter().enumerate() {
        if !a.active() { continue; }
        let seat = a.seat.trim().to_lowercase();
        if let Some(&(key, ..)) = places.iter().find(|(k, ..)| seat.contains(k)) {
            by_seat.entry(key).or_default().push(i);
        }
    }

    let avatar = |i: usize, extra: &str| -> String {
        let sx = if w.agents[i].sex == 0 { "w" } else { "m" };
        format!(
            "<a class='av {sx}' href='/folk/{i}' title='{}{extra}' style=\"background-image:url(/portraits/{i})\"></a>",
            esc(&w.agents[i].name)
        )
    };

    let mut markers = String::new();
    let mut placed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut drawn: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for &(key, label, x, y) in places.iter() {
        if !drawn.insert(label) { continue; }
        let here: Vec<usize> = by_seat.get(key).cloned().unwrap_or_default();
        let mut avs = String::new();
        for &i in here.iter().take(8) {
            placed.insert(i);
            avs.push_str(&avatar(i, ""));
        }
        if here.len() > 8 { avs.push_str(&format!("<span class=more>+{}</span>", here.len() - 8)); }
        markers.push_str(&format!(
            "<div class=loc style='left:{x}%;top:{y}%'><div class=ln>{}</div><div class=av-row>{avs}</div></div>",
            esc(label)
        ));
    }
    // any soul whose seat has no spot on the map — gathered below, so no one is lost
    let strays: Vec<usize> = (0..w.agents.len())
        .filter(|&i| w.agents[i].active() && !placed.contains(&i)).collect();
    let mut stray_html = String::new();
    if !strays.is_empty() {
        for &i in &strays {
            stray_html.push_str(&avatar(i, &format!(" - of {}", esc(&w.agents[i].seat))));
        }
        stray_html = format!("<div class=sub style='margin-top:1.2rem'>elsewhere about the parish:</div><div class=av-row style='justify-content:flex-start;margin-top:.3rem'>{stray_html}</div>");
    }

    let style = "<style>\
      .mapwrap{position:relative;width:100%;aspect-ratio:3/2;margin:.5rem 0 1rem;border:1px solid #cdbf9d;border-radius:10px;\
        background:radial-gradient(circle at 50% 45%,#f4ecd6,#e6dabb 70%,#dccfa9);\
        box-shadow:inset 0 0 60px rgba(120,95,55,.18);overflow:hidden;touch-action:none;user-select:none}\
      .mapinner{position:absolute;inset:0;transform-origin:0 0;background-size:cover;background-position:center;cursor:grab}\
      .mapinner.drag{cursor:grabbing}\
      .church{position:absolute;left:50%;top:45%;transform:translate(-50%,-50%);font:italic 13px Georgia;color:#5b4d39;text-align:center;opacity:.8;pointer-events:none}\
      .church:before{content:'\\271D';display:block;font-size:20px;line-height:1}\
      .loc{position:absolute;transform:translate(-50%,-50%);text-align:center;width:130px}\
      .ln{font:600 11px Georgia;color:#5b4d39;background:rgba(247,240,220,.74);border-radius:8px;padding:1px 6px;display:inline-block;margin-bottom:3px;white-space:nowrap}\
      .av-row{display:flex;flex-wrap:wrap;gap:2px;justify-content:center}\
      .av{display:inline-block;width:30px;height:30px;border-radius:50%;background-size:cover;background-position:center 18%;\
        border:2px solid #fff;box-shadow:0 1px 3px rgba(80,60,30,.45);transition:transform .1s}\
      .av:hover{transform:scale(1.3);z-index:5;position:relative}\
      .av.m{background-color:#6f86a6}.av.w{background-color:#b07f8e}\
      .more{font:600 11px Georgia;color:#7a6a4e;align-self:center;margin-left:2px}\
      .mapctl{position:absolute;right:8px;bottom:8px;display:flex;gap:4px;z-index:10}\
      .mapctl button{width:30px;height:30px;border:1px solid #b8a888;background:rgba(247,240,220,.94);color:#5b4d39;border-radius:6px;font:600 17px Georgia;cursor:pointer;line-height:1;padding:0}\
      .mapctl button:hover{background:#fff}\
    </style>";

    let mut body = format!(
        "{style}<h1>Thrushcombe St Mary</h1>\
         <div class=sub>The parish, place by place - every living soul a face at their own door. \
         <span style='color:#6f86a6'>&#9679;</span> a man, <span style='color:#b07f8e'>&#9679;</span> a woman; \
         scroll to zoom, drag to pan, click a soul for their page. \
         (A drawn map slots in behind once one is placed at <code>portraits/map.jpg</code>.)</div>\
         <div class=mapwrap id=mw>\
           <div class=mapinner id=mi style=\"background-image:url(/portraits/map)\"><div class=church>St Mary's</div>{markers}</div>\
           <div class=mapctl><button id=zin title='zoom in'>+</button><button id=zout title='zoom out'>&minus;</button><button id=zre title='reset'>&#8635;</button></div>\
         </div>\
         {stray_html}\
         <p style='margin-top:1rem'><a href=/folk>&larr; the cast in full</a></p>"
    );
    body.push_str(MAP_SCRIPT);
    page("Thrushcombe - the map", &body)
}

/// Pan/zoom for the map: wheel zooms toward the cursor, drag pans, the buttons step and reset, and
/// touch gives one-finger pan and two-finger pinch. Plain JS, no library.
const MAP_SCRIPT: &str = "<script>(function(){\
  var mi=document.getElementById('mi'),mw=document.getElementById('mw');if(!mi||!mw)return;\
  var s=1,tx=0,ty=0,drag=false,lx=0,ly=0;\
  function cl(v,a,b){return v<a?a:(v>b?b:v);}\
  function ap(){mi.style.transform='translate('+tx+'px,'+ty+'px) scale('+s+')';}\
  function zoomAt(cx,cy,f){var ns=cl(s*f,0.6,9),k=ns/s;tx=cx-(cx-tx)*k;ty=cy-(cy-ty)*k;s=ns;ap();}\
  mw.addEventListener('wheel',function(e){e.preventDefault();var r=mw.getBoundingClientRect();zoomAt(e.clientX-r.left,e.clientY-r.top,e.deltaY<0?1.15:1/1.15);},{passive:false});\
  mw.addEventListener('mousedown',function(e){if(e.target.closest('.mapctl'))return;drag=true;lx=e.clientX;ly=e.clientY;mi.classList.add('drag');});\
  window.addEventListener('mousemove',function(e){if(!drag)return;tx+=e.clientX-lx;ty+=e.clientY-ly;lx=e.clientX;ly=e.clientY;ap();});\
  window.addEventListener('mouseup',function(){drag=false;mi.classList.remove('drag');});\
  function ctr(f){var r=mw.getBoundingClientRect();zoomAt(r.width/2,r.height/2,f);}\
  document.getElementById('zin').onclick=function(){ctr(1.3);};\
  document.getElementById('zout').onclick=function(){ctr(1/1.3);};\
  document.getElementById('zre').onclick=function(){s=1;tx=0;ty=0;ap();};\
  var pts={},pd=0;\
  mw.addEventListener('touchstart',function(e){for(var i=0;i<e.changedTouches.length;i++){var t=e.changedTouches[i];pts[t.identifier]={x:t.clientX,y:t.clientY};}},{passive:false});\
  mw.addEventListener('touchmove',function(e){e.preventDefault();var ids=Object.keys(pts);\
    if(ids.length===1){var t=e.changedTouches[0],p=pts[t.identifier];if(p){tx+=t.clientX-p.x;ty+=t.clientY-p.y;p.x=t.clientX;p.y=t.clientY;ap();}}\
    else if(ids.length>=2){for(var i=0;i<e.changedTouches.length;i++){var u=e.changedTouches[i];if(pts[u.identifier]){pts[u.identifier].x=u.clientX;pts[u.identifier].y=u.clientY;}}\
      var a=pts[ids[0]],b=pts[ids[1]],nd=Math.hypot(a.x-b.x,a.y-b.y);if(pd){var r=mw.getBoundingClientRect();zoomAt((a.x+b.x)/2-r.left,(a.y+b.y)/2-r.top,nd/pd);}pd=nd;}},{passive:false});\
  mw.addEventListener('touchend',function(e){for(var i=0;i<e.changedTouches.length;i++){delete pts[e.changedTouches[i].identifier];}pd=0;});\
})();</script>";

/// A filename-safe slug of a name: lowercase, runs of non-alphanumerics become single hyphens.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// A name with any leading honorific dropped ("Mrs Cynthia Pelham" → "Cynthia Pelham").
fn strip_title(name: &str) -> String {
    const TITLES: &[&str] = &["mr", "mrs", "miss", "ms", "dr", "revd", "rev", "major", "lady", "sir", "constable", "old", "capt", "col"];
    let mut parts: Vec<&str> = name.split_whitespace().collect();
    if parts.len() > 1 && TITLES.contains(&parts[0].trim_end_matches('.').to_lowercase().as_str()) {
        parts.remove(0);
    }
    parts.join(" ")
}

/// Find a soul's portrait file in the folder — by index OR a name-slug, in any common image format.
/// Returns the bytes and the right content-type. Lets the user name files however reads naturally.
fn resolve_portrait(dir: &std::path::Path, sim: &Sim, req: &str) -> Option<(Vec<u8>, &'static str)> {
    let base = req.rsplit_once('.').map(|(b, _)| b).unwrap_or(req).trim();
    if base.is_empty() {
        return None;
    }
    // candidate filename stems, most specific first
    let mut cands: Vec<String> = Vec::new();
    if base.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        cands.push(base.to_lowercase()); // an index (18) or a slug already
    }
    if let Ok(idx) = base.parse::<usize>() {
        let w = sim.world_snapshot(today());
        if let Some(a) = w.agents.get(idx) {
            cands.push(slugify(&a.name)); // major-pringle, mrs-pringle
            let st = slugify(&strip_title(&a.name));
            if !st.is_empty() { cands.push(st); } // cynthia-pelham, aldermaston
            // drop a trailing post-nominal (MRCVS, MD, …): 'Mr Farran MRCVS' -> mr-farran
            let words: Vec<&str> = a.name.split_whitespace().collect();
            if words.len() > 1 && words.last().is_some_and(|w| w.len() >= 2 && w.chars().all(|c| c.is_ascii_uppercase())) {
                cands.push(slugify(&words[..words.len() - 1].join(" ")));
            }
        }
    }
    for cand in &cands {
        if cand.is_empty() || cand.contains("..") { continue; }
        for (ext, ct) in [("jpg", "image/jpeg"), ("jpeg", "image/jpeg"), ("png", "image/png"), ("webp", "image/webp")] {
            if let Ok(bytes) = std::fs::read(dir.join(format!("{cand}.{ext}"))) {
                return Some((bytes, ct));
            }
        }
    }
    None
}

fn route(sim: &Sim, url: &str) -> String {
    let path = url.split('?').next().unwrap_or("/");
    if path == "/" {
        dashboard(sim)
    } else if path == "/folk" {
        folk(sim)
    } else if path == "/graph" {
        graph(sim)
    } else if path == "/day" {
        day(sim, url)
    } else if path == "/talk" {
        talk_page(sim, url)
    } else if path == "/thoughts" {
        thoughts(sim, url)
    } else if path == "/inquiry" {
        inquiry(sim)
    } else if path == "/map" {
        map_page(sim)
    } else if let Some(rest) = path.strip_prefix("/folk/") {
        match rest.parse::<usize>() {
            Ok(i) => person(sim, i),
            Err(_) => page("Not found", "<h1>Not found</h1>"),
        }
    } else {
        page("Not found", "<h1>Not found</h1><p><a href=/>Home</a></p>")
    }
}

/// Render the history list [["user",msg],["assistant",reply],…] into a labelled transcript.
fn transcript_of(sim: &Sim, source: usize, target: usize, hist: &serde_json::Value) -> String {
    let w = sim.world_snapshot(today());
    let sn = w.agents.get(source).map(|a| a.name.clone()).unwrap_or_default();
    let tn = w.agents.get(target).map(|a| a.name.clone()).unwrap_or_default();
    hist.as_array()
        .map(|turns| {
            turns
                .iter()
                .filter_map(|t| {
                    let pair = t.as_array()?;
                    let who = if pair.first()?.as_str()? == "user" { &sn } else { &tn };
                    Some(format!("{who}: {}", pair.get(1)?.as_str()?))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// GET /api/roster — the living adult cast as JSON {idx,name,seat,standing,sex}, so the Discord
/// bot can resolve who a message addresses and which souls belong to which place-channel.
fn handle_roster(sim: &Sim) -> String {
    let w = sim.world_snapshot(today());
    let arr: Vec<serde_json::Value> = w.agents.iter().enumerate()
        .filter(|(_, a)| a.active() && a.archetype != "child")
        .map(|(i, a)| serde_json::json!({
            "idx": i, "name": a.name, "seat": a.seat, "standing": a.standing, "sex": a.sex, "suspicion": a.suspicion,
        }))
        .collect();
    serde_json::json!({ "roster": arr }).to_string()
}

/// POST /talk/say — one in-character reply from the target. Returns {"reply": "..."}.
fn handle_say(sim: &Sim, body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let source = v.get("source").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let target = v.get("target").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let message = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
    let history: Vec<(String, String)> = v
        .get("history")
        .and_then(|h| h.as_array())
        .map(|a| a.iter().filter_map(|t| {
            let p = t.as_array()?;
            Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
        }).collect())
        .unwrap_or_default();
    let reply = persona(sim, source, target)
        .and_then(|sys| chat_reply(&sys, &history, message))
        .map(|l| thrush_core::strip_filler(&l))
        .unwrap_or_else(|| "…".into());
    serde_json::json!({ "reply": reply }).to_string()
}

/// POST /talk/end — judge and record the conversation's residue. Returns {warmth, memory}.
fn handle_end(sim: &mut Sim, body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let source = v.get("source").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let target = v.get("target").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let transcript = transcript_of(sim, source, target, v.get("history").unwrap_or(&serde_json::Value::Null));
    let w = sim.world_snapshot(today());
    let (sn, tn) = match (w.agents.get(source), w.agents.get(target)) {
        (Some(s), Some(t)) => (s.name.clone(), t.name.clone()),
        _ => return serde_json::json!({"warmth": "unchanged", "memory": ""}).to_string(),
    };
    match assess_dialogue(&tn, &sn, &transcript) {
        Some((warmth, memory, sway)) => {
            let _ = sim.record_dialogue(today(), &sn, &tn, &transcript, &memory, &warmth, &sway);
            let _ = sim.catch_up(today(), phase_now());
            serde_json::json!({ "warmth": warmth, "memory": memory }).to_string()
        }
        None => serde_json::json!({ "warmth": "unchanged", "memory": "" }).to_string(),
    }
}

/// Generate one in-character line from `speaker` (addressing `other`), given the conversation
/// so far as a list of (speaker_idx, line) turns. The opener gets a gentle seed.
fn converse_line(sim: &Sim, speaker: usize, other: usize, transcript: &[(usize, String)]) -> Option<String> {
    let mut system = persona(sim, other, speaker)?; // the speaker's own voice, aware of who they address
    let w = sim.world_snapshot(today());
    let oname = w.agents.get(other)?.name.clone();
    if transcript.is_empty() {
        system.push_str(" Reply in one or two sentences only.");
        let seed = format!(
            "(You come upon {oname} about the parish. Open with a brief greeting, then say what is actually on your mind — \
             a remark on the goings-on, a question for them, a piece of news, a complaint. Do not be bland.)"
        );
        return chat_reply(&system, &[], &seed).map(|l| thrush_core::strip_filler(&l));
    }
    // mid-conversation: the hard rules that keep it from re-greeting and parroting
    system.push_str(&format!(
        " You are now mid-conversation with {oname} — pleasantries are done. \
         Do NOT greet again, do NOT say their name unless it lands, and NEVER echo, repeat or paraphrase what either of you has already said — do not reuse a phrase that has already been spoken in this talk. \
         Answer it for real: ask after something, share a piece of news, agree and build on it, reminisce, confide, tease gently, or — only if you have real cause — press or disagree. \
         Let the talk breathe; do not strain to top their last line or sharpen with every turn. Reply in one or two sentences, in your own true voice."
    ));
    let mut history: Vec<(String, String)> = Vec::new();
    for (who, line) in &transcript[..transcript.len() - 1] {
        let role = if *who == speaker { "assistant" } else { "user" };
        history.push((role.to_string(), line.clone()));
    }
    // the last turn is the other's line — feed it as the message this speaker now answers
    chat_reply(&system, &history, &transcript[transcript.len() - 1].1).map(|l| thrush_core::strip_filler(&l))
}

/// Parse {a, b, history:[[idx,line],…]} into (a, b, transcript).
fn parse_between(body: &str) -> (usize, usize, Vec<(usize, String)>) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let a = v.get("a").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let b = v.get("b").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let transcript = v
        .get("history")
        .and_then(|h| h.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let p = t.as_array()?;
                    Some((p.first()?.as_u64()? as usize, p.get(1)?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    (a, b, transcript)
}

/// How many lines a watched conversation runs to before it is wound up.
const BETWEEN_CAP: usize = 6;

/// POST /talk/between — produce the next single line of a two-soul conversation.
fn handle_between(sim: &Sim, body: &str) -> String {
    let (a, b, transcript) = parse_between(body);
    if a == b || transcript.len() >= BETWEEN_CAP {
        return serde_json::json!({ "done": true }).to_string();
    }
    let speaker = if transcript.is_empty() {
        a
    } else if transcript.last().map(|t| t.0) == Some(a) {
        b
    } else {
        a
    };
    let other = if speaker == a { b } else { a };
    let line = converse_line(sim, speaker, other, &transcript).unwrap_or_else(|| "…".into());
    let w = sim.world_snapshot(today());
    let name = w.agents.get(speaker).map(|x| x.name.clone()).unwrap_or_default();
    serde_json::json!({ "speaker": speaker, "name": name, "line": line, "done": transcript.len() + 1 >= BETWEEN_CAP }).to_string()
}

/// POST /talk/between/end — judge the conversation's residue for *both* souls, record each
/// (so it folds deterministically), and let the town hear of it.
fn handle_between_end(sim: &mut Sim, body: &str) -> String {
    let (a, b, transcript) = parse_between(body);
    let w = sim.world_snapshot(today());
    let (an, bn) = match (w.agents.get(a), w.agents.get(b)) {
        (Some(x), Some(y)) => (x.name.clone(), y.name.clone()),
        _ => return serde_json::json!({ "ok": false, "notes": [] }).to_string(),
    };
    let text = transcript
        .iter()
        .map(|(who, line)| format!("{}: {line}", if *who == a { &an } else { &bn }))
        .collect::<Vec<_>>()
        .join("\n");
    // judge each direction before recording (assessment reads only the transcript)
    let toward_a = assess_dialogue(&an, &bn, &text); // how A feels about B
    let toward_b = assess_dialogue(&bn, &an, &text); // how B feels about A
    let mut notes = Vec::new();
    if let Some((warmth, memory, sway)) = toward_b {
        let _ = sim.record_dialogue(today(), &an, &bn, &text, &memory, &warmth, &sway);
        notes.push(format!("{bn} came away {warmth}. They will remember: \u{201c}{memory}\u{201d}"));
    }
    if let Some((warmth, memory, sway)) = toward_a {
        let _ = sim.record_dialogue(today(), &bn, &an, &text, &memory, &warmth, &sway);
        notes.push(format!("{an} came away {warmth}. They will remember: \u{201c}{memory}\u{201d}"));
    }
    let _ = sim.catch_up(today(), phase_now());
    serde_json::json!({ "ok": true, "notes": notes }).to_string()
}

fn main() {
    let db = std::env::args().nth(1).unwrap_or_else(|| "world.db".into());
    let addr = std::env::var("THRUSH_WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:8717".into());

    let mut sim = Sim::open(&db).unwrap_or_else(|e| {
        eprintln!("could not open {db}: {e}");
        std::process::exit(1);
    });
    let _ = sim.catch_up(today(), thrush_core::Phase::from_hour(now().hour()));

    let server = Server::http(&addr).unwrap_or_else(|e| {
        eprintln!("could not bind {addr}: {e}");
        std::process::exit(1);
    });
    // When THRUSH_WEB_KEY is set (e.g. behind a public Funnel), gate every request behind
    // HTTP Basic auth — any username, password == the key. Unset (tailnet use) = wide open.
    let web_key = std::env::var("THRUSH_WEB_KEY").ok().filter(|k| !k.is_empty());
    // sepia portraits live in a `portraits/` folder beside the world db — served as /portraits/<n>.jpg
    let portrait_dir = std::path::Path::new(&db).parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("portraits"))
        .unwrap_or_else(|| std::path::PathBuf::from("portraits"));
    println!(
        "Thrushcombe reader on http://{addr}  (db: {db}){}",
        if web_key.is_some() { "  [auth on]" } else { "" }
    );
    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        // a soul's portrait is public (no auth) — Discord and other readers fetch these as avatars.
        // Served by index OR name-slug, any image format (18.jpg, major-pringle.png … all just work).
        if let Some(name) = url.strip_prefix("/portraits/") {
            let key = name.split(['?', '/']).next().unwrap_or("");
            if let Some((bytes, ct)) = resolve_portrait(&portrait_dir, &sim, key) {
                let hdr = Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap();
                let cache = Header::from_bytes(&b"Cache-Control"[..], &b"public, max-age=86400"[..]).unwrap();
                let _ = req.respond(Response::from_data(bytes).with_header(hdr).with_header(cache));
            } else {
                let _ = req.respond(Response::from_string("").with_status_code(404));
            }
            continue;
        }
        if let Some(key) = &web_key {
            let ok = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .and_then(|h| {
                    let v = h.value.as_str();
                    let b64 = v
                        .strip_prefix("Basic ")
                        .or_else(|| v.strip_prefix("basic "))?;
                    let raw = base64::engine::general_purpose::STANDARD
                        .decode(b64.trim())
                        .ok()?;
                    let s = String::from_utf8(raw).ok()?;
                    Some(s.splitn(2, ':').nth(1).unwrap_or("") == key)
                })
                .unwrap_or(false);
            if !ok {
                let chal = Header::from_bytes(
                    &b"WWW-Authenticate"[..],
                    &b"Basic realm=\"Thrushcombe\""[..],
                )
                .unwrap();
                let _ = req.respond(
                    Response::from_string("Thrushcombe — authentication required")
                        .with_status_code(401)
                        .with_header(chal),
                );
                continue;
            }
        }
        // pick up anything the hourly driver has written since (new decrees: feuds, courtships,
        // plans, conversation residue, and any calendar jump), so the dashboard never lags behind.
        let _ = sim.reload_inputs();
        DAY_OFFSET.store(sim.day_offset(), std::sync::atomic::Ordering::Relaxed);
        let is_post = matches!(req.method(), tiny_http::Method::Post);
        let json_hdr = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
        let html_hdr = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
        if is_post && matches!(url.as_str(), "/talk/say" | "/talk/end" | "/talk/between" | "/talk/between/end") {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let json = match url.as_str() {
                "/talk/say" => handle_say(&sim, &body),
                "/talk/end" => handle_end(&mut sim, &body),
                "/talk/between" => handle_between(&sim, &body),
                _ => handle_between_end(&mut sim, &body),
            };
            let _ = req.respond(Response::from_string(json).with_header(json_hdr));
        } else if url.split('?').next() == Some("/api/roster") {
            let _ = req.respond(Response::from_string(handle_roster(&sim)).with_header(json_hdr));
        } else {
            let html = route(&sim, &url);
            let _ = req.respond(Response::from_string(html).with_header(html_hdr));
        }
    }
}
