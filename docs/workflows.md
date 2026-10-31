# Workflows

A workflow is a JavaScript script that fans subagents out. The model writes it
(or you do), Wizard runs it in the background, and the conversation receives
only the result. The script holds the plan, the loops and the intermediate
results; the agents do the work. This is Claude Code's `Workflow` tool, with the
same input schema, script API and run ids, so scripts written for one run in the
other.

```
> use a workflow to review every file I changed and have a second agent try to
  refute each finding
```

Wizard only starts a workflow when you ask for one in your own words ("use a
workflow", "fan out agents") or name a saved one. A task that would merely
benefit from parallelism gets a plain subagent, or a question.

## A script

```js
export const meta = {
  name: 'review-changes',
  description: 'Review changed files across dimensions, verify each finding',
  phases: [{ title: 'Review' }, { title: 'Verify' }],
}

const FINDINGS = { type: 'object', properties: { findings: { type: 'array', items: {
  type: 'object', properties: { file: { type: 'string' }, title: { type: 'string' } },
  required: ['file', 'title'] } } }, required: ['findings'] }
const VERDICT = { type: 'object', properties: { isReal: { type: 'boolean' } }, required: ['isReal'] }

const results = await pipeline(
  [{ key: 'bugs', prompt: 'Review the diff for bugs' }, { key: 'perf', prompt: 'Review the diff for slow code' }],
  d => agent(d.prompt, { label: `review:${d.key}`, phase: 'Review', schema: FINDINGS }),
  review => parallel(review.findings.map(f => () =>
    agent(`Try to refute: ${f.title} in ${f.file}`, { phase: 'Verify', schema: VERDICT })
      .then(v => ({ ...f, verdict: v })))))
return results.flat().filter(Boolean).filter(f => f.verdict?.isReal)
```

`export const meta = {...}` comes first and is a pure literal: `name` and
`description` are required, `phases` lists the titles you pass to `phase()`. The
rest is plain JavaScript with top-level `await` and `return`. What it returns, as
JSON, is the run's result.

| In the script | Does |
| :- | :- |
| `agent(prompt, opts)` | One subagent. Resolves to its final text; with `opts.schema` to the validated object. `null` if the agent dies or is stopped. |
| `parallel(thunks)` | Runs `() => promise` functions at once, waits for all. A thunk that throws gives `null`. |
| `pipeline(items, ...stages)` | Each item through every stage, no barrier between stages. A stage gets `(previous, item, index)`; a throw drops that item to `null`. |
| `phase(title)` | Groups the agents that follow in the progress view. |
| `log(message)` | A progress line. `console.log` is the same. |
| `workflow(nameOrRef, args)` | Runs another workflow inline, one level deep. |
| `args` | The tool's `args`, verbatim. |
| `budget` | `{total: null, spent(), remaining()}`. Wizard has no per-turn token target, so `total` is `null`. |

`agent` options: `label`, `phase`, `schema`, `model`, `agentType` (the name of a
subagent, default `worker`), `disallowedTools`, `isolation: 'worktree'` (the agent
works in its own git worktree, removed if it changed nothing, kept and named in the
result if it did). `effort` and `stallMs` are accepted and ignored.

A `schema` needs `{type: 'object', properties: {...}}` at the root, with `required`
inside `properties`; a schema nothing can satisfy is refused with the
contradiction named. The agent answers through a `StructuredOutput` tool, and a
wrong shape goes back to it as an error to fix, up to five times.

Scripts are deterministic so a rerun can be resumed: `Date.now()`, `Math.random()`
and `new Date()` with no argument throw (pass a timestamp in `args`). There is no
filesystem, network or `import` in the script; agents do the work.

`/workflow-authoring` prints the full authoring reference (quality patterns,
resume, worked shapes). The model reads it before writing a script.

## Running one

The model calls the `Workflow` tool with `script`, `name` or `scriptPath`, and
optionally `args`. In the terminal UI it returns at once with a run id and the run
continues as a background task: the task list shows it, `task_output` shows its
progress lines, `TaskStop` cancels it, each agent appears on the subagent rail,
and when it ends the result is added to the conversation like any finished
background task. In `-p`, the gateway and ACP, where nothing can be told later, the
tool waits and returns the result itself.

The script is saved with every launch, and its path comes back in the tool result.
To iterate, edit that file and call `Workflow` with `{scriptPath}`.

### Limits

| | Default | Change |
| :- | :- | :- |
| Agents running at once | `min(16, max(2, cpus - 2))` | `WIZARD_WORKFLOW_MAX_CONCURRENT_AGENTS` or `CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS`, 1 to 256 |
| Agents one run may start | 1000 | `WIZARD_WORKFLOW_MAX_AGENTS`, 1 to 100000 |
| Items in one `parallel()` or `pipeline()` | 4096 | |
| One agent's wall time | 30 minutes | |

The 1000 is a backstop against a loop that never ends, not a budget. The call that
would be number 1001 throws. Every agent is a full subagent with its own model
calls, so a large fan-out costs what that many subagents cost; the permission
rules, hooks and plan mode apply to their tool calls as to any subagent's.
`CLAUDE_CODE_DISABLE_WORKFLOWS=1` removes the tool.

## Resume

Every run is a directory under `~/.wizard/workflows/runs/<run id>/`:

| File | Holds |
| :- | :- |
| `run.json` | status, args, agent counts, phases, failures, result |
| `script.js` | the script as launched |
| `journal.jsonl` | one line per finished `agent()` call: its number, a digest of the prompt and options, and its result |

To continue after a stop, a failure or an edit, call `Workflow` again with the
script and `resumeFromRunId`. The run must not be running. Calls are replayed in
the order the script makes them: a call whose prompt and options (schema, model,
agent type, isolation, disallowed tools; not the label or phase) are unchanged
returns its saved result. The first call that differs, never finished or failed
runs again, and so does every call after it, even one that finished. Same script,
same `args`: nothing runs.

Claude Code only resumes inside the session that started the run. Wizard keeps
runs on disk, so a run can be resumed from a later session.

## Saved workflows

A `*.js` file named with lowercase letters, digits, `-` and `_` in a `workflows/`
directory is a saved workflow, run with `Workflow({name, args})`:

- `.wizard/workflows/` and `.claude/workflows/` in the project, or any parent
  directory up to the home directory (`.wizard` wins over `.claude` in the same
  directory, the closest directory wins, and a project beats the home directory)
- the same two directories in your home directory

The name is `meta.name`; the file name works too. A file that fails to parse is
listed with the reason by `/workflows` and skipped.

## Bundled workflows

Three ship in the binary and run by name, like a saved one:

| Name | Takes `args` | Does |
| :- | :- | :- |
| `deep-research` | the question | Splits it into search angles, searches each, drops duplicate URLs, reads the best twelve sources for checkable claims, has three skeptics try to refute each claim (two refutations kill it), and writes a cited report with confidence, caveats and open questions. |
| `code-review` | a target such as a branch or PR number, optional | Five reviewers (correctness, security, concurrency, tests, simplicity) read the change; two skeptics try to refute each finding; what survives comes back ranked. |
| `batch` | the instruction | A planner splits the change into units that own different files, one worker per unit edits in its own git worktree, a reviewer checks each unit's diff. Nothing is merged or pushed; each unit's result names its worktree. |

`/deep-research <question>` and `/batch <instruction>` are commands that send the
prompt Claude Code's workflow commands send ("Run the "deep-research" workflow. ...
Invoke: Workflow(...)"), so the model calls the tool with your text as `args`.
`/code-review` stays the single-agent review prompt; run the workflow with "use the
code-review workflow". A file named `deep-research.js` (and so on) in a
`workflows/` directory replaces the bundled one, and
`CLAUDE_CODE_DISABLE_BUNDLED_SKILLS=1` hides all of them. The scripts are
`src/workflow/bundled/*.js`; they are written for Wizard's tools (`WebSearch`,
`WebFetch`, `git`) and are not copies of Claude Code's.

