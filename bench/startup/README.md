# Startup benchmark

Cold and warm start, idle memory and install size of wizard next to Claude Code,
Codex CLI, OpenCode, Crush, Goose and Aider. Each agent gets its own ubuntu:24.04
image (pinned by digest), installed the way its README says at a pinned version,
and is started N times under a pty that answers terminal queries the way xterm
would. The numbers in `results.md` come from `run.sh` and nothing else.

## Run

    bench/startup/run.sh              # builds seven images, then measures
    bench/startup/run.sh wizard codex # a subset, written to results-wizard-codex.md
    RUNS=20 bench/startup/run.sh
    WIZARD_BINARY=target/release/wizard bench/startup/run.sh wizard
    WIZARD_VERSION=v3.1.0 bench/startup/run.sh

Needs Docker, bash, python3 and a network. `run.sh` refuses to measure when the
1-minute load average is above nproc/4 or less than 8 GB of memory is available
(`LOAD_OK=1` overrides, and the host state is recorded in the results either
way). `before.sh` records the other half: install.sh from main timed in a fresh
container, then every onboarding screen wizard shows before a prompt can be
typed, as text under `before/screens/`.

## The rule

Every agent is measured as shipped, default config, phone-home switched off
where the agent offers a switch, model endpoint pointed at a dead local port
(127.0.0.1:9, nothing listens). So no agent can block on onboarding or on a
model server, and what is left on the wire is whatever an agent does with no
switch. What each one was told is in `agents/*/setup.sh` and `bench.env`, and
is printed under its row in `results.md`. `OPENAI_API_KEY` and
`ANTHROPIC_API_KEY` are exported into every container with dummy values.

## What is measured

- install: bytes the install added on top of the shared base image, runtimes
  (Node for Codex's npm install, uv's CPython for Aider) included. `docker
  history` layer sizes.
- bundle: the agent's own artifact. A binary where there is one, the npm
  package for Codex, the uv tool venv for Aider.
- cold start: start 1 in a fresh container, `fork`+`exec` to the first output
  containing the agent's prompt marker, ANSI stripped, page cache cold.
- warm start: median and p90 of starts 2 to N in the same container.
- `--version`: wall time of the version flag, the simplest possible start.
- RSS and PSS: `VmRSS` and `Pss` summed over the process tree 3 s after the
  prompt is up, `VmHWM` summed the same way as the peak, and the tree's CPU
  time at that point so a slow start can be read as work or as waiting.

## What it does not measure

- Anything after the prompt: keystroke latency, first token, tool calls. The
  prompt marker is the first frame with an input line, not a loaded agent.
- Startup with a real key or with the phone-home left on. Both are one
  `setup.sh` edit away, but the table does not show them.
- wizard's optional browser MCP. The default loadout runs Playwright through
  `npx`; the base image has no Node, so that spawn fails at once and neither
  its cost nor the npm lookup it would do is in wizard's numbers.
- Your terminal. The pty answers DA1, DSR, cell size, DECRQM and OSC color
  queries like xterm and stays silent on kitty and sixel probes. wizard sends
  its kitty/DA1 probe before the first frame, so a terminal that never answers
  DA1 costs it a 2 s timeout; xterm, kitty, Ghostty and this harness answer.
- A quiet machine is required, not provided. The guard above and the host
  line in the results are all it can do.
