# Startup benchmark

Cold start, idle memory and install size of wizard next to Claude Code, Codex CLI,
OpenCode, Crush, Goose and Aider. Each agent gets its own ubuntu:24.04 image,
installed the way its README says, and is started ten times under a pty that
answers terminal queries the way xterm would. The numbers in `results.md` come
from `run.sh` and nothing else.

## Run

    bench/startup/run.sh              # builds seven images, ~10 min of measuring
    bench/startup/run.sh wizard codex # a subset
    RUNS=20 NET=none bench/startup/run.sh

Needs Docker and a network for the builds. `before.sh` records the other half:
install.sh from main timed in a fresh container, then every onboarding screen
wizard shows before a prompt can be typed, as text under `before/screens/`.

## What is measured

- install: bytes the install added on top of the shared base image, runtimes
  (node, uv's python) included. `docker history` layer sizes.
- bundle: the agent's own artifact. A binary where there is one, the npm
  package for codex, the uv tool venv for aider.
- cold start: `fork`+`exec` to the first output containing the agent's prompt
  marker, ANSI stripped. Median and p90 of the ten starts, plus the first start
  on its own.
- `--version`: wall time of the version flag, the simplest possible start.
- RSS: `VmRSS` summed over the process tree 3 s after the prompt is up, and
  `VmHWM` summed the same way as the peak.

Every agent is configured with a dummy key and a model endpoint on
127.0.0.1:9, where nothing listens, so none of them can block on onboarding or
on a slow model server. What each one was told is in `agents/*/setup.sh` and
in the notes under each table row.

## What it does not measure

- Anything after the prompt: keystroke latency, first token, tool calls.
- Startup with a real key. Claude Code cannot be pointed at a dead endpoint
  without also breaking its startup, so it talks to api.anthropic.com with a
  key that gets 401. Its number includes whatever that costs it.
- A quiet machine. Runs are sequential and nothing else is started by the
  harness, but the host is whatever you run it on.
- Your terminal. The pty answers DA1, DSR, cell size and color queries like
  xterm and stays silent on kitty and sixel probes. A terminal that answers
  differently changes the path an agent takes at startup.
