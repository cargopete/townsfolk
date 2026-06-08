//! Thrushcombe — the reader & dashboard. A read-only web view over the chronicle and
//! the folded world: a detailed live board of where everyone is and what they're about,
//! the cast and their lineage, and each soul's whole record.
//!
//!   thrush-web [world.db]        # serves http://127.0.0.1:8717

use thrush_core::{Agent, Phase, Sim};
use tiny_http::{Header, Response, Server};
use time::{Date, OffsetDateTime};

fn now() -> OffsetDateTime {
    OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc())
}
fn today() -> Date {
    now().date()
}
fn phase_now() -> Phase {
    Phase::from_hour(now().hour())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

const CSS: &str = "
:root{--ink:#2b2620;--faint:#8a7f70;--rule:#e2d9c8;--bg:#f5f0e6;--link:#7a4a2b;--card:#fbf8f1}
*{box-sizing:border-box}
body{background:var(--bg);color:var(--ink);font:16px/1.55 Georgia,'Iowan Old Style',serif;margin:0}
.wrap{max-width:980px;margin:0 auto;padding:2.2rem 1.3rem 5rem}
a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
h1{font-size:1.95rem;margin:0;letter-spacing:.02em}
h2{font-size:1.05rem;margin:2rem 0 .5rem;border-bottom:1px solid var(--rule);padding-bottom:.3rem;font-variant:small-caps;letter-spacing:.05em;color:#5a5046}
.sub{color:var(--faint);font-style:italic;margin:.2rem 0 1.3rem}
.nav{margin:.4rem 0 1.6rem;font-variant:small-caps;letter-spacing:.05em}
.nav a{margin-right:1.1rem}
.entry{margin:.45rem 0}
.date{color:var(--faint);font-size:.8rem;font-variant:small-caps;letter-spacing:.03em}
.bars{font-family:ui-monospace,monospace;color:var(--faint);font-size:.78rem}
.tag{display:inline-block;font-size:.68rem;color:var(--faint);font-variant:small-caps;border:1px solid var(--rule);border-radius:3px;padding:0 .35rem;margin-left:.4rem}
.gone td{opacity:.5}
.card{border:1px solid var(--rule);border-radius:6px;padding:.7rem 1rem;margin:.8rem 0;background:var(--card)}
.cols{display:grid;grid-template-columns:1fr 1fr;gap:1rem}
.cols ul{margin:.2rem 0;padding-left:1.1rem}.cols li{margin:.15rem 0}
table{width:100%;border-collapse:collapse;font-size:.92rem}
th{text-align:left;color:var(--faint);font-weight:normal;font-variant:small-caps;font-size:.78rem;border-bottom:1px solid var(--rule);padding:.3rem .4rem}
td{padding:.32rem .4rem;border-bottom:1px solid #efe7d8;vertical-align:top}
.doing{color:#3f6b52}.next{color:var(--faint);font-style:italic}
.where{font-variant:small-caps;letter-spacing:.02em}
@media(max-width:640px){.cols{grid-template-columns:1fr}.hidesm{display:none}}
";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>{}</title><style>{}</style></head><body><div class=wrap>\
         <div class=nav><a href=/>Dashboard</a><a href=/folk>The Town</a><a href=/graph>Kinship</a></div>{}</div></body></html>",
        esc(title), CSS, body
    )
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
    let mut body = format!(
        "<h1>Thrushcombe St Mary</h1><div class=sub>{}, {} &middot; {} ({}) &middot; {} souls<br><span class=date>armed this season: {}</span></div>",
        esc(&d.weekday), esc(&d.date), esc(&d.season), esc(&d.phase), d.population, esc(&d.armed)
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
    body.push_str("</h2><table><tr><th>Soul</th><th class=hidesm>Where</th><th>Doing now</th><th class=hidesm>Next</th><th>Standing</th><th>£</th></tr>");
    for p in &d.people {
        body.push_str(&format!(
            "<tr><td><a href=/folk/{}>{}</a> <span class=date>{}y</span></td>\
             <td class='where hidesm'>{}</td><td class=doing>{}</td><td class='next hidesm'>{}</td>\
             <td><span class=bars>{}</span></td><td>{}</td></tr>",
            p.idx, esc(&p.name), p.age, esc(&p.location), esc(&p.doing), esc(&p.next), bar(p.standing), p.purse
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
    page("Thrushcombe — Dashboard", &body)
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
            format!(
                "<tr class='{}'><td><a href=/folk/{}>{}</a>{}</td><td class=hidesm>{}</td><td>{}y</td><td><span class=bars>{}</span></td></tr>",
                if dim { "gone" } else { "" }, i, esc(&a.name), label, esc(pretty_arch(&a.archetype)), a.age(day), bar(a.standing)
            )
        }).collect();
        format!("<h2>{}</h2><table><tr><th>Name</th><th class=hidesm>Station</th><th>Age</th><th>Standing</th></tr>{}</table>", esc(title), rows)
    };
    body.push_str(&section("The grown folk", &grown, false));
    body.push_str(&section("The children", &children, false));
    body.push_str(&section("Gone before & gone away", &gone, true));
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
        esc(&a.name), status, esc(pretty_arch(&a.archetype)), esc(&a.seat), a.age(day)
    );

    body.push_str(&format!(
        "<div class=card>standing <span class=bars>{}</span> {} &middot; purse £{}",
        bar(a.standing), a.standing, a.purse
    ));
    // live placement/doings for the present cast
    if a.active() {
        if let Ok(d) = sim.detail(t, phase_now()) {
            if let Some(p) = d.people.iter().find(|p| p.idx == idx) {
                body.push_str(&format!(
                    "<br><span class=where>{}</span> &middot; <span class=doing>{}</span> &middot; next: <span class=next>{}</span>",
                    esc(&p.location), esc(&p.doing), esc(&p.next)
                ));
            }
        }
    }
    body.push_str("</div>");

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
    page(&format!("Thrushcombe — {}", a.name), &body)
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
         <div class=nav><a href=/>Dashboard</a><a href=/folk>The Town</a><a href=/graph>Kinship</a></div>\
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

fn route(sim: &Sim, url: &str) -> String {
    let path = url.split('?').next().unwrap_or("/");
    if path == "/" {
        dashboard(sim)
    } else if path == "/folk" {
        folk(sim)
    } else if path == "/graph" {
        graph(sim)
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
    let _ = sim.catch_up(today());

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
