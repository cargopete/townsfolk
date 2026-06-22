#!/usr/bin/env python3
"""Keep each place-channel's topic showing who is there this phase. A soul is 'here' if their
current location is their workplace/home in this channel; souls out roving (paying calls, the
rounds, the market) are abroad and listed in #town-chronicle. Only edits a topic when it changed
(Discord rate-limits topic edits hard). Run hourly from daily.sh."""
import json, subprocess, urllib.request, urllib.error, pathlib, time, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CH = json.loads((ROOT / "ops" / "discord_channels.json").read_text())
CHANNELS = CH["channels"]; GID = CH["guild_id"]
THRUSH = ROOT / "target" / "release" / "thrush"
DB = ROOT / "world.db"
UA = "DiscordBot (https://thrushcombe.local, 0.1)"
env = {}
for l in (ROOT / "ops" / "secrets.env").read_text().splitlines():
    if "=" in l and not l.strip().startswith("#"):
        k, v = l.split("=", 1); env[k.strip()] = v.strip()
TOK = env["DISCORD_BOT_TOKEN"]

ROVING = {"out paying calls", "on the rounds", "about the parish", "the market", "dealing at the mart", "paying calls"}
PLACES = sorted([(c["seat_key"], slug) for slug, c in CHANNELS.items() if not c["seat_key"].startswith("__")],
                key=lambda kv: -len(kv[0]))
CHRONICLE = next((s for s, c in CHANNELS.items() if c["seat_key"] == "__chronicle__"), None)
# souls held in the lock-up: shown in #the-crypt, and kept OUT of their old home channel so a jailed
# soul no longer appears at large. Single source of truth in ops/prisoners.json (shared with the bot).
_PRISON_FILE = ROOT / "ops" / "prisoners.json"
PRISONERS = set(json.loads(_PRISON_FILE.read_text())) if _PRISON_FILE.exists() else set()
JAIL_SLUG = next((s for s, c in CHANNELS.items() if c["seat_key"] == "__jail__"), None)


def api(method, path, body=None):
    req = urllib.request.Request("https://discord.com/api/v10" + path,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Authorization": "Bot " + TOK, "Content-Type": "application/json", "User-Agent": UA},
        method=method)
    for _ in range(5):
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code == 429:
                time.sleep(float(json.load(e).get("retry_after", 1.5)) + 0.3); continue
            raise


def channel_for(loc, seat):
    loc = loc.lower()
    for key, slug in PLACES:               # at a recognisable place this phase
        if key in loc:
            return slug
    if loc in ROVING:                      # out and about — not in any one room
        return None
    for key, slug in PLACES:               # else at home/work: their seat's channel
        if key in seat.lower():
            return slug
    return None


def main():
    out = subprocess.run([str(THRUSH), "--db", str(DB), "discord-presence"],
                         capture_output=True, text=True, timeout=120)
    if out.returncode != 0:
        print("discord-presence failed:", out.stderr[:200], file=sys.stderr); return 1
    rows = json.loads(out.stdout or "[]")

    def seat_channel(seat):                 # which channel a soul LIVES in, by their seat
        s = (seat or "").lower()
        for key, slug in PLACES:
            if key in s:
                return slug
        return None

    here = {slug: [] for slug in CHANNELS}   # present this phase
    home = {slug: [] for slug in CHANNELS}   # live here, present or not
    abroad = []
    for r in rows:
        if r["name"] in PRISONERS:           # the jailed are shown in the crypt, not at large
            continue
        slug = channel_for(r["location"], r["seat"])
        if slug:
            here[slug].append(r["name"])
        elif r["location"].lower() in ROVING:
            abroad.append(r["name"])
        hc = seat_channel(r["seat"])
        if hc:
            home[hc].append(r["name"])

    topics = {}
    for slug, c in CHANNELS.items():
        if c["seat_key"].startswith("__"):
            continue
        present = here.get(slug, [])
        residents = home.get(slug, [])
        out = [n for n in residents if n not in present]
        if present:
            t = "Here now: " + ", ".join(present)
            if out:
                t += " · away: " + ", ".join(out)
        elif residents:                      # no one in just now, but it's someone's home
            t = "Quiet — " + ", ".join(residents) + " out about the parish"
        else:
            t = "Quiet just now."
        topics[slug] = t[:1024]
    if JAIL_SLUG:
        held = sorted(PRISONERS)
        topics[JAIL_SLUG] = ("Held below the church: " + ", ".join(held)) if held else "Empty just now."
    if CHRONICLE and abroad:
        topics[CHRONICLE] = "Out about the parish: " + ", ".join(abroad)

    # only edit a topic that actually changed (Discord throttles topic edits to ~2/10min/channel)
    current = {c["id"]: (c.get("topic") or "") for c in api("GET", f"/guilds/{GID}/channels")}
    changed = 0
    for slug, topic in topics.items():
        cid = CHANNELS[slug]["channel_id"]
        if current.get(cid, "") != topic[:1024]:
            api("PATCH", f"/channels/{cid}", {"topic": topic[:1024]})
            changed += 1; time.sleep(0.4)
    print(f"presence: {sum(len(v) for v in here.values())} placed, {len(abroad)} abroad; {changed} topics updated")
    return 0


if __name__ == "__main__":
    sys.exit(main())
