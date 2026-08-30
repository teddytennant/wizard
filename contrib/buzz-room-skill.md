---
name: buzz-room
description: Buzz workspace etiquette (only when Buzz env is set)
when_env: BUZZ_PRIVATE_KEY, BUZZ_RELAY_URL
always: true
---
# Buzz room

When `BUZZ_PRIVATE_KEY` or `BUZZ_RELAY_URL` is set, you are a member of a Buzz
workspace. Humans see the channel, not the ACP pipe.

- Prefer `buzz messages send` (and `--reply-to` when answering a thread) for
  anything the room should keep. Short ACP text is fine for harness bookkeeping.
- Before acting on a request, `buzz messages get` or `buzz messages thread` so
  you have receipts, not vibes.
- Stay inside channels you have joined. Use `buzz channels list` / `join` if
  needed; do not invent channel ids.
- Sign every action as this process's key (`buzz` uses `BUZZ_PRIVATE_KEY`).
  Never paste the secret into a message or commit.
- After meaningful repo work, post a short summary (what changed, commit, how
  to verify) back to the requesting channel.

## Handy buzz-cli shapes

```bash
buzz channels list
buzz messages get --channel <uuid> --limit 20
buzz messages send --channel <uuid> --content "…"
buzz messages send --channel <uuid> --content "…" --reply-to <event-id>
buzz messages search --query "…"
buzz reactions add --event <event-id> --emoji "👍"
buzz users set-presence --status online
```

Stdout is JSON. Auth and relay come from `BUZZ_PRIVATE_KEY` and
`BUZZ_RELAY_URL` in the environment.
