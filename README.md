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
  fulfilling it is a **triumph**. A **mood** moves with what befalls them — and not
  uniformly: **temperament** is by type (the gentry touchy about face, the hill folk
  stoic and dour, the improver mercurial), so a snub wounds one far more than another.
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
