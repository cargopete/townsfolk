#!/usr/bin/env bash
# Thrushcombe narration call — the shape the sim's "recorded oracle" uses.
# Mechanics decide WHAT happened; this renders HOW it reads, in voice.
#
#   ./narrate.sh "Cook gave notice for the fifth time; the overdraft is deeper than last spring."
#
# Notes:
#  - think:false   -> disable qwen3's <think> block (faster, no wasted tokens for prose)
#  - num_ctx 4096  -> small KV cache; the per-agent dossier is only a few hundred tokens
#  - stream:false  -> one JSON back, easy to log verbatim as an immutable event
set -euo pipefail

HOST="${OLLAMA_HOST:-http://127.0.0.1:11435}"
MODEL="${OLLAMA_MODEL:-qwen3:8b}"
EVENT="${1:?usage: narrate.sh \"<event description>\"}"

SYSTEM='You are the chronicler of Thrushcombe St Mary, a small West-Country market town in 1934. Render the given event as a short, warm, wry diary-style paragraph in the register of interwar English provincial comedy — gentle misfortune borne with dignity, the small humiliation, the understated joke. Never melodrama. 2-4 sentences. No preamble.'

jq -n --arg m "$MODEL" --arg s "$SYSTEM" --arg p "$EVENT" \
  '{model:$m, system:$s, prompt:$p, think:false, stream:false, options:{num_ctx:4096, temperature:0.8}}' \
| curl -s "$HOST/api/generate" -d @- \
| jq -r '.response'
