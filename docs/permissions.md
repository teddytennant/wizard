# Permissions and settings

Wizard runs the model's tool calls without asking. That has not changed: with no rules set and no mode named, every call runs. Permission rules narrow that, in the same syntax Claude Code uses, so a rule you already wrote for Claude works here. Naming a mode (`--permission-mode default`, `acceptEdits`, `plan`) turns on Claude Code's prompts: writes, commands and the like are asked about before they run.

Rules live in three lists, `deny`, `ask` and `allow`, under `permissions` in a settings file, in `[permissions]` in `config.toml`, or on the command line.

## Rule syntax

A rule is a tool name, optionally followed by a specifier in parentheses.

| Rule | Matches |
|------|---------|
| `Bash` | every shell command |
| `Bash(git diff:*)` | `git diff` and anything after it (`git diff --stat`), but not `git difftool` |
| `Bash(npm run * --fix)` | `*` matches any text, spaces included |
| `Bash(ls)` | exactly `ls` |
| `Read(./secrets/**)` | reading or searching anything under `secrets/` |
| `Edit(src/**)` | writing under `src/` with any of the edit tools |
| `Write`, `Glob`, `Grep` | the single tool of that name |
| `WebFetch(domain:example.com)` | fetching from one host; `*.example.com` covers subdomains |
| `mcp__github` or `mcp__github__*` | every tool of an MCP server |
| `mcp__github__create_issue` | one MCP tool |

Wizard's own tool names work too (`execute`, `read_file`). The Claude names map like this: `Bash` is `execute`, `Read` is `read_file` (and covers `list_files` and `search_files` for path rules), `Write` is `write_file`, `Edit` is `edit_file` and also covers `write_file` and `apply_patch`, `Glob` is `list_files`, `Grep` is `search_files`. An MCP tool `server__tool` is `mcp__server__tool`.

Path patterns follow gitignore rules with these anchors: `//path` is the filesystem root, `~/path` is your home, `/path` is the project root, and `path` or `./path` is the working directory. A bare name such as `.env` matches at any depth. A pattern that names a directory covers what is under it. In a `deny` or `ask` rule a relative pattern like `src/**` also matches at any depth (`app/src/x.rs`); in an `allow` rule it does not. `..` is folded away before matching, so `src/../secrets/a` is still `secrets/a`.

For shell commands:

- A compound command (`a && b`, `a | b`, `a; b`, newlines) is split. A `deny` or `ask` rule applies if **any** piece matches, including pieces inside `$(...)` and backticks, and past leading `VAR=value` assignments and wrappers like `timeout` and `nice`. An `allow` rule applies only if **every** piece is covered, and never to a command Wizard cannot split cleanly (an unclosed quote, a trailing `&&`).
- Read-only commands (`echo`, `ls`, `cat`, `grep`, `git status`, `git diff` and a few more, without redirection) need no `allow` rule even in `dontAsk` mode, so `touch x && echo done` needs a rule only for `touch`. This matches Claude Code.
- Rules match command text, not programs. `Bash(rm:*)` does not stop `python -c 'import os; os.remove(...)'`. Enforcement that must not depend on spelling belongs in a `pre_tool_use` hook.

A `Read` deny on a path also stops the edit tools on it.

## Evaluation order

1. `deny` rules. First match refuses the call, whatever else says.
2. `ask` rules. A match means the call needs approval, **even in `bypassPermissions` mode**.
3. The permission mode (below).
4. `allow` rules, where the mode needs them.

Specificity does not matter; an `allow` rule cannot carve an exception out of a `deny`. Hooks run first and cannot get around this: a `pre_tool_use` hook can deny, and can ask, but its `allow` does not override a deny or ask rule.

## What "ask" does

A call needs approval when an `ask` rule matches it, when a `pre_tool_use` hook says `ask`, or when the mode asks about that kind of call (below). It then goes, in order, to:

1. `permission_request` hooks (see [hooks.md](hooks.md)). A hook that answers `{"behavior": "allow"}` lets the call run; `deny` refuses it. The payload carries `tool_name`, `tool_input`, `reason` and `permission_suggestions`, the rules the prompt would offer to save.
2. Whoever is at the surface:
   - **The terminal UI** draws Claude Code's dialog: *Yes*, *Yes, and don't ask again for ...*, and *No, and tell Claude what to do differently (esc)*. Number keys, arrows and Enter pick; typing under the third choice is the reason the model reads. For a file edit the second choice is *Yes, allow all edits during this session*, which switches the session to `acceptEdits`. For a command it is a rule for the command (`git commit:*`, `cargo test:*`), for `WebFetch` the domain, for an MCP tool the tool. Wizard adds the rule to the ones in force and writes it to `.claude/settings.local.json` in the project (`.wizard/settings.local.json` when the project has a `.wizard/` and no `.claude/`), keeping every other key in the file.
   - **ACP clients** (`wizard acp`, the claude look, the GUI) get a `session/request_permission` on the tool call, with allow once, allow always (when a rule is on offer) and reject options.
   - **Headless runs, the gateway, fleet runs and subagents** have nobody to ask, so the call is refused.
