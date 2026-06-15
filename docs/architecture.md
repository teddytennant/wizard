# Architecture

Wizard is a thin, fast Rust front end over the **AHE (Agentic Harness Engineering)**
backend. It owns the terminal UI and the lifecycle of two external things: a long-lived
**NexAU code agent** (for chat) and, on demand, **AHE's `evolve.py` loop** (for
self-evolve). It does not own an agent loop, a tool registry, or a model client of its
own — those live in NexAU.

```
┌──────────────────────────┐   NDJSON over stdio   ┌───────────────────────────┐
│  wizard (Rust / Ratatui) │ ───── prompt ───────▶ │  backend/nexau_bridge.py  │
│  TUI · input · rendering │ ◀──── events ──────── │  long-lived subprocess    │
└──────────────────────────┘                       └────────────┬──────────────┘
                                                                 │ builds + runs
                                                                 ▼
                                                   ┌───────────────────────────┐
                                                   │  NexAU Agent (agent/)     │
                                                   │  LocalSandbox · shell tool │
                                                   │  → your LLM endpoint       │
                                                   └───────────────────────────┘
```

## Components (in `src/`)

| Module | Responsibility |
|--------|----------------|
| `main.rs` / `lib.rs` | entry point; CLI dispatch, config load / onboarding, then launch the TUI |
| `cli.rs` | argument parsing (`--cwd`, `--model`, `-p`, `--onboard`; `login`, `evolve` subcommands) |
| `config.rs` | `~/.wizard/config.toml` model; resolves a `BridgeConfig` to launch the bridge |
| `onboarding.rs` | first-run setup: provider presets → endpoint/key/workdir form → config |
| `auth/xai_oauth.rs` | xAI account sign-in (PKCE) and automatic bearer-token refresh |
| `backend/nexau.rs` | spawns and talks to the bridge; maps NexAU events → `AgentEvent`s |
| `agent/mod.rs` | the `Agent` handle the TUI drives, and the `AgentEvent` enum |
| `app.rs` | TUI state machine: input, slash commands, the main event loop |
| `ui.rs` | rendering (transcript, tool cards, status bar) |
| `commands.rs` | custom `/commands` and `@path` file-reference preprocessing |
| `evolve.rs` | drives AHE's `scripts/evolve.sh`; reads its status files |

## The bridge protocol

`backend/nexau_bridge.py` is a long-lived NDJSON stdio adapter around a NexAU agent. It is
spawned **once** per session, so the NexAU agent keeps its multi-turn history.

The bridge hardens its stdout: it dups the real fd 1, then points fd 1 at stderr, so
anything NexAU (or a C extension) prints lands on stderr — only the bridge's `emit()`
reaches the real stdout, keeping the NDJSON stream clean. The child's stderr is redirected
by Wizard to `~/.wizard/logs/bridge.log`.

**TUI → bridge** (one JSON object per line):

```json
{"type": "prompt", "text": "<user message>"}
{"type": "interrupt"}                    // cancel the in-flight turn
{"type": "set_api_key", "key": "..."}    // OAuth token refresh (between turns)
{"type": "shutdown"}                     // exit cleanly
```

**bridge → TUI:**

- `BRIDGE_READY` — emitted once on boot (Wizard blocks on this handshake)
- `BRIDGE_ERROR` — fatal / construction error
- streamed NexAU events: `TEXT_MESSAGE_CONTENT`, `THINKING_TEXT_MESSAGE_CONTENT`,
  `TOOL_CALL_START` / `TOOL_CALL_ARGS` / `TOOL_CALL_END` / `TOOL_CALL_RESULT`, `RUN_ERROR`
- `TURN_COMPLETE` — the authoritative end-of-turn marker

`backend/nexau.rs` accumulates the streamed tool-call args per id and renders each as a
tool card; `TURN_COMPLETE` (or a closed stream) ends the turn and unblocks the UI.

## The agent definition (`agent/`)

The NexAU agent is built from `agent/code_agent.yaml` (loaded by the bridge with
`AgentConfig.from_yaml`). The vendored agent registers a single **shell tool**
(`agent/tools/shell_tools/run_shell_command.py`) and runs in NexAU's `LocalSandbox`, so
file and shell operations happen directly in `workdir` (`SANDBOX_WORK_DIR`). The system
prompt is `agent/systemprompt.md`. This directory is vendored from
agentic-harness-engineering (Apache-2.0).

The agent's capabilities are therefore NexAU's, not a Wizard-native tool registry: the
tools the model can call are whatever the agent definition registers.

## Configuration → bridge launch

`config.rs` resolves the config into a `BridgeConfig` and passes the agent's settings to
the bridge as environment variables: `SANDBOX_WORK_DIR`, `LLM_MODEL`, `LLM_BASE_URL`,
`LLM_API_KEY`, `LLM_API_TYPE`. The interpreter defaults to the crate's bundled
`.venv/bin/python` and the script to `backend/nexau_bridge.py`, both resolved relative to
`CARGO_MANIFEST_DIR` — which is why the checkout must stay in place after install. Both
paths can be overridden with `python` / `bridge_script` in the config.

## Self-evolve (`evolve.rs`)

Self-evolve is a separate path that does **not** go through the bridge. `evolve.rs` shells
out to AHE's own `scripts/evolve.sh` (with the working directory set to `ahe_repo`),
which launches `python evolve.py` in a detached tmux session named `ahe-<name>-<ts>`.
Wizard then reads the markdown status files AHE writes under
`<ahe_repo>/experiments/<TIMESTAMP>__<name>/` (`iteration_scores.md`,
`evolution_history.md`) to summarize progress. AHE owns its own `.env`, `configs/`, and
dataset. See [ahe-evolve.md](ahe-evolve.md).
