# Permissions and settings

Wizard runs the model's tool calls without asking. That has not changed: with no rules set, every call runs. Permission rules are how you narrow that, in the same syntax Claude Code uses, so a rule you already wrote for Claude works here.

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

Claude Code shows a prompt. Wizard has none for tool calls, so an `ask` outcome goes to `permission_request` hooks (see [hooks.md](hooks.md)). A hook that answers `{"behavior": "allow"}` lets the call run; `deny` refuses it. With no hook, or no answer, the call is refused and the model is told that nobody was available to approve it. If you want a rule to prompt you, a `permission_request` hook that asks you some other way is how.

## Modes

Set with `--permission-mode`, `permissions.mode` in `config.toml`, or `permissions.defaultMode` in a settings file.

| Mode | Behavior |
|------|----------|
| `default` | Anything no rule objects to runs. This is Wizard's long-standing behavior, so it is where Wizard differs most from Claude Code's `default`, which asks about writes and commands. |
| `acceptEdits` | File edits run. Commands and other side effects need an `allow` rule, otherwise they are an `ask`. Read-only tools run. |
| `plan` | Starts the session in Wizard's plan mode (the same as `--plan`): read-only tools until you approve the plan. |
| `dontAsk` | Read-only tools and calls an `allow` rule names run. Everything else is refused, without asking. |
| `bypassPermissions` | Nothing is checked except `deny` and `ask` rules. Same as `default` in Wizard today. |
| `auto` | Claude Code's classifier mode. Wizard has no classifier, so this is `default`. |

Modes accept `acceptEdits`, `accept-edits` and `accept_edits` alike.

Checked against Claude Code 2.1.314 with a mock API: its `default` refuses (when it cannot ask) a write or a `touch`, its `dontAsk` refuses anything no `allow` rule names, and an `ask` rule still asks under `bypassPermissions`. Wizard matches those last two. It differs on `default` as described above, and on `acceptEdits`, where Claude Code also lets simple file commands (`touch`, `mkdir`, `rm`, `mv`, `cp`, `sed`) run inside the project; Wizard treats every shell command as needing an `allow` rule there.

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
