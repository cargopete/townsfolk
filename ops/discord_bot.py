#!/usr/bin/env python3
"""Thrushcombe reply bot — two-way chat with the town.

You ARE Pete Peckers (cast #59): whatever you type in a place-channel is taken as Pete speaking.
The bot works out which townsperson you're addressing (a name in your message, else a resident of
that channel), asks the sim for that soul's in-character reply (POST /talk/say, source = Pete),
and posts it back through the channel webhook AS that townsperson — own name, own portrait.

One bot, sixty voices. Run as a persistent service (ops/thrushcombe-discord.service)."""
import json, os, pathlib, urllib.request, urllib.error, base64, asyncio
import discord

ROOT = pathlib.Path(__file__).resolve().parent.parent
PETE = 59                                  # you are Mr Pete Peckers
WEB = "http://127.0.0.1:8717"              # the sim's reply API (local)
PUBLIC_BASE = "https://pepe-thinkpad.tailb0627.ts.net:8443"
UA = "DiscordBot (https://thrushcombe.local, 0.1)"

env = {}
for l in (ROOT / "ops" / "secrets.env").read_text().splitlines():
    if "=" in l and not l.strip().startswith("#"):
        k, v = l.split("=", 1); env[k.strip()] = v.strip()
TOKEN = env["DISCORD_BOT_TOKEN"]
WEB_KEY = env.get("THRUSH_WEB_KEY", "")
AUTH = "Basic " + base64.b64encode(f"thrush:{WEB_KEY}".encode()).decode()

CH = json.loads((ROOT / "ops" / "discord_channels.json").read_text())["channels"]
# channel_id -> (slug, seat_key, webhook)
BY_ID = {int(c["channel_id"]): (slug, c["seat_key"], c["webhook"]) for slug, c in CH.items()}
PLACES_SORTED = sorted([(c["seat_key"], slug) for slug, c in CH.items() if not c["seat_key"].startswith("__")],
                       key=lambda kv: -len(kv[0]))
CHRONICLE = next((s for s, c in CH.items() if c["seat_key"] == "__chronicle__"), "town-chronicle")

# live townie-to-townie encounters, staged by the sim between the hourly beats
THRUSH = ROOT / "target" / "release" / "thrush"
DB = ROOT / "world.db"
ENCOUNTER_INTERVAL = 600                  # seconds between staged encounters (~6/hour)
SUBENV = dict(os.environ,
              PATH=os.path.expanduser("~/.local/bin") + ":" + os.environ.get("PATH", ""),
              CLAUDE_BIN=os.path.expanduser("~/.local/bin/claude"))


def place_channel(place):
    p = (place or "").lower()
    for key, slug in PLACES_SORTED:
        if key in p:
            return slug
    return None


def web_get(path):
    req = urllib.request.Request(WEB + path, headers={"Authorization": AUTH, "User-Agent": UA})
    return json.load(urllib.request.urlopen(req, timeout=20))


def web_post(path, body):
    req = urllib.request.Request(WEB + path, data=json.dumps(body).encode(),
        headers={"Authorization": AUTH, "Content-Type": "application/json", "User-Agent": UA}, method="POST")
    return json.load(urllib.request.urlopen(req, timeout=120))


ROSTER = web_get("/api/roster")["roster"]                       # [{idx,name,seat,standing,sex}]
BY_NAME = {r["name"].lower(): r for r in ROSTER}
# residents of a channel: souls whose seat contains the channel's seat-key, prominent first
def residents(seat_key):
    if not seat_key or seat_key.startswith("__"):
        return []
    rs = [r for r in ROSTER if seat_key in r["seat"].lower() and r["idx"] != PETE]
    return sorted(rs, key=lambda r: -r["standing"])


def resolve_target(text, seat_key):
    low = text.lower()
    # 1) a townsperson named in the message (longest name first, so 'Mrs Bunce' beats 'Bunce')
    for name in sorted(BY_NAME, key=len, reverse=True):
        if name in low and BY_NAME[name]["idx"] != PETE:
            return BY_NAME[name]
    # 2) else a resident of this place
    res = residents(seat_key)
    return res[0] if res else None


def post_as(webhook, name, idx, content):
    body = {"username": name, "content": content[:1950],
            "avatar_url": f"{PUBLIC_BASE}/portraits/{idx}.jpg"}
    req = urllib.request.Request(webhook + "?wait=true", data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "User-Agent": UA}, method="POST")
    urllib.request.urlopen(req, timeout=20)


# --- conversation memory: each channel holds at most one live 1:1, carried as history so the
# talk stays continuous, then flushed to the sim (POST /talk/end folds warmth + a memory into the
# world) when it goes quiet — so everything said in Discord feeds back into Thrushcombe. -----------
ACTIVE = {}          # channel_id -> {"target": idx, "name": str, "turns": [[role,text]], "last": t}
IDLE = 240           # seconds of quiet before a conversation is judged and folded


def history_for(cid, target):
    c = ACTIVE.get(cid)
    return c["turns"] if c and c["target"] == target["idx"] else []


def remember_turn(cid, target, said, reply):
    c = ACTIVE.get(cid)
    if not c or c["target"] != target["idx"]:
        c = ACTIVE[cid] = {"target": target["idx"], "name": target["name"], "turns": [], "last": 0}
    c["turns"] += [["user", said], ["assistant", reply]]
    c["turns"] = c["turns"][-16:]
    c["last"] = asyncio.get_running_loop().time()


