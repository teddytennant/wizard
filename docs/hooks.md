# Lifecycle hooks

Hooks are commands (or an HTTP call, a model prompt, an MCP tool) that Wizard runs at fixed points in the agent's lifecycle: before and after every tool call, when a prompt is submitted, when the model stops, around compaction and model switches, and at session boundaries. There are 34 events. They fire in **every mode** (genie TUI, sovereign headless, perpetual `--continuous`, and the gateway) and apply to subagent tool calls too. Use them to enforce policy (block dangerous commands), inject context (project status, time of day), or log activity.

Wizard has no per-call approval prompt for the model's tool calls. Hooks and [permission rules](permissions.md) are the programmable seam where you put guardrails instead. There *is* one prompt about hooks themselves: a project's own hook files need a trust decision before they load, see [Project trust](#project-trust).

Hooks come from two kinds of file. Wizard's own `hooks.toml` is described first. Claude Code's `settings.json` format works too, see [Claude-format hooks](#claude-format-hooks).

## Declaring hooks

Hooks live in TOML files, loaded when an agent is built:

- `~/.wizard/hooks.toml`: global, applies everywhere
- `<project>/.wizard/hooks.toml`: per project, appended after the global hooks

```toml
[[hooks]]
event = "pre_tool_use"           # which lifecycle event (see below)
matcher = "execute"              # optional glob over the tool name
command = "/path/to/script.sh"   # run via `sh -c` in the project root
timeout_secs = 30                # optional; default 60
```

`matcher` is a glob (`"execute"`, `"git_*"`, `"*file*"`) over the tool name and only applies to tool events; other events ignore it. Omit it to match every tool. Hooks for the same event run sequentially in declaration order, global first, then project.

Optional keys, for handler types beyond a shell command:

```toml
[[hooks]]
event = "stop"
type = "prompt"                    # command (default), http, prompt, agent, mcp_tool
prompt = "Did the tests run? $ARGUMENTS"

[[hooks]]
event = "post_tool_use"
type = "http"
url = "https://example.com/hook"
headers = { Authorization = "Bearer $HOOK_TOKEN" }
allowed_env_vars = ["HOOK_TOKEN"]  # only these names expand in headers

[[hooks]]
event = "notification"
type = "mcp_tool"
server = "chat"
tool = "post"
input = { text = "${message}" }    # ${path} reads the payload

[[hooks]]
event = "pre_tool_use"
protocol = "json"                  # read stdout as a decision, see below
if = "Bash(git push:*)"            # permission-rule syntax; tool events only
async = false                      # true: run without waiting, ignore the result
on_failure = "block"               # a failed hook blocks instead of warning
command = "./check.sh"
```

`args = ["a", "b"]` next to `command` runs the program directly without a shell.

A missing file means no hooks: default behavior is unchanged. An invalid file or matcher is skipped with a logged warning; it never prevents startup.

**Project hooks do not run until you trust the project.** `~/.wizard/hooks.toml` is yours by construction and always loads; `<project>/.wizard/hooks.toml` arrives with a `git clone` and is gated. See [Project trust](#project-trust) below.

## Project trust

A hook is a shell command, and `session_start` fires before the model has said a word, so a repository that ships `.wizard/hooks.toml` is shipping code that would run the moment you launch Wizard in its directory. Cloning is not consent, so there is a gate.

The gate also covers `.claude/settings.json`, `.claude/settings.local.json` and the `.wizard/settings*.json` twins, when they carry `hooks`, `env`, `statusLine` or a helper-command key. A settings file that holds only data (permission rules) is not part of the surface.

A project with no `.wizard/hooks.toml` is never gated and never mentioned: there is nothing executable to decide about, and that case has to stay silent. A project that has one contributes no hooks at all until there is a recorded yes for exactly that file.

### The decision

There is one decision per project, and it is recorded in `~/.wizard/trusted_projects`: one JSON object per line, written atomically at mode 0600 (this file decides what may execute, so no other user gets to add a line to it).

- **The project root is canonicalised** before it is used as the key, so a symlink or a `..`-dressed path cannot ride another project's approval.
- **The hooks file is fingerprinted** (sha256 over its relative path, its length, and its bytes). Editing it, replacing it, or adding one after the fact re-opens the question: the old yes covers the old content only. A `git pull` that touches `.wizard/hooks.toml` therefore needs a fresh yes.
- **Both answers are recorded.** A no is a decision, not a deferral, so it is not asked again either.
- **A recorded no outranks everything**, including `WIZARD_TRUST_PROJECT`. To be asked again, delete that project's line from `~/.wizard/trusted_projects`.

### When Wizard asks, and when it just refuses

Two things have to be true before the question is put to you.

**A surface has to declare that the terminal is safe to block on.** That is a claim the calling code makes, not something inferred from `isatty`, because `isatty` answers the wrong question. Under the TUI there *is* a tty on fd 0 and prompting is still catastrophic: crossterm holds that same descriptor in raw mode behind the alternate screen, so the question would be painted invisibly over the frame, the keystroke answering it would be taken by the event stream, and the blocking read would park the very thread running the event loop. A foreground `wizard gui` passes the same probe and would block a window-driven task on a question painted into a terminal nobody is watching.

**And the terminal facts have to agree**: a tty on fd 0 *and* fd 1, with this process in the terminal's foreground process group. So `echo hi | wizard -p "…"` refuses rather than blocking on a pipe, and a backgrounded process (which would earn a SIGTTIN for reading stdin and stop) never gets that far.

Exactly two surfaces make the declaration, and both do it in the same place: once, up front, before anything has taken the terminal over.

| Surface | Can ask? | Where the refusal shows up |
|---------|----------|----------------------------|
| Genie TUI (`wizard`) | **Yes**, before raw mode and the alternate screen are set up | As a notice in the transcript, on the first draw |
| Headless (`wizard -p "…"`, `--mode sovereign`, `--continuous`) with the default `--output-format text` | **Yes**, before the spinner starts | `wizard: <reason>` on stderr |
| The same headless run under any other `--output-format` | No: stdout is a machine-readable stream and a prompt would be the first thing in it | The log |
| Gateway (`wizard --gateway`) | No | Printed to stdout at startup and logged, so it lands in `journalctl` next to the refusals |
| The window (`wizard gui`) | No | A notice in that chat's own event stream, plus the log |
| Editor embedding (`wizard acp`) | No: stdout is JSON-RPC to the editor | The log |
| Fleet (`wizard fleet`), both the planner and the workers | No | The log |
| Scheduler daemon jobs | No: the child is spawned with stdin on `/dev/null` | The job's log under `~/.wizard/logs/jobs/` |
| CI, systemd units, cron, anything piped or backgrounded | No | The log |
| Every mid-session agent rebuild (`/model`, a provider switch, `/fusion`, crash recovery) | No, on every surface | The log |

`wizard schedule run <name>` is the one scheduler case that inherits your terminal, so a job you start by hand can ask; the daemon that runs the same entry on its cron line cannot.

When it does ask, it names the file and takes one answer:

```
This project ships files that Wizard would run as shell commands:
  /home/you/src/theirrepo/.wizard/hooks.toml

There is no trust decision on record for /home/you/src/theirrepo.
Read those files first: a hook runs unsandboxed, with your privileges.
Trust this project and run its hooks? [y/N]
```

Anything but `y` / `yes` (including end of input) is a no. The file is never quoted back at you, only named: its contents are whatever its author wrote, and echoing them would hand a repository your terminal's escape sequences. Read it yourself.

The answer is recorded either way, and it is recorded once for the whole process: the rebuilds later in the session read it back and ask nothing. A `y` typed at a `wizard -p` run is as durable as one typed at the TUI.

**Everywhere the question cannot be put, the answer is no.** Those surfaces load no project hooks and say why:

```
not running project hooks (/srv/app/.wizard/hooks.toml): /srv/app has no trust decision
on record and there is no terminal to ask on. Start wizard once interactively in that
directory to decide, or set WIZARD_TRUST_PROJECT=1 for unattended runs.
```

Starting Wizard interactively in that directory is advice that works: the TUI is one of the two surfaces that asks, and one `y` there settles it for the gateway, the GUI, and CI afterwards.

Every refusal also goes to the log (`~/.wizard/logs/`, see [logging.md](logging.md); it is a warning, so the default filter records it). That is the per-rebuild trace, and it is deliberately the *only* place the rebuild path writes: the check re-runs on every agent rebuild, and printing there would put raw multi-line text on the TUI's alternate screen once per rebuild. Each surface says it once more where its own user is actually looking, per the table above. If a project's hooks are not firing and you expected them to, the log is where the reason is.

Nothing is recorded in the unattended case, so the next run that *can* ask still gets to decide.

### Trusting a project without the prompt

If you are never asked (an unattended run, or any surface that does not own the terminal outright), this is the way in.

**`WIZARD_TRUST_PROJECT=1`** (also `true` or `yes`; anything else, including `0`, is not an opt-in) trusts the project for that one process:

```bash
WIZARD_TRUST_PROJECT=1 wizard --mode sovereign -p "run the release checklist"
```

It answers an *open* question only. It cannot override a no you recorded, and it is never persisted, so it cannot leak a decision into a later interactive run. It is the right tool for a machine whose project hooks are your own: a CI job, a systemd unit running the gateway on a repo you control. Exporting it globally in a shell profile is not, since it then applies to every repository you happen to clone.

It also does not lift a refusal that an *edit* re-opened. Editing the hooks file normally puts the project back to "open" so a human can be re-asked, and the file belongs to whoever wrote the repository, so without this rule "append a blank line and push" would be a way around your no on every machine that exports the variable. A human at a terminal is a different matter: the content genuinely changed, so you are asked again, and a yes then stands.

Only project-supplied files go through this gate. The global `~/.wizard/hooks.toml` is not gated: it is yours, gating it would prompt in every directory on earth, and it would close no hole.

## Events

Names are snake_case in `hooks.toml` and PascalCase in Claude-format files. The first six are the original Wizard events; their plain-stdout behavior is unchanged.

| Event | Claude name | When | A hook can |
|-------|-------------|------|------------|
| `pre_tool_use` | PreToolUse | Before a tool call executes | Rewrite the arguments, block, allow, deny or ask |
| `post_tool_use` | PostToolUse | After a tool call executes | Append context to the tool result |
| `user_prompt_submit` | UserPromptSubmit | When a message starts a turn | Block the turn, or append context |
| `session_start` | SessionStart | Once when a session begins (matcher: `startup`) | Append system context |
| `session_end` | SessionEnd | Once when a session ends | Observe |
| `turn_end` | (none) | After every turn finishes | Observe |
| `post_tool_use_failure` | PostToolUseFailure | A tool reported failure | Append context |
| `post_tool_batch` | PostToolBatch | All calls of one model response resolved | Append context, stop the turn |
| `user_prompt_expansion` | UserPromptExpansion | A slash command expands into a prompt (matcher: command name) | Block, append context |
| `stop` | Stop | The model finished its turn | Send it back to work with feedback |
| `stop_failure` | StopFailure | The turn ended on a provider error (matcher: `rate_limit`, `overloaded`, `authentication_failed`, `billing_error`, `invalid_request`, `model_not_found`, `server_error`, `unknown`) | Observe |
| `subagent_start` | SubagentStart | A subagent is about to run (matcher: its name) | Append context for it |
| `subagent_stop` | SubagentStop | A subagent finished | Send it back to work |
| `pre_compact` | PreCompact | Before compaction (matcher: `manual`, `auto`) | Cancel it |
| `post_compact` | PostCompact | After compaction | Observe |
| `setup` | Setup | Reserved; nothing in Wizard fires it yet | none |
| `notification` | Notification | A tool call needed approval and no one could give it (`permission_prompt`) | Observe |
| `permission_request` | PermissionRequest | A call needs approval: an `ask` rule matched, or the mode asks (see [permissions.md](permissions.md)). Runs before the dialog, and an answer skips it | Approve or deny |
| `permission_denied` | PermissionDenied | A rule or mode refused a call | Observe |
| `elicitation` | Elicitation | The model asked the user questions with `interview` (server name `wizard`) | Observe |
| `elicitation_result` | ElicitationResult | The user answered | Observe |
| `config_change` | ConfigChange | Wizard wrote `config.toml` | Observe |
| `cwd_changed` | CwdChanged | Reserved; Wizard's commands do not change the session directory | none |
| `file_changed` | FileChanged | A file changed (matcher: basename) | Observe |
| `instructions_loaded` | InstructionsLoaded | An instructions file (`WIZARD.md`, `CLAUDE.md`, ...) was loaded at session start | Observe |
| `task_created` | TaskCreated | A `todo` write added an item | Observe |
| `task_completed` | TaskCompleted | A `todo` write marked an item done | Observe |
| `teammate_idle` | TeammateIdle | Reserved; the fleet does not fire it yet | none |
| `worktree_create` | WorktreeCreate | Wizard created a git worktree | Observe |
| `worktree_remove` | WorktreeRemove | Wizard removed a git worktree | Observe |
| `directory_added` | DirectoryAdded | Reserved; Wizard has no `/add-dir` | none |
| `message_display` | MessageDisplay | The final assistant text of a turn was shown | Observe |
| `pre_model_switch` | PreModelSwitch | Before `/model` or a provider switch | Observe (it cannot refuse the switch yet) |
| `post_model_switch` | PostModelSwitch | After the model changed | Append context to the next request |

"Session" means: TUI launch to quit, one headless run (including all continuous cycles), or the gateway's whole serve lifetime.

Where each event fires today, and what is thinner than Claude Code's version:

- Events marked "reserved" are accepted in config so a settings file written for Claude Code loads, and have a place in the engine, but no Wizard feature calls them yet.
- `file_changed` fires after Wizard's own edit tools (`write_file`, `edit_file`, `apply_patch`) succeed. There is no filesystem watcher, so changes made by other programs do not fire it.
- `stop` and `subagent_stop` can keep the run going up to 8 times in a row; after that the model stops anyway.
- `elicitation` follows the `interview` tool, Wizard's way of asking the user a question. There is no MCP elicitation, and the hook cannot answer for the user.
- Events that fire from code with no handle on the session (`worktree_*`, `instructions_loaded`, `config_change`, `*_model_switch`) go to the most recently built agent and run without waiting for it.
- `session_end` always reports its reason as `other`.

## Payload

Each hook receives one JSON object on stdin:

```json
{
  "event": "pre_tool_use",
  "hook_event_name": "PreToolUse",
  "tool_name": "execute",
  "args": {"command": "cargo test"},
  "tool_input": {"command": "cargo test"},
  "cwd": "/path/to/project",
  "session_id": "2026-06-11T09-30-00",
  "transcript_path": "/home/you/.wizard/sessions/2026-06-11T09-30-00.jsonl",
  "permission_mode": "default",
  "mode": "genie"
}
```

`hooks.toml` hooks get Wizard's own names (`tool_name` is `execute`, `args` holds the arguments) and also the Claude fields (`hook_event_name`, `tool_input`, `permission_mode`, `transcript_path`). Claude-format hooks get Claude's names: `tool_name` is `Bash`, `tool_input` has `file_path` where Wizard's tool has `path`, and `wizard_tool_name` carries the real name. `tool_name` and `args` are `null` for non-tool events in `hooks.toml` hooks.

Event-specific fields:

- `user_prompt_submit`: `prompt`.
- `post_tool_use`: `tool_output` (truncated to 32 KB), `tool_response` (`{"output": ..., "stdout": ...}`), `is_error`, `tool_use_id`.
- `post_tool_use_failure`: `error`, `tool_use_id`.
- `pre_tool_use`, `permission_request`, `permission_denied`: `tool_use_id`; the last two add `reason`, and `permission_request` adds `permission_suggestions`, the `addRules` entries the dialog's "don't ask again" would save.
- `session_start`: `source`.
- `stop`, `subagent_stop`: `stop_hook_active`, `last_assistant_message`; `subagent_*` add `agent_id` and `agent_type`.
- `pre_compact`, `post_compact`: `trigger`; the second adds `compact_summary`.
- `pre_model_switch`, `post_model_switch`: `from_model`, `to_model`, `source`.
- `file_changed`: `file_path`, `event`. `cwd_changed`: `old_cwd`, `new_cwd`. `config_change`: `source`, `file_path`. `instructions_loaded`: `file_path`, `memory_type`, `load_reason`.
- `task_created`, `task_completed`: `task_id`, `task_subject`. `notification`: `notification_type`, `message`, `title`.

A hook that doesn't care about the payload can just skip reading stdin.

## Exit codes and stdout

- **Exit 0: continue.**
  - `hooks.toml` hooks on the original six events: `pre_tool_use` stdout of `{"updated_args": {...}}` rewrites the arguments (later hooks see the rewrite); `post_tool_use`, `user_prompt_submit` and `session_start` append non-empty stdout as context; `session_end` and `turn_end` ignore stdout.
  - Every other case reads stdout as the [decision protocol](#decision-protocol) when it is a single JSON object. Plain text counts as context only on `user_prompt_submit`, `user_prompt_expansion`, `session_start` and `post_model_switch`; elsewhere it goes to the log.
- **Exit 2: block.** stderr is the reason.
  - `pre_tool_use`: the tool doesn't run; the model gets `blocked by pre_tool_use hook: <reason>` as an ordinary tool error and can adjust course. Repeated blocked calls count toward the same failure breakers as any other tool failure.
  - `user_prompt_submit`: the turn ends immediately with a notice; the prompt never reaches the model.
  - `stop`, `subagent_stop`: the model keeps working, with stderr as the next instruction.
  - `post_tool_use`, `post_tool_use_failure` (Claude-format hooks): stderr goes to the model as feedback next to the result.
  - `pre_compact`, `pre_model_switch`, `user_prompt_expansion`, `post_tool_batch`, `config_change`, `task_created`, `task_completed`, `teammate_idle`: the action is refused.
  - Other events can't block; exit 2 there is treated like any other failure.
- **Anything else: ignored.** A different exit code, a timeout (`timeout_secs`, default 60, the process is killed), or a spawn failure surfaces as a warning and the pipeline continues. Hooks must never wedge the agent. Set `on_failure = "block"` (`"onFailure": "block"` in settings files) to make a failed hook on a blockable event block instead.

Hooks for one event run one after another, in declaration order, and a block stops the chain. (Claude Code runs them in parallel.)

Hook activity that changes something (a rewrite, appended context, a block, a warning) shows up as a dim log line in the TUI, or a printed line headless. Silent successes stay silent.

Appended context is reported **once per hook per session**, and every append after that goes to the log (`WIZARD_LOG=debug`) instead. A hook that injects context injects it every time it matches, which is what it is for, so a `post_tool_use` hook reporting each one costs a line per tool call. The first notice is the useful one: text entering the model's context from outside the conversation is not otherwise visible anywhere. `/clear` starts a new session and resets the filter. Rewrites, blocks and warnings are never deduplicated.

### Decision protocol

A hook that prints one JSON object on exit 0 can say more than yes or no. The shape is Claude Code's:

```json
{
  "continue": false,
  "stopReason": "shown to the user",
  "systemMessage": "a note for the user",
  "decision": "block",
  "reason": "why",
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow | deny | ask",
    "permissionDecisionReason": "why",
    "updatedInput": {"command": "ls -la"},
    "additionalContext": "text for the model"
  }
}
```

- `permissionDecision` on `pre_tool_use`: `deny` refuses the call. `ask` makes it need approval even when no rule says so. `allow` does **not** skip deny or ask rules, which still apply afterwards. Deny beats ask beats allow across hooks.
- `updatedInput` replaces the tool arguments, in Claude's shape (`file_path` for `path`). Wizard-format hooks can use `updated_args` in Wizard's shape.
- `additionalContext` goes to the model: next to the tool result for tool events, onto the prompt for `user_prompt_submit`, as feedback for `stop`.
- `decision: "block"` with a `reason` blocks on the events that can be blocked, and is feedback on `post_tool_use`.
- `continue: false` ends the turn, with `stopReason` as the note.
- `permission_request` hooks answer with `hookSpecificOutput.decision`: `{"behavior": "allow"}` or `{"behavior": "deny", "message": "..."}`.
- Deprecated `{"decision": "approve"}` means allow.

Not supported: `updatedToolOutput`, `defer`, `watchPaths`, `terminalSequence`, `asyncRewake`.

### Handler types

| `type` | What happens | Result |
|--------|--------------|--------|
| `command` | Runs in the project root, payload on stdin | Exit code and stdout as above |
| `http` | POSTs the payload as JSON | A 2xx body is read as stdout (exit 0); any other status is a warning; a connection failure is a failure |
| `prompt` | One model call with your prompt (`$ARGUMENTS` is the payload; it is appended if absent), asked to answer `{"ok": true}` or `{"ok": false, "reason": "..."}` | `ok: false` acts like exit 2 with the reason on stderr |
| `agent` | Same as `prompt`. Wizard runs it as one model call with no tool access | As `prompt` |
| `mcp_tool` | Calls a tool on a connected MCP server | Text result is stdout; an error result is a warning |

`prompt`, `agent` and `mcp_tool` hooks need a model or an MCP server attached, so they do nothing in contexts that have neither. Their default timeout is 30 s for `prompt`, 60 s for `agent`.

## Claude-format hooks

Wizard reads the `hooks` key of Claude Code settings files:

- `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`)
- `<project>/.claude/settings.json`
- `<project>/.claude/settings.local.json`
- the same names under `.wizard/` (`~/.wizard/settings.json`, `<project>/.wizard/settings.json`, `<project>/.wizard/settings.local.json`)
- `/etc/claude-code/managed-settings.json` and `/etc/wizard/managed-settings.json`

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {"type": "command", "command": ".claude/hooks/check.sh", "timeout": 10, "if": "Bash(git *)"}
        ]
      }
    ]
  }
}
```

Matchers follow Claude's rules: empty, missing or `*` matches everything; letters, digits, `_`, `-`, spaces, `,` and `|` make a list of exact names (`Edit|Write`); anything else is an unanchored regular expression (`mcp__github__.*`). Tool matchers are tested against both the Claude name (`Bash`) and Wizard's (`execute`). Identical hooks that appear in several files run once. Unknown event names are skipped so a newer settings file still loads. `"disableAllHooks": true` turns off every hook that came from a settings file, not `hooks.toml`.

Project settings files are executable (hooks, `env`, `statusLine`), so they sit behind the same [project trust](#project-trust) gate as `.wizard/hooks.toml`. A settings file that holds only data, such as permission rules, is not gated.

## Examples

Block shell commands that mention `rm -rf`:

```toml
[[hooks]]
event = "pre_tool_use"
matcher = "execute"
command = "jq -e '.args.command | test(\"rm -rf\") | not' > /dev/null || { echo 'rm -rf is not allowed' >&2; exit 2; }"
```

Append a note to every failed shell command's result:

```toml
[[hooks]]
event = "post_tool_use"
matcher = "execute"
command = "jq -r 'if .is_error then \"check the error above before retrying\" else empty end'"
```

Remind the model of the branch policy on every prompt:

```toml
[[hooks]]
event = "user_prompt_submit"
command = "echo \"current branch: $(git branch --show-current). Never commit to main directly.\""
```

Log every completed turn:

```toml
[[hooks]]
event = "turn_end"
command = "date >> ~/.wizard/logs/turns.log"
timeout_secs = 5
```
