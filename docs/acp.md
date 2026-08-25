# ACP (editor embedding)

Wizard speaks the [Agent Client Protocol](https://agentclientprotocol.com/),
so ACP editors — **Zed**, **Neovim** (CodeCompanion / avante), **Emacs** — can
drive it as their coding agent. Same agent core as the TUI and the window;
the surface is your editor on the other end of a pipe.

```bash
wizard acp
```

Runs an ACP agent over stdin/stdout. It loads your `~/.wizard` config (so it
uses whatever provider and model you've set) but never onboards or opens a TUI —
stdin and stdout carry the JSON-RPC protocol. You normally don't run it by hand;
you point an editor at the command.

## Wiring it into an editor

The command an editor needs is `wizard acp`. In **Zed** (`settings.json`):

```json
{
  "agent_servers": {
    "Wizard": {
      "command": "wizard",
      "args": ["acp"]
    }
  }
}
```

Neovim and Emacs ACP clients take the same command in their own config. Point
the editor at a project directory; each conversation you open becomes a Wizard
session rooted at that directory (`session/new` carries the cwd, and every tool
resolves paths against it).

## What you get

Per turn, Wizard streams back as it works:

- **Assistant text** and **reasoning** (`agent_message_chunk` /
  `agent_thought_chunk`) as they stream.
- **Tool calls** (`tool_call` / `tool_call_update`) — every tool shows up in the
  editor's tool view with a title and a running → completed/failed status. Five
  kinds are mapped by name: read (`read_file`, `list_files`, `git_status`,
  `git_diff`), edit (`write_file`, `edit_file`), search (`search_files`,
  `web_search`, `x_search`), execute (`execute`), fetch (`web_fetch`).
  Everything else — `memory`, `todo`, `generate_image`, every MCP tool — is
  reported as `other`, which is in practice the most common kind.
- A **stop reason** when the turn ends: exactly `end_turn`, `cancelled`, or
  `max_turn_requests`. A run that hit `--max-hours` or a circuit breaker also
  reports `end_turn`, since ACP has no reason for either.

Cancelling in the editor (`session/cancel`) interrupts the turn cooperatively at
the next stream or tool boundary. Wizard runs tools without a per-action
approval prompt, so it does its own file and shell I/O and never has to ask the
editor for permission.

## Protocol and scope

`agent-client-protocol` 0.10.4. Implemented: `initialize`, `authenticate` (a
no-op — Wizard authenticates to its own providers from `~/.wizard`, so the
editor never signs it in), `session/new`, `session/prompt`, `session/cancel`.
Everything else the crate declares — `session/load`, `session/set_mode`,
`session/set_model`, session forking and listing — answers "method not found".
The protocol version is echoed back rather than asserted, so a V1 client
negotiates V1. Because the crate's connection futures are `!Send`, the server
runs on a single-threaded `LocalSet`; the agent's own turns still use the
multi-thread runtime underneath.

Text prompts only for now (a prompt's non-text blocks are dropped). Not
surfaced over ACP: image results, todos, background tasks and subagent runs,
token-usage updates, and client-delegated file/terminal operations — Wizard
performs its own I/O rather than routing it through the editor. A plan is not
sent to the editor either: `exit_plan` is auto-approved and an `interview` is
declined, the same as any other run with nobody at a prompt. These are additive
and can come later without changing the wiring above.

## Other ACP clients

Editors are the common case. Any client that speaks ACP over stdio can drive
the same `wizard acp` process. One documented layout is **[Buzz](buzz.md)**
(Block's human+agent workspace): `buzz-acp` or Buzz Desktop spawns
`wizard acp` on @mention, and the agent talks back to the room with
`buzz-cli`. Both of those are Buzz's own binaries — this repo ships only the
harness JSON and a room skill under `contrib/`, no Buzz code. See
[Buzz](buzz.md).

## Build

The server is a plugin, behind `--features acp`, on by default. Every published
binary has it and nothing needs doing. Leaving it out (`--no-default-features`,
or a feature list without `acp`) drops the `agent-client-protocol` dependency
too, and `wizard acp` then prints one line naming the flag rather than starting
a server that cannot work. See [plugins.md](plugins.md).
