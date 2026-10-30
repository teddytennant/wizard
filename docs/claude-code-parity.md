# Claude Code parity

What Claude Code has and what Wizard has, one section per area. A prompt, skill
or habit written for Claude Code should find the thing it names.

## Tools

Compared against Claude Code 2.1.314. Where the real binary could be driven
(`claude mcp serve` lists and runs the tools it exposes without a model), the
schemas and error strings below were checked against it; where it could not,
the row says what the comparison rests on instead.

**How they are exposed.** Wizard's own tools keep their names and stay the
defaults. The Claude Code names are added on top (`src/tools/compat/`), take
Claude's argument names, and are advertised by name only until `tool_search`
loads them, the same way MCP tools are under the default `safe` token profile.
The cost of having all of them is a line of names in the system prompt. The
`stock` profile defers nothing, so it registers none of them. A name an MCP
server or a scripted tool already took is left alone.

Statuses: **present** is Wizard's own tool with the same job; **alias** is
present and also reachable under Claude's name and schema; **new** was missing
and is added; **partial** is added with a stated gap; **skipped** has a reason.

| Claude Code | Wizard | Status | Notes |
| :- | :- | :- | :- |
| `Agent` | `spawn_subagent` | alias | `subagent_type` `general-purpose`, `Explore` and `Plan` run the built-in `worker`; other values name a subagent definition. `model`, `effort`, `name`, `isolation`, `mode`, `team_name` are accepted and ignored. `run_in_background` is an addition. |
| `AskUserQuestion` | `interview` | alias | Option labels are kept; descriptions and `header` are dropped; `multiSelect` is noted in the question text. |
| `Bash` | `execute` | alias | `timeout` is milliseconds, rounded up to whole seconds, capped at 600. `description` and `dangerouslyDisableSandbox` are ignored. |
| `CronCreate`, `CronList`, `CronDelete` | none | new | Backed by `wizard scheduler`. A job is an entry named `cron-<id>` in `~/.wizard/schedule.toml` that fires a headless session in the project directory, not a prompt inside the live session. Every job is persisted, so `durable` changes nothing. Recurring jobs expire after 7 days; `recurring: false` fires once and deletes itself. Each run is capped at 1 hour. The result warns when no scheduler daemon holds the lock. See [Scheduler](scheduler.md). |
| `Edit` | `edit_file` | alias | `replace_all`, the uniqueness error and the same-string error use Claude's wording. An empty `old_string` creates a file that is missing or empty. No read-before-edit rule: the real binary did not enforce one over MCP either. |
| `EndConversation` | none | skipped | Ends a session when a user is abusive. A model-provider policy tool, first-party only; Wizard talks to many providers and has no such protocol to end. |
| `EnterPlanMode` | `/plan` | new | Sets the plan-mode flag; no approval dialog. Read-only like `exit_plan`. |
| `ExitPlanMode` | `exit_plan` | alias | `plan` is required (Claude Code reads its plan file instead). `allowedPrompts` is ignored. |
| `EnterWorktree`, `ExitWorktree` | none | new | Worktrees live in `.wizard/worktrees/<name>` on branch `worktree-<name>`, from `HEAD` (Claude branches from `origin` by default). The session's `cwd` is swapped for every later tool call; the system prompt still names the starting directory. `remove` refuses when there are uncommitted files or commits not on the original branch unless `discard_changes` is true. A worktree entered by `path` can be left with `keep` but never removed. |
| `Glob` | `list_files` | new | Real ripgrep `--files --glob` semantics, oldest first by mtime, capped at 100 with a truncation line, dotfiles included, `.gitignore` not applied, absolute patterns split at their static prefix. Falls back to a directory walk without `rg`. Claude Code does not offer it on Linux and macOS (its `Bash` searches instead), so there was no real binary to compare. |
| `Grep` | `search_files` | new | `output_mode`, `-A`/`-B`/`-C`/`context`, `-n`, `-i`, `type`, `glob`, `head_limit` (default 250, 0 for all), `offset`, `multiline`; numbers and booleans may arrive as strings; `file_path` is accepted for `path`. Result wording ported from the leaked source. Falls back to `grep -E` without `rg`, which cannot do `multiline` or `type`. Not offered on Linux by the real binary, so the comparison was against its `Bash` running `grep`, not `Grep`. |
| `ListAgents` | `subagent_status` | partial | Lists running background subagents of this session, and `main` for a subagent. No other local sessions, teammates, cloud or Remote Control sessions. |
| `ListMcpResourcesTool`, `ReadMcpResourceTool` | none | new | `resources/list` and `resources/read` on the connected servers, registered with the MCP tools when any server is up. Result wording from the leaked source. No MCP server was on hand to run them against. |
| `LSP` | none | skipped | Registered with Claude's schema; it answers that no language server is configured. A real client has to pick, start and index a server per language, and a server started per call answers `null` on most projects. Use `Grep` and `Read`. |
| `Monitor` | `execute` with `run_in_background` | new | A background task whose stdout lines are put into the conversation at the top of each step, batched, capped at 100 lines per batch and stopped at 5000 events. `timeout_ms` defaults to 5 minutes, capped at 30. Events arrive between steps and do not wake an idle session. The `ws` source is refused; use `websocat` in a command. `TaskStop` and `task_kill` stop it. |
| `NotebookEdit` | `edit_file` | new | Edits by cell id (or `cell-N`): replace, insert after, delete. New ids are 8 hex digits, only for nbformat 4.5 and later. Replacing a code cell clears outputs. Written with Jupyter's one-space indent. Checked against the real tool over MCP, including its error strings. |
| `PowerShell` | `execute` | skipped | `execute` runs `sh -c`. A PowerShell tool needs a PowerShell-backed execute and its own permission rules, and Wizard's shell tool has no Windows-first path. |
| `PushNotification` | none | partial | A session notice every surface renders, plus `notify-send` or `osascript` when present. No phone push; the result says so. |
| `Read` | `read_file` | alias | `offset` is the 1-based start line, `limit` a line count. A `.ipynb` renders as `<cell id="..">` blocks. Lines are not numbered, on purpose: numbered output made models copy the gutter into `old_string`. `pages` and `allow_large` are accepted and ignored. |
| `RemoteTrigger` | none | skipped | Calls the claude.ai routines API with a claude.ai login. Wizard's cloud-side equivalent is `wizard scheduler` on a machine you run. |
| `ReportFindings` | none | partial | Validates the shape (max 32, required fields) and returns the ranked list as text. There is no typed review panel to render it in. |
| `ScheduleWakeup` | none | new | One-shot scheduler entry `wakeup-<id>` at now plus the delay (clamped to 60 to 3600 s, rounded up to a whole minute), replacing the last one for the directory. `stop: true` cancels it. Fires a headless session, not this one. |
| `SendFeedback` | none | skipped | Files feedback with Anthropic. Wizard has no feedback endpoint. |
| `SendMessage` | none | partial | To `main` from a subagent, or to a running background subagent by `agent:<id>` or name. Delivered at the recipient's next step. No teammates or other sessions. |
| `SendUserFile` | none | partial | Checks the file and shows its path and caption as a session notice. Nothing is copied to a device: Wizard has no Remote Control. |
| `ShareOnboardingGuide` | none | skipped | Uploads to claude.ai for Team and Enterprise plans. |
| `Skill` | skills in the system prompt | new | Loads a skill's body by name from the same roots, with `args` appended. Unknown names get a "did you mean". |
| `SubagentHandback` | final reply of a subagent | skipped | A subagent's last message is already its report. Handing back early needs a way to end a sub-run from inside a tool and a classifier gate Wizard does not have. |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate` | `todo` | new | A numbered board with `blocks`/`blockedBy`, `owner` and `metadata`; every change is mirrored into the `todo` list the TUI draws. A `todo` write does not touch the board. Result wording from the leaked source (the real binary exposes none of them over MCP in 2.1.314). |
| `TaskOutput` | `task_output` | present | Claude Code removed it in 2.1.277. |
| `TaskStop` | `task_kill`, `subagent_kill` | new | Stops a shell task or Monitor by number; `agent:<n>` or a name reaches a background subagent. Error strings match the real binary. |
| `TodoWrite` | `todo` | alias | `activeForm` is dropped. |
| `ToolSearch` | `tool_search` | present | Same `select:` syntax. The deferred names are listed in the system prompt. |
| `WaitForMcpServers` | none | skipped | Wizard connects MCP servers before the registry is built and loads their tools once; there is no late arrival to wait for. |
| `WebFetch` | `web_fetch` | alias | Returns the page with the `prompt` placed above it for you to apply; there is no second model pass. `offset` drops that many characters. A non-URL fails with Claude's `Invalid URL`. |
| `WebSearch` | `web_search` | alias | `allowed_domains` and `blocked_domains` become `site:` and `-site:` operators in the query, so they depend on the backend honouring them. |
| `Workflow` | `run_code` | skipped | Claude Code's workflows are a scripting runtime with resume. Wizard's nearest are `run_code` programs and `wizard fleet`; neither is a drop-in. |
| `Write` | `write_file` | alias | |
| `Artifact`, `DesignSync`, `Projects`, `StructuredOutput`, `RefreshMcpTools`, `ReadMcpResourceDirTool`, `ShowOnboardingRolePicker` | none | skipped | claude.ai-hosted pages and design sync, the SDK's structured output, and onboarding UI. Wizard has no claude.ai backend, and its `--json` output is separate. |

