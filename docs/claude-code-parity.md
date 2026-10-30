# Claude Code parity

What Claude Code has that Wizard answers to, and what it does not. Checked against
Claude Code 2.1.314 installed on the build box: `claude --help`, the real slash menu,
and `claude -p` with an isolated `HOME` and `CLAUDE_CONFIG_DIR`. The leaked source and
the spec in `awesome-claude` were used for what to look for, not as the answer.

Status words: **present** (same name, Wizard's own implementation), **aliased** (Claude
Code's name for a command Wizard already has), **bundled** (a prompt command shipped in
the binary, overridable by a file of the same name), **added** (new, with a Rust body),
**missing** (and why).

## What was compared, and what was not

Compared against the real binary:

- The top-level flag list from `claude --help`.
- The slash menu, scrolled through in a tmux session (`tmux -L`, its own socket). The menu is
  shorter when logged out; gated commands (`/voice`, `/schedule`, `/remote-control`, ...) are
  taken from the spec and marked.
- `claude -p "/context" --output-format json`, which runs locally without a login: the result
  object has `type`, `subtype`, `is_error`, `num_turns`, `result`, `session_id`, `usage`.
  Wizard's `--output-format json` now carries `type`, `subtype`, `is_error`, `num_turns` and
  `usage.input_tokens`/`output_tokens` next to its own keys.
- What `.claude/` pickup looks like in `claude -p ... --output-format stream-json --verbose`:
  the `system` init line lists `slash_commands`, `skills`, `agents`, `plugins` and
  `mcp_servers`. A fixture project showed `.claude/commands/hi.md` as `/hi`,
  `commands/team/deploy.md` as `/team:deploy`, a plugin's `commands/pc.md` as `/pl:pc`, a skill
  as `/hello`, `.claude/agents/rev.md` as agent `rev`, and `.mcp.json` as a `project` server.
  Wizard's loaders follow the same names.

Not compared, because it needs a login the isolated home does not have:

- Live model turns: the text of `claude -p` answers, `stream-json` message shapes
  (`assistant`, `user`, `result` lines with a real model), cost fields, `--max-budget-usd`,
  `--json-schema` validation, `--fallback-model` behaviour.
- Anything that talks to Anthropic services: `/login`, `/usage`, cloud sessions, `/schedule`.
- The interactive editing surface (vim mode, prompt input, voice).

Wizard's own side was checked with unit tests and the parser; no model is configured in the
test environment either.

## Slash commands

### Present

| Claude Code | Wizard | Notes |
|---|---|---|
| `/help` `/clear` `/compact` `/diff` `/doctor` `/effort` `/exit` `/goal` `/memory` `/model` `/plan` `/resume` `/rewind` `/status` `/usage` `/cost` `/btw` `/vim` `/login` | same names | `/login` with no argument signs in to xAI, the one provider with a browser sign-in. `/effort` takes `low medium high xhigh`. |
| `/agents` | `/agents` | Opens a picker of subagents. |
| `/fork` | `/fork <task>` | A background side quest that inherits the conversation. Claude's `/fork` copies the conversation into a new background session; the effect is close, the lifecycle differs. |
| `/quit` | `/quit`, `/exit`, `/q` | |

### Aliased

Rewritten before parsing, so the agent's `run_command` gate and every surface see the real name.

| Claude Code | Wizard |
|---|---|
| `/reset`, `/new` | `/clear` |
| `/continue` | `/resume` |
| `/undo`, `/checkpoint` | `/rewind` |
| `/config` | `/settings` |
| `/stats` | `/usage` |
| `/checkup` | `/doctor` |
| `/tasks` | `/bashes` (background shell tasks; the task list tool belongs to the tools work) |
| `/reload-plugins`, `/reload-skills` | `/reload` (reloads skills, commands, MCP servers, plugins together) |
| `/subtask` | `/fork` |
| `/list-agents`, `/peers` | `/agents` |
| `/allowed-tools` | `/permissions` |
| `/bug`, `/share` | `/feedback` |
| `/marketplace` | `/plugin marketplace` |
| `/routines` | `/schedule` |
| `/theme` | `/ui` (skins; the colour theme is a config key) |

