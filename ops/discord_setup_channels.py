#!/usr/bin/env python3
"""Idempotently build Thrushcombe's per-place text channels + one webhook each, under a
category, and write ops/discord_channels.json: a map the feed/bot read. Re-runnable."""
import json, time, urllib.request, urllib.error, pathlib, re

ROOT = pathlib.Path(__file__).resolve().parent.parent
SECRETS = ROOT / "ops" / "secrets.env"
OUT = ROOT / "ops" / "discord_channels.json"

env = {}
for l in SECRETS.read_text().splitlines():
    if "=" in l and not l.strip().startswith("#"):
        k, v = l.split("=", 1); env[k.strip()] = v.strip()
TOK = env["DISCORD_BOT_TOKEN"]; GID = env["DISCORD_GUILD_ID"]

# (channel title, seat-substring key the sim's agent.seat is matched against, lowercase).
# A town-wide / overflow feed and the inquiry get their own keyless channels.
PLACES = [
    ("The Pelican", "the pelican"), ("Five Elms", "five elms"), ("The Laurels", "the laurels"),
    ("Crale Court", "crale court"), ("The Crale Estate", "crale estate"), ("Home Farm", "home farm"),
    ("High Foldside", "high foldside"), ("Gunnerside", "gunnerside"), ("Beck House", "beck house"),
    ("Springs House", "springs house"), ("Ivy Cottage", "ivy cottage"), ("Widcombe Lane", "widcombe"),
    ("The Vicarage", "the vicarage"), ("The Church", "church"), ("The Churchyard", "churchyard"),
    ("The School", "the school"), ("The Post Office", "post office"), ("The Bank House", "bank house"),
    ("The Shop", "the shop"), ("The Draper's", "draper"), ("The Shambles", "shambles"),
    ("The Mill", "the mill"), ("The Forge", "forge"), ("The Bakehouse", "bakehouse"),
    ("The Constabulary", "constabulary"), ("The Station", "the station"),
    ("Carrier's Yard", "carrier"), ("Knacker's Yard", "knacker"),
]
SPECIAL = [("Town Chronicle", "__chronicle__"), ("The Inquiry", "__inquiry__")]
CATEGORY = "Thrushcombe St Mary"


def api(method, path, body=None):
    for attempt in range(6):
        req = urllib.request.Request(
            "https://discord.com/api/v10" + path,
            data=json.dumps(body).encode() if body is not None else None,
            headers={"Authorization": "Bot " + TOK, "Content-Type": "application/json",
                     "User-Agent": "Thrushcombe (local,0.1)"},
            method=method)
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.load(r) if r.read != b"" else {}
        except urllib.error.HTTPError as e:
            if e.code == 429:
                retry = json.load(e).get("retry_after", 1.5)
                time.sleep(float(retry) + 0.3); continue
            raise
    raise RuntimeError("rate-limited out: " + path)


def slug(title):
    return re.sub(r"-+", "-", re.sub(r"[^a-z0-9]+", "-", title.lower())).strip("-")


existing = api("GET", f"/guilds/{GID}/channels")
by_name = {c["name"]: c for c in existing}

# category
cat = next((c for c in existing if c["type"] == 4 and c["name"] == CATEGORY), None)
if not cat:
    cat = api("POST", f"/guilds/{GID}/channels", {"name": CATEGORY, "type": 4})
    print("created category", CATEGORY)
cat_id = cat["id"]

result = {"guild_id": GID, "category_id": cat_id, "channels": {}}


def ensure_channel(title, key):
    s = slug(title)
    ch = by_name.get(s)
    if not ch:
        ch = api("POST", f"/guilds/{GID}/channels",
                 {"name": s, "type": 0, "parent_id": cat_id})
        by_name[s] = ch
        print("  + channel", s)
        time.sleep(0.4)
    # webhook
    hooks = api("GET", f"/channels/{ch['id']}/webhooks")
    hook = next((h for h in hooks if h.get("name") == "Thrushcombe"), None)
    if not hook:
        hook = api("POST", f"/channels/{ch['id']}/webhooks", {"name": "Thrushcombe"})
        print("    webhook for", s)
        time.sleep(0.4)
    url = f"https://discord.com/api/webhooks/{hook['id']}/{hook['token']}"
    result["channels"][s] = {"title": title, "seat_key": key, "channel_id": ch["id"], "webhook": url}


for title, key in SPECIAL + PLACES:
    ensure_channel(title, key)

OUT.write_text(json.dumps(result, indent=2))
print(f"\nwrote {OUT} — {len(result['channels'])} channels with webhooks")
