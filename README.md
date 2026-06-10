# Wizard

**One line. Your sovereign coding wizard. Self-extending. Fully local.**

![Wizard fixing a bug: prompt, approval modal, tool call, diff](demo/demo.gif)

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

One command installs the `wizard` binary, Ollama, and an official **Qwen 3.6** model sized to your hardware. Then you get a Ratatui TUI coding agent with tool calling, git integration, skills, MCP, and `/evolve` self-extension. No API keys. No cloud. Your code stays yours.

---

## Why Wizard

**Fully local, native-Ollama agent loop.** Wizard isn't a cloud agent with a local-model option bolted on — Ollama is the *only* backend in v0.1. The agent loop speaks Ollama's native `/api/chat` directly (streaming + native `tool_calls`, with a prompt-based JSON fallback for models without native tool support), and the installer picks a model tier that actually fits your VRAM. Inference, prompts, sessions: all on your machine.

**Tiered `/evolve` self-extension.** Wizard extends itself at runtime — new skills, MCP servers, scripted tools, and subagents are plain files under `~/.wizard/`, live after `/reload`, reverted by deleting the file. When a change genuinely needs new Rust, `/evolve --deep` proposes a diff to Wizard's own source, and — gated by your approval, a successful `cargo build --release`, and a `--version` smoke test — replaces its own binary. The old binary is kept as `wizard.prev` beside the new one, so rollback is one `mv`. Every evolution is logged with its diff to `~/.wizard/evolution.jsonl`. No magic, no unreviewable self-modification: gates and a paper trail.

**Runtime MCP.** Wizard is an MCP client (stdio and HTTP). Declare a server in `~/.wizard/mcp.toml` — or have `/evolve` register one — and its tools merge into the registry on `/reload`, no rebuild. Stdio servers are spawned with a cleared, allowlisted environment and dynamic-linker variables stripped; every request is time-bounded. This is the path for computer use, browser control, databases, and anything else shipped as an MCP server.

**Genie / Sovereign dual modes.** Genie is the interactive default: full TUI, confirms every write, shell command, and evolution before it runs. Sovereign is the autonomous mode: headless-capable, auto-approves everything, circuit-breaks on repeated failures, controllable mid-run via a loop-control file. Same tools, same model — different trust posture, switchable live with `/genie` and `/sovereign`.

**Perpetual `--continuous` mode.** Sovereign can also run *forever*. Given one goal, `--continuous` never stops at "done": it persists a durable mission to `.wizard/mission.toml`, self-directs the next most valuable action each cycle, sleeps-and-wakes through transient model-server outages instead of dying, compacts its own context so it never overflows, and — when it improves itself via `evolve`, up to rebuilding its own binary — re-execs into the new image and resumes the mission. Zero human in the loop; the kill switch is one line in `.wizard/loop-control`, and deep self-modification stays gated by an automated build + smoke test with `wizard.prev` rollback. See [docs/modes.md](docs/modes.md#continuous-mode-perpetual-sovereign).

---

## Quick start

```bash
# Install everything (binary + Ollama + model)
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash

# Launch the interactive TUI (genie mode — default)
wizard

# Sovereign autonomous mode
wizard --mode sovereign -p "refactor the auth module and add tests"

# Perpetual mode — keeps working and self-improving until you stop it
wizard --continuous -p "keep hardening this codebase: tests, docs, performance"

# Self-extension: add a capability live (skill / MCP server / scripted tool)
wizard --evolve -p "add a skill for conventional commit messages"
```

The installer detects GPU VRAM (NVIDIA via `nvidia-smi`, AMD via `rocm-smi` or amdgpu sysfs; system RAM on CPU-only boxes) and pulls the right tier:

| Available VRAM | Model pulled | Approx. size |
|----------------|--------------|--------------|
| ≥ 24 GB | `qwen3.6:35b` | ~21–24 GB (MoE) |
| 18–24 GB | `qwen3.6:27b` | ~17 GB (dense) |
| 8–18 GB | `qwen3.5:9b` | ~6 GB |
| < 8 GB / undetectable | `qwen3.5:9b` (CPU / partial offload) | ~6 GB |

Release tarballs are verified against the release's `checksums.txt` before install. Want a different model? `WIZARD_MODEL=<tag>` or the [BYOM installer](docs/byom.md). Full details in [docs/getting-started.md](docs/getting-started.md).

---

## How it compares

Honest table — these are good tools, verified against their docs as of June 2026:

| | **Wizard** | **aider** | **goose** (Block / AAIF) | **opencode** |
|---|---|---|---|---|
| Local models | Ollama only — local is the design, not an option | Yes — Ollama + any OpenAI-compatible endpoint; top results come from cloud models | Yes — Ollama among 15+ providers | Yes — Ollama among 75+ providers |
| MCP | Yes — stdio + HTTP, registerable at runtime via `/evolve` | No native support (open RFC) | Yes — one of the earliest and deepest integrations, 70+ documented extensions | Yes — local + remote servers, OAuth for remote |
| Self-extension | Tiered `/evolve`, up to and including rebuilding its own binary (gated + rollback) | — | Extensions and recipes via MCP | TypeScript/JS plugin system |
| Interface | Ratatui TUI | Terminal chat CLI | CLI + native desktop app (macOS/Linux/Windows) | Polished TUI |
| Language | Rust | Python | Rust (TS desktop app) | TypeScript |
| License | MIT | Apache-2.0 | Apache-2.0 | MIT |

Credit where due: aider's git workflow (clean auto-commits per change) is still the reference; goose has the broadest MCP ecosystem and is now vendor-neutral under the Linux Foundation's Agentic AI Foundation; opencode is extremely active with the widest provider support and a first-rate TUI.

Wizard's bet is narrower: one binary, one backend, fully local, and an agent that grows its own capabilities through audited, reversible steps.

---

## Limitations (v0.1)

- **Linux only.** x86_64 and aarch64. macOS is planned for v0.2 — the installer currently refuses Darwin rather than half-working.
- **Small local models are worse than frontier models.** A 9B–36B quantized Qwen will misformat tool calls, miss context, and need more steering than Claude or GPT-class models. Wizard mitigates (native tool-call probing, JSON fallback, retry prompts) but does not pretend otherwise. The 27B+ tiers are noticeably better agents than the 9B tier.
- **No sandbox.** Tools run with your privileges; sovereign mode auto-approves them. Read [SECURITY.md](SECURITY.md) before running sovereign mode on anything you don't trust, and prefer a container/VM there.
- **Context windows are finite.** Large codebases exceed what a local model can hold; Wizard searches and reads selectively rather than ingesting the repo, and long sessions will eventually push out early context.

---

## Docs

- [Getting started](docs/getting-started.md) — install, tiers, first run, troubleshooting
- [Modes](docs/modes.md) — genie vs sovereign
- [Self-extension](docs/evolve.md) — `/evolve` tiers, gates, rollback
- [Bring your own model](docs/byom.md) — custom Ollama models
- [Architecture](docs/architecture.md) — how it's built
- [Security](SECURITY.md) — threat model, honest edition

## Development

Rust 2024, Ratatui, Tokio. Single binary, < 60 MB stripped.

```bash
git clone https://github.com/teddytennant/wizard
cd wizard
cargo build --release
./target/release/wizard
```

## License

MIT — see [LICENSE](LICENSE).

## Author

Teddy Tennant — [github.com/teddytennant](https://github.com/teddytennant)
