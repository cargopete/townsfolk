#!/usr/bin/env python3
"""Thrushcombe reply bot — two-way chat with the town.

You ARE Pete Peckers (cast #59): whatever you type in a place-channel is taken as Pete speaking.
The bot works out which townsperson you're addressing (a name in your message, else a resident of
that channel), asks the sim for that soul's in-character reply (POST /talk/say, source = Pete),
and posts it back through the channel webhook AS that townsperson — own name, own portrait.

One bot, sixty voices. Run as a persistent service (ops/thrushcombe-discord.service)."""
import json, os, pathlib, urllib.request, urllib.error, base64
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


intents = discord.Intents.default()
intents.message_content = True
intents.members = True
client = discord.Client(intents=intents)


@client.event
async def on_ready():
    print(f"Thrushcombe bot online as {client.user} — Pete Peckers at the keyboard, {len(BY_ID)} channels")


@client.event
async def on_message(msg):
    # ignore the bot's own posts, other bots, and webhook posts (the townsfolk themselves)
    if msg.author.bot or msg.webhook_id is not None:
        return
    entry = BY_ID.get(msg.channel.id)
    if entry is None:
        return
    slug, seat_key, webhook = entry
    # address priority: (1) reply to a townie's message → that soul; (2) a name in the message;
    # (3) the most prominent resident of this place.
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
    async with msg.channel.typing():
        try:
            reply = web_post("/talk/say", {
                "source": PETE, "target": target["idx"],
                "history": [], "message": msg.content,
            }).get("reply", "…")
        except Exception as e:
            print("say failed:", e); return
    try:
        post_as(webhook, target["name"], target["idx"], reply)
    except Exception as e:
        print("post failed:", e)


client.run(TOKEN)
