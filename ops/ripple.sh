#!/usr/bin/env bash
# A lean "ripple" runner for MANUAL story turns — after you've staged beats by hand (a murder, a
# proclamation, secrets), this advances the town's inner life and pushes to Discord WITHOUT the
# narrate backlog grinder. That skip alone drops ~14 oracle calls per turn; the oracle now fans its
# calls out concurrently (see THRUSH_ORACLE_CONCURRENCY), so a wide reflect that used to take ~5
# minutes lands in well under one.
#
#   ops/ripple.sh [DAYS] [REFLECT_COUNT]
#     DAYS          how many days to jump first (default 0 — stay put)
#     REFLECT_COUNT how many souls advance their thread (default 6 routine; pass 12 for a big turn)
#
# For the FULL chronicle beat (incl. narrate) use ops/daily.sh — that is what the hourly timer runs.
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
source ops/secrets.env 2>/dev/null
export PATH="$HOME/.local/bin:$PATH"
[ -x "$HOME/.local/bin/claude" ] && export CLAUDE_BIN="$HOME/.local/bin/claude"
export CLAUDE_MODEL="${CLAUDE_MODEL:-sonnet}"
export THRUSH_ORACLE_CONCURRENCY="${THRUSH_ORACLE_CONCURRENCY:-12}"
BIN="$REPO/target/release/thrush"

DAYS="${1:-0}"
RC="${2:-6}"

if [ "$DAYS" -gt 0 ] 2>/dev/null; then
  echo "===== JUMP +$DAYS days ====="
  "$BIN" jump --days "$DAYS" 2>&1 | tail -3
fi
"$BIN" status 2>&1 | head -4

echo "===== JUDGE ====="
"$BIN" judge || true                       # rules only if a cloud has settled; a closed case stays closed
echo "===== PULSE (the whole town's murmur — every soul thinks a beat) ====="
"$BIN" pulse || true
echo "===== REFLECT --count $RC (parallel, the deep stream for the pressed) ====="
"$BIN" reflect --count "$RC" || true
echo "===== ACT --count 2 ====="
"$BIN" act --count 2 || true
echo "===== DEPART --count 1 ====="
"$BIN" depart --count 1 || true

echo "===== DISCORD ====="
[ -f ops/discord_channels.json ] && python3 ops/discord_feed.py 2>&1 | tail -2 || true
[ -f ops/discord_channels.json ] && python3 ops/discord_presence.py 2>&1 | tail -2 || true
echo "===== DONE ====="
