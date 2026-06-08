# Thrushcombe — A Small Society for Simulation

*The Diary of a Provincial Lady · All Creatures Great and Small · Clarkson's Farm, fused.*

---

## The setting

**Thrushcombe St Mary**, a small West-Country market town, **c. 1934**. A high street, a market square, a Norman church, a coaching inn gone to pub (*The Pelican*), and around it a patchwork of farms climbing out of the genteel valley into the hard hill country above. Population roughly 1,200 counting the outlying holdings.

1934 is where the three sources cohere: Delafield's *Provincial Lady* (1930) and Herriot's Dales (mid-1930s) are near-contemporaries; Clarkson's modern comedy of farming-versus-bureaucracy ports backwards via the early-1930s **Tithe War** and the **Milk Marketing Board** (1933).

## The fusion principle

Each source is a *system*, not merely a cast:

- **Provincial Lady → the status economy.** Genteel comedy of standing maintained on money that isn't there.
- **All Creatures → the animal & agricultural layer**, and the **vet as universal connector**.
- **Clarkson's Farm → the external-shock layer**: weather, bureaucracy, market, and the gentleman-farmer's doomed schemes.

## The cast (→ behaviour modules)

- `genteel_status_seeker` — Mrs Cynthia Pelham of The Laurels; Lady Aldermaston of Crale Court (same policy, different parameters).
- `hill_farmer` — the Sunters of High Foldside.
- `practitioner` — Mr Farran MRCVS, the vet (the connector who traverses every stratum).
- `scheming_improver` — Mr Rupert Crale, back to "take the home farm in hand."
- `blunt_hand` — Tot Wragg, right about most of Rupert's ideas.
- `official` — the man from the Committee; the Revd Mr Soames and his tithe.

Notable animals are first-class tracked entities: *Strawberry* the prize shorthorn (in difficult calf before the Show), *Captain* the homicidal carthorse.

## The systems

1. **Status economy** — `standing` decays without maintenance; maintenance drains `purse`. The forced choice between solvency and face.
2. **Gossip / reputation network** — events emit *news* diffusing across the social graph with delay and distortion, at class-dependent rates. Hubs (church, WI, the Pelican, market day) accelerate spread.
3. **Animal & agricultural layer** — livestock with health/gestation/value; seasonal work, risk, windfall, disaster; the vet services health-events everywhere.
4. **External-shock layer** — weather, market, bureaucracy derail plans.
5. **Time** — nested cycles: the day (phases by role), the week (market/Sunday/WI/pub), the agricultural season, the social season. Drama concentrates at synchronisation points.
6. **Drama generator** — a vocabulary of comic situation-templates fired by world state.

## The register — the important part

The warmth is a **design choice, not an emergent property.** Bias two things deliberately:

- **Drives** toward *face* and *belonging* over raw optimisation.
- **Events** toward *misfortune-with-recovery* — the collapsed soufflé and the difficult calving, not famine.

## What makes it interesting

Not enemies — **accumulation**. Split state into *fast* variables (reset each cycle: the soufflé, this week's calving) and *slow* variables (never reset, they integrate: the relationship ledger, purse, mortgage, age, reputation with hysteresis). Drama is fast feeding slow. The life cycle is the great anti-cycle: birth and death don't repeat, so the cast turns over and you generate history, not a loop. Structural tensions with no stable solution (solvent *or* high-standing; collect the tithe *or* keep goodwill) guarantee there's always a next move. **Every cycle must be able to leave a permanent mark on a slow variable.** Form recurs; content never does.

## The player

Not a god-game. You play the **novelist** — diegetic intervention only: a letter arrives, a stranger takes the cottage, the bank calls a loan, a rumour reaches the wrong ears. You arrange circumstance; the autonomous agents react in character. Layers: **Watch → Providence → Director → Inhabit → Legends.**

## Architecture

Event-source the whole thing: an append-only log; current state is the fold. Deterministic core in native Rust (ECS/SoA, SQLite log + snapshots + projections, seeded `rand_chacha`). WASM (wasmtime) for hot-swappable per-archetype behaviour policies. The LLM is an **external recorded oracle** — mechanics decide *what* happens; it narrates *how it reads*; every response is logged so replay stays bit-for-bit reproducible. Cognition is tiered: reflex → rules; salient decision → LLM (rare, constrained); narration → LLM (async, batched, off the hot path).

## Time coupling (locked)

**Companion mode, 1 sim-day : 1 real-day, full phase-lock.** `t = today.julian − epoch.julian`; the driver catches up to today. Exact phase-lock, self-healing downtime, literal real-weather resonance. An ambient diary lived beside for years, not a saga binged.

## Opening state

Late spring. Haymaking imminent, the Agricultural Show six weeks off. *Strawberry* is in difficult calf. Cook has given notice. Lady Aldermaston has announced a garden party for the 14th — to which Cynthia is invited, for which she has no suitable dress and an overdrawn account. Rupert has ordered a tractor he cannot drive. A tithe demand for High Foldside sits in the Vicar's out-tray. Seeded onto the real date you press *play*.
