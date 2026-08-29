# MCP

Wizard speaks the [Model Context Protocol](https://modelcontextprotocol.io/)
in **both directions**.

- **As a client** it connects to external MCP servers declared in
  `~/.wizard/mcp.toml` and merges their tools into the registry with no
  rebuild. That is the path for computer use, browser control, and databases.
  See [Self-extension](evolve.md#mcp-servers).
- **As a server** it exposes its own native tools over stdio, so any MCP
  client (Claude Code, Cursor, another Wizard) can call them. That is what
  this page covers.

Both halves ship behind one cargo feature, `mcp`, which is on by default and
in every published binary. A build without it reaches no MCP server (an
`mcp.toml` with servers in it says so, one with none says nothing) and answers
`wizard mcp-serve` with the flag that brings it back. `mcp.toml` itself keeps
parsing either way, so `wizard import-claude` and `/evolve` still write one for
the next build to use. See [Plugin architecture](plugins.md).

Two client-side details worth knowing before you add a server, because both
change what a server sees. A spawned stdio child's environment is **cleared**
and rebuilt from a fixed allowlist — `PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM`,
`USER`, `SHELL`, `TMPDIR` — plus whatever the server's own `[server.env]`
declares, so a server does not inherit the API keys in Wizard's environment
and needs its own key passed explicitly (a `env:VAR` value in `[server.env]` or
`[server.headers]` is resolved from the environment at connect time, so the
token never sits in `mcp.toml`). Dynamic-linker variables (`LD_PRELOAD`,
`LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`)
are dropped from `[server.env]` with a warning. Every call is bounded: 20 s to
spawn and complete `initialize`, 30 s for one `tools/list` page, 120 s for one
`tools/call`. None of this is a sandbox — an MCP server runs with your
privileges, exactly like a shell command.

## `wizard mcp-serve`

```bash
wizard mcp-serve
```

Runs a Model Context Protocol server on stdin/stdout, advertising Wizard's
native tools:

`read_file`, `write_file`, `edit_file`, `list_files`, `search_files`, `execute`,
`git_status`, `git_diff`, `memory`, `todo`, `manual`, `web_fetch`, `web_search`,
`x_search`, `generate_image`, `task_output`, `task_kill`, `subagent_status`,
`subagent_kill`, `run_command`, `compact`, and `computer`.

It is self-contained: no config load, no onboarding, no LLM. It serves until
stdin closes. Note that `run_command` is only useful inside an interactive
Wizard surface that drains the slash-command queue; over plain MCP it will
refuse most calls because there is no TUI/GUI attached. `compact` likewise only
runs inside the main agent loop (it needs live conversation history); over plain
MCP it returns an error explaining that. `manual` works anywhere, since it only
reads sections of the operating charter compiled into the binary, so a foreign
client that calls it gets Wizard's own `WIZARD.md` back rather than anything
about the project being served.

`computer` is on this server too, and it is worth saying out loud: it drives
the desktop — mouse, keyboard, screenshots — for the whole logged-in session,
not just the directory being served. It is registered unconditionally so a
caller can discover it and be told *why* it is unavailable, and on a machine
that has not been provisioned for it (`wizard desktop-setup` on Linux,
Accessibility and Screen Recording permissions on macOS) every call refuses
with those instructions. On a machine that *has* been provisioned, an MCP
client you point at `wizard mcp-serve` can use it.

Tools run in the directory the server starts in; pass `--cwd <dir>` to serve a
specific project. Add `--scripted` to also advertise agent-authored scripted
tools from `~/.wizard/tools/`. Agent-loop-only tools such as `spawn_subagent`,
`exit_plan`, `interview`, `evolve`, and `publish` are **not** on this server.

```bash
wizard --cwd ~/code/myproject mcp-serve --scripted
```

## Wiring it into a client

Point any stdio-transport MCP client at the command. For Claude Code, in
`~/.mcp.json` (or a project `.mcp.json`):

```json
{
  "mcpServers": {
    "wizard": {
      "command": "wizard",
      "args": ["--cwd", "/abs/path/to/project", "mcp-serve"]
    }
  }
}
```

The client then sees Wizard's tools alongside its own.

## Protocol

Newline-delimited JSON-RPC 2.0 over stdio, protocol revision `2025-03-26`
(the same revision Wizard's client speaks). Methods answered:

| Method | Result |
| --- | --- |
| `initialize` | `protocolVersion`, `capabilities.tools`, `serverInfo` (`wizard` + version) |
| `tools/list` | every native tool as `{ name, description, inputSchema }` |
| `tools/call` | dispatches to the registry; returns `content` blocks + `isError` |
| `ping` | `{}` |

A tool that runs but reports failure (missing file, non-zero exit) returns a
normal result with `isError: true`. A call that cannot be carried out at all
(unknown tool, unparseable arguments) returns a JSON-RPC error. Notifications
(no `id`, e.g. `notifications/initialized`) are accepted and not answered.

## Scope

The server is intentionally minimal: stdio only (no HTTP/SSE transport), no
auth, and it does not chain the tools of MCP servers Wizard is itself a client
of, it advertises Wizard's own tools. Run it behind a client you trust; the
`execute` tool runs shell commands with your privileges, exactly as it does
inside a Wizard session.