### Added

| Command | What it does |
|---|---|
| `/permissions` | Shows the mode, plan mode and the project's trust decision. Wizard has no per-tool prompt, so there are no allow/deny rules to edit yet. **Depends on the permissions work**: when rules exist this is where they list. |
| `/hooks` | Lists the hooks in `~/.wizard/hooks.toml` and the project's. **Depends on the hooks work**; it reads the files directly so it does not break when the hooks module changes. |
| `/context` | Model, next-request size when known, session token totals, instruction size, skill, command and MCP counts. No coloured grid. |
| `/mcp [reload]` | Servers from `mcp.toml`, `.mcp.json`, plugins and `--mcp-config`, with the reason a project's `.mcp.json` is waiting (see Trust below). |
| `/output-style [name]` | Lists and switches output styles. |
| `/skills` | Skills, their source file and rough prompt cost. |
| `/release-notes` | The newest released section of the changelog embedded in the binary. |
| `/keybindings [init]` | Prints, or creates, `~/.wizard/keybindings.json`. The file is read by nothing yet; see missing. |
| `/feedback [note]` | Where to report a bug and what to attach. |
| `/schedule` | Lists `~/.wizard/schedule.toml`; `wizard schedule` manages it. |
| `/logout <xai\|chatgpt>` | Removes the stored sign-in. The account is named on purpose. |
| `/import [claude]` | Imports MCP servers, commands and spinner verbs from Claude Code. Not needed for pickup, see below. |
| `/export [file]` | Writes the conversation to a file, or the clipboard with no argument. |
| `/copy [N]` | Copies the Nth latest reply. |
| `/plugin ...` | Claude-format plugins and marketplaces: `list`, `install <name@marketplace\|owner/repo\|url\|dir>`, `uninstall`, `enable`, `disable`, `validate <dir>`, `marketplace add\|list\|remove\|update`. |
| `/terminal-setup`, `/ide` | Notes. Wizard needs no keyboard setup; editors use `wizard acp`. |

Terminal and window only (refused in the gateway and over ACP, by name): `/keybindings`, `/logout`,
`/import`, `/export`, `/copy`, `/plugin`. The agent may run the read-only ones itself
(`/permissions`, `/hooks`, `/context`, `/mcp`, `/release-notes`, `/skills`, `/schedule`, `/ide`).

### Bundled prompt commands

Markdown in `src/commands/bundled/`, loaded as custom commands at the lowest priority. A file called
`review.md` in `.wizard/commands`, `.claude/commands` or either home directory replaces it.

`/init` (writes `AGENTS.md`, or improves the instruction file already there), `/security-review`,
`/review` and `/code-review`, `/pr-comments`, `/simplify`, `/recap`, `/add-dir`, `/debug`, `/verify`,
`/update-config`.

These are Wizard's own prompts, not Claude Code's text. `/add-dir` asks the model to treat the
directory as part of the task; it does not widen a file-access sandbox, because Wizard has none to widen.

### Missing

| Claude Code | Why |
|---|---|
| `/advisor`, `/fast` | Anthropic model features. `/fusion` and `/ultra` are Wizard's multi-model equivalents. |
| `/autocompact` | Compaction follows thresholds in config; no per-session knob. |
| `/background`, `/stop` | Needs detach and reattach for a live session. `wizard agents` and `/dashboard` list and stop sessions started from there. |
| `/branch` | Needs a copy of the session file under a new id. |
| `/cd` | The project root is fixed for a session. `--cwd` or `--worktree` at launch. |
| `/rename` | Sessions have no display name. |
| `/color`, `/focus`, `/tui`, `/scroll-speed`, `/stickers`, `/radio`, `/powerup`, `/statusline` | Terminal UI work, owned elsewhere. |
| `/loop` | Closest are `/goal` and `wizard schedule`. |
| `/sandbox` | Wizard has no OS sandbox to toggle. |
| `/voice` | No dictation. |
| `/chrome`, `/claude-in-chrome` | The `computer` tool covers browser driving differently. |
| `/desktop`, `/mobile` | `wizard gui` and the Android app are separate products. |
| `/teleport`, `/remote-control`, `/remote-env`, `/cloud`, `/autofix-pr`, `/ultrareview`, `/web-setup`, `/install-github-app`, `/install-slack-app` | Anthropic cloud and GitHub App features. |
| `/design*`, `/slides`, `/artifacts`, `/artifact-*`, `/dataviz`, `/deep-research`, `/claude-api`, `/batch`, `/workflows`, `/workflow-authoring`, `/plugin-authoring`, `/run`, `/run-skill-generator`, `/team-onboarding`, `/skill-doctor` | Bundled skills and workflows tied to Anthropic products; skills in `.claude/skills` still load. |
| `/fewer-permission-prompts` | Wizard has no prompts to reduce. |
| `/heapdump`, `/insights`, `/upgrade`, `/usage-credits`, `/passes`, `/privacy-settings`, `/rate-limit-options`, `/restart`, `/setup-bedrock`, `/setup-vertex` | Account, billing or runtime plumbing specific to Claude Code. `wizard update` updates in place. |

