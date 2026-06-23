#!/usr/bin/env python3
"""Thrushcombe reply bot — two-way chat with the town.

You ARE Pete Peckers (cast #59): whatever you type in a place-channel is taken as Pete speaking.
The bot works out which townsperson you're addressing (a name in your message, else a resident of
that channel), asks the sim for that soul's in-character reply (POST /talk/say, source = Pete),
and posts it back through the channel webhook AS that townsperson — own name, own portrait.

One bot, sixty voices. Run as a persistent service (ops/thrushcombe-discord.service)."""
import json, os, pathlib, urllib.request, urllib.error, base64, asyncio, random, re, sqlite3
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
MURMUR = next((s for s, c in CH.items() if c["seat_key"] == "__murmur__"), None)

# live townie-to-townie encounters, staged by the sim between the hourly beats
THRUSH = ROOT / "target" / "release" / "thrush"
DB = ROOT / "world.db"
ENCOUNTER_INTERVAL = 600                  # seconds between staged encounters (~6/hour)
MURMUR_INTERVAL = 540                     # seconds between the continuous-consciousness murmur (~6/hour)
MURMUR_BATCH = 6                          # souls who think aloud each murmur cycle (least-recent first)

# the overthrow plot is OVER (the petition collapsed, the cell broke) — the conspiracy loop is retired.
CONSPIRATORS = ["Mr Coad", "Tot Wragg", "Jeb Pascoe", "Mr Vye", "Mr Dunnage"]
OVERTHROW = "overthrow"
# souls held in the lock-up below the church: sidelined from the ambient sim (no wandering, no yard
# scenes), but reachable for a word in #the-crypt. Single source of truth in ops/prisoners.json
# (shared with discord_presence.py) — add a name there when the parish jails someone.
_PRISON_FILE = ROOT / "ops" / "prisoners.json"
PRISONERS = json.loads(_PRISON_FILE.read_text()) if _PRISON_FILE.exists() else []
JAIL = "__jail__"
CONSPIRACY_INTERVAL = 900                  # seconds between plotting scenes (~4/hour)
PLOT_SETTINGS = [
    "after dark in the carrier's yard, the lamp low and the gate barred — the talk turns again to Major Pringle, who will hang no name while the parish bears the fear, and what the working men might do to have him off the bench",
    "in the back corner of the Pelican, voices kept down, reckoning who in the parish would set a hand to a petition against the magistrate, and who would lose their nerve",
    "by the knacker's wall where no respectable soul walks, weighing whether paper to the county bench will serve or whether it must come to a harder thing",
    "a snatched word at the edge of the market — counting heads, naming the doubtful, asking how far each man is willing to go to be rid of Pringle",
    "the long room above the carrier's stable, the talk grown bolder — no longer whether the Major should go, but how it is to be done, and who will carry the word to the other farms",
]
_last_consp = [None]
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

# distinctive name-tokens (surname / given name) -> full name, but ONLY where a token is unique, so a
# rumour naming "Lydgate" or "Clewes" latches on, while ambiguous "Pringle" (several) needs the full name.
HONORIFICS = {"mr", "mrs", "miss", "dr", "major", "lady", "revd", "rev", "sir", "master", "the", "old"}
NAME_TOKENS = {}


def build_name_tokens():
    from collections import defaultdict
    tok = defaultdict(set)
    for r in ROSTER:
        nm = r["name"].lower()
        tok[nm].add(nm)                                          # the full name always resolves
        for w in nm.split():
            if w not in HONORIFICS and len(w) > 2:
                tok[w].add(nm)
    NAME_TOKENS.clear()
    NAME_TOKENS.update({t: next(iter(s)) for t, s in tok.items() if len(s) == 1})


build_name_tokens()


def detect_subject(text):
    """The one soul a rumour is plainly about — a full name or an unambiguous surname/given name
    matched as a whole word (longest key first, so 'Major Pringle' beats a bare token). None if unclear."""
    low = text.lower()
    for key in sorted(NAME_TOKENS, key=len, reverse=True):
        if re.search(r"(?<![a-z])" + re.escape(key) + r"(?![a-z])", low):
            r = BY_NAME.get(NAME_TOKENS[key])
            if r and r["idx"] != PETE:
                return r
    return None
# residents of a channel: souls whose seat contains the channel's seat-key, prominent first
def residents(seat_key):
    # #overthrow has no geography — its "people" are the conspirators (ringleader Coad first)
    if seat_key == "__overthrow__":
        return [BY_NAME[n.lower()] for n in CONSPIRATORS if n.lower() in BY_NAME]
    # the lock-up below the church — its "residents" are whoever the parish has jailed
    if seat_key == "__jail__":
        return [BY_NAME[n.lower()] for n in PRISONERS if n.lower() in BY_NAME]
    if not seat_key or seat_key.startswith("__"):
        return []
    rs = [r for r in ROSTER if seat_key in r["seat"].lower() and r["idx"] != PETE]
    return sorted(rs, key=lambda r: -r["standing"])


