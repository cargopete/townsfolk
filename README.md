# Townsfolk — Thrushcombe St Mary

A small society for simulation. A West-Country market town, **bound to the real
calendar** — its season is your season, its day is your day. *Provincial Lady*
status comedy, *All Creatures* animal layer, *Clarkson's Farm* friction, run over
one social graph.

> Full design: [`docs/thrushcombe.md`](docs/thrushcombe.md)

## What runs today (v0.1 — the spine)

A deterministic, event-sourced core whose whole state is a pure function of
`(seed, epoch, today)`. The clock is **derived, not incremented**:

```
t = today.julian − epoch.julian
```

so the cron driver just "catches up to today" — exact phase-lock, and missed days
self-heal. v0.1 has the clock, the season state machine, a seeded cast, and a daily
incident generator drawn from each season's armed risks/windfalls, logged to SQLite.
The full WASM behaviour layer, gossip diffusion, and LLM narration come next.

## Use

```bash
cargo build
./target/debug/thrush init            # found a town, epoch = today
./target/debug/thrush init --start 2026-04-01 --seed 7   # or backdate for instant history
./target/debug/thrush tick            # advance the chronicle to today (cron runs this daily)
./target/debug/thrush narrate         # render new salient beats in voice (local Qwen)
./target/debug/thrush status          # the town at a glance
./target/debug/thrush watch           # live TUI monitor (q to quit)
./target/debug/thrush --wasm init …   # run the behaviour layer through the wasm policy guests
./target/debug/thrush-web world.db    # browse the chronicle & legends at http://127.0.0.1:8717
```

The behaviour layer runs behind a `decide(observation) -> action` boundary with two
interchangeable engines: native (in-process) and **wasm** (sandboxed guests via
wasmtime). They run the same shared policy code, so `--wasm` is bit-for-bit identical
to native — proof the substrate is transparent. Rebuild guests with `ops/build-wasm.sh`.

The town runs itself on a daily systemd user timer — see [`ops/`](ops/). Each beat
advances to today and narrates the new salient events; both steps are idempotent and
self-heal across missed days.

## Layout

```
core/           deterministic event-sourced kernel (calendar · seasons · behaviour ·
                gossip · life cycle · snapshots · the PolicyEngine boundary)
policy-genteel/ shared no_std policy crate — compiled native AND to wasm
wasm-genteel/   the genteel policy as a wasm guest (built by ops/build-wasm.sh)
thrush/         CLI + ratatui monitor + the wasmtime-backed engine
thrush-web →web/ read-only chronicle & legends browser
llm/            capped, sandboxed local Qwen for narration (the recorded oracle)
```

## The narration oracle

`llm/` runs `qwen3:8b` in Docker, GPU-capped, on `:11435`. Mechanics decide *what*
happens; the LLM only renders *how it reads*, and every response is logged verbatim
so replay stays bit-for-bit deterministic. See [`llm/README.md`](llm/README.md).

## Time model (locked)

**Companion mode, 1 sim-day : 1 real-day, full phase-lock.** Not a saga to binge —
an ambient diary you live beside for years. The town ages at human pace; resonance
with real life comes from seeding the shock layer with real weather, gated by the
season machine.
