#!/usr/bin/env bash
# Thrushcombe daily beat — companion mode (1 sim-day : 1 real-day).
# Advance the town to today, then render the new salient beats in voice.
# Idempotent and self-healing: catch_up regenerates any missed days; narrate only
# touches un-narrated events. Safe to run late, twice, or after days offline.
set -euo pipefail

REPO=/home/pepe/townsfolk
DB="$REPO/world.db"
BIN="$REPO/target/release/thrush"

# The oracle is the `claude` CLI (Sonnet, local subscription). A headless --user systemd service
# has a minimal PATH, so make the CLI findable both ways: on PATH and via CLAUDE_BIN as a fallback.
export PATH="$HOME/.local/bin:$PATH"
[ -x "$HOME/.local/bin/claude" ] && export CLAUDE_BIN="$HOME/.local/bin/claude"

# Counts are tuned for Sonnet-via-CLI (~40s/call): fewer, richer beats per hour, kept well inside
# the hourly window and gentle on the subscription. The inner-life jobs are gated internally anyway.
"$BIN" --db "$DB" weather  || true   # record Sofia's sky ahead of today (best-effort)
"$BIN" --db "$DB" tick
"$BIN" --db "$DB" narrate --limit 14
"$BIN" --db "$DB" wildcard || true   # now and then, an LLM-invented happening (throttled)
"$BIN" --db "$DB" hinge    || true   # now and then, a soul faces a turning point
# conversations are now staged LIVE between beats by the Discord bot's encounter loop (recorded,
# folded back into the world), so the hourly autonomous converse is retired here.
"$BIN" --db "$DB" interrogate --count 2 || true  # the magistrate questions a couple more souls, if a murder is open
"$BIN" --db "$DB" judge     || true  # if a murder's cloud has settled past bearing, the magistrate rules: accuse | hold | widen
"$BIN" --db "$DB" reflect --count 7 || true  # advance several souls' streams a beat (continuity: keep every soul's thread fresh)
"$BIN" --db "$DB" introspect --count 1 || true  # let a soul consolidate self-model + theory of mind
"$BIN" --db "$DB" act --count 2 || true  # let a pressed soul or two take an action of their own accord — the town drives itself
"$BIN" --db "$DB" depart --count 1 || true  # a soul driven past bearing may choose to leave Thrushcombe for good
"$BIN" --db "$DB" betroth --count 1 || true  # a ripe courtship comes to its question — the courted soul answers
"$BIN" --db "$DB" gamble --count 1 || true  # in a growing season, a farmer weighs a gamble on the land
"$BIN" --db "$DB" biography --limit 1 || true  # write the life of a soul still lacking one

# Push the hour's new voiced beats to Discord — each townsperson posts to their place-channel,
# in their own name and face. Cursor-tracked; safe to run every hour. (No-op if Discord unset.)
[ -f "$REPO/ops/discord_channels.json" ] && python3 "$REPO/ops/discord_feed.py" || true
# refresh each place-channel's topic with who is there this phase (only edits what changed)
[ -f "$REPO/ops/discord_channels.json" ] && python3 "$REPO/ops/discord_presence.py" || true