# the commons channels (#town-chronicle / #the-inquiry) have no geography of their own; when you
# hail @everyone there, the people who answer are whoever is out about the parish this phase.
ROVING = {"out paying calls", "on the rounds", "about the parish", "the market", "dealing at the mart", "paying calls"}
async def abroad_souls():
    try:
        proc = await asyncio.create_subprocess_exec(
            str(THRUSH), "--db", str(DB), "discord-presence",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL, env=SUBENV)
        out, _ = await proc.communicate()
        rows = json.loads((out.decode() or "[]").strip() or "[]")
    except Exception as e:
        print("abroad_souls failed:", e); return []
    res = [BY_NAME[r["name"].lower()] for r in rows
           if r.get("location", "").lower() in ROVING
           and r["name"].lower() in BY_NAME and BY_NAME[r["name"].lower()]["idx"] != PETE]
    return sorted(res, key=lambda r: -r["standing"])


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
        build_name_tokens()
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


async def murmur():
    """The town's continuous consciousness, streamed in real time: every cycle a rotating handful of
    souls (least-recently thought, so the whole town comes round) pulse one present thought, and the
    bot streams them to #the-town-thinking — so the place is visibly alive whether anyone is here or
    not. Cheap: one batched oracle call per cycle. This is the heartbeat of the always-on town."""
    if not MURMUR:
        return
    while True:
        await asyncio.sleep(MURMUR_INTERVAL)
        try:
            proc = await asyncio.create_subprocess_exec(
                str(THRUSH), "--db", str(DB), "pulse", "--count", str(MURMUR_BATCH),
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL, env=SUBENV)
            await proc.communicate()
        except Exception as e:
            print("murmur pulse failed:", e); continue
        try:
            con = sqlite3.connect(str(DB))
            rows = con.execute("SELECT subject, thought FROM pulses ORDER BY id DESC LIMIT ?", (MURMUR_BATCH,)).fetchall()
            con.close()
        except Exception as e:
            print("murmur read failed:", e); continue
        webhook = CH[MURMUR]["webhook"]
        ch = client.get_channel(int(CH[MURMUR]["channel_id"]))
        for name, thought in reversed(rows):
            r = BY_NAME.get(name.lower())
            try:
                if ch:
                    async with ch.typing():
                        await asyncio.sleep(0.8)
                post_as(webhook, name, r["idx"] if r else 0, f"\U0001f4ad _{thought}_")
            except Exception as e:
                print("murmur post failed:", e)
            await asyncio.sleep(0.6)
        print(f"murmur: {len(rows)} thoughts streamed")


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
        # the jailed do not wander the parish — drop any ambient scene that would feature a prisoner
        jailed = {BY_NAME[n.lower()]["idx"] for n in PRISONERS if n.lower() in BY_NAME}
        if any(ln["idx"] in jailed for ln in lines):
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


async def conspiracy():
    """The overthrow plot, playing out over the days: pick two of the discontented (biased to the
    ringleader Coad), stage their plotting with a conspiratorial setting, and post it to #overthrow.
    Recorded like any encounter, so the cell's bond hardens in the world as they scheme."""
    while True:
        await asyncio.sleep(CONSPIRACY_INTERVAL)
        present = [n for n in CONSPIRATORS if n.lower() in BY_NAME]
        if len(present) < 2:
            continue
        pair = random.sample(present, 2)
        for _ in range(6):                               # bias toward Coad; avoid the exact last pair
            pair = (["Mr Coad", random.choice([n for n in present if n != "Mr Coad"])]
                    if "Mr Coad" in present and random.random() < 0.6 else random.sample(present, 2))
            if frozenset(pair) != _last_consp[0]:
                break
        _last_consp[0] = frozenset(pair)
        try:
            proc = await asyncio.create_subprocess_exec(
                str(THRUSH), "--db", str(DB), "encounter",
                "--between", f"{pair[0]}|{pair[1]}", "--setting", random.choice(PLOT_SETTINGS),
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL, env=SUBENV)
            out, _ = await proc.communicate()
            d = json.loads((out.decode() or "{}").strip() or "{}")
        except Exception as e:
            print("conspiracy failed:", e); continue
        lines = d.get("lines") or []
        if not lines:
            continue
        webhook = CH[OVERTHROW]["webhook"]
        ch = client.get_channel(int(CH[OVERTHROW]["channel_id"]))
        for ln in lines:
            try:
                if ch:
                    async with ch.typing():
                        await asyncio.sleep(1.5)
                post_as(webhook, ln["name"], ln["idx"], ln["text"])
            except Exception as e:
                print("conspiracy post failed:", e)
            await asyncio.sleep(1.3)
        print(f"conspiracy: {d.get('a_name')} & {d.get('b_name')} ({len(lines)} lines)")