### What was compared against the real binary

Claude Code 2.1.314 was run from an isolated `HOME` and `CLAUDE_CONFIG_DIR`
with no login, so it cannot call a model. `claude mcp serve` still lists its
tools and runs them, which gave real schemas for `Agent`, `Bash`, `Read`,
`Edit`, `Write`, `NotebookEdit`, `WebFetch`, `WebSearch`, `TaskStop`, `Skill`,
`EnterWorktree`, `ExitWorktree`, `SendMessage`, `ListAgents`, `CronCreate`,
`CronDelete`, `CronList`, `ScheduleWakeup`, `Monitor`, `ReportFindings` and
`ToolSearch`, and real results for most of them. The compat tests pin the
property set and required list of each of those schemas
(`tools::compat::tests::schemas_accept_everything_claude_code_2_1_314_accepts`).

Not comparable that way, and taken from the leaked source and the specs alone:
`Glob` and `Grep` (not shipped on Linux), `TodoWrite`, `TaskCreate` and the
other task tools, `AskUserQuestion`, the plan-mode tools, `LSP`,
`ListMcpResourcesTool`, `ReadMcpResourceTool`, `PushNotification` and
`SendUserFile`. The model-facing result text of every tool is the string the
leaked source's `mapToolResultToToolResultBlockParam` builds where there is
one; `mcp serve` returns the structured output instead, so those strings could
not be diffed.

