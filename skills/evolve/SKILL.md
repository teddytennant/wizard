---
name: evolve
description: How to extend Wizard via /evolve. Cheapest tier first — skills, MCP, scripted tools, subagents; deep recompile only when the binary must change.
---

# Self-extension (/evolve)

Add capabilities through the cheapest tier that works. Tier 1 = files under
`~/.wizard/`, live on `/reload`, reverted by deleting the file. Tier 2
(`deep=true`) recompiles Wizard and should be uncommon.

## Picking a tier

| Capability is… | Channel | Recompile? |
|----------------|---------|------------|
| Knowledge, workflow, domain guidelines | Skill | No |
| External (browser, computer use, DB, search) | MCP server | No |
| Small glue / project automation | Scripted tool | No |
| Specialized sub-worker (own prompt/budget) | Subagent | No |
| Change to built-in behavior or UI | Deep (`deep=true`) | Yes |

If an MCP server or script can do it, stay Tier 1.

## Tier 1 channels

### Skills

`~/.wizard/skills/<name>/SKILL.md` — optional `name`/`description` frontmatter,
then an imperative markdown body. The prompt lists name and description; the
body is read from disk when the skill matches. Set `always: true` to inline
the body. Set `when_env: VAR1, VAR2` to omit the skill unless one of those
variables is set. User skills shadow bundled names.

### MCP servers

Append to `~/.wizard/mcp.toml`:

```toml
[[server]]
name = "computer-use"
transport = "stdio"        # or "http" with url = "..."
command = "uvx"
args = ["mcp-computer-use"]
```

`/reload` merges tools (`server__tool` on collision). Verify `command`/`url`
exists before registering.

### Scripted tools (LuaJIT)

`~/.wizard/tools/<name>.toml` + `.lua`. Prefer LuaJIT in-process; external
interpreters only when the job needs the host shell or a native CLI.

```toml
name = "slugify"
description = "Slugify a string"
script = "slugify.lua"
runtime = "luajit"

[parameters]
type = "object"
required = ["text"]
[parameters.properties.text]
type = "string"
```

```lua
local s = tostring(args.text or ""):lower()
s = s:gsub("[^%w]+", "-"):gsub("^%-", ""):gsub("%-$", "")
print(s)
```

Globals: `args`, `cwd`, `wizard.read_file` / `write_file` / `json_encode` /
`json_decode` / `runtime`. `print` → stdout; non-nil `return` if nothing printed.
Prefix `error:` for soft failure. Test once before declaring done.

### Subagents

`~/.wizard/subagents/<name>.toml`:

```toml
name = "reviewer"
description = "Audits diffs for security issues"
system_prompt = "You are a security reviewer. Report findings with file:line."
# tool_scope = ["read_file", "search_files", "git_diff"]
# max_steps = 0   # 0/omit = unlimited
```

Keep `tool_scope` narrow; subagents run auto-approved in isolated context.

## Tier 2 — deep evolve

Only for new Rust in core (built-in tool kind, protocol, TUI). Pipeline: checkout
`~/.wizard/src`, provision toolchain if needed, diff, `cargo build --release`,
exec-replace. If source/toolchain cannot be provisioned, fall back to Tier 1 and
say so — do not fail silently.

## Always

- Log to `~/.wizard/evolution.jsonl`; tell the user what was added and where.
- One file/block per capability (independently reversible).
- After write: `/reload` and confirm presence before claiming success.
- MCP and scripts run with the user's privileges — prefer well-known servers and
  small auditable scripts; never add unrequested capabilities.