@client.event
async def on_ready():
    print(f"Thrushcombe bot online as {client.user} — Pete Peckers at the keyboard, {len(BY_ID)} channels")
    client.loop.create_task(housekeeper())
    client.loop.create_task(encounters())
    client.loop.create_task(murmur())          # the always-on heartbeat: the town thinks in real time
    # conspiracy() retired — the overthrow plot has run its course


@client.event
async def on_message(msg):
    # ignore the bot's own posts, other bots, and webhook posts (the townsfolk themselves)
    if msg.author.bot or msg.webhook_id is not None:
        return
    entry = BY_ID.get(msg.channel.id)
    if entry is None:
        return
    slug, seat_key, webhook = entry

    # --- command channels ---------------------------------------------------------------------
    # #skip-days: type a number → the town advances that many days and the day's beats are run.
    if seat_key == "__skip__":
        m = re.search(r"\d+", msg.content)
        if not m:
            await msg.channel.send("Give me a number of days — e.g. `skip 3`.")
            return
        days = max(1, min(14, int(m.group())))
        await msg.channel.send(f"⏭ Skipping **{days} day(s)** — the town moves on. The beats take a few minutes…")

        async def run_skip():
            try:
                proc = await asyncio.create_subprocess_exec(
                    "bash", str(ROOT / "ops" / "ripple.sh"), str(days), "6",
                    stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL, env=SUBENV)
                await proc.communicate()
                refresh_roster()
                head = "The days have passed."
                try:
                    p2 = await asyncio.create_subprocess_exec(
                        str(THRUSH), "--db", str(DB), "status",
                        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL, env=SUBENV)
                    so, _ = await p2.communicate()
                    for ln in so.decode().splitlines():
                        if "THRUSHCOMBE ST MARY" in ln:
                            head = ln.strip(); break
                except Exception:
                    pass
                await msg.channel.send(f"✅ {head}  — the feed and presence are updated.")
            except Exception as e:
                await msg.channel.send(f"⚠ skip failed: {e}")

        client.loop.create_task(run_skip())
        return

    # #the-rumour-mill: whatever you type enters the lanes as anonymous gossip. Name a soul to bend
    # the parish's opinion of them; prefix with + for a flattering rumour (default is damaging).
    if seat_key == "__rumours__":
        text = msg.content.strip()
        if not text:
            return
        amount = "-1"
        if text.startswith("+"):
            amount = "1"; text = text[1:].strip()
        subj = detect_subject(text)
        target = subj["name"] if subj else ""

        async def run_rumour():
            try:
                proc = await asyncio.create_subprocess_exec(
                    str(THRUSH), "--db", str(DB), "providence", "rumor",
                    "--target", target, "--note", text, f"--amount={amount}",
                    stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL, env=SUBENV)
                await proc.communicate()
                who = f" about **{target}**" if target else ""
                kind = "kindly" if amount == "1" else "dark"
                await msg.channel.send(
                    f"🗣 The {kind} whisper{who} is loose in the lanes — no one will know it began with you. "
                    f"It will spread, warp in the telling, and colour what the parish thinks over the days to come.")
            except Exception as e:
                await msg.channel.send(f"⚠ rumour failed: {e}")

        client.loop.create_task(run_rumour())
        return

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

    # @everyone / @here in a room — every resident of that place answers in turn (a group is hailed),
    # and each soul's reply is folded back into the world so they REMEMBER that Pete spoke up.
    low = msg.content.lower()
    if msg.mention_everyone or "@everyone" in low or "@here" in low:
        said = msg.content.replace("@everyone", "").replace("@here", "").strip() or "Well — what say you all?"
        loop = asyncio.get_running_loop()
        folds = []
        hail = residents(seat_key)
        if not hail and seat_key in ("__chronicle__", "__inquiry__"):
            hail = await abroad_souls()          # the commons: hail whoever is out about the parish
        for t in hail[:6]:
            reply = await speak(t, said, [])
            if reply is not None:                        # record this exchange (warmth + a memory)
                folds.append(loop.run_in_executor(None, lambda tt=t, rr=reply: web_post(
                    "/talk/end", {"source": PETE, "target": tt["idx"], "history": [["user", said], ["assistant", rr]]})))
            await asyncio.sleep(0.6)
        if folds:
            await asyncio.gather(*folds, return_exceptions=True)
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
