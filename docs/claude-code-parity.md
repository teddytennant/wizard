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

- Live turns against a fake Messages server (`looks/crates/claudew/tools/fake_anthropic.py`, with
  `ANTHROPIC_BASE_URL` and a fake `CLAUDE_CODE_OAUTH_TOKEN`, in a throwaway `HOME` and `CLAUDE_CONFIG_DIR`): the
  `stream-json` message shapes, `--include-partial-messages`, `json`, `--input-format stream-json` with
  `control_request` and `--replay-user-messages`, `--max-turns` and `--max-budget-usd` results, and the exit codes.
  Wizard's side ran against a mock OpenAI server doing the same scripts, and the two were diffed key by key
  ("Headless output" below).

Not compared:

- The text of real model answers, `--fallback-model` against a real overloaded API (Wizard's was tried against a
  mock that answers 529), and `--json-schema` against a model that calls Claude Code's structured-output tool: the
  fake server never makes that call.
- The new slash commands against the real ones, which need the interactive UI; `/loop`, `/branch` and the rest were
  checked against Claude Code's documented behaviour and Wizard's own tests.
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
| `/permissions [allow\|ask\|deny\|remove <rule>]` | The mode and what it means, plan mode, the project's trust decision, and every rule grouped by where it was written (user settings, project settings, `--allowedTools`, `config.toml`), plus the settings files read. With a rule it adds it to `~/.wizard/settings.json` (`remove` takes it out); that applies from the next session. The agent may list but not edit. |
| `/hooks` | Hooks by event (Claude's event names), each with its matcher, handler type (`command`, `http`, `prompt`, `agent`, `mcp_tool`), timeout, `async` and `if` notes and the file it came from. Read through the loader a session uses, so a project's hooks show only once it is trusted. |
| `/context` | Model, next-request size when known, session token totals, instruction size, skill, command and MCP counts. No coloured grid. |
| `/mcp [reload]` | Servers from `mcp.toml`, `.mcp.json`, plugins and `--mcp-config`, with the reason a project's `.mcp.json` is waiting (see Trust below). |
| `/output-style [name]` | Lists and switches output styles. |
| `/skills` | Skills, their source file and rough prompt cost. |
| `/release-notes` | The newest released section of the changelog embedded in the binary. |
| `/keybindings [init]` | Prints, or creates, `~/.wizard/keybindings.json`. The file is read by nothing yet; see missing. |
| `/loop [interval] <prompt>` | `/loop 5m check the deploy` writes a recurring `cron-loop-<id>` entry to `~/.wizard/schedule.toml` (every N minutes, hours or days; seconds round up to a minute; gone after 7 days; each run capped at 1 hour). With no interval it is self-paced: one run in a minute, told to call `ScheduleWakeup` for the next or stop. `/loop list`, `/loop stop [id\|all]`. Each tick is its own headless run, not a turn in this session, and `wizard scheduler` has to be running. Claude Code's `/loop` queues prompts into the live session. |
| `/branch [name]`, `/fork` | `/branch` copies the conversation to a new session file, names it, and switches to it; `/resume <old id>` comes back. A bare `/fork` is `/branch`, as in Claude Code; `/fork <task>` stays the background side quest. |
| `/rename [name]` | Names the session (a `<id>.name` file beside it). The name shows in `/resume` and works as `--resume <name>`. |
| `/cd <dir>` | Moves the process into another directory, so tools that resolve relative paths follow. Instructions, hooks and settings were read for the original project and stay it. Partial: Claude Code re-roots the whole session. |
| `/background [task]`, `/stop [id]` | `/background` copies the conversation and continues the copy in a detached `wizard --bg` run, which shows in `/dashboard`. `/stop` lists background sessions and `/stop <id>` ends one (SIGTERM to its process group). Claude Code detaches the live session itself; Wizard cannot hand a running TUI session to another process. |
| `/statusline [command\|off]` | Shows, sets or removes `statusLine` in `~/.wizard/settings.json`. Settings side only: the terminal UI draws the line. |
| `/autocompact [tokens\|off]` | Shows or sets `max_context_tokens`, the window compaction counts (it starts at 80%). `200k` and `1m` work; applies from the next session. |
| `/focus` | Alias of `/view`, which flips the compact transcript. |
| `/fast`, `/advisor`, `/color`, `/voice`, `/usage-credits`, `/passes`, `/upgrade`, `/rate-limit-options`, `/privacy-settings`, `/heapdump`, `/insights`, `/restart`, `/setup-bedrock`, `/setup-vertex` | Exist so the name answers. Each prints one sentence saying there is no counterpart and what to use instead (`/fast` points at `/effort low`, `/advisor` at `/fusion` and `/ultra`, `/voice` says there is no audio stack). They change nothing. |
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
| `/tui`, `/scroll-speed`, `/stickers`, `/radio`, `/powerup` | Terminal UI work, owned elsewhere. |
| `/sandbox` | Wizard has no OS sandbox to toggle. |
| `/chrome`, `/claude-in-chrome` | The `computer` tool covers browser driving differently. |
| `/desktop`, `/mobile` | `wizard gui` and the Android app are separate products. |
| `/teleport`, `/remote-control`, `/remote-env`, `/cloud`, `/autofix-pr`, `/ultrareview`, `/web-setup`, `/install-github-app`, `/install-slack-app` | Anthropic cloud and GitHub App features. |
| `/design*`, `/slides`, `/artifacts`, `/artifact-*`, `/dataviz`, `/deep-research`, `/claude-api`, `/batch`, `/workflows`, `/workflow-authoring`, `/plugin-authoring`, `/run`, `/run-skill-generator`, `/team-onboarding`, `/skill-doctor` | Bundled skills and workflows tied to Anthropic products; skills in `.claude/skills` still load. |
| `/fewer-permission-prompts` | Wizard has no prompts to reduce. |

`/theme` is still `/ui`: a skin carries its palette, so Claude's theme names (`dark`, `light-daltonized`, ...) do not map to anything.

## CLI flags

`wizard` takes Claude Code's flags where they mean something here. Flags that name a feature Wizard does
not have are removed from the command line with a warning on stderr, so a script written for `claude` still
starts. Nothing is dropped silently.

### Mapped

| Flag | Wizard |
|---|---|
| `-p` / `--print` | `--print` runs headless whatever the saved mode is. A bare `-p` (end of line, or another flag next) means `--print`. `-p "task"` keeps its older meaning (the task; in a terminal it opens the TUI with it). The prompt comes from `-p`, the words after the flags, and stdin. A piped stdin plus a prompt is both, stdin first. A lone word close to a subcommand name (`wizard doctr`) stops with a suggestion instead of starting a run. |
| `--output-format text\|json\|stream-json` | `json` is Claude Code's `result` object plus Wizard's keys. `stream-json` is Claude Code's lines (`system` init, `assistant` and `user` messages with `tool_use` and `tool_result` blocks, `result`), with Wizard's own event lines (`notice`, `plan`, `todo`, `task_*`, `subagent_*`, ...) between them. See "Headless output" below. |
| `--input-format text\|stream-json` | `stream-json` is a conversation: each `{"type":"user"}` line is a turn, answered with its own `init` and `result`, and `control_request` lines (`initialize`, `interrupt`, `set_model`) get a `control_response`. Needs `--output-format stream-json`, as in Claude Code. |
| `-c` / `--continue` | `--resume` (most recent session here). |
| `-r` / `--resume [id]` | `-r` is short for `--resume`. `<id>` is a session id, a unique prefix of one, or a name given with `--name`; terminal and headless runs both open it. |
| `--session-id <id>` | Starts a new session under that id (letters, digits, `-`, `_`, `.`). It fails if the id exists, and with `--resume` or `--continue` it needs `--fork-session`. |
| `-n` / `--name <name>` | Names the session; `/resume` shows it and `--resume <name>` finds it. |
| `--fork-session` | With `--resume` or `--continue`: a new session holding a copy of that conversation. |
| `--no-session-persistence` | Headless: the session files are removed when the run ends. |
| `--fallback-model <models>` | Headless: when the model is down (two "LLM unavailable" reports in a turn), fails outright or trips the circuit breaker, the turn is cut short and the task continues on the next model, same provider. Also tried if the first model does not start. Comma separated; `fallbackModel` in settings is the default. |
| `--max-budget-usd <n>` | Headless: the run is cancelled once its estimated cost (the configured rates, or the price table) passes `n`. The `result` is `error_max_budget_usd` with `terminal_reason: budget_exhausted`, and the exit code is 1. The cost is an estimate from reported tokens, not a bill. |
| `--settings <file-or-json>` | One more Claude-format settings layer, read after the project's files and before managed ones. A JSON string is written to a private temp file. |
| `--agent <name>` | The session's system prompt gains that agent's prompt (built-in `worker`, `.claude/agents`, plugins, `--agents`; Claude's `general-purpose`, `Explore` and `Plan` are the worker). The agent's `tools` and `model` are not applied. |
| `--system-prompt`, `--system-prompt-file` | Published as `WIZARD_SYSTEM_PROMPT`, the override the prompt builder already reads. |
| `--append-system-prompt`, `--append-system-prompt-file` | Added to the instructions block. File text first, as in Claude Code. |
| `--add-dir <dirs...>` | A line in the system prompt naming the directories. |
| `--model`, `--effort`, `--max-turns` | Applied to the active provider, the reasoning effort and the step budget for the run. `max` effort is `xhigh`. |
| `--mcp-config <file-or-json...>`, `--strict-mcp-config` | Merged into the MCP servers; strict drops `mcp.toml`'s. |
| `--agents <json>` | Subagents for this run. A file path (the `--print` form) is not read. |
| `--plugin-dir <dir>` | A plugin directory, or a folder of them, for this run. `.zip` is not read. |
| `--json-schema <schema>` | The system prompt asks for a final JSON answer, and headless runs check it: the last message (or the first fenced or braced JSON in it) is validated against the schema (`type`, `properties`, `required`, `enum`, `items`, bounds, `pattern`, `anyOf`/`oneOf`/`allOf`, local `$ref`), sent back for a fix up to twice, and put in the `result` as `structured_output`. If it still fails the `result` is `error_max_structured_output_retries` and the exit code is 1. Claude Code does this through a tool call; Wizard checks the text. |
| `-w` / `--worktree [name]` | `git worktree add` under `.claude/worktrees/<name>`, then works there. |
| `--bare`, `--safe-mode` | No discovery of instruction files, `.claude` commands, skills, agents, styles, plugins or `.mcp.json`. Named flags still apply. Hooks are the hooks work's to honor. |
| `--disable-slash-commands` | No custom commands and no skills. |
| `--verbose`, `-d` / `--debug[=filter]` | Sets `WIZARD_LOG` for the run (log file, not stderr). |
| `--dangerously-skip-permissions` | Accepted. Wizard never asks, so there is nothing to skip. |
| `--include-partial-messages`, `--include-hook-events`, `--replay-user-messages` | Accepted, no effect: the structured formats already carry that. |
| `[prompt]` | The words after the flags are the task. |

### Accepted and reported as unsupported

`--no-session-persistence`, `--fallback-model` and `--max-budget-usd` in a run that opens the terminal UI: they say
so on stderr, since they only mean something for a headless run. Everything else that was on this list is mapped
above.

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
| Plugins | `~/.claude/plugins/installed_plugins.json` entries enabled in `enabledPlugins`; `~/.wizard/claude-plugins/installed`; `--plugin-dir` | Commands (`plugin:cmd`), skills, agents, output styles, `.mcp.json` and hooks. A plugin's `hooks/hooks.json` (or the manifest's `hooks`, a path or an inline object) goes through the hooks engine like a Claude-format settings hook, with `${CLAUDE_PLUGIN_ROOT}` filled in; `/hooks` lists them as `plugin <name>` and `disableAllHooks` turns them off. |
| Marketplaces | `~/.wizard/claude-plugins/marketplaces`, and Claude Code's `known_marketplaces.json` | `owner/repo` and URL sources are cloned with `git`; relative, `github` and `url` plugin sources install, others are named as unsupported. |

### Trust

`.mcp.json` entries are commands to run, and the file arrives with a `git clone`. They connect only when the
project has a recorded yes in `~/.wizard/trusted_projects` (the same decision the project's hooks wait on) or
`WIZARD_TRUST_PROJECT` is set. `/mcp` lists the ones waiting. Servers from plugins and `--mcp-config` load,
since the user named them.

### Not covered

- `keybindings.json`: `/keybindings init` creates the file and nothing reads it yet. Remapping needs the TUI's
  key handling, which is another worker's code.
- `.claude/settings.json` plugin enablement for Wizard-installed plugins: those are enabled by being installed;
  `/plugin disable` turns one off.
- `!` commands inside command files and `allowed-tools`/`model` frontmatter on commands.

### Settings keys

Read from the same layers as hooks and permissions (see `src/config/claude.rs`). Honored:

| Key | What Wizard does |
|---|---|
| `permissions`, `hooks`, `env`, `disableAllHooks` | The permissions and hooks work. |
| `outputStyle` | The active style when `/output-style` has not chosen one. |
| `statusLine` | Stored and reported by `/statusline`; the terminal UI draws it. |
| `model` | Wizard-named files only, since a Claude model id means nothing to another provider. |
| `modelOverrides` | A name mapped to the model it stands for, applied to `--model` and `/model`: `{"opus": "grok-4.3"}`. |
| `availableModels` | When set, `--model` and `/model` refuse anything else. |
| `effortLevel` | The reasoning effort when `--effort` is not given. |
| `fallbackModel` | The default for `--fallback-model` in headless runs. |
| `language` | Adds "Respond in X" to the system prompt. |
| `attribution.commit`, `attribution.pr` | Text the model is told to end commit messages and pull request descriptions with; an empty string tells it to add none. Wizard adds no attribution by default, so `includeCoAuthoredBy` has nothing to switch. |
| `cleanupPeriodDays` | Sessions not written to for that many days are deleted at startup (0 means never). A session named by `--resume` is kept. |
| `apiKeyHelper` | A command run once at startup (10 second limit); its output becomes the active provider's key unless that provider's variable is already set. User and managed files only: a project cannot name a command to run. |

Not honored, and why: `sandbox.*` (no OS sandbox), `voice*` and `spinnerTips*` (no audio stack, other UI), `theme`
and `tui` (the skin carries them), `forceLogin*`, `awsAuthRefresh`, `gcpAuthRefresh`, `otelHeadersHelper` and the
managed-policy keys (`allowManaged*`, `strictKnownMarketplaces`, ...: Anthropic account and enterprise plumbing),
`autoMode*`, `fastMode*` and `advisorModel` (Anthropic model features), `worktree.*` and `plansDirectory`
(Wizard's worktree and plan locations are fixed), `includeCoAuthoredBy` (see above), `permissions.additionalDirectories`
(the permissions work). Keys not listed here are read and ignored.

### Headless output

Captured from Claude Code 2.1.314 driven against a fake Messages server and compared with Wizard against a mock
OpenAI server, by the shape of every line (`stream-json --verbose`, `--include-partial-messages`, `json`, and
`--input-format stream-json` with `--replay-user-messages`).

Matches: the `system` init keys (`cwd`, `session_id`, `tools`, `model`, `permissionMode`, `slash_commands`,
`skills`, `agents`, `plugins`, `mcp_servers`, `output_style`, `claude_code_version`, ...); `assistant` messages
with the same keys (`message.id`, `content` blocks, `usage`, `parent_tool_use_id`, `uuid`), one per content block as
in Claude Code; `user` messages carrying `tool_result` blocks with the matching `tool_use_id`, and `tool_use_result`
for `Bash`; the `stream_event` sequence (`message_start`, `content_block_start`, `content_block_delta`,
`content_block_stop`, `message_delta`, `message_stop`) and the `system` `status` line; the `result` keys
(`subtype`, `is_error`, `num_turns`, `result`, `session_id`, `total_cost_usd`, `usage`, `modelUsage`,
`permission_denials`, `terminal_reason`, `errors`, ...) with `error_max_turns`, `error_max_budget_usd` and their
`terminal_reason` and `errors` strings; the per-turn `init` and `result` of a stream-json conversation, and
`--replay-user-messages` echoes with `isReplay`.

Tool names in `tool_use` blocks and the init `tools` list are Claude's (`Bash`, `Read`, ...), through the same map the
hooks use, with `path` renamed `file_path`. Wizard's own fields stay on the `result`: `reason`, `turns`, `steps`,
`usage.prompt_tokens` and `completion_tokens`, and for `json` also `text` (every step's text, where `result` is the
last step's, as in Claude Code), `tool_calls`, `images`.

Differs: `stream-json` no longer ends with a `{"type":"done"}` line and the old `tool_call`, `tool_result`,
`text_delta`, `usage`, `step` and `turn_done` lines are gone; the `result` line replaces them. The costs are
estimates and token counts carry no cache split. Not carried: `rate_limit_event`, the timing fields (`ttft_ms`, ...),
`subagent_stats`, `fast_mode_*`, `safety_stops`, `memory_paths`, `capabilities`, `tool_result_meta`, and
`tool_use_result` for tools other than `Bash` (Claude's carry a per-tool object). `thinking` blocks have an empty
`signature`. `num_turns` counts model calls. Claude Code's `--json-schema` result has no `structured_output` when the
model never called its tool; Wizard sets it whenever the text validated.