## `/workflows`

```
/workflows                  open the panel (a text list where there is no panel)
/workflows list             list runs, newest first, and saved workflows
/workflows <run id>         details of one run, its agents included (a prefix works)
/workflows stop <run id>    stop a run, in this process or another
```

A run recorded as running whose process is gone shows as `interrupted`. A run that
belongs to another process is stopped by writing a `stop` file in its run directory,
which that process checks every second. `TaskStop` with the task id from the launch
text does the same as `/workflows stop` for the live session.

### The panel

In the terminal UI and in the claude look, `/workflows` opens a panel that follows
the runs on disk. With one run (or one running) it opens on that run: its name and
description, `done/total agents` and elapsed time on top, its phases on the left
(`1 Scan   1/2`) and the agents of the selected phase on the right. Enter goes one
level in (phase, agents, one agent's prompt and outcome), Esc comes back and then
closes, the arrows select, and `x` stops the run. With several runs it opens on the
list of runs first.

The state behind it is `agents.json` in the run's directory, written about once a
second by the process running the workflow: each `agent()` call's label, phase,
subagent, state (`queued`, `running`, `done`, `cached`, `failed`, `stopped`), times,
prompt and outcome (cut at 1500 characters; the journal has the whole result). Any
reader can use it, and the look does: it is a separate process that reads the files
and shares no code with Wizard.

Compared with Claude Code 2.1.314's panel (driven with a fake Messages server and a
three-agent script, four screens captured at 140x45), the look's panel matches cell
by cell, colours included, in every cell except the model and token column (Wizard
shows the subagent that ran it, and has no per-agent token count), the clocks, and
the key hint line, which lists only the keys Wizard has. Not built: pause, restart,
per-agent stop, filter, save-as-workflow, the pill under the prompt that shows a
running workflow (`◯ name ▰▰▱▱ 1/2`), and the "Running in background" line in the
tool result.

