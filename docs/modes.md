# Personality modes

Wizard ships with two personalities that share the same tools and model but differ in autonomy, prompting, and confirmation behavior.

## Genie mode (default)

```bash
wizard
wizard --mode genie
```

Genie is the interactive, conversational mode. It is eager and creative — "your wish is my command" — but asks before doing anything risky.

### Behavior

- Full Ratatui interface with chat history and tool output panels
- Confirms before: file writes, shell commands, git commits (unless `--auto`)
- Temperature: 0.8 (more creative responses)
- Default loop limit: 25 agent steps per turn
- Best for: pair programming, exploration, incremental changes

### Flags

| Flag | Effect |
|------|--------|
| `--auto` | Skip confirmation prompts (still interactive TUI) |
| `-p "task"` | Pre-fill the first message |
| `--resume` | Continue the last session |

### Example session

```
wizard
> Review src/auth.rs for security issues

[Wizard reads auth.rs, runs grep for hardcoded secrets, shows findings]

> Fix the issues you found

[Wizard proposes patches → asks "Apply changes? [y/n]" → runs cargo test]
```

## Sovereign mode

```bash
wizard --mode sovereign -p "implement rate limiting on all API routes"
```

Sovereign mode is the autonomous, proactive agent. It runs with minimal human intervention and keeps working until the task is done or limits are hit.

### Behavior

- Can run headless (no TUI) or with a minimal status display
- Auto-approves all tool calls
- Temperature: 0.6 (tighter tool-call formatting)
- Default loop limit: 100 steps
- Circuit breaker: stops after 3 consecutive identical failures
- Best for: long-running refactors, test suites, multi-file features

### Flags

| Flag | Effect |
|------|--------|
| `--max-hours 2` | Time limit for the run |
| `--loop 10` | Max outer loop iterations |
| `--continuous` | Run perpetually — never stop at "done" (implies sovereign). See below |
| `--auto` | Implicit in sovereign mode; included for consistency |
| `--cwd /path/to/repo` | Set project root |

### Control file

During a long sovereign-mode run, write to `.wizard/loop-control` in the project:

| Value | Effect |
|-------|--------|
| `stop` | Graceful shutdown after current step |
| `pause` | Wait until file is removed or set to `resume` |
| `skip` | Skip the current sub-task |

### Example

```bash
wizard --mode sovereign \
  -p "add comprehensive tests for the payment module" \
  --max-hours 1 \
  --cwd ~/projects/myapp
```

## Continuous mode (perpetual sovereign)

```bash
wizard --continuous -p "keep hardening this codebase: tests, docs, performance"
```

`--continuous` turns sovereign mode into a perpetual, self-directing agent. Given an
initial goal it works toward it and **does not stop when a sub-task completes** — it
records the cycle, re-examines the project, and chooses the next most valuable action.
When the mission itself is fully done it shifts to high-value improvements (tests, docs,
robustness) or extends its own capabilities via the `evolve` tool. There is no human in
the loop; the automated rails below are what keep it safe.

### What makes it run forever

- **Durable mission.** The goal is persisted to `<project>/.wizard/mission.toml` along
  with a cycle count and rolling progress log. It survives restarts and binary
  self-replacement — relaunch with `--continuous` (no `-p`) and it resumes the mission.
- **Sleep-and-wake.** Transient Ollama failures (server unreachable, busy, `429`/`5xx`,
  dropped stream) no longer abort the run. The loop backs off exponentially
  (`retry_base_secs` → `retry_max_secs`) and retries indefinitely, so a paused or
  restarting model server is waited out, not fatal.
- **Context compaction.** When the conversation grows past `compact_threshold_bytes`,
  older history is summarized into a compact progress note so a run can continue
  indefinitely without overflowing the model's context window.
- **Self-evolution + re-exec.** When the agent calls `evolve` (adding a skill, MCP
  server, scripted tool, subagent, or — with `deep` — rebuilding its own binary), the
  loop saves the mission and re-execs into the freshly built/extended image to load the
  new capabilities, then resumes the mission.

### Stopping it

The same `.wizard/loop-control` file is your kill switch — write `stop` for a graceful
shutdown after the current step, or `pause` to hold. `--max-hours` and the circuit
breaker (3 identical failures in a row) also terminate a continuous run. Deep
self-modification remains gated by an automated `cargo build` + `--version` smoke test,
with the previous binary kept as `wizard.prev` for one-`mv` rollback and every evolution
appended to `~/.wizard/evolution.jsonl`.

### Tuning (`~/.wizard/config.toml`)

| Key | Default | Effect |
|-----|---------|--------|
| `continuous` | `false` | Start in perpetual mode without the flag |
| `retry_base_secs` | `5` | Base backoff when the model server is unavailable |
| `retry_max_secs` | `300` | Cap on backoff between retries |
| `cycle_pause_secs` | `0` | Pause between continuous cycles |
| `compact_threshold_bytes` | `48000` | History size that triggers compaction |

> **Run it in a container or VM.** Continuous mode auto-approves every tool call with no
> human in the loop and can rewrite its own binary. Point it only at work you're willing
> to let it touch unattended, and read [SECURITY.md](../SECURITY.md) first.

## Switching modes in the TUI

```
/mode sovereign    # switch to autonomous behavior (still in TUI)
/mode genie        # switch back to interactive confirmations
/sovereign         # shorthand for /mode sovereign
/genie             # shorthand for /mode genie
```

Mode changes affect prompting and auto-approve behavior for the current session. The choice is not persisted unless you update `~/.wizard/config.toml`.

## System prompts

Each mode injects a different system prompt:

**Genie** emphasizes collaboration, explanation, and asking before destructive actions.

**Sovereign** emphasizes autonomy, completing the full task end-to-end, running tests, and committing when appropriate.

Both prompts include loaded skills from the `skills/` directory and any project-level `AGENTS.md`.

## Choosing a mode

| Situation | Recommended mode |
|-----------|-----------------|
| Exploring unfamiliar code | Genie |
| Quick one-off fix | Genie |
| Large multi-file refactor | Sovereign |
| CI/automation/scripted runs | Sovereign |
| Learning what the agent will do | Genie |
| Overnight autonomous work | Sovereign |