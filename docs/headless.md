# Headless output formats and exit codes

A sovereign run (`wizard --mode sovereign -p "task"`, or any `-p` run without a terminal) prints to stdout. `--output-format` selects how:

## `text` (default)

The human-readable stream you get today: assistant deltas as they arrive, dimmed reasoning, `→ tool {args}` / `← tool [ok]` one-liners (`[error]` on a failure), a busy spinner on terminals, a `[turn done: Completed]` line per turn, and a trailer:

```
[run finished: Completed — 1200 prompt + 240 completion tokens]
```

The reason there is the Rust variant name (`Completed`, `MaxSteps`, `TimeLimit`, `Stopped`, `CircuitBreaker`), not the snake_case spelling the structured formats use, and the token clause is dropped when both counters are zero.

A subagent's tool calls print inline under the subagent's name (`→ researcher ▸ web_fetch {…}` / `← researcher ▸ web_fetch [ok]`); the TUI puts them on its [subagent rail](usage.md#the-subagent-rail) instead. The structured formats below label them the same way in their `name` fields.

Both structured formats follow Claude Code's headless shapes (captured from 2.1.314; the details and the differences are in [claude-code-parity.md](claude-code-parity.md#headless-output)). Wizard's own fields ride along on the `result` object.

## `json`

Silent until the run ends, then exactly one JSON object on stdout. It carries Claude Code's `result` keys (`type`, `subtype`, `is_error`, `num_turns`, `result`, `session_id`, `total_cost_usd`, `duration_ms`, `usage`, `modelUsage`, `permission_denials`, `terminal_reason`, `errors`) and these of Wizard's:

```json
{
  "result": "the last reply's text",
  "text": "every reply's text, step after step",
  "reason": "completed",
  "turns": 1,
  "steps": 3,
  "usage": {"prompt_tokens": 1200, "completion_tokens": 240},
  "tool_calls": [{"name": "execute", "calls": 2, "errors": 0}],
  "errors": [],
  "images": []
}
```

`images` collects, in order, every image the run announced (`generate_image`, an image-carrying tool result, a subagent's), as a reference to where it was written — so a script consuming the summary can pick the files up.

`reason` is one of `completed | max_steps | time_limit | stopped | circuit_breaker`. `max_steps` only appears when the config caps the turn with a positive `max_steps`; the default budget (`max_steps = 0`) has no ceiling to hit.

## `stream-json`

One JSON object per line (JSONL) as events arrive, in Claude Code's shapes, ending with the `result` object:

```
{"type":"system","subtype":"init","session_id":"...","model":"...","tools":["Bash",...],"slash_commands":[...],...}
{"type":"assistant","message":{"id":"msg_...","role":"assistant","content":[{"type":"tool_use","id":"toolu_...","name":"Bash","input":{"command":"cargo test"}}],"usage":{...}},"session_id":"..."}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_...","content":"...","is_error":false}]},"session_id":"..."}
{"type":"assistant","message":{"content":[{"type":"text","text":"All tests pass."}],...}}
{"type":"result","subtype":"success","is_error":false,"result":"All tests pass.","reason":"completed","num_turns":2,"total_cost_usd":0.0003,"usage":{"input_tokens":2400,"output_tokens":240,"prompt_tokens":2400,"completion_tokens":240},...}
```

Each content block is its own `assistant` message, as in Claude Code. `--include-partial-messages` adds the `stream_event` lines (`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`) and a `system` `status` line before each model call. `--include-hook-events` writes hook activity as `system` `hook_response` lines instead of Wizard's `hook` lines. `--replay-user-messages` echoes user input lines back (with `isReplay`) when reading `--input-format stream-json`.

Lines Claude Code has no message for are still written, each with its own `type`: `notice`, `error`, `stream_retrying`, `hook`, `plan` (auto-approved, with the plan text; `omakase: true` on the chef's-choice path), `interview`, `todo`, `images`, `task_started` / `task_finished`, `subagent_started` / `subagent_finished`, `command_requested`, `ultra_guidance`, and `console_opened` / `console_output` / `console_closed`. Earlier versions wrote `tool_call`, `tool_result`, `text_delta`, `usage`, `step`, `turn_done` and a closing `done` line; the Claude-shaped lines and the `result` object replace them (`result` carries `reason` and `usage.prompt_tokens` / `completion_tokens`, the fields `done` had).

`--input-format stream-json` turns stdin into a conversation: one turn per `{"type":"user"}` line, each answered with its own `init` and `result`, until stdin closes. `control_request` lines get a `control_response` (`initialize`, `interrupt`, `set_model`; others are refused by name).

A hard error (config, provider unreachable) ends the process before the sink writes its summary, so a run that exits 1 emits **no** `result` line, no `json` object, and no text trailer. Read the exit code, not the last line. A run that ends on a limit still writes its `result`: `--max-budget-usd` gives `error_max_budget_usd` (exit 1), `--json-schema` that never validates gives `error_max_structured_output_retries` (exit 1).

In both structured formats the spinner and decorative headers are suppressed and stdout is pure JSON. Failures a run survives arrive in-band (the `errors` array in the `result`, `{"type":"error"}` lines in `stream-json`), and tracing diagnostics go to the log file under `~/.wizard/logs/` (see [logging.md](logging.md)). Stderr carries two things, both worth keeping: the `error: …` line a hard failure prints on its way to exit 1, and, when a quality gate is failing at the end of a run, the line naming it (the `json` summary object has no room for it and stdout stays pure JSON). Do not discard them with `2>/dev/null`.

## The claim of done is reviewed

A headless run ends when the model replies without calling a tool. That reply is
a claim, and on its own nothing checks it: a run that wrote no file and said
"Completed" used to exit 0 and look exactly like one that did the work.

So every headless run, one-shot or `--continuous` or started by the scheduler,
reviews the claim once before acting on it. A fresh subagent reads the original
request and the machine, checks that what the request named exists and is not
empty, runs the request's own acceptance command when it states one, and returns
`PASS` or `FAIL`. A `FAIL` goes back to the agent as one more turn, with what is
wrong; the rework turn does not consume `--loop`, which is the budget for the
work itself. The review's own bound is one round.

The review is skipped when the turn wrote nothing and ran nothing, when the claim
has already had its round, and in the last minute of `--max-hours`. The verdict arrives as a notice, so it
shows up in every output format: `~ review: PASS ...` under `text`, a
`{"type":"notice"}` line under `stream-json`. `--no-completion-review` turns it off, and
so does `completion_review = false`. See
[modes.md](modes.md#completion-review).

## Exit codes

The process exit code encodes why the run ended:

| Code | Meaning |
|------|---------|
| 0 | completed (or gracefully stopped via `.wizard/loop-control`) |
| 1 | hard error (config, provider unreachable, ...) |
| 2 | step budget exhausted — only when a positive `max_steps` caps the turn |
| 3 | circuit breaker |
| 4 | `--max-hours` time limit |
| 5 | a quality gate (`--gate`) was still failing when the run ended, whatever ended it (see [modes.md](modes.md#quality-gates)) |

Exit 3 covers three breakers: a sovereign run's same call *faulting* identically 6 times in a row, any one tool failing 8 times in a row, and the provider breaker opening after 8 consecutive transient model-call failures. Only a fault — a tool that could not be run at all — reaches the first one; a tool that ran and reported a non-zero exit or a missing file is diagnostic signal and never trips it, and 3 identical repeats are the nudge to change approach rather than the trip. Exit 0 also covers a user interrupt, not only `.wizard/loop-control`.

So a CI invocation can distinguish "the agent finished" from "the agent gave up":

```sh
wizard --mode sovereign -p "make the tests pass" --output-format json > result.json
case $? in
  0) echo ok ;;
  2) echo "ran out of steps" ;;
  *) echo "failed" ;;
esac
```

A real TUI session and the gateway are unaffected by `--output-format`; they exit 0 on a clean quit. Note that `wizard -p "task"` with no terminal on stdin/stdout falls through to the headless runner, which does honor `--output-format` and these exit codes — that is what makes the format usable from a pipe or a CI job without `--mode sovereign`.
