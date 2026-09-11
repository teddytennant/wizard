# Wizard documentation

Wizard is the fastest agent in your terminal. Start with [Getting started](getting-started.md);
the numbers behind that sentence, and the script that regenerates them, are in
[bench/startup](../bench/startup/README.md). Everything else is grouped by what you are trying
to do.

## Start here

| Doc | What it covers |
| --- | --- |
| [Getting started](getting-started.md) | Install, providers, model tiers, the first run and how long it takes |
| [Usage](usage.md) | The TUI, slash commands, agent-managed context |
| [Commands](commands.md) | Every slash command, and what each surface allows |
| [Modes](modes.md) | Genie, sovereign, `--continuous`, plan mode |
| [Startup benchmark](../bench/startup/README.md) | Cold and warm start, memory and install size next to six other agents, and `run.sh` |

## Models and providers

| Doc | What it covers |
| --- | --- |
| [Bring your own model](byom.md) | Local GGUFs, `llama-server`, custom endpoints |
| [Fusion](fusion.md) | `/fusion`: a panel of providers that critique each other |
| [Ultra](ultra.md) | `/ultra`: N read-only subagents, then a judge |
| [Web](web.md) | `web_fetch`, web search, and `x_search` |

## Extending it

| Doc | What it covers |
| --- | --- |
| [Self-extension](evolve.md) | `/evolve`, scripted tools on embedded LuaJIT, deep evolve, AHE |
| [Code mode](code-mode.md) | `run_code`: one Lua program per call, calling Wizard's own tools |
| [MCP](mcp.md) | Runtime MCP clients, and `wizard mcp-serve` in the other direction |
| [Hooks](hooks.md) | Lifecycle hooks, and the project trust gate |
| [Loadout](loadout.md) | Shipping a preconfigured tool surface |
| [Fork and distribute](market.md) | `/publish`, and the `wizard skills` registry client |
| [Computer use](computer-use.md) | Desktop control on Linux and macOS, and what it does not gate |

## Other surfaces

| Doc | What it covers |
| --- | --- |
| [Native GUI](native-gui.md) | `wizard gui`: the iced window, and what went with the browser GUI |
| [UI skins](ui-skins.md) | `/ui wizard`, `codex`, `grok` |
| [Interactive commands](interactive-commands.md) | Answering a shell command that stops to ask |
| [ACP](acp.md) | `wizard acp`: Zed, Neovim, Emacs |
| [Gateway](gateway.md) | The Telegram bot, and the fail-closed allow-list |
| [Services](services.md) | Running the gateway and scheduler as systemd or launchd units |
| [Buzz](buzz.md) | Joining a Buzz room as an ACP member |
| [Mesh](mesh.md) | `wizard peers`, three-state trust, the QUIC listener |
| [Headless](headless.md) | `wizard -p`, JSON and stream-JSON output |
| [Fleet](fleet.md) | Running several agents at once |
| [Scheduler](scheduler.md) | Recurring runs |
| [Tasks](tasks.md) | The todo surface the agent keeps |
| [Sync](sync.md) | Moving state between machines |
| [Images](image.md) | Attaching and viewing images |

## Keeping it healthy

| Doc | What it covers |
| --- | --- |
| [Memory](memory.md) | Typed markdown memory under `~/.wizard/memory/` |
| [Checkpoints](checkpoints.md) | `/rewind` and what it restores |
| [Doctor](doctor.md) | `wizard doctor`, and `--bundle` for bug reports |
| [Logging](logging.md) | Structured JSONL diagnostics and `WIZARD_LOG` |
| [Architecture](architecture.md) | How the crate fits together |

## Design records

These describe intent rather than current behaviour.

- [GUI design spec](gui-design-spec.md), the look the window is built to.
- [Graph explorer](graph-explorer.md), which is **deferred**: it has no route into the window in
  2.0. `wizard peers` and the mesh itself are unaffected.

## Elsewhere in the repo

- [CHANGELOG.md](../CHANGELOG.md), including the breaking changes to read before upgrading a 1.x
  machine.
- [SECURITY.md](../SECURITY.md), which applies to every autonomous run: each tool executes as you.
- [CONTRIBUTING.md](../CONTRIBUTING.md).
