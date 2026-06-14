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
"$BIN" --db "$DB" reflect  || true   # the most-overdue soul takes a quiet hour to think
"$BIN" --db "$DB" biography --limit 2 || true  # write the lives of any souls still lacking one