## CLI flags

`wizard` takes Claude Code's flags where they mean something here. Flags that name a feature Wizard does
not have are removed from the command line with a warning on stderr, so a script written for `claude` still
starts. Nothing is dropped silently.

### Mapped

| Flag | Wizard |
|---|---|
| `-p` / `--print` | `--print` runs headless whatever the saved mode is. A bare `-p` (end of line, or another flag next) means `--print`. `-p "task"` keeps its older meaning (the task; in a terminal it opens the TUI with it). The prompt comes from `-p`, the words after the flags, and stdin. A piped stdin plus a prompt is both, stdin first. A lone word close to a subcommand name (`wizard doctr`) stops with a suggestion instead of starting a run. |
| `--output-format text\|json\|stream-json` | As before. `json` also carries Claude Code's result keys. `stream-json` keeps Wizard's event lines, not Claude's message objects. |
| `--input-format text\|stream-json` | `stream-json` reads every `{"type":"user"}` line as part of one prompt; it is not a live multi-turn input stream. |
| `-c` / `--continue` | `--resume` (most recent session here). |
| `-r` / `--resume [id]` | `-r` is short for `--resume`. `--resume <id>` opens that session in the terminal; headless runs continue the latest. |
| `--system-prompt`, `--system-prompt-file` | Published as `WIZARD_SYSTEM_PROMPT`, the override the prompt builder already reads. |
| `--append-system-prompt`, `--append-system-prompt-file` | Added to the instructions block. File text first, as in Claude Code. |
| `--add-dir <dirs...>` | A line in the system prompt naming the directories. |
| `--model`, `--effort`, `--max-turns` | Applied to the active provider, the reasoning effort and the step budget for the run. `max` effort is `xhigh`. |
| `--mcp-config <file-or-json...>`, `--strict-mcp-config` | Merged into the MCP servers; strict drops `mcp.toml`'s. |
| `--agents <json>` | Subagents for this run. A file path (the `--print` form) is not read. |
| `--plugin-dir <dir>` | A plugin directory, or a folder of them, for this run. `.zip` is not read. |
| `--json-schema <schema>` | Asks for a final JSON answer in the system prompt. Not validated. |
| `-w` / `--worktree [name]` | `git worktree add` under `.claude/worktrees/<name>`, then works there. |
| `--bare`, `--safe-mode` | No discovery of instruction files, `.claude` commands, skills, agents, styles, plugins or `.mcp.json`. Named flags still apply. Hooks are the hooks work's to honor. |
| `--disable-slash-commands` | No custom commands and no skills. |
| `--verbose`, `-d` / `--debug[=filter]` | Sets `WIZARD_LOG` for the run (log file, not stderr). |
| `--dangerously-skip-permissions` | Accepted. Wizard never asks, so there is nothing to skip. |
| `--include-partial-messages`, `--include-hook-events`, `--replay-user-messages` | Accepted, no effect: the structured formats already carry that. |
| `[prompt]` | The words after the flags are the task. |

### Accepted and reported as unsupported

`--session-id`, `--name`/`-n`, `--fork-session`, `--no-session-persistence`, `--fallback-model`,
`--max-budget-usd`, `--settings`, `--agent`.

### Removed with a warning

