#!/usr/bin/env bash
# Thrushcombe daily beat — companion mode (1 sim-day : 1 real-day).
# Advance the town to today, then render the new salient beats in voice.
# Idempotent and self-healing: catch_up regenerates any missed days; narrate only
# touches un-narrated events. Safe to run late, twice, or after days offline.
set -euo pipefail

REPO=/home/pepe/townsfolk
DB="$REPO/world.db"
BIN="$REPO/target/release/thrush"

"$BIN" --db "$DB" weather || true   # record Sofia's sky ahead of today (best-effort)
"$BIN" --db "$DB" tick
"$BIN" --db "$DB" narrate --limit 50