3. A refusal tells the model why. If a mode was named, the text is Claude Code's: `Claude requested permissions to write to /path, but you haven't granted it yet.`, `Claude requested permissions to use WebFetch, but you haven't granted it yet.`, `This command requires approval`, `This Bash command contains multiple operations. The following parts require approval: ...`. Otherwise (an `ask` rule and no mode named) it says no one was available and points at an allow rule or a hook. A *No* from the dialog reads `The user doesn't want to proceed with this tool use. The tool use was rejected ... STOP what you are doing and wait for the user to tell you how to proceed.`, with what you typed in its place when there is something.

Esc, Ctrl-C or `session/cancel` on a waiting prompt refuses it and ends the wait.

A subagent cannot show a dialog, so a call it makes that needs approval is refused. Give the subagent an `allow` rule for what it should do.

## Modes

Set with `--permission-mode`, `permissions.mode` in `config.toml`, or `permissions.defaultMode` in a settings file.

| Mode | Behavior |
|------|----------|
| `default` | Not named: anything no rule objects to runs, which is Wizard's long-standing behavior. Named (`--permission-mode default`, or `permissions.mode` or `defaultMode` set to `default`): Claude Code's. Read-only tools and read-only commands run; file edits, other commands, the web, MCP tools, `run_code` and `Monitor` ask. The todo list, questions to you, subagents and task control never ask. |
| `acceptEdits` | File edits inside the project run, and so do `touch`, `mkdir`, `rm`, `rmdir`, `mv`, `cp` and `sed` on paths inside it. Edits and those commands on paths outside the project, a `cd` next to a write, and every other command or side effect ask. |
| `plan` | Starts the session in Wizard's plan mode (the same as `--plan`): read-only tools and read-only commands (`ls`, `pwd`) run, writes are refused with `Cannot write to <path> while in plan mode`, and the plan is put to you with `exit_plan`. After you approve it the session carries on in `default` with Claude's prompts on. |
| `dontAsk` | Read-only tools and calls an `allow` rule names run. Everything else is refused, without asking. |
| `bypassPermissions` | Nothing is checked except `deny` and `ask` rules. |
| `auto` | Claude Code's classifier mode. The classifier is a model call that judges each action, and Wizard has none, so `auto` behaves as Wizard's own `default`: everything runs unless a rule objects. |

Modes accept `acceptEdits`, `accept-edits` and `accept_edits` alike.

Checked against Claude Code 2.1.314 with a mock API (`-p` runs, which cannot ask and so refuse; the refusal texts above are its tool results). Its `default` refused writes, `touch`, `curl` and `WebFetch` and ran `ls` and `Read`. Its `acceptEdits` ran `touch`, `mkdir`, `mv`, `cp`, `rm` and `sed -i` in the project and asked for `touch /tmp/x`, `git commit`, `cd d && touch q` and a write outside the project. Its `dontAsk` refused anything no `allow` rule names, an `ask` rule still asked under `bypassPermissions`, and `plan` answered a write with `Cannot write to <path> while in plan mode.` while `ls` ran. Wizard matches all of that. Two differences stay: in `-p`, Claude's plan mode hands every other command to its classifier, where Wizard refuses it; and Claude's `acceptEdits` also lets a redirection such as `echo hi > z.txt` write inside the project, which Wizard still asks about.

A mode named in `~/.claude/settings.json` counts: if your Claude settings say `"defaultMode": "default"`, Wizard prompts too.

## Command-line flags

```
wizard --permission-mode dontAsk --allowedTools "Bash(git diff:*)" Read
wizard --disallowedTools "Bash(rm:*)" "Edit(.env)"
```

`--allowedTools` adds `allow` rules and `--disallowedTools` adds `deny` rules, for that run only. Arguments are separated by spaces or commas; spaces and commas inside parentheses stay in the rule. Both are accepted as `--allowed-tools` and `--disallowed-tools` too.

## Settings files

Wizard reads the keys it can act on from Claude Code's settings files, lowest precedence first:

1. `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`)
2. `~/.wizard/settings.json`
3. `<project>/.claude/settings.json`
4. `<project>/.wizard/settings.json`
5. `<project>/.claude/settings.local.json`
6. `<project>/.wizard/settings.local.json`
7. `/etc/claude-code/managed-settings.json` and `/etc/wizard/managed-settings.json`

Then `[permissions]` in `config.toml`, then the command line. Lists (`permissions.allow`, `ask`, `deny`, and hooks) are concatenated across all of these. Single values and individual `env` entries are won by the later layer.

| Key | What Wizard does with it |
|-----|--------------------------|
| `permissions.allow`, `ask`, `deny` | The rules above. |
| `permissions.defaultMode` | The permission mode. |
| `hooks` | Hooks, see [hooks.md](hooks.md#claude-format-hooks). |
| `env` | Added to the environment of every command Wizard runs: the `execute` tool, background tasks, hooks. |
| `model` | Only from `~/.wizard/settings.json` and read at startup. A Claude model id means nothing to a Grok or local provider, so a shared `.claude/` file cannot choose Wizard's model. |
| `disableAllHooks` | Turns off every hook that came from a settings file (not `hooks.toml`). |
| `statusLine`, `outputStyle` | Read and kept, but Wizard's interface does not draw them yet. |

Keys that are not listed are ignored. `permissions.additionalDirectories` and the other Claude-only fences have no effect because Wizard has no directory sandbox.

### Trust

A project's settings files can carry hooks and `env`, which run code, so they go through the same [project trust](hooks.md#project-trust) decision as `.wizard/hooks.toml`. A settings file with none of `hooks`, `env`, `statusLine` or a helper-command key is just data and is not gated.

Until a project is trusted, its settings files still contribute `deny` and `ask` rules, and a `defaultMode` of `plan` or `dontAsk`, because those can only make Wizard stricter. Their `allow` rules, `env`, hooks and a looser `defaultMode` are ignored. Your own `~/.claude/settings.json` and `~/.wizard/settings.json` are always read.
