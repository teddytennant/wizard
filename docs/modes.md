# Personality modes

Wizard ships with two personalities that share the same tools and model but differ in autonomy, interaction style, and sampling temperature.

## Genie mode (default)

```bash
wizard
wizard --mode genie
```

Genie is the interactive, conversational mode. It's eager and creative ("your wish is my command", hence the name) and acts without asking: it bypasses permissions and executes file writes, shell commands, git commits, and evolutions directly, narrating briefly as it goes.

### Behavior

- Full Ratatui interface with chat history and tool output panels
- Executes all tool calls directly; there is no per-action y/n prompt
- Temperature: 0.8 (more creative responses)
- Loop limit: none by default (`max_steps = 0`). A turn runs until the model stops calling tools; Esc interrupts it
- Best for: collaboration, exploration, incremental changes

### Flags

| Flag | Effect |
|------|--------|
| `-p "task"` | Pre-fill the first message |
| `--resume` | Continue the last session |

### Example session

```
wizard
> Review src/auth.rs for security issues

[Wizard reads auth.rs, runs grep for hardcoded secrets, shows findings]

> Fix the issues you found

[Wizard applies patches directly → runs cargo test → reports results]
```

## Sovereign mode

```bash
wizard --mode sovereign -p "implement rate limiting on all API routes"
```

Sovereign mode is the autonomous, proactive agent. It runs headless for a single task with minimal human intervention and keeps working until that task is done or limits are hit. It does **not** keep going after "done"; that is continuous mode (below).

### Behavior

