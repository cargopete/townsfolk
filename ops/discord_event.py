#!/usr/bin/env python3
"""Scheduled events: when the town reaches an event's day, fire it once — post a narrated account
(embed) to its channel, then stage its live character scenes there, each recorded into the world.
Driven by ops/events.json (a `fired` flag makes it idempotent). Called hourly from ops/daily.sh;
can also be run by hand after a jump."""
import json, os, re, subprocess, time, urllib.request, pathlib, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
EVENTS = ROOT / "ops" / "events.json"
CH = json.loads((ROOT / "ops" / "discord_channels.json").read_text())["channels"]
THRUSH = ROOT / "target" / "release" / "thrush"
DB = ROOT / "world.db"
CLAUDE = os.environ.get("CLAUDE_BIN") or os.path.expanduser("~/.local/bin/claude")
PUBLIC_BASE = "https://pepe-thinkpad.tailb0627.ts.net:8443"
UA = "DiscordBot (https://thrushcombe.local, 0.1)"
CHRON_SYS = ("You are the chronicler of Thrushcombe St Mary, a West-Country market town in 1934. "
             "Write in the dry, observed, lightly wry register of interwar English provincial prose. "
             "Be strictly faithful to who and what you are given; never invent a villager or a surname.")


def sim_day():
    out = subprocess.run([str(THRUSH), "--db", str(DB), "status"], capture_output=True, text=True, timeout=60)
    m = re.search(r"day (\d+)", out.stdout)
    return int(m.group(1)) if m else 0


def send(hook, body):
    for _ in range(5):
        try:
            urllib.request.urlopen(urllib.request.Request(hook + "?wait=true", data=json.dumps(body).encode(),
                headers={"Content-Type": "application/json", "User-Agent": UA}, method="POST"), timeout=25)
            return
        except urllib.error.HTTPError as e:
            if e.code == 429:
                time.sleep(float(json.load(e).get("retry_after", 1.0)) + 0.3); continue
            print("send failed", e.code, e.read()[:160].decode(errors="ignore"), file=sys.stderr); return


def account(prompt):
    out = subprocess.run([CLAUDE, "-p", "--model", "sonnet", "--append-system-prompt", CHRON_SYS, prompt],
                         capture_output=True, text=True, timeout=180, env=dict(os.environ))
    return out.stdout.strip()


def scene(between, setting):
    out = subprocess.run([str(THRUSH), "--db", str(DB), "encounter", "--between", between, "--setting", setting],
                         capture_output=True, text=True, timeout=240, env=dict(os.environ))
    return json.loads((out.stdout or "{}").strip() or "{}")


def fire(ev):
    slug = ev["channel"]
    hook = CH[slug]["webhook"]
    print(f"firing '{ev['id']}' -> #{slug}")
    if ev.get("account_prompt"):
        text = account(ev["account_prompt"])
        if text:
            send(hook, {"username": "Thrushcombe", "embeds": [{"title": ev.get("title", "An event"),
                        "description": text[:4000], "color": 0x8a7654}]})
            time.sleep(1.0)
    for sc in ev.get("scenes", []):
        d = scene(sc["between"], sc["setting"])
        for ln in d.get("lines", []):
            send(hook, {"username": ln["name"], "content": ln["text"][:1900],
                        "avatar_url": f"{PUBLIC_BASE}/portraits/{ln['idx']}.jpg"})
            time.sleep(1.2)
        print(f"  scene {sc['between']} ({len(d.get('lines', []))} lines)")


def main():
    data = json.loads(EVENTS.read_text())
    today = sim_day()
    fired_any = False
    for ev in data.get("events", []):
        if not ev.get("fired") and ev.get("day", 1 << 30) <= today:
            fire(ev)
            ev["fired"] = True
            fired_any = True
    if fired_any:
        EVENTS.write_text(json.dumps(data, indent=2))
        print(f"day {today}: events fired")
    else:
        print(f"day {today}: no events due")
    return 0


if __name__ == "__main__":
    sys.exit(main())
