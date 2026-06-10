# Townsfolk — Thrushcombe St Mary

A small society for simulation. A West-Country market town, **bound to the real
calendar** — its season is your season, its day is your day. *Provincial Lady* status
comedy, *All Creatures* animal layer, *Clarkson's Farm* friction, run over one social
graph and aged across decades.

> Full design: [`docs/thrushcombe.md`](docs/thrushcombe.md)

## What it is

A deterministic, event-sourced town whose entire state is a pure function of
`(seed, epoch, today)` plus any interventions you make. The clock is **derived, not
incremented** — `t = today.julian − epoch.julian` — so the driver just "catches up to
today": exact phase-lock to the real calendar, and missed days self-heal. Same inputs
always reproduce the same town, bit-for-bit.

It runs end to end:

- **Calendar, seasons & phases** — the simulation steps by *phase* (`slot = day×5 +
  phase`: dawn, forenoon, afternoon, evening, night), so beats fall *through* the day —
  weather and stock at dawn, the market and the rounds in the forenoon, the gentry's
  calls in the afternoon, the Pelican in the evening, the life cycle at night. A season
  state machine (lambing → sowing → hay → harvest → mart → winter) arms the
  external-shock layer (the storm on the cut hay, the Board's price cut, the tithe).
- **Behaviour layer** — every present adult decides a day's intention from a flat
  `Observation` and returns an `Action` the host arbitrates, encoding the genre
  tensions (the genteel weigh solvency vs face; the improver schemes and comes to
  grief). It's generative for *any* holder of a role, so a great-grandchild who
  inherits a seat keeps the comedy going with no new code. Runs behind a
  `decide(observation) -> action` boundary with two interchangeable engines —
  **native** and **wasm** (see below).
- **Individuation — goals, mood, temperament** — each soul carries a personal
  **ambition** fitting their situation (clear the debt, rise in the world, marry a
  child off, get the better of a rival, make a fortune), which *shapes what they do*;
  fulfilling it is a **triumph**. A **mood** moves with what befalls them — not only the
  rare blows of the life cycle but the **day's own fortunes**: a scheme come to grief or a
  bad morning at the mart dispirits, a triumphant dinner or a hard bargain well struck
  buoys. And not uniformly: **temperament** is by type (the gentry touchy about face, the
  hill folk stoic and dour, the improver mercurial), so the same snub wounds one far more
  than another — so the town's spirits genuinely diverge, soul by soul, day by day.
- **Situation-aware** — `decide()` reacts to its own goal and mood *and to who's about
  it this phase*: the gentry are far busier at the calling hour when a friend or a
  rival is present (company to keep, or a standing to defend), the debtor economises,
  the riser spends on face. The flat scalar ABI just gains a few derived fields, so
  native/wasm parity still holds bit-for-bit.
- **Targeted society — calls, dinners, and the pointed snub** — a social act is aimed,
  not broadcast. A soul calls on a *named* person and asks *named* guests to dine: their
  warmest friends, the one they're courting, and — for a riser — a grand acquaintance
  worth cultivating, picked from the top few so the whole town isn't forever courting the
  one titled lady. And who is *left off* the list matters as much as who is asked: a riser
  quietly omits the rival just above them, which cools the two and plants a grudge — and
  because real grudges and true bonds now persist where faint feelings fade, that coolness
  compounds across the years into a settled rivalry, expressed in a standing pattern of
  pointed exclusions and the gossip they throw off ("who Lady Aldermaston left off the
  dinner list"). The ledger genuinely *develops*: 58 independent moods become 58 social
  strategies, and a bitter enough hatred of a superior becomes a soul's whole ambition.
- **Planning — courtship** — a soul forms a *multi-day intention* and pursues it over
  weeks: a courtship begun is walked out and called on, affection slowly warming (and
  warming the slower for a suit pressed above one's station), until it builds to a
  wedding after a proper courtship — or comes to nothing when another gets there first
  or the feeling is never returned. Marriages are no longer sudden; they have a throughline.
- **The LLM at the hinges** — at a soul's genuine turning point (a long feud that might
  be forgiven, ruin to be faced, a match across the class line) the choice is put to a
  local Qwen with that soul's whole **dossier** — who they are, their situation, their
  recent history. It chooses from a fixed vocabulary and writes the line; the verdict is
  **recorded and folded** with a bounded effect, so the decision is the model's but the
  world stays exact. The genuine intelligence, only where it matters.
- **Speak to a soul** — on the dashboard's *A word…* page you adopt one soul's voice and
  converse with another; Qwen answers in character, mindful of who's addressing them and
  what they remember. Each soul's page has a *speak to me directly* button. When you end
  the conversation the oracle judges its residue — a **warming or cooling, a memory kept,
  and sometimes a change of heart about what they want** — and only that recorded effect
  enters the fold, so souls genuinely learn from talking while the world stays exact.
- **Souls talk among themselves** — and not only to you: each day two souls fall into
  conversation of their own accord (a courting pair, friends, rivals, or two who simply
  met), Qwen plays both, and the same residue is recorded for each — a memory kept, a
  warming or cooling, now and then a resolve (to clear a debt, to rise, to mend a quarrel).
  The town's relationships and ambitions drift through talk, all of it folded deterministically.
- **Gossip diffusion** — salient events become news that spreads one hop a day across
  a channelled social graph (the vet fast across farms, the parson across homes, the
  servants' grapevine between drawing-rooms ×market-day, the Pelican among the men, the
  **post office** that hears everything, the **station & carrier** bringing word from
  away, church gathering everyone on Sunday), with delay and distortion.
- **Relationship ledger** — directed pairwise affinities that gossip moves *personally*
  (hearing ill of someone lowers *your* opinion of them, and it persists). Feuds flare
  and deepen at the market and church door, friendships show; grudges and warmth fade
  unless fed, while families stay warm. Reputation is no longer a single global score.
- **Life cycle & migration** — ageing, marriage, births, death and succession turn the
  cast over by seat; bloodlines inherit (and inherit the estate's capital — nobody
  starts on nothing), non-heir children leave town, adults emigrate for work elsewhere.
  Outsiders drift *in* too — a steady trickle of incomers with tracked origins — and the
  population settles ~50. A 50-year run is *history*, not a loop, and regenerates in ~0.5s.
- **Real weather** — `thrush weather` records Sofia's actual sky (open-meteo); a hard
  rain rots the hay, heat burns the grass, a frost takes lambs. Your wet week becomes
  the town's. Recorded, so replay stays deterministic.
- **Animals** — a herd across the farms (cows, sheep, horses, dogs) with health,
  gestation and value; calving and lambing in season, the vet's ailments, the knacker
  for fallen stock.
- **The Show** — the year's great set-piece, the Thrushcombe & District Show each 23rd
  of August: classes judged and rosettes awarded (best beast, best garden produce, best
  preserves) and the silver Champion's Cup for best in show. A win lifts a soul's standing
  and spirits; the losing of it — the improver beaten to the beast prize by a hill farmer,
  going home black as thunder — is its own small tragedy, and a disputed judging can end
  a friendship. Deterministic, in the fold.
- **Providence** — you play the novelist: inject circumstance (a letter, a called
  loan, a legacy, a scandal, a stranger at the cottage) and the autonomous agents
  react in character.
- **Gossip & the rumour mill** — beyond the news incidents throw off, scandal and
  romance are *made* at the market, after church, and over the Pelican's beer:
  courtships, affairs, drink, debt, airs. Each rumour spreads by diffusion and works
  on the relationship ledger.
- **Narration oracle** — a capped local Qwen renders the salient beats in voice,
  async and recorded, so replay stays deterministic.
- **Wildcard happenings** — now and then Qwen *invents* a one-off incident (a fire, a
  windfall, a travelling fair, a blight, a scandal, a stranger, a foundling, a wonder)
  and picks an effect-*kind* from a fixed vocabulary; the host applies a bounded,
  deterministic consequence (a fire costs someone, a fair lifts the town, a windfall
  enriches them) and gossip carries it onward. Recorded and folded like weather and
  providence, so the town surprises even you while staying reproducible to the byte.
- **Snapshots** — the folded world is checkpointed yearly, so reads load the nearest
  checkpoint and fold only the remainder (`status` on a 50-year world ≈ 2 ms).

## Use

```bash
cargo build --release

thrush init                              # found a town, epoch = today (companion mode)
thrush init --start 1976-06-08 --seed 7  # or backdate for instant decades of history
thrush --wasm init --start 1976-06-08    # run the behaviour layer inside the wasm sandbox
thrush weather                           # record Sofia's real sky for the days ahead
thrush tick                              # advance the chronicle to the current phase
thrush narrate                           # render new salient beats in voice (local Qwen)
thrush wildcard                          # now and then, let Qwen invent a happening
thrush status                            # the town at a glance
thrush watch                             # detailed live TUI — scroll the cast (↑/↓), q to quit

# play the novelist:
thrush providence loan    --target "Mr Rupert Crale" --amount 50
thrush providence legacy  --target "Mrs Cynthia Pelham" --amount 80
thrush providence scandal --target "Major Pringle" --note "a matter at the bank in town"
thrush providence stranger --note "Mr Silas Vane"

thrush-web world.db                      # dashboard, legends & kinship at http://127.0.0.1:8717
```

The town can run itself on an **hourly** systemd user timer — see [`ops/`](ops/); each
beat catches the town up to the current phase, fetches the weather, and narrates the
new events. Idempotent and self-healing across missed hours.

## The two engines (native ↔ wasm)

The behaviour layer is one `decide(observation) -> action` boundary with two
implementations: the **native** in-process engine, and a **wasm** engine that runs the
policies inside wasmtime. Both call the *same* shared `policies` crate (a `no_std`,
dependency-free crate compiled into the host and into a 625-byte wasm guest over a pure
scalar ABI), so `thrush --wasm` produces a **byte-identical event log** to native —
proof the substrate swap is transparent. Rebuild the guest with `ops/build-wasm.sh`
(needs `rustup target add wasm32-unknown-unknown`).

## The reader (`thrush-web`)

A read-only, period-styled web view over the SQLite log and the folded world:

- **`/`** — a detailed dashboard: where every soul is *this phase*, what they're doing
  now and next, standing/purse, plus the day's global events, the gossip in flight,
  the calendar, and the chronicle.
- **`/folk`** — every soul, living and gone (dead & departed).
- **`/folk/N`** — a person: standing/purse, live placement, family (linked), and their
  whole record.
- **`/graph`** — the kinship network (marriages dashed, descent arrowed; click through).

## Layout

```
core/           deterministic event-sourced kernel — calendar · seasons · behaviour ·
                gossip · life cycle · providence · snapshots · the PolicyEngine boundary
policies/       shared no_std crate: every archetype's policy, compiled native AND to wasm
wasm-policies/  the behaviour layer as one wasm guest (built by ops/build-wasm.sh)
thrush/         CLI + detailed ratatui monitor + the wasmtime-backed engine
web/            thrush-web — dashboard, legends & kinship browser
llm/            capped, sandboxed local Qwen for narration (the recorded oracle)
ops/            the daily timer and the wasm build script
docs/           the design
```

## Time model

**Companion mode, 1 sim-day : 1 real-day, full phase-lock** — and the day itself is
phase-locked too, so the town's dawn/forenoon/afternoon/evening *is* yours. Ticked
hourly, checking in at 3pm vs 8pm shows genuinely different fresh happenings. Not a
saga to binge — an ambient diary you live beside for years, the town ageing at human
pace. (Backdate the epoch when you want to generate decades at once.)

## Determinism

Same `(seed, epoch, today)` + the same interventions → the same town, verified by
event-log checksum. The wasm engine matches native exactly; the LLM is a *recorded*
oracle (its prose is logged once and replayed); snapshots are a cache that never
affects generation. Change the behaviour or world layout and bump `SNAPSHOT_VERSION`.
