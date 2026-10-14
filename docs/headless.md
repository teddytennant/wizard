# Headless output formats and exit codes

A sovereign run (`wizard --mode sovereign -p "task"`, or any `-p` run without a terminal) prints to stdout. `--output-format` selects how:

## `text` (default)

The human-readable stream you get today: assistant deltas as they arrive, dimmed reasoning, `→ tool {args}` / `← tool [ok]` one-liners (`[error]` on a failure), a busy spinner on terminals, a `[turn done: Completed]` line per turn, and a trailer:

```
[run finished: Completed — 1200 prompt + 240 completion tokens]
```

The reason there is the Rust variant name (`Completed`, `MaxSteps`, `TimeLimit`, `Stopped`, `CircuitBreaker`), not the snake_case spelling the structured formats use, and the token clause is dropped when both counters are zero.

A subagent's tool calls print inline under the subagent's name (`→ researcher ▸ web_fetch {…}` / `← researcher ▸ web_fetch [ok]`); the TUI puts them on its [subagent rail](usage.md#the-subagent-rail) instead. The structured formats below label them the same way in their `name` fields.

## `json`

Silent until the run ends, then exactly one JSON object on stdout:

```json
{
  "result": "the assistant's accumulated text",
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

One JSON object per line (JSONL) as events arrive, terminated by a `done` line:

```
{"type":"tool_call","name":"execute","args":{"command":"cargo test"}}
{"type":"tool_result","name":"execute","is_error":false,"output":"..."}
{"type":"step","step":1}
{"type":"text_delta","text":"All tests pass."}
{"type":"usage","prompt_tokens":1200,"completion_tokens":240}
{"type":"turn_done","reason":"completed"}
{"type":"done","reason":"completed","usage":{"prompt_tokens":1200,"completion_tokens":240}}
```

Other line types you may see: `thinking_delta`, `notice`, `error`, `stream_retrying`, `hook`, `plan` (auto-approved, with the plan text; `omakase: true` on the chef's-choice path), `interview`, `todo`, `images`, `task_started` / `task_finished`, `subagent_started` / `subagent_finished`, `command_requested`, `ultra_guidance`, and `console_opened` / `console_output` / `console_closed`. `turn_done` closes each agent turn; the final `done` line carries the run outcome and total usage.

A hard error (config, provider unreachable) ends the process before the sink writes its summary, so a run that exits 1 emits **no** `done` line, no `json` object, and no text trailer. Read the exit code, not the last line.

In both structured formats the spinner and decorative headers are suppressed and stdout is pure JSON. Failures a run survives arrive in-band — the `errors` array in the `json` summary, `{"type":"error"}` lines in `stream-json` — and tracing diagnostics go to the log file under `~/.wizard/logs/` (see [logging.md](logging.md)). Stderr carries two things, both worth keeping: the `error: …` line a hard failure prints on its way to exit 1, and, when a quality gate is failing at the end of a run, the line naming it (the `json` summary object has no room for it and stdout stays pure JSON). Do not discard them with `2>/dev/null`.

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

Before the review, a sovereign run in a git repository gets four mechanical
checks that cost no model call: names the request states that appear nowhere in
the tree, tests that existed at the start and are gone, a commit the request
asked for and did not get, and a new Go test file that redeclares another's
names. A finding arrives as a `~ finish check: ...` notice and one more turn.
`finish_checks = false` turns them off. See [modes.md](modes.md#finish-checks).

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