`--betas`, `--brief`, `--chrome`, `--no-chrome`, `--ide`, `--cloud`, `--remote`, `--environment`, `--ref`,
`--remote-control`/`--rc` and its prefix flag, `--teleport`, `--background`, `--desktop`, `--tmux`,
`--from-pr`, `--file`, `--init`, `--init-only`, `--maintenance`, `--ax-screen-reader`, `--autocompact`,
`--advisor`, `--prompt-suggestions`, `--teammate-mode`, `--channels` and the development-channels flag,
`--forward-subagent-text`, `--exclude-dynamic-system-prompt-sections`, `--system-prompt-snapshot`,
the two `--append-subagent-system-prompt` flags, `--plugin-url`, `--debug-file`, `--setting-sources`,
`--restricted`.

### Not mine

`--permission-mode`, `--allowedTools`, `--disallowedTools`, `--tools`, `--permission-prompt-tool`,
`--permission-prompts` and `--allow-dangerously-skip-permissions` belong to the permissions work and are
neither added nor stripped here.

## The `.claude` directory

Read in place, no import step. Wizard's own directory shadows Claude's on a name clash, and a project
shadows the user's.

| What | Read from | Notes |
|---|---|---|
| Commands | `~/.claude/commands`, `<project>/.claude/commands` | Subfolders namespace: `team/deploy.md` is `/team:deploy`. `$ARGUMENTS`, `$1`..`$9` and `@file` work. `!` shell injection and `allowed-tools` frontmatter are ignored. |
| Skills | `~/.claude/skills`, `<project>/.claude/skills`, `<project>/.wizard/skills` | Same `SKILL.md` format. |
| Subagents | `~/.claude/agents`, `<project>/.claude/agents` | Markdown with `name`, `description`, `tools`. `tools` map by purpose (`Read` to `read_file`, `Bash` to `execute`); names with no Wizard tool are dropped. `model` is ignored. |
| Instructions | `CLAUDE.md` (already read), `.claude/CLAUDE.md`, `CLAUDE.local.md`, `.claude/rules/*.md`, `~/.claude/CLAUDE.md`, `~/.claude/rules` | `WIZARD.md` and `AGENTS.md` still shadow `CLAUDE.md` in the same directory. A rule with `paths:` is skipped, since nothing here injects per file. |
| MCP servers | `<project>/.mcp.json` | Held back until the project is trusted (below). |
| Output styles | `~/.claude/output-styles`, `<project>/.claude/output-styles`, `~/.wizard/output-styles`, `<project>/.wizard/output-styles`; built-ins `default`, `explanatory`, `learning` | Chosen with `/output-style`, stored in `~/.wizard/output-style`; falls back to `outputStyle` in `.claude/settings.json`. Applied when the agent is built, so `/output-style` reloads. |
| Plugins | `~/.claude/plugins/installed_plugins.json` entries enabled in `enabledPlugins`; `~/.wizard/claude-plugins/installed`; `--plugin-dir` | Commands (`plugin:cmd`), skills, agents, output styles and `.mcp.json`. Plugin hooks are not loaded. |
| Marketplaces | `~/.wizard/claude-plugins/marketplaces`, and Claude Code's `known_marketplaces.json` | `owner/repo` and URL sources are cloned with `git`; relative, `github` and `url` plugin sources install, others are named as unsupported. |

### Trust

`.mcp.json` entries are commands to run, and the file arrives with a `git clone`. They connect only when the
project has a recorded yes in `~/.wizard/trusted_projects` (the same decision the project's hooks wait on) or
`WIZARD_TRUST_PROJECT` is set. `/mcp` lists the ones waiting. Servers from plugins and `--mcp-config` load,
since the user named them.

### Not covered

- `keybindings.json`: `/keybindings init` creates the file and nothing reads it yet. Remapping needs the TUI's
  key handling, which is another worker's code.
- Claude Code `settings.json` (permissions, hooks, env, `statusLine`): the settings work.
- `.claude/settings.json` plugin enablement for Wizard-installed plugins: those are enabled by being installed;
  `/plugin disable` turns one off.
- `!` commands inside command files and `allowed-tools`/`model` frontmatter on commands.
