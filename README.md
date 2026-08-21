# Wizard

<p align="center">
  <img src="assets/wizard-512.png" alt="Wizard" width="128" height="128" />
</p>


[![CI](https://github.com/teddytennant/wizard/actions/workflows/ci.yml/badge.svg)](https://github.com/teddytennant/wizard/actions/workflows/ci.yml)

**One line. Your sovereign agent. Self-extending. Bring any model.**

![Wizard fixing a bug: provider list, prompt, live edit, diff](demo/demo.gif)

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

That installs the `wizard` binary. First run asks which provider you want and handles the rest. Pick **Local** and Wizard sizes a Qwen 3 GGUF to your hardware and runs [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` for you, no API key needed. Or sign in with an xAI or ChatGPT account, or drop in a key for xAI, OpenAI, Anthropic, Google Gemini, DeepSeek, Groq, Mistral, Moonshot, Z.AI, MiniMax, Together, Fireworks, Cerebras, OpenRouter, Cloudflare Workers AI, or any OpenAI-compatible endpoint, and switch live with `/provider`. One fast Rust binary on Linux and macOS. Everything it keeps lives under `~/.wizard/` in files you can read: TOML for config, markdown for memory, JSONL for sessions — edit or delete any of them.

> **Other ways to install:** local-stack preinstall, minimal, bring-your-own-model, Nix, macOS, Termux, plus a first-run walkthrough, all in **[Getting started](docs/getting-started.md)**.

---

## What it does

- **Any model, switchable live.** Speaks the OpenAI-compatible chat API (streaming + native tool calls, with a prompt-based JSON fallback), so xAI, OpenAI, Anthropic, Gemini, DeepSeek, Groq, Mistral, Moonshot, Z.AI, MiniMax, Together, Fireworks, Cerebras, OpenRouter, Cloudflare Workers AI, Ollama, and any OpenAI-compatible endpoint (vLLM, LM Studio, and friends) all work. `/provider` switches the live agent between them. Keys live in env vars or `~/.wizard/credentials.toml` (mode 0600), never in plaintext config. See [Providers](docs/getting-started.md#using-a-cloud-or-remote-provider).
- **Runs models locally, fully managed.** Pick Local and Wizard downloads a GGUF sized to your VRAM, then starts, supervises, and reuses `llama-server` for you, including a Metal build on Apple Silicon. See [Model tiers](docs/getting-started.md#model-tiers-automatic) and [Bring your own model](docs/byom.md).
- **Model fusion (`/fusion`).** Run a panel of your providers as a debate. The members critique each other's drafts, then you get one tool-capable answer synthesized from the lot. See [Fusion](docs/fusion.md).
- **Mixture of agents (`/ultra`).** Fan a turn out to N read-only subagents on the model you're already using (each with a different lens, each reading the actual repo), have a judge compare their drafts, then execute from the verdict. See [Ultra](docs/ultra.md).
- **Self-extension (`/evolve`), powered by LuaJIT.** Add skills, MCP servers, scripted tools, and subagents as plain files that go live on `/reload`. Scripted tools default to **embedded LuaJIT**: the just-in-time compiler runs your glue in-process, no external interpreter on `PATH`. Wizard can also rebuild its own binary, gated on a clean `cargo build --release --locked`, a passing `cargo test --release --locked`, and a smoke test (minutes, not seconds). Every change is logged, and the prior binary is kept one `mv` from rollback. See [Self-extension](docs/evolve.md).
- **Runtime MCP, both directions.** stdio and HTTP MCP servers merge into the tool registry without a rebuild: the path for computer use, browser control, and databases. Wizard also serves its own tools over stdio (`wizard mcp-serve`), so any MCP client can call them. See [MCP](docs/mcp.md).
- **Editor embedding (ACP).** `wizard acp` runs Wizard as an [Agent Client Protocol](https://agentclientprotocol.com/) agent, so ACP editors (Zed, Neovim, Emacs) drive it as their coding agent, streaming assistant text, reasoning, and tool calls into the editor. See [ACP](docs/acp.md).
- **Genie / Sovereign modes, plus `--continuous`.** An interactive direct-action TUI (genie), headless autonomous runs (sovereign), or a perpetual mission (`--continuous`) that compacts its own context and self-heals through outages. See [Modes](docs/modes.md).
- **Agent-managed context.** Sessions are JSONL under `~/.wizard/sessions/`. Auto-compaction keeps long runs inside the window. A live pressure signal and the mid-turn `compact` tool let the agent shed history on every surface (including headless/gateway). Durable facts go to `memory` when the task changes. See [Agent-managed context](docs/usage.md#agent-managed-context).
- **A window** *(preview)*. `wizard gui` opens the agent in its own window: one process, no webview, no loopback HTTP, with a transcript you can select across, a subagent rail, a git rail whose changed files open their diff, and a console that can answer a shell command that prompts. It needs a build with `--features native`, and that build ships as its own binary rather than replacing the one you already have: `curl | bash` with `WIZARD_NATIVE=1` places `wizard-native` beside `wizard`, so the command is `wizard-native gui`. From a checkout, `cargo build --release --features native` and then `wizard gui`. There is no second GUI: the browser one this replaced — a loopback HTTP server and a JavaScript page — is deleted. To drive a headless box, run the TUI over SSH, use `wizard -p`, point an ACP editor at `wizard acp`, or run the [Telegram gateway](docs/gateway.md). Still settling; the TUI remains the surface everything ships to first. See [Native GUI](docs/native-gui.md).
- **Persistent memory.** The agent keeps what it learns as plain markdown under `~/.wizard/memory/<project>/`: typed (`user` / `feedback` / `project` / `reference`), linked with `[[name]]`, indexed into the system prompt each session. `/memory` reads it back. It is your file, not a black box. See [Memory](docs/memory.md).
- **Harness evolution (AHE).** The shipped defaults were improved offline by an evaluate → analyze → improve loop (80% → 100% pass@1 on a Terminal-Bench sample). The old local `wizard bench` trajectory runner is gone; AHE superseded it. See [Self-extension / harness](docs/evolve.md).
- **Messaging gateway.** Run headless as a bot you talk to from your phone (Telegram), each inbound message a sovereign agent turn in your project. See [Gateway](docs/gateway.md).
- **Buzz rooms.** Drop Wizard into [Buzz](https://github.com/block/buzz) (Block's human+agent workspace) as an ACP member via `buzz-acp` or a Desktop custom harness; room I/O is `buzz-cli`. See [Buzz](docs/buzz.md).
- **Make it your own.** After a deep evolve modifies its source, `/publish` forks upstream to your GitHub and hands out a one-line installer for your variant. `wizard skills search` / `install` shares one piece instead of the whole thing, from a git-backed registry with no backend and no accounts. The default registry lives in this repo under `registry/`; `wizard skills search` reads it from raw.githubusercontent.com on `main`. Submissions are pull requests to this repo. Point `WIZARD_REGISTRY_URL` at another index to use a different one. See [Fork and distribute](docs/market.md).

**Fewer moving parts.** A single memory-safe Rust binary with an embedded LuaJIT for self-extension: no garbage-collected app runtime, no second interpreter to install and keep patched. Be clear about what that is and is not: embedding LuaJIT removes a dependency, not a capability. A scripted tool you or `/evolve` wrote runs with Lua's `os` and `io` libraries available, so `os.execute` works, and it runs *inside* the Wizard process with your privileges. A tool installed from the registry is held to an allowlisted subset (no `os`, no `io`) unless you explicitly grant it more, which is a restriction on a stranger's code and not a sandbox around it. Read [SECURITY.md](SECURITY.md) before autonomous runs; every tool executes as you.

### haha suckers

You thought you needed an interpreter and to write your TUI in bloated TypeScript. No. Just have the just-in-time compiler for LuaJIT. Wizard is Rust + Ratatui for the surface, and when it extends itself the glue is Lua that the **JIT compiles** in-process: fast, no Node, no Electron, no shipping a second runtime next to the binary. Self-extension without the bloat tax.

---

## Limitations

- **Platforms.** Linux (x86_64, aarch64), macOS (Apple Silicon and Intel), and Termux on Android (source build into `$PREFIX/bin`). Windows isn't supported; run it under WSL2.
- **Releases are signed, and the installer enforces it.** `install.sh` fetches `checksums.txt` and `checksums.txt.minisig`, verifies the signature under the key inlined in the script, then verifies each asset's SHA-256 against that file; `wizard update` applies the same rules with the key compiled in. Every failure aborts, and there is no flag to skip the check. Verifying needs `minisign` on `PATH` or an OpenSSL with ed25519 and blake2b (macOS ships LibreSSL, which has neither, so `brew install minisign` first), or add `WIZARD_BUILD_FROM_SOURCE=1` to build from the release tag instead. See [Getting started](docs/getting-started.md#install).
- **Small local models are worse than frontier models.** A 4B-36B quantized Qwen will misformat tool calls, miss context, and need more steering than Claude- or GPT-class models. Wizard mitigates with retry prompts and a refusal to act on a truncated tool call, plus a prompt-based JSON tool protocol for models with no native tool calling — though the probe that selects that protocol only asks Ollama, so a model served by `llama-server` (the default local backend) is taken at its word and always gets native tool calls. The 27B+ tiers make much better agents than the 9B tier, and the 4B tier that 8 GB machines get is a floor: it exists so such a machine has a local option at all, not because it is a good agent.
- **No sandbox.** Tools run with your privileges, with no per-action approval gate in either mode. Read [SECURITY.md](SECURITY.md) before running on anything you don't trust, and prefer a container/VM for autonomous or continuous work.
- **Context windows are finite.** Wizard searches and reads selectively rather than ingesting the whole repo, auto-compacts older history, and instructs the agent to compact / reset deliberately when the task changes, but long sessions still eventually push out early detail. See [Agent-managed context](docs/usage.md#agent-managed-context).

---

## Docs

Everything is indexed in **[docs/README.md](docs/README.md)**, grouped by what you are trying to do. The headline pages:

- [Getting started](docs/getting-started.md): install (all flavors, Nix, macOS, Termux), tiers, providers, first run, in-place updates (`wizard update`), the download mirror, troubleshooting
- [Usage](docs/usage.md): slash commands, `wizard agents`, the subagent rail, token usage and cost, todos, project instructions, `/ui` interfaces, themes and color depth
- [Native GUI](docs/native-gui.md): `wizard gui`, the agent in its own window (`WIZARD_NATIVE=1` to install a build that has it) · [Graph explorer](docs/graph-explorer.md) (deferred, not reachable in 2.0)
- [Gateway](docs/gateway.md): run Wizard as a Telegram bot
- [Buzz](docs/buzz.md): join a Buzz workspace as an ACP agent (`buzz-acp` or Desktop)
- [Modes](docs/modes.md): genie, sovereign, and continuous
- [Self-extension](docs/evolve.md): `/evolve` tiers, gates, rollback
- [Fusion](docs/fusion.md): the `/fusion` debate panel
- [Ultra](docs/ultra.md): the `/ultra` mixture of agents
- [Bring your own model](docs/byom.md): any GGUF, or custom Ollama models
- [Computer use](docs/computer-use.md): desktop control — mouse, keyboard and screenshots — on Linux and macOS, for a model that can see
- [Custom commands & @files](docs/commands.md): your own `/commands`; `@path` file references
- [Memory](docs/memory.md): what Wizard remembers between sessions, and `/memory`
- [Hooks](docs/hooks.md): lifecycle hooks, and the trust gate on a project's own hooks · [Tasks](docs/tasks.md) · [Web](docs/web.md) · [Headless output](docs/headless.md) · [Checkpoints](docs/checkpoints.md)
- [Interactive commands](docs/interactive-commands.md): answering a shell command that prompts, and what the timeout does while it waits
- [Doctor & status](docs/doctor.md): `wizard doctor` diagnostics, `wizard doctor --bundle` bug reports, `/status`
- [Logs](docs/logging.md): `~/.wizard/logs/`, the `WIZARD_LOG` filter
- [Scheduler](docs/scheduler.md): cron-scheduled headless runs
- [Fleet](docs/fleet.md): parallel workers over git worktrees
- [Mesh](docs/mesh.md): `wizard peers`, trust states, and what does and does not ship
- [Sync](docs/sync.md): `wizard sync` moves config, skills, and custom tooling between machines as a signed bundle
- [Fork and distribute](docs/market.md): publish your evolved Wizard, and the skills and tools registry (`wizard skills`)
- [Architecture](docs/architecture.md): how it's built
- [Security](SECURITY.md): threat model
- [WIZARD.md](WIZARD.md): the agent's bundled behavioral charter, inherited and editable by every fork
- [CHANGELOG.md](CHANGELOG.md): what changed in 2.1, what breaks coming from 1.x, and what to do about each break

## Development

Rust 2024, Ratatui, Tokio, embedded LuaJIT (`mlua`). Single binary, < 60 MB stripped.

```bash
git clone https://github.com/teddytennant/wizard
cd wizard
cargo build --release
./target/release/wizard
```

There's also a Nix flake: `nix run github:teddytennant/wizard`, or `nix develop` for a shell with the Rust toolchain and `llama-cpp`.

## Acknowledgements

Local inference is powered by [llama.cpp](https://github.com/ggml-org/llama.cpp) (ggml-org): Wizard installs its `llama-server` when you pick the local option. [Ollama](https://ollama.com) is a first-class supported provider.

## License

`MIT AND Apache-2.0` — both at once, not a choice between them. Wizard's own code is MIT ([LICENSE-MIT](LICENSE-MIT)). The terminal-UI code ported from OpenAI Codex and xAI grok-build stays under Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)); [NOTICE](NOTICE) names every file it landed in and [docs/ui-skins.md](docs/ui-skins.md) has the file-by-file table.