async def flush(cid):
    c = ACTIVE.pop(cid, None)
    if not c or not c["turns"]:
        return
    try:
        r = web_post("/talk/end", {"source": PETE, "target": c["target"], "history": c["turns"]})
        print(f"folded talk with {c['name']}: warmth={r.get('warmth')} — {r.get('memory','')[:80]}")
    except Exception as e:
        print("flush failed:", e)


def refresh_roster():
    global ROSTER, BY_NAME
    try:
        ROSTER = web_get("/api/roster")["roster"]
        BY_NAME = {r["name"].lower(): r for r in ROSTER}
    except Exception as e:
        print("roster refresh failed:", e)


intents = discord.Intents.default()
intents.message_content = True
intents.members = True
client = discord.Client(intents=intents)


async def housekeeper():
    """Flush idle conversations into the world; keep the roster current as souls move/marry/leave."""
    while True:
        await asyncio.sleep(60)
        now = asyncio.get_running_loop().time()
        for cid in [c for c, v in ACTIVE.items() if now - v.get("last", 0) > IDLE]:
            await flush(cid)
        refresh_roster()


async def encounters():
    """Stage live townie-to-townie talk between the hourly beats: the sim picks two souls, the talk
    is generated AND recorded (its residue folds back into the world), and the bot posts it line by
    line into the room where it happened — each soul speaking as themselves."""
    while True:
        await asyncio.sleep(ENCOUNTER_INTERVAL)
        try:
            proc = await asyncio.create_subprocess_exec(
                str(THRUSH), "--db", str(DB), "encounter",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL, env=SUBENV)
            out, _ = await proc.communicate()
            d = json.loads((out.decode() or "{}").strip() or "{}")
        except Exception as e:
            print("encounter failed:", e); continue
        lines = d.get("lines") or []
        if not lines:
            continue
        # route to the room only when both souls belong there; a cross-household meeting happens
        # in the commons (the lane / about the parish), not in one person's house.
        seat_chans = set()
        for ix in {ln["idx"] for ln in lines}:
            r = next((x for x in ROSTER if x["idx"] == ix), None)
            if r:
                c = place_channel(r["seat"])
                if c:
                    seat_chans.add(c)
        slug = seat_chans.pop() if len(seat_chans) == 1 else CHRONICLE
        if not slug:
            continue
        webhook = CH[slug]["webhook"]
        ch = client.get_channel(int(CH[slug]["channel_id"]))
        for ln in lines:
            try:
                if ch:
                    async with ch.typing():
                        await asyncio.sleep(1.6)
                post_as(webhook, ln["name"], ln["idx"], ln["text"])
            except Exception as e:
                print("encounter post failed:", e)
            await asyncio.sleep(1.4)
        print(f"live encounter: {d.get('a_name')} & {d.get('b_name')} in #{slug} ({len(lines)} lines)")


@client.event
async def on_ready():
    print(f"Thrushcombe bot online as {client.user} — Pete Peckers at the keyboard, {len(BY_ID)} channels")
    client.loop.create_task(housekeeper())
    client.loop.create_task(encounters())


@client.event
async def on_message(msg):
    # ignore the bot's own posts, other bots, and webhook posts (the townsfolk themselves)
    if msg.author.bot or msg.webhook_id is not None:
        return
    entry = BY_ID.get(msg.channel.id)
    if entry is None:
        return
    slug, seat_key, webhook = entry

    async def speak(t, message, history):
        async with msg.channel.typing():
            try:
                reply = web_post("/talk/say", {"source": PETE, "target": t["idx"],
                                               "history": history, "message": message}).get("reply", "…")
            except Exception as e:
                print("say failed:", e); return None
        try:
            post_as(webhook, t["name"], t["idx"], reply)
        except Exception as e:
            print("post failed:", e)
        return reply

    # @everyone / @here in a room — every resident of that place answers in turn (a group is hailed)
    low = msg.content.lower()
    if msg.mention_everyone or "@everyone" in low or "@here" in low:
        said = msg.content.replace("@everyone", "").replace("@here", "").strip() or "Well — what say you all?"
        for t in residents(seat_key)[:6]:
            await speak(t, said, [])
            await asyncio.sleep(0.6)
        return

    # otherwise a 1:1 — address priority: (1) reply to a townie's message; (2) a name in the
    # message; (3) the most prominent resident of this place.
    target = None
    ref = msg.reference
    if ref and ref.message_id:
        try:
            refmsg = ref.resolved if isinstance(ref.resolved, discord.Message) \
                else await msg.channel.fetch_message(ref.message_id)
            nm = (refmsg.author.display_name or "").lower()
            if nm in BY_NAME and BY_NAME[nm]["idx"] != PETE:
                target = BY_NAME[nm]
        except Exception:
            pass
    if target is None:
        target = resolve_target(msg.content, seat_key)
    if target is None:
        return
    cid = msg.channel.id
    cur = ACTIVE.get(cid)
    if cur and cur["target"] != target["idx"]:   # you turned to someone else — fold the last talk
        await flush(cid)
    reply = await speak(target, msg.content, history_for(cid, target))
    if reply is not None:
        remember_turn(cid, target, msg.content, reply)


client.run(TOKEN)