## Settings and permissions

| Key | Meaning |
| :- | :- |
| `disableWorkflows` | `true` removes the tool (any settings layer). |
| `enableWorkflows` | `false` removes it too; the `/config` toggle in Claude Code. |
| `workflowSizeGuideline` | `unrestricted`, `small` (fewer than 5 agents), `medium` (fewer than 10, the default) or `large` (fewer than 50). Added to the tool's description as Claude Code words it; advisory, not a limit. |

With permissions on (a named mode or rules), a `Workflow` call is asked about, in
`default` and `acceptEdits`, with "Review dynamic workflow before running"; the
prompt lists the script's description and phases. `bypassPermissions` runs it,
`dontAsk` refuses it, `plan` refuses it. Rules:

- `Workflow` allows or denies every call.
- `Workflow(deep-research)` names a saved or bundled workflow. A call with a
  `scriptPath` is never covered by a name rule, whatever its `name` says.
- A deny rule answers "Workflow <name> blocked by permission rules"; an ask rule
  asks even when an allow rule also matches.
- "Yes, and don't ask again" on a named workflow writes `Workflow(<name>)`.

## What the default build does

Workflows need QuickJS, the `plugin-js` Cargo feature, which is on in the default
build (about 1.5 MB). Without it there is no `Workflow` tool and nothing registers
it; `/workflows` still lists and shows runs on disk and `/workflow-authoring` still
prints the reference.

## Where it differs from Claude Code

- Runs are tasks of the session. `TaskStop` and `task_output` stop and read one, and
  the panel has no pause, per-agent stop or restart.
- The approval prompt lists the phases from the script's `meta` and offers yes, yes and
  don't ask again, and no. Claude Code's also offers "View workflow summary", "View raw
  script" and editing the script in `$EDITOR`.
- No remote (CCR) runs, usage-limit waiting, stall restarts (`stallMs` is ignored),
  prefix-cache staggering, size warning, or `ultracode` keyword or mode
  (`workflowKeywordTriggerEnabled` is read and ignored).
- `agent()` options `bashCommandClamp` and `isolation: 'remote'` are not supported.
- `effort` is accepted and ignored; subagents use the configured effort.
- Bundled: Claude Code 2.1.314 ships one workflow, `deep-research`; Wizard ships that
  and `code-review` and `batch`, with its own scripts.
