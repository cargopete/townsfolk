# Ops — the daily driver

Companion mode runs the town one sim-day per real-day. A **systemd user timer**
(no sudo) fires `daily.sh` just after midnight: advance to today, narrate the new
salient beats. It's idempotent and self-healing — miss days and the next run catches
up; the Qwen container (`llm/`, `restart: unless-stopped`) is already always-on.

## Install (no root)

```bash
cargo build --release                      # the timer uses target/release/thrush
mkdir -p ~/.config/systemd/user
cp ops/thrushcombe.service ops/thrushcombe.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now thrushcombe.timer

# let it run even when you're not logged in:
loginctl enable-linger "$USER"
```

## Check / run by hand

```bash
systemctl --user list-timers thrushcombe.timer
systemctl --user start thrushcombe.service     # run today's beat now
journalctl --user -u thrushcombe.service -n 20  # what it did
```

> Companion mode is gentle — a real day is a sim day. To binge history instead,
> re-found with a backdated epoch: `thrush init --start 1934-04-01`.
