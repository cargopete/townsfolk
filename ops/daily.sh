#!/usr/bin/env bash
# Thrushcombe daily beat — companion mode (1 sim-day : 1 real-day).
# Advance the town to today, then render the new salient beats in voice.
# Idempotent and self-healing: catch_up regenerates any missed days; narrate only
# touches un-narrated events. Safe to run late, twice, or after days offline.
set -euo pipefail

REPO=/home/pepe/townsfolk
DB="$REPO/world.db"
BIN="$REPO/target/release/thrush"

"$BIN" --db "$DB" weather  || true   # record Sofia's sky ahead of today (best-effort)
"$BIN" --db "$DB" tick
"$BIN" --db "$DB" narrate --limit 50
"$BIN" --db "$DB" wildcard || true   # now and then, an LLM-invented happening (throttled)
"$BIN" --db "$DB" hinge    || true   # now and then, a soul faces a turning point
"$BIN" --db "$DB" converse || true   # let two souls fall into talk of their own accord
"$BIN" --db "$DB" interrogate --count 3 || true  # the magistrate questions a few more souls, if a murder is open
"$BIN" --db "$DB" reflect --count 6 || true  # advance several souls' streams of consciousness a beat
"$BIN" --db "$DB" introspect --count 2 || true  # let a couple of souls consolidate self-model + theory of mind
"$BIN" --db "$DB" biography --limit 2 || true  # write the lives of any souls still lacking one