Known differences from the real results, beyond the rows above: `Read` returns
plain text rather than `{type, file}` JSON; `Edit`, `Write` and `NotebookEdit`
return a one-line confirmation, not the patch; `CronCreate` text describes
where the job runs.
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
| `/keybindings [init]` | Prints, or creates, `~/.wizard/keybindings.json`. The file is read; see [Keys and the prompt](#keys-and-the-prompt). |
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
| `/color`, `/focus`, `/tui`, `/scroll-speed`, `/stickers`, `/radio`, `/powerup` | Terminal UI work, owned elsewhere. |
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

- Claude Code `settings.json` (permissions, hooks, env): the settings work. The terminal reads two keys from it,
  `editorMode` and `statusLine`, see below.
- `.claude/settings.json` plugin enablement for Wizard-installed plugins: those are enabled by being installed;
  `/plugin disable` turns one off.
- `!` commands inside command files and `allowed-tools`/`model` frontmatter on commands.

## Keys and the prompt

Compared against Claude Code 2.1.314, driven through tmux with no account (`looks/crates/claudew/tools`).
The same engines serve the house terminal UI and `wizard-ui-claude`: `src/vim.rs` and `src/keybindings.rs`
are compiled into both, so the two cannot read a key differently.

### Vim mode

`editorMode: "vim"` in `~/.claude/settings.json`, the project's `.claude/settings.json` or its
`settings.local.json` turns it on in both UIs; so does `/vim`, which also keeps the choice (`[ui] vim` in the
house UI, `editorMode` in `~/.claude/settings.json` in the claude look). The prompt starts in INSERT, Esc goes
to NORMAL, Enter submits from either.

Present: `h j k l`, `w b e W B E`, `0 ^ $`, `gg G` with a count, `f F t T` and `; ,`, `%`; the operators `d c y`
with a motion, doubled (`dd`), or a text object (`iw aw iW aW`, quotes, `( [ { <`); `x r s S D C Y p P J ~ u .`,
`>> <<`, `i a I A o O`, counts, and VISUAL and VISUAL LINE (`v V`, motions, `d x y c s p r ~ u U > < J o`, text
objects). `j` and `k` at the first or last line recall history. `-- INSERT --`, `-- VISUAL --` and
`-- VISUAL LINE --` show in the footer; NORMAL shows nothing, as in Claude Code.

How it was checked: `tests/fixtures/vim-cases.tsv` holds 500 key sequences. `looks/crates/claudew/tools/vimdiff.py`
typed each into the real 2.1.314 in vim mode and read back the prompt and the cursor (`vim-claude.tsv`);
`vim::tests::matches_claude_code_2_1_314` replays them against the engine. Where Claude Code does something
other than vim, the engine follows Claude Code: `F` and `T` operators take the character under the cursor, `<<`
removes up to two blanks, `w` on the last word rests one past it, `dw` on an empty line removes the line break,
`d1G` means line 1, a change is two undo steps (the deletion, then what was typed), typing is an undo step of its
own. One case it does not follow: `3dd` on exactly three lines leaves the text alone in 2.1.314 (`2dd` and `5dd`
delete), which looks like a bug there; the list of exceptions is `NOT_FOLLOWED` in `src/vim_tests.rs`.

Not covered: `vimInsertModeRemaps`, `gj`/`gk` on soft-wrapped lines (they move by logical line), Ctrl+V block mode
(Claude Code has none either), and graphemes (the engine moves by `char`).

### keybindings.json

Read from `~/.wizard/keybindings.json` (what `/keybindings init` writes), then `~/.claude/keybindings.json`, then
the project's `.claude/keybindings.json`; later wins. Changes are picked up on the next key, no restart.

The format is Claude Code's: `bindings` blocks of `{ "context": "Chat", "bindings": { "ctrl+k": "chat:stash",
"ctrl+s": null } }`. `null` unbinds a default. Modifiers `ctrl`, `alt`/`opt`/`meta`, `shift`, `cmd`; a space makes
a chord that has three seconds per step; `command:<name>` runs a slash command. Unknown contexts, actions and
the reserved keys (`ctrl+c`, `ctrl+d`, `ctrl+m`, `ctrl+[`, `ctrl+i`, `ctrl+h`) are skipped with a warning and the
default stays.

What runs: `app:exit`, `app:toggleTodos`, `app:toggleTranscript`, `history:search`, `history:previous`,
`history:next`, `chat:cancel`, `chat:cycleMode`, `chat:modelPicker`, `chat:submit`, `chat:newline`,
`chat:externalEditor`, `chat:clearInput`, `command:<name>` in the house UI; the claude look also has `chat:undo`,
`chat:stash`, `chat:sendNow`, the autocomplete and confirm actions and `transcript:exit`. An action Wizard has no
use for is accepted and does nothing. The `?` panel shows a key as rebound. Not read: `Footer`, `Agents`, `Pane`
and the other contexts of screens Wizard does not have.

### Status line

`statusLine` in the same settings files: `{ "type": "command", "command": "...", "padding": 0,
"refreshInterval": 5 }`. In the house UI the command runs with Claude Code's JSON on stdin (`session_id`,
`transcript_path`, `model`, `workspace`, `cwd`, `version`, `output_style`, `cost`, `context_window`,
`exceeds_200k_tokens`, and `vim.mode` when vim mode is on), `COLUMNS`, `LINES` and `CLAUDE_CODE_CHILD_SESSION=1`
in its environment, and its lines (up to four, ANSI colour kept) are drawn under the composer, above the status
bar. It runs on start, after each turn, on a vim mode change and on the timer, debounced to 300 ms, in a thread
with a five second limit. `/statusline <command>` sets it, `/statusline off` removes it, `/statusline` says what
is set. Not in the payload: `cost.total_cost_usd` is 0, no rate limits, no `prompt_cache`, no PR or worktree
fields. The claude look does not run it yet.

### Footer, cursor, placeholder

`← for agents` follows `? for shortcuts` on an empty prompt, and ← opens `/agents`. The terminal cursor is hidden
and an inverse cell is painted where it is, as 2.1.314 does. The `Try "..."` example is kept: 2.1.314 fills it from
a cache that only a signed-in account refreshes, so a capture with a made-up key shows none and the one in
`tests/fixtures/ref` (a subscription) shows it.
