# Thrushcombe — local narration LLM

Capped, sandboxed Qwen for the sim's **narration oracle** (mechanics decide what
happens; this renders how it reads). Coexists with the native Ollama.

## What it is
- **Model:** `qwen3:8b` (Q4, ~6 GB on GPU) — reused read-only from the native
  Ollama store, no re-download.
- **Endpoint:** `http://127.0.0.1:11435` (native Ollama keeps 11434).
- **GPU:** full offload to the 8 GB RTX 2000 Ada; released ~60 s after idle so it
  doesn't sit on the card between narration batches.
- **Caps:** 12 CPU threads, 14 GB RAM, **no swap**, 1 model / 1 parallel slot.
  It cannot eat the ThinkPad or balloon.

## Use
```bash
docker compose up -d        # start
docker compose logs -f      # watch
docker compose down         # stop & remove (fully reversible, native untouched)
docker exec thrushcombe-qwen ollama ps   # is it resident? GPU vs CPU?

./narrate.sh "Cook gave notice for the fifth time; the overdraft is deeper than last spring."
```

## Latency
- Warm (model resident): ~2–3 s per beat.
- Cold (after 60 s idle): ~45 s one-off reload. Keep a batch flowing to stay warm,
  or raise `OLLAMA_KEEP_ALIVE` in `compose.yaml` if you narrate in slow drips.

## The recorded-oracle contract
The LLM is non-deterministic at generation time. **Log every `.response` verbatim
as an immutable event.** On replay, replay the logged text — never re-infer — and
the whole run stays bit-for-bit reproducible. Qwen is an event *source*, not part
of the deterministic core.

## GPU sharing note
One 8 GB card hosts ~one 8B model at a time. If the native Ollama has a model
loaded when narration fires, this container spills to CPU (slower, still works).
The 60 s keep-alive keeps collisions brief. If you want this to *always* win the
GPU, stop the native service first: `sudo systemctl stop ollama`.
