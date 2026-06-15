# Wizard

[![CI](https://github.com/teddytennant/wizard/actions/workflows/ci.yml/badge.svg)](https://github.com/teddytennant/wizard/actions/workflows/ci.yml)

**A Claude-Code-style chat TUI over the NexAU code agent, with AHE-driven self-evolve.**

![Wizard chat session](demo/demo.gif)

Wizard is a [Ratatui](https://ratatui.rs) terminal UI written in Rust. It is a thin,
fast front end; the actual agent loop is **AHE (Agentic Harness Engineering)**:

- **Chat** is the [NexAU](https://github.com/nex-agi/NexAU) code agent. Wizard spawns a
  small Python bridge that builds a NexAU `Agent` from the vendored definition in
  [`agent/`](agent/) and streams its events back to the TUI. The agent runs in NexAU's
  `LocalSandbox`, so its shell tool operates directly on your working directory.
- **Self-evolve** drives [AHE's](https://github.com/) real `evolve.py` harness-evolution
  loop as a subprocess — the same loop AHE ships, not a reimplementation. See
  [docs/ahe-evolve.md](docs/ahe-evolve.md).

> The `agent/` directory is vendored from agentic-harness-engineering (Apache-2.0).

---

## How it works

```
┌──────────────────────────┐   NDJSON over stdio   ┌───────────────────────────┐
│  wizard (Rust / Ratatui) │ ───── prompt ───────▶ │  backend/nexau_bridge.py  │
│  TUI · input · rendering │ ◀──── events ──────── │  long-lived subprocess    │
└──────────────────────────┘  text/thinking/tool   └────────────┬──────────────┘
                                                                 │ builds + runs
                                                                 ▼
                                                   ┌───────────────────────────┐
                                                   │  NexAU Agent (agent/)     │
                                                   │  LocalSandbox · shell tool │
                                                   │  → your LLM endpoint       │
                                                   └───────────────────────────┘

   wizard evolve / /evolve ──launches──▶  AHE  scripts/evolve.sh → python evolve.py
                                          (detached tmux session, AHE owns its config)
```

The bridge is spawned once and kept alive for the whole session, so the NexAU agent
keeps its multi-turn history. Each user turn writes one `{"type":"prompt",...}` NDJSON
line to the child; the child streams NexAU events (`TEXT_MESSAGE_CONTENT`,
`THINKING_TEXT_MESSAGE_CONTENT`, `TOOL_CALL_*`, …) back on stdout, which Wizard maps onto
the TUI. The child's stderr is redirected to `~/.wizard/logs/bridge.log` so library
logging can never corrupt the terminal.

---

## Setup

Wizard builds from source. It needs a Rust toolchain (2024 edition) and
[uv](https://github.com/astral-sh/uv) for the Python bridge environment.

```bash
git clone https://github.com/teddytennant/wizard
cd wizard

# 1. Build the bridge venv (.venv with NexAU pinned)
uv sync

# 2. Build the binary
cargo build --release            # → ./target/release/wizard
# or install it onto your PATH:
cargo install --path .
```

> **The repo must stay in place.** The binary resolves the bridge script
> (`backend/nexau_bridge.py`), the virtualenv (`.venv`), and the agent definition
> (`agent/`) from the crate's build-time path (`CARGO_MANIFEST_DIR`). Moving or deleting
> the checkout after `cargo install` breaks the bridge. You can override the bridge paths
> with `python` / `bridge_script` in the config (see below).

> **One-liner:** `install.sh` automates the steps above (installs Rust/uv if needed, clones to
> `~/.local/share/wizard`, runs `uv sync`, and `cargo install`s the binary):
> ```bash
> curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
> ```
> (Building this branch before it lands on `main`? `… | WIZARD_REF=ahe-backend bash`.)

Run it:

```bash
wizard            # first run opens onboarding; pick a provider, done
```

---

## Configuration

Config lives at `~/.wizard/config.toml`. The first run (or `wizard --onboard` any time)
writes it for you through an onboarding flow — no editing TOML by hand.

Onboarding offers these provider presets:

| Preset | Auth |
|--------|------|
| **xAI (sign in with your account)** | OAuth, no API key — `wizard login xai` |
| **xAI (Grok) — API key** | API key |
| **OpenAI** | API key |
| **OpenRouter** | API key |
| **Local (llama.cpp / Ollama)** | none (local endpoint) |
| **Custom (OpenAI-compatible)** | API key |

### config.toml keys

```toml
model    = "grok-4.3"                 # LLM_MODEL for the agent
base_url = "https://api.x.ai/v1"      # OpenAI-compatible endpoint (LLM_BASE_URL)
api_type = "openai_chat_completion"   # wire API the agent speaks (see below)
api_key  = "sk-..."                   # key stored inline, OR:
# api_key_env = "XAI_API_KEY"         # read the key from this env var at launch
auth     = "api_key"                  # "api_key" or "xai_oauth"
workdir  = "/path/to/project"         # directory the agent's shell tool operates in
mode     = "genie"                    # cosmetic label: "genie" or "sovereign"

# python      = "/custom/python"               # override the bridge interpreter
# bridge_script = "/custom/nexau_bridge.py"    # override the bridge script

[evolve]                              # optional — enables self-evolve (see below)
ahe_repo = "/path/to/agentic-harness-engineering"
```

- **`api_type`** is NexAU's `LLM_API_TYPE`. Accepted values:
  `openai_chat_completion` (default, broadest compatibility incl. local llama.cpp),
  `openai_responses`, `anthropic_chat_completion`, `gemini_rest`.
- **xAI OAuth:** pick "xAI (sign in with your account)" in onboarding, or run
  `wizard login xai` to complete the PKCE browser flow. No key is stored; bearer tokens
  are refreshed automatically (including between turns, mid-session).
- API keys are read from `api_key_env` (an environment variable) when set; otherwise the
  inline `api_key` is used. Local endpoints need no key.

---

## Usage

```bash
wizard                       # launch the TUI in the current directory
wizard --cwd /path/to/proj   # run the agent from a different directory
wizard --model grok-4        # override the model for this session
wizard -p "fix the build"    # pre-fill the input line (still requires Enter)
wizard --onboard             # re-run setup (provider, model, working dir)
wizard login xai             # xAI account sign-in (OAuth)
```

### Slash commands

| Command | Action |
|---------|--------|
| `/help` | show commands and keys |
| `/clear` | clear the conversation (respawns a fresh agent) |
| `/model [tag]` | show the active model, or switch to `tag` |
| `/mode [genie\|sovereign]` | pick or switch the personality label (`/genie`, `/sovereign` switch directly) |
| `/evolve [start\|status]` | drive AHE's harness-evolution loop |
| `/quit` | exit |

Genie / Sovereign is a cosmetic label (status bar + spinner flavor); the agent loop is
NexAU's in both. The input line has readline-style editing (Ctrl-A/E, Ctrl-W/U/K, …),
command suggestions, input history, and `@path` file completion. Custom `/commands`
(markdown files under `~/.wizard/commands/` or `<project>/.wizard/commands/`) and `@path`
file references are supported — see [docs/commands.md](docs/commands.md).

---

## Self-evolve (AHE)

`wizard evolve …` and `/evolve` launch and monitor AHE's **real** `evolve.py` loop. Wizard
does not reimplement it — it shells out to AHE's own `scripts/evolve.sh`, which runs
`python evolve.py` in a detached tmux session. **AHE owns all of its own configuration.**

AHE runs every harness on the **local Docker daemon** — no cloud sandbox — so a loop
needs only Docker + LLM keys.

Point Wizard at an AHE checkout:

```toml
[evolve]
ahe_repo = "/path/to/agentic-harness-engineering"
# experiment_config = "configs/experiments/exp-local-sample.yaml"  # optional (default)
```

Evolve is **off** until this section is present. AHE supplies its own configuration and
data — Wizard provides none of them. To actually run a loop, the AHE checkout needs:

- **Docker** → the `docker` CLI on PATH with a running daemon (`docker ps` works)
- **LLM keys** → `LLM_API_KEY`, `LLM_BASE_URL`, `LLM_MODEL` (a local `llama-server` is
  fine), in `<ahe_repo>/.env` or the environment
- a **dataset** for the experiment config (the default `exp-local-sample` ships its own)

No **E2B** account or **GitHub token** is needed — those were only for the old cloud
sandbox mode, which AHE no longer defaults to.

```bash
wizard evolve start      # preflight, then launch in a detached tmux session
wizard evolve status     # latest experiment's scores + newest iteration
wizard evolve sessions   # list running ahe-* tmux sessions
wizard evolve stop <s>   # kill a session
wizard evolve attach     # print the tmux attach command
```

`evolve start` preflights for `scripts/evolve.sh`, `evolve.py`, a working `docker`, LLM
keys (`.env` or environment), and the experiment config, reporting anything missing
first. Progress is read from
`<ahe_repo>/experiments/<TIMESTAMP>__<name>/` (`iteration_scores.md`,
`evolution_history.md`). Full details: [docs/ahe-evolve.md](docs/ahe-evolve.md).

---

## Docs

- [Getting started](docs/getting-started.md): build, onboarding, first chat, troubleshooting
- [Architecture](docs/architecture.md): the TUI ⇄ bridge ⇄ NexAU design
- [Custom commands & @files](docs/commands.md): your own `/commands`; `@path` references
- [Driving AHE's evolve loop](docs/ahe-evolve.md): `wizard evolve` / `/evolve`
- [Security](SECURITY.md): threat model

## Development

Rust 2024, Ratatui, Tokio. The Python bridge pins NexAU via `uv` (`pyproject.toml`).

```bash
uv sync
cargo build
cargo test            # Rust unit tests
./target/debug/wizard
```

## License

Wizard is MIT; see [LICENSE](LICENSE). The vendored NexAU agent definition under
[`agent/`](agent/) is from agentic-harness-engineering and is Apache-2.0.

## Author

Teddy Tennant ([github.com/teddytennant](https://github.com/teddytennant))
