# ACP (editor embedding)

Wizard speaks the [Agent Client Protocol](https://agentclientprotocol.com/),
so ACP editors — **Zed**, **Neovim** (CodeCompanion / avante), **Emacs** — can
drive it as their coding agent. Same agent core as the TUI, and the protocol
Wizard GUI drives it over;
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
      "type": "custom",
      "command": "wizard",
      "args": ["acp"]
    }
  }
}
```

Then pick **Wizard** from the Agent Panel's new-thread menu. Past Wizard
conversations for the open project show up in the panel's thread history —
including ones started in the TUI or another client — and reopening one
continues it with its full context.

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
  `git_diff`), edit (`write_file`, `edit_file`, `apply_patch`), search (`search_files`,
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

## Choosing the model, effort, and mode

Every session advertises five config options in its `session/new` and
`session/load` answers, and a client changes them with
`session/set_config_option`:

- **`model`** (category `model`): every provider in `~/.wizard/config.toml`
  with the models it can run, as `<provider>/<model>` ids —
  `xai-oauth/grok-4.6`, `chatgpt/gpt-5.6-sol`,
  `openrouter/anthropic/claude-sonnet-5`. A provider's configured model is
  always offered; the rest come from the provider's own model list, fetched in
  the background and cached in `~/.wizard/cache/acp-models.json` (refreshed
  after 12 hours, or when a provider is added or pointed somewhere else). Image,
  video, speech, and embedding models are left out. With no providers
  configured, the one choice is `local/<model>`.
- **`thought_level`** (category `thought_level`): `default`, `low`, `medium`,
  `high`, `xhigh` — the same levels as `/effort`, sent to models that take a
  reasoning effort and ignored by the rest.
- **`wizard_mode`**: `genie`, `sovereign` or `chat`, as `/mode`. `chat` is
  plain conversation with web search and nothing else (see
  [modes.md](modes.md#chat-mode)); it is what a chat app wants.
- **`search_backend`**: which backend `web_search` uses: `auto` (Grok when
  signed in with xAI, else DuckDuckGo; the description says which it is right
  now), `grok`, or `duckduckgo`. A keyed backend from `[web] search_backend`
  (Brave, Tavily, Exa, Serper) is listed too when it is the configured one.
  See [web.md](web.md).

A choice applies to that session only and is never written to your config, so
switching models in one editor thread does not change the TUI's model or any
other session. Effort, mode and search backend apply in place; a model switch
rebuilds the session's agent over its saved history, so the conversation
continues on the new model. The answer carries the options' new state. Options can be set
between turns, not while one is running. A session reopened with
`session/load` starts from your config's defaults again, and the client sets
its choices anew.

A client that opens a session only to read these options (to fill a model
picker) leaves nothing behind: sessions that were never prompted are removed
when the server exits, whether stdin closes or it is stopped with SIGTERM.

## Slash commands

After `session/new` and `session/load`, Wizard sends an
`available_commands_update` listing the slash commands it runs over ACP, each
with a short description and, where the command takes one, an input hint. A
prompt whose first word is one of them runs through the same command code the
TUI uses instead of going to the model, and its answer comes back as
`agent_message_chunk`s followed by `end_turn`. It applies to that session only.
When it changes the model, effort, or mode, a `config_option_update` follows so
the client's pickers match.

Advertised:

| Command | Over ACP |
|---|---|
| `/model <provider>/<model>` | switch this session's model (a bare tag stays on the current provider). Bare `/model` points at the model menu |
| `/mode <genie\|sovereign\|chat>`, `/genie`, `/sovereign`, `/chat` | switch mode |
| `/effort <low\|medium\|high\|xhigh\|default>` | set reasoning effort |
| `/plan`, `/omakase` | toggle plan mode (plans are auto-approved over ACP) |
| `/rewind [turn]` | list rewindable turns, or restore files and history to before one |
| `/compact` | summarize older history now |
| `/btw <question>` | side question that stays out of history |
| `/fork <task>` | background side quest; its report lands on the next turn |
| `/agents` | list the subagents |
| `/evolve [--deep] <desc>`, `/publish [branch]` | as in the TUI |
| `/provider` | list configured providers (list only) |
| `/fusion`, `/ultra` | toggle, per session. A model switch leaves fusion |
| `/server [status\|start\|stop]` | local model server |
| `/diff` | the working tree's git diff, as text |
| `/todos` | the todo list, as text |
| `/cost`, `/usage`, `/status`, `/bashes`, `/doctor`, `/computer` | reports |
| `/memory [read\|forget <name>]`, `/turing`, `/goal [text]` | as in the TUI |
| `/reload` | skills and scripted tools (MCP servers stay connected) |
| `/help` | this list |

Not run over ACP, and refused by name if typed: `/clear` (start a new thread
instead; the client owns the session id), `/resume` and `/resume-claude` (use
the client's thread history), `/login`, `/settings`, `/dashboard`, `/vim`,
`/ui`, `/view`, `/quit`, `/exit`. Plugin commands are advertised unless the
plugin restricts them to other surfaces. Any other `/word`, including custom
commands from `.wizard/commands/` and paths like `/etc/hosts`, goes to the model
as typed.

## Protocol and scope

`agent-client-protocol` 2.0. Implemented: `initialize`, `authenticate` (see
below), `session/new`, `session/list`, `session/load`,
`session/set_config_option`, `session/prompt`, `session/cancel`, and the
`available_commands_update` and `config_option_update` notifications. Everything
else the crate declares — `session/set_mode`, `session/set_model` (the model is
a config option instead), session forking, resuming, and deleting — answers
"method not found".

An editor on a machine where someone already ran `wizard --login` or the
first-run screen never needs `authenticate`. A client with no terminal, such
as an app running `wizard acp` on a phone, signs in through it instead.
`initialize` lists the methods this build can use:

| `methodId` | What it does |
|---|---|
| `xai-oauth` | Binds xAI's callback on `127.0.0.1:56121`, sends the sign-in URL as a `_wizard/auth_url` notification (`{"methodId", "url"}`) and to `$BROWSER`, and answers once the browser comes back, within five minutes. An xAI session already on disk is reused unless `_meta.force` is `true`. |
| `chatgpt-oauth` | The same for ChatGPT, on `localhost:1455`. |
| `api-key` | `_meta.provider` names a provider from the first-run key list (`xai`, `claude`, `openai`, `openrouter`, `gemini`, `deepseek`, ...) and `_meta.apiKey` its key; `_meta.model` and `_meta.baseUrl` are optional. The key goes to `~/.wizard/credentials.toml`. |

Each one makes its provider the active one in `config.toml`, so the next
`session/new` uses it. A second `authenticate` cancels an OAuth wait still in
flight, which frees the callback port for it.

`session/list` pages through `~/.wizard/sessions`, newest first, filtered to
the client's working directory when it names one; each entry's title is the
session's first prompt. Sessions nothing was said in are left out.

A session's id is its file under `~/.wizard/sessions`, so `session/load` works
across restarts of `wizard acp`: it rebuilds the agent over the saved history
(the same replay `--resume` does), streams the transcript back as
`session/update`s — user and assistant text, reasoning, and each tool call with
its result, each marked `_meta.isReplay` — and then answers. `initialize`
advertises this as `loadSession`.

The protocol version is echoed back rather than asserted, so a V1 client
negotiates V1. Because the crate's connection futures are `!Send`, the server
runs on a single-threaded `LocalSet`; the agent's own turns still use the
multi-thread runtime underneath.

Text prompts only for now (a prompt's non-text blocks are dropped). Not
surfaced over ACP: image results, todos (except on request, with `/todos`),
background tasks and subagent runs,
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
