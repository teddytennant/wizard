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
| `AskUserQuestion` | `interview` | alias | Option labels are kept; descriptions and `header` are dropped; `multiSelect` is noted in the question text. An answered set comes back as Claude's `Your questions have been answered: "Which?"="A". You can now continue with these answers in mind.` (checked in an interactive turn against the real binary). |
| `Bash` | `execute` | alias | Output is laid out as Claude lays it out: stdout and stderr merged with no label, `Exit code N` as the first line of a failure, `(Bash completed with no output)` for silence (all three checked against the real tool, through `claude mcp serve` and a model turn). `timeout` is milliseconds, rounded up to whole seconds, capped at 600, and kills the command at that point with `Exit code 143` and `Command timed out after Ns`. Without a `timeout`, Wizard's own rule holds: past the foreground budget the command moves to a background task instead of being killed. `run_in_background` starts a task; the result names it for `task_output` and `TaskStop`, where Claude names an output file to read. A result past the output cap is cut to its head and tail with a spill file when the session has one; Claude's is a `<persisted-output>` block with the first 2KB, so the tail of a long log is the one place Wizard shows more. `description` and `dangerouslyDisableSandbox` are ignored. |
| `CronCreate`, `CronList`, `CronDelete` | none | new | Backed by `wizard scheduler`. A job is an entry named `cron-<id>` in `~/.wizard/schedule.toml` that fires a headless session in the project directory, not a prompt inside the live session. Every job is persisted, so `durable` changes nothing. Recurring jobs expire after 7 days; `recurring: false` fires once and deletes itself. Each run is capped at 1 hour. The result warns when no scheduler daemon holds the lock. See [Scheduler](scheduler.md). |
| `Edit` | `edit_file` | alias | `replace_all`, the uniqueness error and the same-string error use Claude's wording. An empty `old_string` creates a file that is missing or empty. No read-before-edit rule: the real binary did not enforce one over MCP either. |
| `EndConversation` | none | partial | Registered. It posts a notice and tells the model the conversation is over and to stop; the session stays open for you. Claude's version is a model-provider policy tool that ends the session on the server side, which Wizard has no equivalent of. |
| `EnterPlanMode` | `/plan` | new | Sets the plan-mode flag; no approval dialog. Plan mode refuses writes with `Cannot write to <path> while in plan mode` (Claude's words, checked against the real tool) and runs read-only commands such as `ls`, which the real one does too. In `-p` the real plan mode sends every other command to its classifier; Wizard refuses them. |
| `ExitPlanMode` | `exit_plan` | alias | `plan` is required (Claude Code reads its plan file instead). `allowedPrompts` is ignored. |
| `EnterWorktree`, `ExitWorktree` | none | new | Worktrees live in `.wizard/worktrees/<name>` on branch `worktree-<name>`, from `HEAD` (Claude branches from `origin` by default). The session's `cwd` is swapped for every later tool call; the system prompt still names the starting directory. `remove` refuses when there are uncommitted files or commits not on the original branch unless `discard_changes` is true. A worktree entered by `path` can be left with `keep` but never removed. |
| `Glob` | `list_files` | new | Real ripgrep `--files --glob` semantics, oldest first by mtime, capped at 100 with a truncation line, dotfiles included, `.gitignore` not applied, absolute patterns split at their static prefix. Falls back to a directory walk without `rg`. Claude Code does not offer it on Linux and macOS (its `Bash` searches instead), so there was no real binary to compare. |
| `Grep` | `search_files` | new | `output_mode`, `-A`/`-B`/`-C`/`context`, `-n`, `-i`, `type`, `glob`, `head_limit` (default 250, 0 for all), `offset`, `multiline`; numbers and booleans may arrive as strings; `file_path` is accepted for `path`. Result wording ported from the leaked source. Falls back to `grep -E` without `rg`, which cannot do `multiline` or `type`. Not offered on Linux by the real binary, so the comparison was against its `Bash` running `grep`, not `Grep`. |
| `ListAgents` | `subagent_status` | partial | Lists running background subagents of this session, and `main` for a subagent. No other local sessions, teammates, cloud or Remote Control sessions. |
| `ListMcpResourcesTool`, `ReadMcpResourceTool` | none | new | `resources/list` and `resources/read` on the connected servers, registered with the MCP tools when any server is up. Run through the real binary against a stdio test server with two resources: the list is a JSON array of `{name, uri, description, mimeType, server}`, an unknown server is the error `Server "nope" not found. Available servers: res`, a read is `{"contents":[{uri, mimeType, text}]}`, and an unknown URI (JSON-RPC -32002) is an ordinary result, `Resource not found: <uri> — it may have been deleted or the URI is stale. Re-run ListMcpResourcesTool to refresh.`; Wizard's wording matches all four, and its list keys come out in alphabetical order instead of Claude's. The real binary only offers the pair when a connected server has resources. The Wizard side was exercised by its unit tests against a scripted server, not driven through a turn. |
| `LSP` | none | new | A real client. The first call for a file starts the language server for its extension (`rust-analyzer`, `pyright-langserver`, `basedpyright-langserver`, `pylsp`, `typescript-language-server`, `gopls` or `clangd`, whichever is on `PATH`, or a server named in `.lsp.json` in Claude's plugin format, project first, then `~/.wizard/lsp.json`), waits up to 45s for it to stop reporting progress, and keeps it for the session. `goToDefinition`, `findReferences`, `hover`, `documentSymbol`, `workspaceSymbol`, `goToImplementation` and the three call-hierarchy operations take Claude's input schema and answer in the wording of the leaked formatters. Changes made on disk are sent to the server before each call. `workspaceSymbol` sends an empty query, as Claude does. With no server for the file type it says `No LSP server available for file type: .rs (install rust-analyzer or name another in .lsp.json)`. The wording was ported from source and the client was run against a scripted server. No real server was run: the only `rust-analyzer` on the build box is a rustup shim without the component, and none of the others is installed. The real binary exposes no `LSP` over `mcp serve`, so it could not be diffed. A server that dies says why, with the tail of its stderr. |
| `Monitor` | `execute` with `run_in_background` | new | A background task whose stdout lines are put into the conversation at the top of each step, batched, capped at 100 lines per batch and stopped at 5000 events. `timeout_ms` defaults to 5 minutes, capped at 30. Events arrive between steps and do not wake an idle session. The `ws` source is refused; use `websocat` in a command. `TaskStop` and `task_kill` stop it. |
| `NotebookEdit` | `edit_file` | new | Edits by cell id (or `cell-N`): replace, insert after, delete. New ids are 8 hex digits, only for nbformat 4.5 and later. Replacing a code cell clears outputs. Written with Jupyter's one-space indent. Checked against the real tool over MCP, including its error strings. |
| `PowerShell` | `execute` | partial | Registered only where `pwsh`, `powershell` or `powershell.exe` is on `PATH`, so on Windows and on a Mac or Linux box with PowerShell installed. The script is passed as `-EncodedCommand`, so no quoting is involved. Rules `PowerShell(...)` use the shell-command matcher, which splits on `;`, `&&` and `|` and does not understand PowerShell syntax beyond that. Not run on Windows here. |
| `PushNotification` | none | partial | A session notice every surface renders, plus `notify-send` or `osascript` when present. No phone push; the result says so. |
| `Read` | `read_file` | alias | `offset` is the 1-based start line, `limit` a line count. A `.ipynb` renders as `<cell id="..">` blocks. Lines are not numbered, on purpose: numbered output made models copy the gutter into `old_string`. `pages` and `allow_large` are accepted and ignored. |
| `RemoteTrigger` | none | skipped | Calls the claude.ai routines API with a claude.ai login. Wizard's cloud-side equivalent is `wizard scheduler` on a machine you run. |
| `ReportFindings` | none | partial | Validates the shape (max 32, required fields) and returns the ranked list as text. There is no typed review panel to render it in. |
| `ScheduleWakeup` | none | new | One-shot scheduler entry `wakeup-<id>` at now plus the delay (clamped to 60 to 3600 s, rounded up to a whole minute), replacing the last one for the directory. `stop: true` cancels it. Fires a headless session, not this one. |
| `SendFeedback` | none | partial | Writes the text to `~/.wizard/feedback/feedback-<time>-<pid>.md` and says nothing was sent. There is no endpoint to send it to. |
| `SendMessage` | none | partial | To `main` from a subagent, or to a running background subagent by `agent:<id>` or name. Delivered at the recipient's next step. The main agent writing to `main` gets Claude's exact answer (`{"success":false,"message":"You are the main conversation ..."}`). No teammates or other sessions. |
| `SendUserFile` | none | partial | Checks the file and shows its path and caption as a session notice. Nothing is copied to a device: Wizard has no Remote Control. |
| `ShareOnboardingGuide` | none | skipped | Uploads to claude.ai for Team and Enterprise plans. |
| `Skill` | skills in the system prompt | new | Loads a skill's body by name from the same roots, with `args` appended. Unknown names get a "did you mean". |
| `SubagentHandback` | none | partial | Registered. A subagent calls it with `report`, which becomes its result in place of its last message; the model is told to stop calling tools, and the run ends when it does. It does not force the run to stop, and there is no gate on the report as Claude has. Outside a subagent it says so. |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate` | `todo` | new | A numbered board with `blocks`/`blockedBy`, `owner` and `metadata`; every change is mirrored into the `todo` list the TUI draws. A `todo` write does not touch the board. Result wording from the leaked source (the real binary exposes none of them over MCP in 2.1.314). |
| `TaskOutput` | `task_output` | present | Claude Code removed it in 2.1.277. |
| `TaskStop` | `task_kill`, `subagent_kill` | new | Stops a shell task or Monitor by number; `agent:<n>` or a name reaches a background subagent. Error strings match the real binary. |
| `TodoWrite` | `todo` | alias | `activeForm` is dropped. |
| `ToolSearch` | `tool_search` | present | Same `select:` syntax. The deferred names are listed in the system prompt. |
| `WaitForMcpServers` | none | partial | Registered. Wizard connects every MCP server before the first turn and loads their tools once, so nothing is ever pending: it answers `ready=true` with the connected servers, or `ready=false` naming the ones that are not connected. |
| `WebFetch` | `web_fetch` | alias | Returns the page with the `prompt` placed above it for you to apply; there is no second model pass. `offset` drops that many characters. A non-URL fails with Claude's `Invalid URL`. |
| `WebSearch` | `web_search` | alias | `allowed_domains` and `blocked_domains` become `site:` and `-site:` operators in the query, so they depend on the backend honouring them. |
| `Workflow` | `run_code` | skipped | Claude Code's workflows are JavaScript that orchestrates subagents in the background, with resume across sessions, `agent()` calls and a task notification on completion. Running that needs a JavaScript runtime with Claude's workflow API, a resume store and a background runner with its own task type. Wizard's `run_code` is a sandboxed Lua program over its own tools, and `wizard fleet` fans out sessions; neither takes a Claude workflow script, and a partial runner would accept scripts it then ran differently. Not built. |
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
`Glob` and `Grep` (not shipped on Linux), `LSP`, `PushNotification` and
`SendUserFile`. The model-facing
result text of every tool is the string the leaked source's
`mapToolResultToToolResultBlockParam` builds where there is one; `mcp serve`
returns the structured output instead, so those strings could not be diffed.

Driven in a model turn instead, with a scripted fake Messages server behind the
real binary (`-p`, and interactive in a `tmux -L` session): the permission
refusals of `default`, `acceptEdits`, `plan`, `bypassPermissions` and `dontAsk`;
the model-facing text of `Bash` (failure, silence, timeout, a result past 30000
characters, `run_in_background`); `AskUserQuestion` answered in the dialog; and
`SendMessage` to `main`; and the two MCP resource tools, against a small stdio
test server (`--mcp-config`). In that setup 2.1.314 offers neither `TodoWrite`
nor the `Task*` tools (`TodoWrite is disabled for this session`), `Grep`,
`Glob` nor `EnterPlanMode`, even interactively, so they have no reference
output; Wizard's versions stand on the leaked source. A model-free
comparison of the permission dialog's own layout was not done: it needs the
terminal pixels, and the claude look owns that.

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

## Audit 2.1.314

A closing pass over everything Claude Code 2.1.314 exposes, taken from the real binary and not from the
docs: `claude --help` and every subcommand's `--help`, the slash menu scrolled out of an interactive session
(`tmux -L`, driven with `fake_anthropic.py` and a fake `CLAUDE_CODE_OAUTH_TOKEN`, so no account), the `system`
init line of `claude -p ... --output-format stream-json --verbose`, strings in the binary for settings keys
and environment variables, and real `claude mcp`, `plugin`, `auth` and `agents --json` runs for the output
shapes. Wizard's side is the built binary and its tests.

Every item has one of four statuses. **present** is the same thing under the same name. **aliased** is the same
job under a Wizard spelling or with a stated difference. **accepted-with-warning** parses (or is read) so a
script or settings file written for Claude Code keeps working, but changes nothing, and says why on stderr or
in the reply. **missing** is not built, with the reason. The tools were compared earlier ("Tools" above) and are
not repeated; the flag table here only adds the status of each flag in 2.1.314's `--help`.

### Subcommands

| Claude Code | Status | Wizard | Notes |
| :- | :- | :- | :- |
| `agents` | present | `wizard agents` | Opens the dashboard. `--json`, `--all` and `--cwd <path>` print the running sessions as a JSON array. The dispatch defaults (`--model`, `--effort`, `--permission-mode`, `--add-dir`, `--mcp-config`, `--plugin-dir`, `--settings`, `--strict-mcp-config`, `--agent`, `--restricted`, `--setting-sources`, the two skip-permissions flags) are Wizard's top-level flags, which `agents` honors when they come before it: `wizard --model m agents`, not `wizard agents --model m`. |
| `attach` | accepted-with-warning | `wizard attach <id>` | Parses and says to use `wizard agents` or `wizard --resume <id>`. A detached Wizard run has no terminal to hand over. |
| `auth login` | aliased | `wizard auth login [provider]` | Is `wizard --login <provider>`; no provider means `xai`, `--claudeai` means `claude`. `--console` is refused with the API-key instructions; `--email` and `--sso` are ignored with a warning. |
| `auth logout` | aliased | `wizard auth logout [provider]` | The `/logout` command; with no name it signs out of the one account that is signed in. |
| `auth status` | present | `wizard auth status [--json\|--text]` | JSON by default, exit 1 when nothing is signed in, as Claude Code does. Lists the oauth sign-ins and every configured provider. |
| `auto-mode` | accepted-with-warning | `wizard auto-mode ...` | Parses, says auto mode is Anthropic's permission classifier. `config`, `critique`, `defaults` and `reset` have nothing to act on. |
| `doctor` | present | `wizard doctor` | Different report, same job. |
| `gateway` | missing | `wizard gateway` is the messaging gateway | Claude Code's `gateway` runs an enterprise auth and telemetry gateway; the name is taken by a different feature here. |
| `import` | present | `wizard import [claude] [--dry-run] [--yes]` | Imports MCP servers and custom commands from Claude Code. `codex`, `gemini` and `cursor` are refused by name: there is no importer for them. |
| `install` | aliased | `wizard install [target]` | Is `wizard update`: `stable` and `latest` mean the newest release, a version means that tag, `--force` reinstalls. |
| `logs` | accepted-with-warning | `wizard logs <id>` | Parses and points at `wizard agents` and `~/.wizard/sessions`: Wizard keeps no terminal log for a background run. |
| `mcp` | present | `wizard mcp` | See the table below. |
| `plugin \| plugins` | present | `wizard plugin` | See the table below. `wizard plugins` stays the Pi package installer, so Claude Code's `plugins` spelling does not reach the Claude-format store. |
| `purge` | present | `wizard purge [path] [--all] [--dry-run] [-i] [-y]` | Deletes the conversations (and their sidecar files) recorded for a project. Claude Code also clears tasks and file history; Wizard keeps neither per project. |
| `respawn` | accepted-with-warning | `wizard respawn` | Parses and explains: a Wizard background run is an ordinary process, so a new binary is picked up the next time it starts. |
| `rm` | aliased | `wizard rm <id>` | Forgets a finished background session's record. Wizard has no worktree for it to delete. |
| `setup-token` | aliased | `wizard setup-token` | Wizard has no long-lived token of its own. When `claude` is on PATH this runs `claude setup-token`, otherwise it says how to sign in. |
| `stop \| kill` | aliased | `wizard stop <id>`, `wizard kill <id>` | The `/stop` command: SIGTERM to the session's process group. |
| `ultrareview` | accepted-with-warning | `wizard ultrareview` | Parses and points at the local `/review`. The cloud review is Anthropic's service. |
| `update \| upgrade` | present | `wizard update`, `wizard upgrade` | Same job; `upgrade` is an alias. |

`wizard mcp` and `wizard plugin` in detail. Servers added with `wizard mcp add` at the default `local` scope are kept per project in `~/.wizard/local-mcp.json`, in the shape of the `projects` map in `~/.claude.json`.

| Claude Code | Status | Notes |
| :- | :- | :- |
| `mcp add` | present | Same flags: `-s local\|user\|project`, `-t stdio\|http`, `-e KEY=v`, `-H "K: v"`. Local is `~/.wizard/local-mcp.json`, user is `~/.wizard/mcp.toml`, project is `.mcp.json` in Claude's own format. `-t sse` is refused: no SSE client. |
| `mcp add --callback-port, --client-id, --client-secret` | accepted-with-warning | MCP OAuth is not built; ignored with a warning. |
| `mcp add-json` | present | Validated like a `.mcp.json` entry. |
| `mcp add-from-claude-desktop` | present | Reads `claude_desktop_config.json` where it exists (macOS, Linux); not limited to Mac and WSL. |
| `mcp get` | present | Same layout, with a live handshake for the status. |
| `mcp list` | present | Health-checks every server at once. A `.mcp.json` server that has not been approved shows as pending, as in Claude Code. |
| `mcp remove` | present | From the scope given, or whichever holds it. |
| `mcp reset-project-choices` | present | Forgets the project's trust decision. |
| `mcp serve` | aliased | Is `wizard mcp-serve`. |
| `mcp login, mcp logout` | accepted-with-warning | Wizard sends the headers you configured and keeps no OAuth credentials, so `login` says so (exit 1) and `logout` has nothing to clear. |

| Claude Code | Status | Notes |
| :- | :- | :- |
| `plugin list` | present | `--json` supported; `--available` and `--data-size` are accepted and ignored. |
| `plugin install \| i` | present | `name@marketplace`, `owner/repo`, a git URL or a directory; `--marketplace <source>` adds it first. `--scope` is treated as user (one store). `--config`, `--registry`, `--accept-command` (npm and command sources) are not read. |
| `plugin uninstall \| remove` | present | `--keep-data` is accepted; Wizard keeps no plugin data directory. |
| `plugin enable, plugin disable [--all]` | present |  |
| `plugin update` | present | Refreshes the marketplaces and installs again from where the plugin came from. |
| `plugin validate` | present | A plugin directory or a marketplace directory; `--strict` also wants a README. `--profile` is not read. |
| `plugin details` | present | Component counts, not the projected token cost. |
| `plugin marketplace add \| list \| remove \| rm \| update` | present | `--sparse` and `--claudeai` are not read. |
| `plugin init \| new` | present | Scaffolds `./<name>` (Claude Code writes `~/.claude/skills/<name>`). |
| `plugin tag` | present | `<name>--v<version>` from the manifest; `--dry-run`, `--force`, `--push`. The marketplace-entry agreement check is not made. |
| `plugin prune \| autoremove` | present | Wizard installs no dependencies, so there is nothing to remove. |
| `plugin configure` | missing | No `userConfig` store for plugin options. |
| `plugin eval` | missing | Claude Code's plugin eval harness runs prompts against graders; there is no counterpart. |
| `plugin test` | missing | Runs a Claude Code mod's tests; mods are not Wizard plugins. |

### CLI flags in `claude --help`

| Flag | Status | Notes |
| :- | :- | :- |
| `--print` | present |  |
| `--add-dir` | present |  |
| `--agent` | present |  |
| `--agents` | present |  |
| `--allow-dangerously-skip-permissions` | accepted-with-warning | Removed with a warning: `--permission-mode bypassPermissions` is always allowed. |
| `--allowedTools` | present |  |
| `--allowed-tools` | present |  |
| `--append-system-prompt` | present |  |
| `--autocompact` | present | Mapped this session: `auto` leaves the window, `200k`/`1m` set `max_context_tokens`. |
| `--ax-screen-reader` | accepted-with-warning |  |
| `--bg` | missing | Claude Code's `--bg` starts the session detached and prints an id. Wizard's `--bg` is an internal flag for runs that `/background` already dispatched, so it does not do that; use `/background` or `wizard agents`. |
| `--background` | accepted-with-warning | Removed with a warning; see `--bg`. |
| `--resume` | present |  |
| `--bare` | present |  |
| `--settings` | present |  |
| `--system-prompt` | present |  |
| `--mcp-config` | present |  |
| `--plugin-dir` | present |  |
| `--betas` | accepted-with-warning |  |
| `--brief` | accepted-with-warning |  |
| `--chrome` | accepted-with-warning |  |
| `--cloud` | accepted-with-warning |  |
| `--continue` | present |  |
| `--dangerously-skip-permissions` | present |  |
| `--debug` | present |  |
| `--debug-file` | accepted-with-warning |  |
| `--desktop` | accepted-with-warning |  |
| `--disable-slash-commands` | present |  |
| `--disallowedTools` | present |  |
| `--disallowed-tools` | present |  |
| `--effort` | present |  |
| `--environment` | accepted-with-warning |  |
| `--exclude-dynamic-system-prompt-sections` | accepted-with-warning |  |
| `--fallback-model` | present |  |
| `--file` | accepted-with-warning |  |
| `--fork-session` | present |  |
| `--forward-subagent-text` | accepted-with-warning |  |
| `--output-format` | present |  |
| `--from-pr` | accepted-with-warning |  |
| `--help` | present |  |
| `--ide` | accepted-with-warning |  |
| `--include-hook-events` | accepted-with-warning | Accepted, no effect: `stream-json` already carries hook events. |
| `--include-partial-messages` | accepted-with-warning | Accepted, no effect. |
| `--input-format` | present |  |
| `--json-schema` | present |  |
| `--max-budget-usd` | present |  |
| `--model` | present |  |
| `--name` | present |  |
| `--no-chrome` | accepted-with-warning |  |
| `--no-session-persistence` | present |  |
| `--permission-mode` | present |  |
| `--permission-prompts` | accepted-with-warning | Removed with a warning: a headless Wizard run never waits for a prompt. |
| `--permission-prompt-tool` | accepted-with-warning | Removed with a warning. |
| `--plugin-url` | accepted-with-warning |  |
| `--prompt-suggestions` | accepted-with-warning |  |
| `--remote-control` | accepted-with-warning |  |
| `--remote-control-session-name-prefix` | accepted-with-warning |  |
| `--replay-user-messages` | accepted-with-warning | Accepted, no effect. |
| `--restricted` | accepted-with-warning |  |
| `--tools` | accepted-with-warning | Removed with a warning: the tool set is fixed, `--allowedTools` and `--disallowedTools` narrow what runs. |
| `--strict-mcp-config` | present |  |
| `--safe-mode` | present |  |
| `--session-id` | present |  |
| `--setting-sources` | accepted-with-warning |  |
| `--system-prompt-snapshot` | accepted-with-warning |  |
| `--teleport` | accepted-with-warning |  |
| `--tmux` | accepted-with-warning |  |
| `--worktree` | present |  |
| `--verbose` | present |  |
| `--version` | present | `-v` is rewritten to `--version` (Wizard's own short form is `-V`). |

The mapped flags are described under "CLI flags" above; the removed-with-a-warning ones are listed there too.

### Slash menu

The 98 names below are the menu of an interactive 2.1.314 session plus the commands its `system` init line lists. `/help` is generated from the same table, so it adds nothing.

| Claude Code | Status | Wizard | Notes |
| :- | :- | :- | :- |
| `/add-dir` | present | `/add-dir` | Wizard's own prompt. |
| `/autocompact` | present | `/autocompact` |  |
| `/autofix-pr` | accepted-with-warning | `/autofix-pr` | Answers with one sentence saying why there is no counterpart. |
| `/background` | present | `/background` |  |
| `/branch` | present | `/branch` |  |
| `/btw` | present | `/btw` |  |
| `/bug` | aliased | `/feedback` |  |
| `/cd` | present | `/cd` |  |
| `/chrome` | accepted-with-warning | `/chrome` | Answers with one sentence saying why there is no counterpart. |
| `/clear` | present | `/clear` |  |
| `/color` | accepted-with-warning | `/color` | Answers with one sentence saying why there is no counterpart. |
| `/compact` | present | `/compact` |  |
| `/config` | aliased | `/settings` |  |
| `/context` | present | `/context` |  |
| `/copy` | present | `/copy` |  |
| `/design-login` | accepted-with-warning | `/design-login` | Answers with one sentence saying why there is no counterpart. |
| `/diff` | present | `/diff` |  |
| `/effort` | present | `/effort` |  |
| `/exit` | present | `/exit` |  |
| `/export` | present | `/export` |  |
| `/fast` | accepted-with-warning | `/fast` | Answers with one sentence saying why there is no counterpart. |
| `/feedback` | present | `/feedback` |  |
| `/focus` | aliased | `/view` |  |
| `/fork` | present | `/fork` |  |
| `/goal` | present | `/goal` |  |
| `/help` | present | `/help` |  |
| `/hooks` | present | `/hooks` |  |
| `/ide` | present | `/ide` |  |
| `/install-github-app` | accepted-with-warning | `/install-github-app` | Answers with one sentence saying why there is no counterpart. |
| `/install-slack-app` | accepted-with-warning | `/install-slack-app` | Answers with one sentence saying why there is no counterpart. |
| `/keybindings` | present | `/keybindings` |  |
| `/list-agents` | aliased | `/agents` |  |
| `/login` | present | `/login` |  |
| `/logout` | present | `/logout` |  |
| `/mcp` | present | `/mcp` |  |
| `/memory` | present | `/memory` |  |
| `/mobile` | accepted-with-warning | `/mobile` | Answers with one sentence saying why there is no counterpart. |
| `/model` | present | `/model` |  |
| `/output-style` | present | `/output-style` |  |
| `/permissions` | present | `/permissions` |  |
| `/plan` | present | `/plan` |  |
| `/plugin` | present | `/plugin` |  |
| `/powerup` | accepted-with-warning | `/powerup` | Answers with one sentence saying why there is no counterpart. |
| `/radio` | accepted-with-warning | `/radio` | Answers with one sentence saying why there is no counterpart. |
| `/rate-limit-options` | accepted-with-warning | `/rate-limit-options` | Answers with one sentence saying why there is no counterpart. |
| `/recap` | present | `/recap` | Wizard's own prompt. |
| `/release-notes` | present | `/release-notes` |  |
| `/reload-plugins` | aliased | `/reload` |  |
| `/reload-skills` | aliased | `/reload` |  |
| `/remote-env` | accepted-with-warning | `/remote-env` | Answers with one sentence saying why there is no counterpart. |
| `/rename` | present | `/rename` |  |
| `/resume` | present | `/resume` |  |
| `/rewind` | present | `/rewind` |  |
| `/sandbox` | accepted-with-warning | `/sandbox` | Answers with one sentence saying why there is no counterpart. |
| `/skills` | present | `/skills` |  |
| `/status` | present | `/status` |  |
| `/stickers` | accepted-with-warning | `/stickers` | Answers with one sentence saying why there is no counterpart. |
| `/subtask` | aliased | `/fork` |  |
| `/tasks` | aliased | `/bashes` |  |
| `/teleport` | accepted-with-warning | `/teleport` | Answers with one sentence saying why there is no counterpart. |
| `/terminal-setup` | present | `/terminal-setup` |  |
| `/theme` | aliased | `/ui` |  |
| `/tui` | accepted-with-warning | `/tui` | Answers with one sentence saying why there is no counterpart. |
| `/upgrade` | accepted-with-warning | `/upgrade` | Answers with one sentence saying why there is no counterpart. |
| `/usage` | present | `/usage` |  |
| `/voice` | accepted-with-warning | `/voice` | Answers with one sentence saying why there is no counterpart. |
| `/web-setup` | accepted-with-warning | `/web-setup` | Answers with one sentence saying why there is no counterpart. |
| `/workflows` | accepted-with-warning | `/workflows` | Answers with one sentence saying why there is no counterpart. |
| `/batch` | accepted-with-warning | `/batch` | Answers with one sentence saying why there is no counterpart. |
| `/claude-api` | accepted-with-warning | `/claude-api` | Answers with one sentence saying why there is no counterpart. |
| `/code-review` | present | `/code-review` | Wizard's own prompt. |
| `/dataviz` | accepted-with-warning | `/dataviz` | Answers with one sentence saying why there is no counterpart. |
| `/debug` | present | `/debug` | Wizard's own prompt. |
| `/deep-research` | accepted-with-warning | `/deep-research` | Answers with one sentence saying why there is no counterpart. |
| `/design` | accepted-with-warning | `/design` | Answers with one sentence saying why there is no counterpart. |
| `/design-sync` | accepted-with-warning | `/design-sync` | Answers with one sentence saying why there is no counterpart. |
| `/doctor` | present | `/doctor` |  |
| `/fewer-permission-prompts` | present | `/fewer-permission-prompts` | Wizard's own prompt. |
| `/init` | present | `/init` | Wizard's own prompt. |
| `/insights` | accepted-with-warning | `/insights` | Answers with one sentence saying why there is no counterpart. |
| `/loop` | present | `/loop` |  |
| `/plugin-authoring` | accepted-with-warning | `/plugin-authoring` | Answers with one sentence saying why there is no counterpart. |
| `/run` | present | `/run` | Wizard's own prompt. |
| `/run-skill-generator` | present | `/run-skill-generator` | Wizard's own prompt. |
| `/schedule` | present | `/schedule` |  |
| `/security-review` | present | `/security-review` | Wizard's own prompt. |
| `/simplify` | present | `/simplify` | Wizard's own prompt. |
| `/statusline` | present | `/statusline` |  |
| `/team-onboarding` | present | `/team-onboarding` | Wizard's own prompt. |
| `/update-config` | present | `/update-config` | Wizard's own prompt. |
| `/verify` | present | `/verify` | Wizard's own prompt. |
| `/workflow-authoring` | accepted-with-warning | `/workflow-authoring` | Answers with one sentence saying why there is no counterpart. |
| `/auto-mode-setup` | accepted-with-warning | `/auto-mode-setup` | Answers with one sentence saying why there is no counterpart. |
| `/__remote-workflow` | missing |  | internal plumbing of the Workflow runtime |
| `/workflow-launch-exec` | missing |  | internal plumbing of the Workflow runtime |
| `/design-consent` | accepted-with-warning | `/design-consent` | Answers with one sentence saying why there is no counterpart. |
| `/design-revoke` | accepted-with-warning | `/design-revoke` | Answers with one sentence saying why there is no counterpart. |
| `/agents` | present | `/agents` |  |

### Settings keys

Every key below was checked to exist in 2.1.314's binary. Wizard reads the keys marked present; "accepted" means a file that has the key still loads, and Wizard does nothing with it.

| Key | Status | Notes |
| :- | :- | :- |
| `permissions` | present | Read and applied |
| `additionalDirectories` | present | Permissions work: parsed there. |
| `allow` | present | Read and applied |
| `deny` | present | Read and applied |
| `defaultMode` | present | Read and applied |
| `hooks` | present | Read and applied |
| `disableAllHooks` | present | Read and applied |
| `env` | present | Read and applied |
| `model` | present | Read and applied |
| `availableModels` | present | Read and applied |
| `modelOverrides` | present | Read and applied |
| `effortLevel` | present | Read and applied |
| `outputStyle` | present | Read and applied |
| `language` | present | Read and applied |
| `statusLine` | present | Read and applied |
| `apiKeyHelper` | present | Read and applied |
| `cleanupPeriodDays` | present | Read and applied |
| `attribution` | present | Read and applied |
| `fallbackModel` | present | Read and applied |
| `editorMode` | present | Read and applied |
| `enabledPlugins` | present | Read and applied |
| `enableAllProjectMcpServers` | present | Read and applied |
| `enabledMcpjsonServers` | present | Read and applied |
| `disabledMcpjsonServers` | present | Read and applied |
| `spinnerVerbs` | aliased | `wizard import` / `/import` copy them into `ui.spinner_verbs`. |
| `includeCoAuthoredBy` | aliased | Wizard adds no attribution by default; `attribution` sets the text. |
| `autoUpdates` | aliased | `DISABLE_AUTOUPDATER` and `[update]` in `config.toml`. |
| `forceLoginMethod` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `forceLoginOrgUUID` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `allowManagedHooksOnly` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `allowManagedPermissionRulesOnly` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `allowManagedMcpServersOnly` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `allowedHttpHookUrls` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `httpHookAllowedEnvVars` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `strictKnownMarketplaces` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `blockedMarketplaces` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `extraKnownMarketplaces` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `pluginSuggestionMarketplaces` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `pluginTrustMessage` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `forceRemoteSettingsRefresh` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `companyAnnouncements` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `requiredMinimumVersion` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `minimumVersion` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `wslInheritsWindowsSettings` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `parentSettingsBehavior` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `policyHelper` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `xaaIdp` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `gatewayInternalNetworks` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `attributionProjectPolicy` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `awsAuthRefresh` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `awsCredentialExport` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `gcpAuthRefresh` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `awsPairs` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `otelHeadersHelper` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `allowedMcpServers` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `deniedMcpServers` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `channelsEnabled` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `allowedChannelPlugins` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `crossSessionInbound` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `remoteControl` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `autoUploadSessions` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `desktopSessionCleanupPeriodDays` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `disableDeepLinkRegistration` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `disableTelemetry` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `modelPricing` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `feedbackSurveyRate` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `autoUpdatesChannel` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `hasCompletedOnboarding` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `lspRecommendationDisabled` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `skipWebFetchPreflight` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `claudeMd` | accepted-with-warning | Read and ignored, because the feature is Anthropic's account, enterprise or cloud plumbing |
| `theme` | accepted-with-warning | `/ui` and the skin carry it. |
| `tui` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `viewMode` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `spinnerTipsEnabled` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `spinnerTipsOverride` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `terminalProgressBarEnabled` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `prefersReducedMotion` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `fileSuggestion` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `respectGitignore` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `showThinkingSummaries` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `showTurnDuration` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `showClearContextOnPlanAccept` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `copyFullResponse` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `copyOnSelect` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `diffTool` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `notifChannel` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `preferredNotifChannel` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `voiceEnabled` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `voice` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `screenReader` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `teammateMode` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `autoConnectIde` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `autoInstallIdeExtension` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `verbose` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `modelPicker` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `subagentStatusLine` | accepted-with-warning | Read and ignored, because the interface belongs to the skin and the terminal UI |
| `sandbox` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `fastMode` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `fastModePerSessionOptIn` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `advisorModel` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `autoMode` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `useAutoModeDuringPlan` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `disableAutoMode` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `workflows` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `disableWorkflows` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `skillOverrides` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `disableSkillShellExecution` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `worktree` | accepted-with-warning | Worktrees are fixed at `.claude/worktrees` and `.wizard/worktrees`. |
| `plansDirectory` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `defaultShell` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `agent` | accepted-with-warning | Use `--agent`. Not read from settings. |
| `claudeMdExcludes` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `autoMemoryDirectory` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `autoMemoryEnabled` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `includeGitInstructions` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `autoCompactEnabled` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `alwaysThinkingEnabled` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `thinkingBudget` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `maxThinkingTokens` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `skipDangerousModePermissionPrompt` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `keybindings` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `skills` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `commands` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `agents` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |
| `mcpServers` | accepted-with-warning | Read and ignored, because Wizard has no such feature (or fixes it) |

`.mcp.json` servers load when the project is trusted, and also when the user's own settings approve them, which is how Claude Code records the choice: `enableAllProjectMcpServers`, or the name in `enabledMcpjsonServers`, read from `~/.claude/settings.json` and the project's `.claude/settings.local.json` only. The shared `.claude/settings.json` arrives with a clone, so it cannot approve its own servers. A name in `disabledMcpjsonServers` never connects. `${VAR}` and `${VAR:-default}` in a server's command, args, env, url and headers expand from the environment, as in Claude Code.

### Environment variables

The binary reads about 400 names through `process.env` and carries over 800 `CLAUDE_CODE_*` strings; most are Anthropic's own feature gates, test hooks and transport. The ones with a meaning for a user are listed by name; the rest are grouped by family. All 419 names the binary reads through `process.env` were matched against these rows when the table was built, and none was left over; the `CLAUDE_CODE_*` strings that are not read that way fall under the last row.

| Name | Status | Notes |
| :- | :- | :- |
| ANTHROPIC_API_KEY | present | The `anthropic` provider's key (existing). |
| ANTHROPIC_AUTH_TOKEN | present | Existing. |
| ANTHROPIC_BASE_URL | present | The `anthropic` provider's endpoint, when it is the active provider. |
| ANTHROPIC_MODEL | present | The model of an active `anthropic` provider; `--model` wins. |
| ANTHROPIC_DEFAULT_OPUS_MODEL, ANTHROPIC_DEFAULT_SONNET_MODEL, ANTHROPIC_DEFAULT_HAIKU_MODEL, ANTHROPIC_DEFAULT_FABLE_MODEL, ANTHROPIC_SMALL_FAST_MODEL | present | What `--model opus` (and so on) stands for, like `modelOverrides`; `modelOverrides` does not beat them. |
| CLAUDE_CONFIG_DIR | present | The `.claude` directory, and where `.claude.json` lives. |
| CLAUDE_CODE_SIMPLE | present | Same as `--bare`. |
| CLAUDE_CODE_SAFE_MODE | present | Same as `--safe-mode`. |
| CLAUDE_CODE_PLUGIN_DIRS | present | Same as `--plugin-dir`, when the flag is not given. |
| CLAUDE_CODE_EFFORT_LEVEL | present | The reasoning effort when `--effort` is not given; beats `effortLevel`. |
| MAX_THINKING_TOKENS | aliased | Wizard has levels, not a token budget: up to 4096 is `low`, 16384 `medium`, 32768 `high`, more `xhigh`. Used only when no effort is set elsewhere. |
| CLAUDE_CODE_MAX_TURNS | present | The step budget when `--max-turns` is not given. |
| BASH_DEFAULT_TIMEOUT_MS | aliased | `shell.timeout_secs`: how long `execute` waits in the foreground before the command becomes a background task. |
| CLAUDE_CODE_SHELL | present | `shell.program`. |
| CLAUDE_CODE_AUTO_COMPACT_WINDOW | present | `max_context_tokens`. |
| CLAUDE_AUTOCOMPACT_PCT_OVERRIDE | aliased | Moves the point compaction starts (80% of the window by default) by scaling `max_context_tokens` by `pct / 80`. |
| DISABLE_AUTOUPDATER, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC | present | Turn off the startup update check and notice (`update.auto`, `update.notify`). |
| CLAUDE_CODE_DISABLE_CLAUDE_MDS | present | Ignore `CLAUDE.md`, `CLAUDE.local.md` and the rules folders; `WIZARD.md` and `AGENTS.md` still load. |
| CLAUDE_CODE_TMPDIR, CLAUDE_TMPDIR | aliased | Become `TMPDIR` for the process and the commands it starts, unless `TMPDIR` is set. |
| DISABLE_LOGIN_COMMAND, DISABLE_LOGOUT_COMMAND, DISABLE_UPGRADE_COMMAND, DISABLE_FEEDBACK_COMMAND, DISABLE_BUG_COMMAND, DISABLE_DOCTOR_COMMAND, DISABLE_EXTRA_USAGE_COMMAND, DISABLE_INSTALL_GITHUB_APP_COMMAND | present | The slash command answers "turned off by NAME" and does nothing. |
| DISABLE_TELEMETRY, DISABLE_ERROR_REPORTING, DISABLE_COST_WARNINGS, DISABLE_GROWTHBOOK, CLAUDE_CODE_ENABLE_TELEMETRY, ENABLE_ENHANCED_TELEMETRY_BETA, OTEL_* | present | Nothing to turn off: Wizard sends no telemetry or error reports. |
| HTTP_PROXY, HTTPS_PROXY, NO_PROXY | present | The HTTP client reads them. |
| CLAUDE_PROJECT_DIR, CLAUDE_PLUGIN_ROOT | present | Set for hooks and plugin commands (existing). |
| CLAUDE_CODE_CHILD_SESSION, MCP_TOOL_TIMEOUT | present | Set for the `claude` provider and the status line (existing). |
| ANTHROPIC_CUSTOM_HEADERS, CLAUDE_CODE_EXTRA_BODY, CLAUDE_CODE_EXTRA_METADATA | missing | No per-provider extra headers or body. |
| ANTHROPIC_BEDROCK_*, ANTHROPIC_VERTEX_*, ANTHROPIC_FOUNDRY_*, ANTHROPIC_AWS_*, ANTHROPIC_GOOGLE_*, CLAUDE_CODE_USE_BEDROCK / VERTEX / FOUNDRY / MANTLE / GATEWAY / ANTHROPIC_AWS / ANTHROPIC_GOOGLE_CLOUD, CLAUDE_CODE_SKIP_BEDROCK_AUTH / VERTEX_AUTH, AWS_*, AZURE_*, GOOGLE_*, CLOUDSDK_*, CLOUD_ML_REGION, VERTEX_REGION_* | missing | Bedrock, Vertex and Foundry are not providers here; `/provider` adds one that is. |
| DISABLE_COMPACT, DISABLE_AUTO_COMPACT | missing | Compaction has no off switch; `CLAUDE_CODE_AUTO_COMPACT_WINDOW` moves where it starts. |
| BASH_MAX_TIMEOUT_MS, BASH_MAX_OUTPUT_LENGTH, MAX_MCP_OUTPUT_TOKENS, MCP_TIMEOUT, MCP_TOOL_TIMEOUT (for MCP calls), CLAUDE_CODE_MAX_OUTPUT_TOKENS, CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS, API_TIMEOUT_MS, CLAUDE_STREAM_IDLE_TIMEOUT_MS, CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS, CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS, CLAUDE_SLOW_FIRST_BYTE_MS, API_FORCE_IDLE_TIMEOUT, CLAUDE_CODE_MAX_RETRIES | accepted-with-warning | Wizard's limits are fixed constants or come from `config.toml`; none reads these names. |
| CLAUDE_CODE_DISABLE_AUTO_MEMORY, CLAUDE_CODE_DISABLE_BACKGROUND_TASKS, CLAUDE_CODE_DISABLE_CRON, CLAUDE_CODE_DISABLE_FAST_MODE, CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY, CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING, CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS, CLAUDE_CODE_DISABLE_TERMINAL_TITLE, CLAUDE_CODE_DISABLE_1M_CONTEXT, CLAUDE_CODE_DISABLE_BUNDLED_SKILLS, CLAUDE_CODE_DISABLE_WORKFLOWS, CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS, DISABLE_PROMPT_CACHING*, DISABLE_INTERLEAVED_THINKING, DISABLE_COLORS, ENABLE_PROMPT_CACHING_1H*, ENABLE_TOOL_SEARCH, ENABLE_LSP_TOOL, DISABLE_INSTALLATION_CHECKS, FORCE_CODE_TERMINAL, CLAUDE_AFTER_LAST_COMPACT, ANTHROPIC_DEFAULT_*_MODEL_NAME, ANTHROPIC_DEFAULT_*_MODEL_DESCRIPTION, ANTHROPIC_CUSTOM_MODEL_OPTION*, CLOUD_RUN_JOB, ENABLE_CLAUDEAI_MCP_SERVERS, CLAUDE_CODE_NO_FLICKER, CLAUDE_CODE_USE_POWERSHELL_TOOL, CLAUDE_CODE_GLOB_HIDDEN, CLAUDE_CODE_GLOB_NO_IGNORE, CLAUDE_CODE_SUBAGENT_MODEL, CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION, CLAUDE_CODE_RESTRICTED, CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR, CLAUDE_ENV_FILE, SLASH_COMMAND_TOOL_CHAR_BUDGET, USE_BUILTIN_RIPGREP, IS_SANDBOX, FORCE_COLOR, FORCE_HYPERLINK | accepted-with-warning | Read by nothing. The behavior is either not a Wizard feature, already how Wizard works, or set in `config.toml` or by a flag; `--disallowedTools` removes a tool. |
| CLAUDE_CODE_OAUTH_TOKEN, CLAUDE_CODE_OAUTH_REFRESH_TOKEN, CLAUDE_CODE_OAUTH_*, CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR, CLAUDE_CODE_ACCOUNT_UUID, CLAUDE_CODE_ORGANIZATION_UUID, CLAUDE_CODE_SUBSCRIPTION_TYPE, CLAUDE_CODE_RATE_LIMIT_TIER, CLAUDE_CODE_CLIENT_CERT / CLIENT_KEY / CERT_STORE, ANTHROPIC_ORGANIZATION_ID, ANTHROPIC_WORKSPACE_ID, ANTHROPIC_PROFILE, ANTHROPIC_CONFIG_DIR, ANTHROPIC_UNIX_SOCKET, ANTHROPIC_FEDERATION_RULE_ID, MCP_CLIENT_SECRET, MCP_OAUTH_*, CLAUDE_TRUSTED_DEVICE_TOKEN, CLAUDE_SECURESTORAGE_CONFIG_DIR, CLAUDE_LOCAL_OAUTH_*, USE_LOCAL_OAUTH, USE_STAGING_OAUTH, CLAUDE_AI_* | accepted-with-warning | Anthropic account and OAuth plumbing. The `claude` provider runs the real Claude Code, which reads them itself; Wizard passes the environment through. |
| CLAUDE_REMOTE_*, CLAUDE_CODE_REMOTE*, CLAUDE_CODE_SESSION_*, CLAUDE_CODE_BRIDGE_*, CLAUDE_BRIDGE_*, CLAUDE_BG_*, CLAUDE_PTY_*, CLAUDE_RUNNER_*, CLAUDE_SSH_*, CCR_*, CLAUDE_CODE_CONTAINER_ID, CLAUDE_CODE_ENVIRONMENT_*, CLAUDE_CODE_SPAWN*, CLAUDE_CODE_MESSAGING_*, CLAUDE_CODE_HOST_*, CLAUDE_CODE_WORKER_EPOCH, CLAUDE_CODE_SUPERVISED, CLAUDE_AGENT_SDK_*, CLAUDE_CODE_ENTRYPOINT, CLAUDE_CODE_ACTION, CLAUDE_AGENTS_SELECT, CLAUDE_JOB_DIR, CLAUDE_SESSION_INGRESS_TOKEN_FILE, CLAUDE_STAGE_FILE_ROOT, CLAUDE_COWORK_*, CLAUDE_CODE_IS_COWORK | missing | Claude Code's cloud sessions, remote control, background-session transport and Agent SDK host handshake. Wizard has no counterpart to hand off to. |
| CLAUDE_CODE_COORDINATOR_*, CLAUDE_CODE_ENABLE_TASKS, CLAUDE_CODE_TASK_LIST_ID, CLAUDE_CODE_ENABLE_AWAY_SUMMARY, CLAUDE_CODE_RESUME_INTERRUPTED_TURN, CLAUDE_MEMORY_STORES, CLAUDE_CODE_AUTO_MODE_*, CLAUDE_CODE_CLASSIFIER_SUMMARY, CLAUDE_CODE_ENABLE_CFC, CLAUDE_CHROME_*, CLAUDE_IN_CHROME*, CLAUDE_AX_*, CLAUDE_AX_SCREEN_READER | missing | Coordinator mode, task lists shared across sessions, memory stores, auto mode, Chrome integration and the screen-reader mode are Claude Code features Wizard does not have. |
| CLAUDE_CODE_TEST_*, CLAUDE_TEST_*, CLAUDE_INTERNAL_*, CLAUDE_CODE_CONFIG_PROBE, CLAUDE_CODE_COUNT_GARBAGE_COLLECTIONS, CLAUDE_CODE_SCRIPT_CAPS, CLAUDE_CODE_SDK_READS_STARTUP_MARKS, CLAUDE_FORCE_DISPLAY_SURVEY, CLAUDE_CODE_JIT_POLICY_AFTER_STARTUP, CLAUDE_CODE_STARTUP_FAILURE_RESULTS, ENABLE_STUB_TOOLS, FORCE_VCR, FORCE_REFUSED_SENTENCE and every other `CLAUDE_CODE_*` name not listed above | accepted-with-warning | Test hooks, feature gates and diagnostics inside Claude Code's own build. No behavior to match. |

### Bundled skills, agents and plugins

| Item | Status | Notes |
| :- | :- | :- |
| skill `update-config` | present | bundled prompt `/update-config` |
| skill `verify` | present | bundled prompt `/verify` |
| skill `debug` | present | bundled prompt `/debug` |
| skill `code-review` | present | `/review`, `/code-review` |
| skill `simplify` | present | bundled prompt `/simplify` |
| skill `doctor` | present | `/doctor` |
| skill `loop` | present | `/loop` |
| skill `schedule` | present | `/schedule`, `wizard schedule` |
| skill `fewer-permission-prompts` | present | bundled prompt, reads `~/.wizard/sessions` |
| skill `run` | present | bundled prompt |
| skill `run-skill-generator` | present | bundled prompt, writes `.wizard/skills/run` |
| skill `batch` | missing | cloud agents that each open a PR; `wizard fleet` is the local version |
| skill `deep-research` | missing | a Workflow script; `/ultra` fans a question out over lenses |
| skill `design` | missing | claude.ai Design product |
| skill `design-sync` | missing | claude.ai Design product |
| skill `dataviz` | missing | reference text only; a `.wizard/skills` entry of your own loads like any other |
| skill `claude-api` | missing | reference text only; same |
| skill `workflow-authoring` | missing | no Workflow runtime |
| skill `plugin-authoring` | missing | Claude Code mods; Wizard plugins are written per `docs/plugins.md` |
| agent `general-purpose`, `Explore`, `Plan` | aliased | run the built-in `worker` |
| agent `claude` | aliased | the main agent |
| agent `statusline-setup` | missing | `/statusline` sets the setting directly |
| plugin `cc-plugin-agents-md` | present | Wizard reads `AGENTS.md` natively |
| plugin `cc-plugin-telemetry` | present | nothing to enable: Wizard sends no telemetry |
| plugin `cc-plugin-plugin-authoring` | missing | see skill `plugin-authoring` |
| output styles `default`, `explanatory`, `learning` | present | `/output-style` |

### Counts

| Area | present | aliased | accepted-with-warning | missing | items |
| :- | :- | :- | :- | :- | :- |
| subcommands | 8 | 6 | 5 | 1 | 20 |
| mcp verbs | 7 | 1 | 2 | 0 | 10 |
| plugin verbs | 11 | 0 | 0 | 3 | 14 |
| flags | 38 | 0 | 31 | 1 | 70 |
| slash | 55 | 9 | 32 | 2 | 98 |
| settings | 24 | 3 | 99 | 0 | 126 |
| env rows | 20 | 4 | 4 | 5 | 33 |
| skills | 11 | 0 | 0 | 8 | 19 |
| agents and plugins | 3 | 2 | 0 | 2 | 7 |
| **all** | 177 | 25 | 173 | 22 | 397 |

### Switching from `~/.claude`

Checked by running the built `wizard` in an isolated `HOME` that holds a realistic Claude Code setup: a
`settings.json` with permissions, a hook, `env`, `model`, `outputStyle`, `enableAllProjectMcpServers` and a
`statusLine`; `CLAUDE.md` and a rules file; `commands/hi.md` and `commands/team/deploy.md`; a skill; an agent;
a `~/.claude.json` with a user server and a local server for the project; a project `.mcp.json` using
`${VAR:-default}`; and two transcripts written by the real `claude` against the fake server.

| What | Result |
| :- | :- |
| Skills, commands, agents | `/skills` shows `hello`, the command count includes `hi`, `team:deploy` and the project's own, `/agents` offers `rev`. |
| Permissions, hooks | `/permissions` shows the allow, ask and deny rules and the `acceptEdits` default mode; `/hooks` shows the `PreToolUse` hook, and a headless run fired it. |
| Instructions | `CLAUDE.md`, `CLAUDE.local.md`, `.claude/rules` and `~/.claude/rules` are in the system prompt (`/context` reports the size). |
| MCP servers | **Fixed.** Servers in `~/.claude.json` (user scope, and the project's local scope) were not read at all; they load now, labeled `claude.json` in `/mcp`. The project's `.mcp.json` server connects because `enableAllProjectMcpServers` approves it, and `${VAR:-default}` expands. |
| `wizard resume --claude --list` | Lists both transcripts with their message counts and branch points. It already worked. |
| `/resume` in the terminal UI | **Fixed.** It listed only Wizard's own sessions, so a Claude Code user saw "no past sessions to resume". Claude's sessions for the directory now follow Wizard's in the same picker, marked `claude`, and choosing one imports a copy as `/resume-claude` does. |
| `wizard --resume` with no Wizard session here | **Fixed.** It started a fresh session without a word. It now prints the Claude sessions recorded for the directory, and the `wizard resume --claude` command that continues one. |
| `CLAUDE_CONFIG_DIR` | **Fixed** for `.claude.json` and the importer, which looked in the home directory regardless. |
| Not picked up | `wizard doctor` still checks only `mcp.toml`'s servers, not the ones from `.mcp.json` or `~/.claude.json`. Hook and permission *project* files behave as before: a project's executable surface waits for trust. |

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
| `/keybindings [init]` | Prints, or creates, `~/.wizard/keybindings.json`. The file is read; see [Keys and the prompt](#keys-and-the-prompt). |
| `/loop [interval] <prompt>` | `/loop 5m check the deploy` writes a recurring `cron-loop-<id>` entry to `~/.wizard/schedule.toml` (every N minutes, hours or days; seconds round up to a minute; gone after 7 days; each run capped at 1 hour). With no interval it is self-paced: one run in a minute, told to call `ScheduleWakeup` for the next or stop. `/loop list`, `/loop stop [id\|all]`. Each tick is its own headless run, not a turn in this session, and `wizard scheduler` has to be running. Claude Code's `/loop` queues prompts into the live session. |
| `/branch [name]`, `/fork` | `/branch` copies the conversation to a new session file, names it, and switches to it; `/resume <old id>` comes back. A bare `/fork` is `/branch`, as in Claude Code; `/fork <task>` stays the background side quest. |
| `/rename [name]` | Names the session (a `<id>.name` file beside it). The name shows in `/resume` and works as `--resume <name>`. |
| `/cd <dir>` | Moves the process into another directory, so tools that resolve relative paths follow. Instructions, hooks and settings were read for the original project and stay it. Partial: Claude Code re-roots the whole session. |
| `/background [task]`, `/stop [id]` | `/background` copies the conversation and continues the copy in a detached `wizard --bg` run, which shows in `/dashboard`. `/stop` lists background sessions and `/stop <id>` ends one (SIGTERM to its process group). Claude Code detaches the live session itself; Wizard cannot hand a running TUI session to another process. |
| `/statusline [command\|off]` | Shows, sets or removes `statusLine` in `~/.wizard/settings.json`. Settings side only: the terminal UI draws the line. |
| `/autocompact [tokens\|off]` | Shows or sets `max_context_tokens`, the window compaction counts (it starts at 80%). `200k` and `1m` work; applies from the next session. |
| `/focus` | Alias of `/view`, which flips the compact transcript. |
| `/fast`, `/advisor`, `/color`, `/voice`, `/usage-credits`, `/passes`, `/upgrade`, `/rate-limit-options`, `/privacy-settings`, `/heapdump`, `/insights`, `/restart`, `/setup-bedrock`, `/setup-vertex`, `/autofix-pr`, `/chrome`, `/design`, `/design-sync`, `/design-login`, `/design-consent`, `/design-revoke`, `/install-github-app`, `/install-slack-app`, `/mobile`, `/powerup`, `/radio`, `/remote-env`, `/sandbox`, `/stickers`, `/teleport`, `/tui`, `/web-setup`, `/workflows`, `/workflow-authoring`, `/batch`, `/claude-api`, `/dataviz`, `/deep-research`, `/plugin-authoring`, `/auto-mode-setup` | Exist so the name answers. Each prints one sentence saying there is no counterpart and what to use instead (`/fast` points at `/effort low`, `/advisor` at `/fusion` and `/ultra`, `/voice` says there is no audio stack, `/batch` at `wizard fleet`, `/tui` at `/ui`). They change nothing. |
| `DISABLE_LOGIN_COMMAND` and the other `DISABLE_*_COMMAND` variables | `/login`, `/logout`, `/upgrade`, `/feedback` (and `/bug`), `/doctor`, `/usage-credits` and `/install-github-app` answer "turned off by NAME" when their variable is set. |
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
`/update-config`, `/run` (launch the app and show the change working), `/run-skill-generator` (write a
`.wizard/skills/run` skill), `/team-onboarding` (write `docs/onboarding.md`) and `/fewer-permission-prompts`
(read `~/.wizard/sessions`, add the read-only commands that keep getting approved to
`.claude/settings.local.json`).

These are Wizard's own prompts, not Claude Code's text. `/add-dir` asks the model to treat the
directory as part of the task; it does not widen a file-access sandbox, because Wizard has none to widen.

### Missing

Of the 98 names in 2.1.314's menu, two have no row to answer them: `/__remote-workflow` and `/workflow-launch-exec`,
which are plumbing of the Workflow runtime and not typed by anyone. Names an older spec lists that the 2.1.314
menu no longer shows (`/scroll-speed`, `/claude-in-chrome`, `/desktop`, `/cloud`, `/remote-control`, `/slides`,
`/artifacts`, `/artifact-*`, `/skill-doctor`, `/ultrareview`) are not implemented; `wizard ultrareview` answers
as the subcommand. The Workflow runtime, claude.ai-only tools and voice stay out for the reasons in the audit
table above.

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
| `--dangerously-skip-permissions` | Same as `--permission-mode bypassPermissions`. |
| `--include-partial-messages`, `--include-hook-events`, `--replay-user-messages` | Accepted, no effect: the structured formats already carry that. |
| `--autocompact <auto\|tokens>` | `auto` leaves the window alone; `200k` or `1m` sets `max_context_tokens` for the run. |
| `-v` | Rewritten to `--version`. |
| `[prompt]` | The words after the flags are the task. |

### Accepted and reported as unsupported

`--no-session-persistence`, `--fallback-model` and `--max-budget-usd` in a run that opens the terminal UI: they say
so on stderr, since they only mean something for a headless run. Everything else that was on this list is mapped
above.

### Removed with a warning

`--betas`, `--brief`, `--chrome`, `--no-chrome`, `--ide`, `--cloud`, `--remote`, `--environment`, `--ref`,
`--remote-control`/`--rc` and its prefix flag, `--teleport`, `--background`, `--desktop`, `--tmux`,
`--from-pr`, `--file`, `--init`, `--init-only`, `--maintenance`, `--ax-screen-reader`,
`--advisor`, `--prompt-suggestions`, `--teammate-mode`, `--tools`, `--permission-prompts`,
`--permission-prompt-tool`, `--allow-dangerously-skip-permissions`, `--channels` and the development-channels flag,
`--forward-subagent-text`, `--exclude-dynamic-system-prompt-sections`, `--system-prompt-snapshot`,
the two `--append-subagent-system-prompt` flags, `--plugin-url`, `--debug-file`, `--setting-sources`,
`--restricted`.

### Not mine

`--permission-mode`, `--allowedTools` and `--disallowedTools` belong to the permissions work and are parsed
there. `--tools`, `--permission-prompt-tool`, `--permission-prompts` and `--allow-dangerously-skip-permissions`
are removed with a warning here, since nothing implements them.

## The `.claude` directory

Read in place, no import step. Wizard's own directory shadows Claude's on a name clash, and a project
shadows the user's.

| What | Read from | Notes |
|---|---|---|
| Commands | `~/.claude/commands`, `<project>/.claude/commands` | Subfolders namespace: `team/deploy.md` is `/team:deploy`. `$ARGUMENTS`, `$1`..`$9` and `@file` work. `!` shell injection and `allowed-tools` frontmatter are ignored. |
| Skills | `~/.claude/skills`, `<project>/.claude/skills`, `<project>/.wizard/skills` | Same `SKILL.md` format. |
| Subagents | `~/.claude/agents`, `<project>/.claude/agents` | Markdown with `name`, `description`, `tools`. `tools` map by purpose (`Read` to `read_file`, `Bash` to `execute`); names with no Wizard tool are dropped. `model` is ignored. |
| Instructions | `CLAUDE.md` (already read; `CLAUDE_CODE_DISABLE_CLAUDE_MDS` skips every Claude file), `.claude/CLAUDE.md`, `CLAUDE.local.md`, `.claude/rules/*.md`, `~/.claude/CLAUDE.md`, `~/.claude/rules` | `WIZARD.md` and `AGENTS.md` still shadow `CLAUDE.md` in the same directory. A rule with `paths:` is skipped, since nothing here injects per file. |
| MCP servers | `<project>/.mcp.json`; `~/.claude.json` (`mcpServers` and this project's entry under `projects`, read only); `~/.wizard/local-mcp.json` | `.mcp.json` is held back until the project is trusted, or `enableAllProjectMcpServers` / `enabledMcpjsonServers` in your own settings approve it (below). `~/.claude.json` and the local file are your own, so they load at once. `${VAR}` and `${VAR:-default}` expand. |
| Output styles | `~/.claude/output-styles`, `<project>/.claude/output-styles`, `~/.wizard/output-styles`, `<project>/.wizard/output-styles`; built-ins `default`, `explanatory`, `learning` | Chosen with `/output-style`, stored in `~/.wizard/output-style`; falls back to `outputStyle` in `.claude/settings.json`. Applied when the agent is built, so `/output-style` reloads. |
| Plugins | `~/.claude/plugins/installed_plugins.json` entries enabled in `enabledPlugins`; `~/.wizard/claude-plugins/installed`; `--plugin-dir` | Commands (`plugin:cmd`), skills, agents, output styles, `.mcp.json` and hooks. A plugin's `hooks/hooks.json` (or the manifest's `hooks`, a path or an inline object) goes through the hooks engine like a Claude-format settings hook, with `${CLAUDE_PLUGIN_ROOT}` filled in; `/hooks` lists them as `plugin <name>` and `disableAllHooks` turns them off. |
| Marketplaces | `~/.wizard/claude-plugins/marketplaces`, and Claude Code's `known_marketplaces.json` | `owner/repo` and URL sources are cloned with `git`; relative, `github` and `url` plugin sources install, others are named as unsupported. |

### Trust

`.mcp.json` entries are commands to run, and the file arrives with a `git clone`. They connect only when the
project has a recorded yes in `~/.wizard/trusted_projects` (the same decision the project's hooks wait on) or
`WIZARD_TRUST_PROJECT` is set, or the user's own settings approve them (`enableAllProjectMcpServers`, or the
name in `enabledMcpjsonServers`, in `~/.claude/settings.json` or the project's `.claude/settings.local.json`;
`disabledMcpjsonServers` always wins). `/mcp` lists the ones waiting, and where each loaded server came from.
Servers from plugins, `--mcp-config`, `~/.claude.json` and `wizard mcp add` load, since the user named them.
`wizard mcp reset-project-choices` forgets the project's decision.

### Not covered

- Claude Code `settings.json` (permissions, hooks, env): the settings work. The terminal reads two keys from it,
  `editorMode` and `statusLine`, see below.
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
"refreshInterval": 5 }`. Both UIs run it through one file, `src/statusline_run.rs` (the claude look compiles it
with `#[path]`, as it does `vim.rs`), so the payload, the environment and the colours cannot drift apart. The
command runs with Claude Code's JSON on stdin (`session_id`, `transcript_path`, `cwd`, `effort.level`, `model`,
`workspace` with `added_dirs`, `version`, `output_style`, `cost`, `context_window` with `current_usage`,
`exceeds_200k_tokens`, `fast_mode`, `thinking`, and `vim.mode` when vim mode is on), and `COLUMNS`, `LINES`,
`CLAUDE_CODE_CHILD_SESSION=1`, `CLAUDE_PROJECT_DIR` and `CLAUDE_CODE_SESSION_ID` in its environment. It runs on
start, after each turn, on a mode, model or vim mode change and on the timer, debounced to 300 ms, in a thread
with a five second limit. `/statusline <command>` sets it, `/statusline off` removes it, `/statusline` says what
is set; in the look the command is wizard's, and the look reads the settings again when the turn ends.

Compared with 2.1.314 (`sl-*` and `slp-*` scenes, a test command, a real pixel layout):

- The rows sit between the lower rule of the prompt and the mode line, in the subtle grey, with the command's own
  colours on top. Blank lines are dropped and trailing space trimmed.
- The text starts two columns in plus `padding`, and ends two columns plus `padding` short of the right edge; a
  longer line ends in `…`.
- There is no cap of four rows: 2.1.314 draws every row the command prints and lets the screen cut the footer.
  The house UI keeps its four.
- A status line takes `? for shortcuts` off the mode line. A command that fails, times out or prints nothing
  leaves one blank row. A command that exits non-zero shows nothing even if it printed first.
- The shortcut table replaces the rows; the slash menu leaves them; a picker or a permission prompt covers them.

Not in the payload: `cost.total_cost_usd` is the running cost in the look when the backend reports one and 0 in the
house UI, no rate limits,
no `prompt_cache`, no PR or worktree fields, no `prompt_id`.

### Footer, cursor, placeholder

`← for agents` follows `? for shortcuts` on an empty prompt for the first ten seconds of a session (2.1.314 hides it then), and ← opens `/agents`. The terminal cursor is hidden
and an inverse cell is painted where it is, as 2.1.314 does. The `Try "..."` example is kept: 2.1.314 fills it from
a cache that only a signed-in account refreshes, so a capture with a made-up key shows none and the one in
`tests/fixtures/ref` (a subscription) shows it.

The effort notice (`◐ medium · /effort`, above the prompt on the right) is up for the first ten seconds of a
session and for as long as a turn runs; it does not come back when the turn ends. The look used to keep it 30
seconds after each turn, which was how 2.1.300 behaved.

### Task list

Wizard's `todo` tool fills the look's task list the way 2.1.314 draws its Task tools: a panel above the prompt,
not rows in the transcript (the `todo` call itself leaves no row either). `ctrl+t` hides and shows it, and the
mode line says which: `ctrl+t to hide tasks` / `ctrl+t to show tasks`, in place of `? for shortcuts`.

- While a turn runs the list hangs under the spinner: `  ⎿  ✔ done`, then the first five tasks in order
  (`✔` green, struck through and subtle; `◼` in the brand colour and bold; `◻` plain), then
  `      … +1 in progress, 3 pending, 3 completed` for the rest.
- Between turns it has two blank rows and a heading, `14 tasks (3 done, 1 in progress, 10 open)`, with the counts
  bold. Up to five tasks show in order; past five, unfinished ones come first (in progress, pending, completed)
  and the rest is counted: `   … +6 pending, 3 completed`.
- A finished list (every task completed) goes away when the turn ends. A task's text is cut with `…` at
  `width - 15`.
- With a task in progress the spinner names it instead of a verb. 2.1.314 shows the task's `activeForm`
  ("Changing hello to hi"); Wizard's `todo` has no such field, so the look shows the task's own text.

Driving 2.1.314 to show it needs `CLAUDE_CODE_ENABLE_TODO_TOOLS=1`: the Task tools are on for models it knows,
and the fake server's `claude-haiku-5-5` is not one. The model also has to ask for them first
(`ToolSearch select:TaskCreate,TaskGet,TaskUpdate`); `fake_anthropic.py` scripts that for the `Make a task list`,
`Make many tasks`, `Make long progress` and `Finish the tasks` prompts. `TodoWrite` stays off in the
interactive 2.1.314 in that setup; only the Task tools show.

### Detailed transcript

`ctrl+o` writes paths from the working directory with a `./` (`Read(./src/lib.rs)`, `Read(./../outside.txt)`) when
that is shorter than the absolute path, and the absolute path otherwise (`Read(/etc/hostname)`). The usual view
drops the directory and the `./` (`Update(main.rs)`). 2.1.300 wrapped the file name in an OSC 8 `file://` link;
2.1.314 does not, in any view, even where it links Markdown (checked with the program's raw output in a tmux that
reports hyperlinks), so the look no longer does either.

### Plan mode

Compared state by state with 2.1.314: `⏸ plan mode on (shift+tab to cycle)` in the footer, the plan and its
approval dialog (`plan`, `plan-down`, `x-planesc`), and what follows approval (`plan-approve`). Where they differ:

- 2.1.314 offers three ways out of the dialog (use auto mode, manually approve edits, tell it what to change,
  with `shift+tab to approve with this feedback`); Wizard offers approve and keep planning.
- An approved plan prints `User approved Claude's plan` with a plan-coloured bullet, the plan file
  (`~/.claude/plans/<slug>.md · /plan to edit`) and the plan. Wizard keeps its plan in `.wizard/plan.md`.
- After approval 2.1.314 is in the mode its option named; Wizard goes back to the mode the plan was entered from.

### Welcome screen model line

2.1.314 prints `Haiku 5.5 · API Usage Billing` (or the plan name for a subscription). The look prints
`<model> · <provider>` from what wizard reports, for example `grok-4.7 · xai-oauth`: the same form, with the real
model and the real way it is paid for, and nothing renamed to look like Claude. The long price string seen in
`tools/compare.sh` runs is the replay's: a Claude model picker row carries its description in the same field
wizard uses for the provider, so the replayed header reads `Haiku · Haiku 5.5 · Fastest for quick answers ...`. The
comparison masks the words of that row (`explained.tsv`, `norm`), not its colours or its place.
