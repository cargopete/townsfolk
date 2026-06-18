#!/usr/bin/env python3
"""The live feed: post new beats to their place-channel as the townsperson (own name + portrait),
each voice rendered distinctly so narration, thought, and speech are easy to tell apart:

    narration  ->  *italic* (a happening, observed by the parish)
    thought    ->  💭 _italic_ (a private reflection)
    speech     ->  plain text in quotes (words said aloud)

Three per-table cursors tracked in ops/discord_state.json. Called by ops/daily.sh after narrate."""
import json, subprocess, time, urllib.request, urllib.error, pathlib, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CHANNELS = json.loads((ROOT / "ops" / "discord_channels.json").read_text())["channels"]
STATE = ROOT / "ops" / "discord_state.json"
THRUSH = ROOT / "target" / "release" / "thrush"
DB = ROOT / "world.db"
PUBLIC_BASE = "https://pepe-thinkpad.tailb0627.ts.net:8443"   # Funnel — portraits public here
UA = "DiscordBot (https://thrushcombe.local, 0.1)"
BACKFILL = 6             # on a fresh feed, seed each voice with its last N beats
PETE = 59                # the player — their own conversations aren't echoed back by the feed
SRC_KEY = {"e": "events", "t": "thoughts", "d": "speech"}

PLACES = sorted(
    [(c["seat_key"], slug) for slug, c in CHANNELS.items() if not c["seat_key"].startswith("__")],
    key=lambda kv: -len(kv[0]))
CHRONICLE = next((s for s, c in CHANNELS.items() if c["seat_key"] == "__chronicle__"), "town-chronicle")
INQUIRY = next((s for s, c in CHANNELS.items() if c["seat_key"] == "__inquiry__"), "the-inquiry")
INQUIRY_HINTS = ("murder", "inquest", "judg", "testimon", "townhall", "town-hall", "accus", "suspic", "interrog", "verdict")


def channel_for(beat):
    if any(h in beat["kind"].lower() for h in INQUIRY_HINTS):
        return INQUIRY
    seat = beat.get("seat", "").lower()
    if seat:
        for key, slug in PLACES:
            if key in seat:
                return slug
    return CHRONICLE


def render(beat):
    t = beat["text"].replace("\n", " ").strip()
    v = beat["voice"]
    if v == "narration":
        return f"*{t}*"
    if v == "thought":
        return f"💭 _{t}_"
    return f"“{t}”"                       # speech


def post(slug, beat):
    url = CHANNELS[slug]["webhook"] + "?wait=true"
    body = {"username": beat["actor"] or "Thrushcombe", "content": render(beat)[:1950]}
    if beat.get("idx", -1) >= 0:
        body["avatar_url"] = f"{PUBLIC_BASE}/portraits/{beat['idx']}.jpg"
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "User-Agent": UA}, method="POST")
    for _ in range(5):
        try:
            urllib.request.urlopen(req, timeout=20); return True
        except urllib.error.HTTPError as e:
            if e.code == 429:
                time.sleep(float(json.load(e).get("retry_after", 1.0)) + 0.3); continue
            print(f"  post failed {e.code} in #{slug}: {e.read().decode()[:160]}", file=sys.stderr); return False
    return False


def main():
    state = json.loads(STATE.read_text()) if STATE.exists() else {}
    fresh = not state
    args = [str(THRUSH), "--db", str(DB), "discord-feed",
            "--since-e", str(state.get("events", 0)),
            "--since-t", str(state.get("thoughts", 0)),
            "--since-d", str(state.get("speech", 0))]
    out = subprocess.run(args, capture_output=True, text=True, timeout=120)
    if out.returncode != 0:
        print("discord-feed failed:", out.stderr[:300], file=sys.stderr); return 1
    beats = json.loads(out.stdout or "[]")

    # advance each cursor to the high-water mark we saw, even for beats we skip on a fresh start
    maxid = dict(state)
    for b in beats:
        k = SRC_KEY[b["src"]]
        maxid[k] = max(maxid.get(k, 0), b["id"])

    # narration & thought post individually; speech is grouped by conversation so a whole exchange
    # lands together in one room (where it happened), not split across each speaker's channel.
    from collections import OrderedDict
    others = [b for b in beats if b["src"] != "d"]
    dialogs = OrderedDict()
    for b in (b for b in beats if b["src"] == "d"):
        dialogs.setdefault(b["id"], []).append(b)
    # the player's own conversations are lived in the channel already — never echo them back
    dialogs = OrderedDict((i, g) for i, g in dialogs.items() if not any(x["idx"] == PETE for x in g))

    if fresh:
        others = ([b for b in others if b["src"] == "e"][-BACKFILL:]
                  + [b for b in others if b["src"] == "t"][-BACKFILL:])
        keep = list(dialogs)[-BACKFILL:]
        dialogs = OrderedDict((i, dialogs[i]) for i in keep)

    posted = 0
    for b in sorted(others, key=lambda b: (b["src"], b["id"])):
        if post(channel_for(b), b):
            posted += 1; time.sleep(0.4)
    for _id, group in dialogs.items():
        slug = channel_for(group[0])       # the whole conversation to where it began
        for b in group:
            if post(slug, b):
                posted += 1; time.sleep(0.4)

    STATE.write_text(json.dumps({"events": maxid.get("events", 0),
                                 "thoughts": maxid.get("thoughts", 0),
                                 "speech": maxid.get("speech", 0)}))
    print(f"posted {posted} beats; cursors -> {json.loads(STATE.read_text())}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
