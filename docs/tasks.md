# Background tasks

Long-running commands (dev servers, builds, watchers) don't have to block the agent loop.

There are two ways a command ends up as a background task: the agent asks for it up front, or it takes too long and Wizard moves it there.

## The handover

A foreground `execute` waits **30 seconds** by default (`[shell] timeout_secs`). Past that the command is not killed — it is handed to the background task registry, still running, with everything it has said so far returned to the model:

```
started
warn
[still running after 30s; moved to background task #2. Output above is what it had
produced by then, and you will be notified when it exits. Carry on with something
else, or: task_output(2) for the tail now, task_output(2, wait_secs=N) to block
until it finishes, task_kill(2) to stop it.]
```

The short budget and the handover are one decision, not two. When the end of the budget was a kill, the number had to cover the longest command anybody might reasonably run — so a wedged command cost two minutes of a turn doing nothing, and a genuinely long one cost two minutes and then died anyway. Now the budget only answers "how long is it worth blocking the turn for an answer", and the command gets its full 30 minutes either way.

Two overrides, and the agent picks by what it knows:

- **`timeout_secs`** on the `execute` call, up to 600 — "I would rather wait inline for this one."
- **`task_output(id, wait_secs=N)`** after the fact, up to 600 — "it turns out I do need this before I go on."

Everything else that shells out — the git tools, `search_files`, scripted tools — still gets killed at its budget. Those run short commands whose result is the entire point of the call, and there is nobody to hand a task id to.

## Asking for it up front

The `execute` tool takes an optional `run_in_background` flag:

```json
{ "command": "cargo build --release", "run_in_background": true }
```

The command is spawned detached and registered as a background task; the call returns immediately with

```
Background task #N started: <command>
You will be notified when it finishes; use task_output to inspect it or task_kill to stop it.
```

The agent keeps working while the task runs.

## Lifecycle

- Each task captures combined stdout/stderr into a per-task buffer capped at ~200 KB; once output exceeds the cap, only the most recent tail is kept
- Background tasks are killed after **30 minutes**; the status reflects the timeout
- At the top of every agent step (and every `--continuous` cycle), finished tasks are reported to the model exactly once, as a history note like:

  ```
  [background task #3 finished (exit 0)] cargo build --release
  <last ~2 KB of output>
  ```

- The TUI, the `text` headless format, and `stream-json` (a `task_finished` line) each report a finished task; the `json` format does not — it only carries the run summary
- All still-running tasks are killed when the agent shuts down

The spawn still flows through the regular tool dispatch pipeline, so `pre_tool_use`/`post_tool_use` hooks apply to it like any other `execute` call.

## Managing tasks

Two companion tools:

| Tool | Arguments | Does |
|------|-----------|------|
| `task_output` | `id`, `tail_bytes` (optional, default 20 000, clamped to 28 000), `wait_secs` (optional, default 0, clamped to 600) | Return the task's status and the tail of its buffered output. With `wait_secs`, blocks until the task finishes or the wait runs out — a wait that expires reports the task as it stands rather than failing. Read-only, works in plan mode. Observes Ctrl-C while parked. |
| `task_kill` | `id` | Terminate a running task. |

Statuses: `running`, `exit <code>`, `killed`, `timed out`.