- Runs headless (no TUI). On a terminal, a busy spinner (same configurable verbs as the TUI's `[ui] spinner_verbs`) shows while the model thinks or a tool runs
- Auto-approves all tool calls (no per-action y/n)
- Temperature: 0.6 (tighter tool-call formatting)
- Loop limit: none by default; a `max_steps` capped below 100 is raised to 100, since nobody is at the prompt to say "continue"
- Circuit breakers: 6 consecutive *identical* tool failures (sovereign only) ends the turn — at 3 the model is nudged to change approach first — or 8 consecutive failures of one tool whatever the arguments
- Best for: long-running refactors, test suites, multi-file features, CI/scripted runs

### Flags

| Flag | Effect |
|------|--------|
| `--max-hours 2` | Time limit for the run |
| `--loop 10` | Max outer loop iterations |
| `--gate "cargo test"` | A command that must exit 0 before the run may finish. Repeatable. See [Quality gates](#quality-gates) |
| `--continuous` | Run perpetually, never stopping at "done" (implies sovereign). See below |
| `--cwd /path/to/repo` | Set project root |

### Control file

During a long sovereign-mode run, write to `.wizard/loop-control` in the project:

| Value | Effect |
|-------|--------|
| `stop` | Graceful shutdown after current step |
| `pause` | Wait until file is removed or set to `resume` |
| `skip` | Skip the current sub-task |

All three are read between steps inside a turn *and* at the boundary between outer
cycles, so a command written into the gap between one cycle finishing and the next
starting is not lost. Every wait the loop performs — the `cycle_pause_secs` idle pause,
the backoff after a failed cycle, and a `pause` hold itself — wakes twice a second to
re-read this file, so `stop` takes effect within about a second however long the wait
was configured to be. `--max-hours` is checked in the same places: a hold or a pause
cannot outlive the run's deadline.

### Example

```bash
wizard --mode sovereign \
  -p "add comprehensive tests for the payment module" \
  --max-hours 1 \
  --cwd ~/projects/myapp
```

### Quality gates

```bash
wizard --mode sovereign --gate "cargo fmt --check" --gate "cargo test" -p "add rate limiting"
```

The circuit breakers and the step and time caps stop a *bad* run. Nothing there stops
a run from declaring success without evidence: a model that writes a plausible patch,
never runs the suite and says "done" ends with exit code 0. A gate is a command that
must exit 0 before the run is allowed to finish. The model does not run it and cannot
see it coming.

When the model reports the task complete, every gate runs in order, in the project
root, through the same shell as `execute`. All pass, and the run finishes. One fails,
and its output is fed back as another turn: fix the cause, not the gate. Gates apply
to sovereign and continuous runs. **Genie mode ignores them entirely**: a human is
there to judge the result, and running the suite behind their back at the end of every
turn would be a surprise, not a safeguard.

Gates come from three places, merged in this order with blanks and duplicates dropped:
the `gates` config key, the project's own `.wizard/gates.toml` (`gates = ["cargo test"]`),
then `--gate` flags.

**A failed gate is not re-run while the workspace is unchanged.** This is the detail
that decides whether the feature helps or just burns the budget: a model that cannot
fix the problem will keep saying "fixed it" without touching a file, and re-running a
twenty-minute suite against identical inputs spends the whole run. So each failure is
recorded against a fingerprint of the workspace, and if the fingerprint has not moved
the model is told exactly that instead. "Changed" means: `HEAD`, `git status`, and the
size plus modification time of every tracked and every untracked-but-not-ignored file
(outside a git repo, a bounded walk of the tree skipping `target/`, `node_modules/`,
`__pycache__` and friends). Ignored files are excluded deliberately, because a `cargo test`
gate rewrites `target/` every time it runs, and counting build output as a change would
make every re-check look like progress. Only *failures* are cached this way; a passing
gate is always re-run, because a stale skip of a failure costs one turn while a stale
skip of a pass would hand back exactly the unverified success gates exist to prevent.

Reaching a limit is not success. The exit code says whether the work is verified:

| Exit | Meaning |
|------|---------|
| `0` | The run finished and every gate passed (or none was configured) |
| `5` | A gate was failing when the run ended, however it ended: attempts spent, `--max-hours`, or an operator stop |
| `2` / `3` / `4` | The usual step-budget / breaker / time-limit codes, for runs that never reached a gate check |

The failing gate is named on the last line of output (on stderr under
`--output-format json`, whose stdout stays one summary object), so
`wizard --gate "cargo test" -p "…" && deploy` refuses to deploy on a red suite. The
`reason` field still reports why the *loop* stopped (`completed`, `time_limit`); whether
the work is verified is the exit code's job.

Remediation turns do not consume `--loop`, which is the budget for the work itself;
they have their own bound, and they obey `--max-hours` like everything else.

| Key (`~/.wizard/config.toml`) | Default | Effect |
|-----|---------|--------|
| `gates` | `[]` | Gate commands applied to every sovereign/continuous run |
| `gate_max_attempts` | `3` | Consecutive failing gate checks before the run gives up and reports failure; `0` is unlimited |
| `gate_timeout_secs` | `1800` | Wall clock for one gate, additionally clamped to what is left of `--max-hours` |

## Continuous mode (perpetual sovereign)

```bash
wizard --continuous -p "keep hardening this codebase: tests, docs, performance"
```

`--continuous` turns sovereign mode into a perpetual, self-directing agent. Given an
initial goal, it doesn't stop when a sub-task completes: it records the cycle,
re-examines the project, and picks the next most valuable action. Once the mission is
done, it moves on to high-value improvements (tests, docs, hardening) or extends its own
capabilities via the `evolve` tool. There's no human in the loop, so the automated rails
below keep it safe.

### What makes it run forever

- **A cycle's "done" is verified, not trusted.** When the builder reports the goal
  complete and any [quality gates](#quality-gates) pass, a *fresh* critic subagent —
  one that never saw the builder's turn, so it cannot inherit its blind spots — judges
  the real artifact and returns a binary verdict: `OURS` (met), `BAR` (not met, plus
  the single biggest gap), or `PLATEAU` (no gap another round would close). `OURS`
  lets the cycle land; `BAR` sends that one gap back to the builder and does not count
  the cycle as done; two `PLATEAU`s in a row stop the run and report. A critic that
  cannot run is treated as a failed cycle, never as a pass, so unverified work never
  lands. See `src/agent/goal_critic.rs`.
- **Durable mission.** The goal is persisted to `<project>/.wizard/mission.toml` along
  with a cycle count, a rolling progress log, and a liveness stamp (see
  [Watching a run](#watching-a-run)). It survives restarts and binary self-replacement:
  relaunch with `--continuous` (no `-p`) and it resumes the mission.
- **Failed cycles do not end the run.** A hard error — a malformed tool call, an
  unreadable path, a provider error that is not transient — ends the *cycle*, not the
  mission. The loop rolls the cycle back (if `rollback_failed_cycles` is on), records
  what happened in `mission.toml`, waits out a backoff that doubles per failure from
  `retry_base_secs` to `retry_max_secs`, and opens the next cycle with a prompt that
  names the failure, warns that the working tree may have been rolled back under it,
  and demands a materially different approach. The bound is
  `max_consecutive_failures` (default 5): **consecutive**, so any cycle that lands
  resets it to zero, and only a run that fails that many times with nothing succeeding
  in between gives up. Set it to `0` to disable the bound entirely.
- **Circuit-breaker trips are waited out, not fatal.** The endpoint breaker opens after
  8 consecutive transient model-call failures and closes itself by admitting one
  recovery probe once its cooldown expires — 30 seconds after a first trip, then
  doubling per consecutive trip up to a 15-minute cap, so a provider in a long
  outage is retried a handful of times an hour rather than a hundred. A continuous
  run sleeps out that cooldown and starts the next cycle instead of exiting;
  a trip counts as one failed cycle toward
  `max_consecutive_failures`, so a provider that is genuinely gone still ends the run
  rather than looping on it forever.
- **Sleep-and-wake.** Transient provider failures (server unreachable, busy, `429`/`5xx`,
  dropped stream) do not abort the run. The wait climbs the ladder from
  `retry_base_secs` to `retry_max_secs` and a continuous run retries for as long as the
  circuit breaker permits, so a paused or restarting model server is waited out. Each
  wait is drawn at random from
  `[retry_base_secs, ceiling]` rather than being exactly the ceiling, so parallel workers
  and subagents pointed at one endpoint do not retry in lockstep. When the endpoint
  answers a `429` with a `Retry-After`, that deadline is honored instead when it is
  longer than the ladder's own wait (capped, so a bad header cannot park a turn), and the
  notice says so.
- **Context compaction.** When the conversation grows past `compact_threshold_bytes`,
  older history is summarized into a compact progress note so a run can continue
  indefinitely without overflowing the model's context window. The agent is also
  taught to compact deliberately (and to save durable facts with `memory` when the
  task changes); every turn is already on disk as session JSONL under
  `~/.wizard/sessions/`. See [Agent-managed context](usage.md#agent-managed-context).
- **Self-evolution + re-exec.** When the agent calls `evolve` (adding a skill, MCP
  server, scripted tool, or subagent, or rebuilding its own binary with `deep`), the
  loop saves the mission, re-execs into the freshly built image to load the new
  capabilities, and resumes the mission. The relaunch carries the run's terms with it:
  the wall clock left on `--max-hours` and the `--output-format` selection. An 8-hour
  run that evolves itself at hour one comes back with 7 hours, not with no deadline;
  a `--output-format json` run does not start printing prose into a consumer's stream.
  If less than a minute of the deadline is left, the re-exec is skipped entirely — the
  new binary is already on disk and the next launch picks it up.

### Watching a run

`mission.toml` is also the liveness record. Alongside `goal`, `cycles`, and `notes` it
carries:

| Field | Meaning |
|-------|---------|
| `phase` | What the loop believed it was doing — `cycle 12: running turn`, `cycle 12: backing off 40s after a failed cycle`, `held by operator pause (.wizard/loop-control)` |
| `heartbeat` | When `phase` was last stamped |
| `consecutive_failures` | Failed cycles since the last one that landed, against `max_consecutive_failures` |

These are stamped at phase boundaries by the loop itself, not by a background timer. A
timer would keep ticking while the agent hangs on a model call that will never answer,
which is exactly the state worth detecting: a `phase` of `cycle 12: running turn` with
an hour-old `heartbeat` is a wedged run, while the same heartbeat under
`cycle 12: idle pause (3600s)` is a run doing precisely what it was told.

### Stopping it

The same `.wizard/loop-control` file is the kill switch: write `stop` for a graceful
shutdown after the current step, or `pause` to hold. Otherwise a continuous run ends
on `--max-hours`, or on `max_consecutive_failures` cycles in a row that ended in a hard
error or a tripped breaker with nothing succeeding in between. A single circuit-breaker
trip does not end it — see above. Deep self-modification remains gated by an automated
`cargo build --release --locked`, `cargo test --release --locked`, and `--version` smoke
test (see [evolve.md](evolve.md#the-gate); the test rung makes it slow), with the
previous binary kept as `wizard.prev` for one-`mv` rollback and every evolution appended
to `~/.wizard/evolution.jsonl`.

### Tuning (`~/.wizard/config.toml`)

| Key | Default | Effect |
|-----|---------|--------|
| `continuous` | `false` | Start in perpetual mode without the flag |
| `retry_base_secs` | `5` | Base backoff when the model server is unavailable |
| `retry_max_secs` | `300` | Cap on backoff between retries |
| `cycle_pause_secs` | `0` | Pause between continuous cycles |
| `max_consecutive_failures` | `5` | Failed cycles in a row before a continuous run gives up; `0` disables the bound |
| `compact_threshold_bytes` | `48000` | History size that triggers compaction |
| `rollback_failed_cycles` | `false` | Restore a failed cycle's file checkpoints (see [checkpoints.md](checkpoints.md)) |

> **Run it in a container or VM.** Continuous mode executes every tool call with no
> human in the loop and can rewrite its own binary. Point it only at work you're willing
> to let it touch unattended, and read [SECURITY.md](../SECURITY.md) first.

## Plan mode

Plan mode is an overlay that works in every mode (genie, sovereign, continuous, gateway): the agent first investigates with read-only tools, presents a plan, and only executes once the plan is approved.

While plan mode is on:

- Only read-only tools run (`read_file`, `list_files`, `search_files`, `git_status`, `git_diff`, ...). Every other tool, including `execute`, file writes, scripted/MCP tools, and `spawn_subagent`, returns a "blocked by plan mode" error to the model. These blocks are fed back as ordinary tool errors (not fatal) and are exempt from the circuit breakers.
- The one way out is the `exit_plan` tool: the model calls it with the finished plan as markdown. The plan is saved to `<project>/.wizard/plan.md` and presented for a verdict.
- Approval ends plan mode and the model executes the plan in the same turn. Rejection (with optional feedback) keeps plan mode on; the feedback is fed back so the model can revise and call `exit_plan` again.
- Before finishing, the model can call the read-only `interview` tool to ask a short batch of clarifying questions (see below) when answers would change the plan.

### Interview

When the agent has explored enough to understand the shape of the task but still has genuine open questions whose answers would change the plan (scope, trade-offs, ambiguous intent), it calls the `interview` tool with a short batch of questions, each optionally offering suggested answers. In the TUI an interview modal opens: type a free-text answer, or press `1`–`9` to fill in a suggested option (then edit or accept it); `Enter` commits the current answer and advances; `Esc` dismisses the whole interview. The answers are fed back to the model, which folds them into the plan before calling `exit_plan`. Headless runs, the gateway, and the fleet have no interactive user, so the interview is declined automatically and the model proceeds on its best judgment. The tool is read-only, so it works mid-plan without tripping the gate.

### Omakase (chef's choice)

Omakase is the chef's-choice flavor of plan mode: it goes beyond a simple review gate. The agent still explores read-only, but then it **decides the approach itself and auto-approves its own plan**, with no interview and no human review. It's for when you want the result, not the deliberation. The plan it commits to is written to `<project>/.wizard/plan.md` and surfaced (in the TUI as a "chef's choice" card; headless prints it) before execution begins, so the chosen approach is never a black box. Because the agent is told there's no review gate, its `exit_plan` plan is self-justifying: it states the approach picked, the alternatives weighed, and the assumptions made. The `interview` tool declines to ask in omakase; the chef decides.

```
/omakase       # toggle omakase mode (implies plan mode)
```

### TUI (genie)

```
/plan          # toggle plan mode (Shift+Tab does the same)
/omakase       # toggle omakase (chef's-choice) mode
```

The status bar shows `PLAN` while plan mode is active, or `OMAKASE` in omakase mode. When the model presents a plan for review (non-omakase), a review modal opens: `y`/Enter approves, `n` opens a feedback line (type the reason, Enter sends the rejection, Esc goes back), ↑/↓ scroll the plan.

### Headless (sovereign / continuous / gateway)

```bash
wizard --mode sovereign --plan -p "refactor the config loader"
wizard --omakase -p "ship the smallest fix that passes the suite"
```

| Knob | Effect |
|------|--------|
| `--plan` (flag) | This run starts in plan mode |
| `--omakase` (flag) | Sets `omakase` + `plan_first` in config. Full chef's-choice prompting (no interview, self-justifying plan) is applied when the agent is built, on every surface: the TUI, the GUI, the gateway and headless/sovereign/continuous runs. All of them also auto-approve `exit_plan`, which is what makes it a single unattended plan-then-execute turn |
| `plan_first = true` (config) | Every session starts in plan mode |
| `omakase = true` (config) | Implies `plan_first`, whether or not `--omakase` or `--plan` was passed. Applied as chef's choice when the agent is built, on every surface |
| `plan_each_cycle = true` (config) | Continuous mode re-enters plan mode at the top of every cycle |

With no human in the loop, `exit_plan` is auto-approved on the plain plan path: the plan is printed (or, on the gateway, included in the chat reply), approval is sent automatically, and the same turn proceeds to execute. Omakase makes the agent decide for itself on top of that (no interview, plan written to `.wizard/plan.md` and surfaced before execution). The gateway also accepts `/plan` and `/omakase` chat messages to toggle these modes for subsequent messages.

The last presented plan is always available at `<project>/.wizard/plan.md`.

## Switching modes in the TUI

```
/mode sovereign    # switch to autonomous behavior (still in TUI)
/mode genie        # switch back to interactive (Ratatui TUI) mode
/sovereign         # shorthand for /mode sovereign
/genie             # shorthand for /mode genie
```

Mode changes affect prompting, interaction style, and, if `max_steps` is capped, the step budget. Switching writes the new mode to `~/.wizard/config.toml`, so it survives a restart.

## System prompts

Each mode injects a different system prompt:

**Genie** emphasizes collaboration, explanation, and narrating actions as it goes.

**Sovereign** emphasizes autonomy, completing the full task end-to-end, running tests, and committing when appropriate.

Both prompts include loaded skills from the `skills/` directory and your instruction files: `~/.wizard/WIZARD.md`, plus, for each directory from the project root outwards, the first of `WIZARD.md`, `AGENTS.md`, `CLAUDE.md` that exists there.

## Choosing a mode

| Situation | Recommended mode |
|-----------|-----------------|
| Exploring unfamiliar code | Genie |
| Quick one-off fix | Genie |
| Large multi-file refactor | Sovereign |
| CI/automation/scripted runs | Sovereign |
| Learning what the agent will do | Genie |
| Overnight autonomous work | Continuous (`--continuous`) |
