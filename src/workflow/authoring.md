# Workflow authoring reference

A workflow is a JavaScript script that orchestrates many subagents. The script
holds the plan: what fans out, what verifies, what gets synthesized. Use one
when the work is too wide for one context (audits, migrations, broad sweeps),
or when a result should be checked by independent agents before it is trusted.
Do not run one unless the user asked for multi-agent orchestration in their own
words or named a saved workflow. A task that would merely benefit from
parallelism is not an opt-in; for that, use `spawn_subagent`, or describe what a
workflow would do and ask.

Often the right shape is hybrid: scout inline first (list the files, scope the
diff) to find the work-list, then call `Workflow` to pipeline over it.

## Calling the tool

`Workflow` takes one of `script` (inline source), `name` (a saved workflow) or
`scriptPath` (a file). Optional: `args`, any JSON value, exposed to the script
as the global `args`; `resumeFromRunId`, see Resume. Pass the script inline. Every
launch saves its script and returns the path; to iterate, edit that file and call
again with `{scriptPath}` instead of resending the source.

The call returns at once with a run id. The run goes on in the background as a
task: `TaskStop` stops it, `task_output` shows its progress, and when it ends
you are told, with the result at the end of the task output. In a headless run
(`-p`) or any surface that cannot be told later, the call waits and returns the
result itself. `/workflows` lists runs, shows one, and stops one.

## The script

It must start with `export const meta = {...}`, a pure literal (no variables, calls,
spreads or interpolation):

    export const meta = {
      name: 'find-flaky-tests',
      description: 'Find flaky tests and propose fixes',
      phases: [
        { title: 'Scan', detail: 'grep test logs for retries' },
        { title: 'Fix', detail: 'one agent per flaky test' },
      ],
    }
    phase('Scan')
    const flaky = await agent('grep CI logs for retry markers', { schema: FLAKY })

`name` and `description` are required. `phases` is optional; use the same titles
in `phase()` calls. The body is plain JavaScript (not TypeScript) with top-level
`await` and `return`. Whatever it returns is the run's result and must be JSON.

### Functions the script has

- `agent(prompt, opts?)` starts one subagent and resolves to its final text. With
  `opts.schema` (a JSON Schema whose root is `{type: 'object', properties}` and
  whose `required` is a subset of `properties`) the agent must answer through a
  `StructuredOutput` tool and `agent()` resolves to the validated object. It
  resolves to `null` if the agent is stopped or dies; filter with
  `.filter(Boolean)`. Other options: `label` (shown in progress), `phase`,
  `model`, `agentType` (a subagent name; default `worker`), `disallowedTools`
  (tool names to take away), `isolation: 'worktree'` (a fresh git worktree,
  removed if the agent changed nothing, kept and named otherwise). `effort` and
  `stallMs` are accepted and ignored.
- `parallel(thunks)` runs `() => promise` thunks at once and waits for all. A
  thunk that throws gives `null`. It is a barrier; use it only when you need
  every result together.
- `pipeline(items, stage1, stage2, ...)` runs each item through all stages with no
  barrier between stages. Each stage gets `(previousResult, item, index)`. A stage
  that throws drops that item to `null`. This is the default for multi-stage work.
- `phase(title)` groups the agents that follow; `log(message)` prints a progress
  line; `workflow(nameOrRef, args?)` runs another workflow inline (one level of
  nesting) and returns its result; `console.log` is `log`.
- `args` is the tool's `args`, or `undefined`. `budget` is `{total: null,
  spent(), remaining()}`: Wizard has no per-turn token target, so `total` is
  always `null` and `remaining()` is `Infinity`; `spent()` is the output tokens
  the session has used since the run began.

`Date.now()`, `Math.random()` and `new Date()` with no argument throw, because
they would make a rerun differ from the run it resumes. Pass a timestamp in
`args` if the script needs one. Scripts have no filesystem, network or `import`;
agents do the work and the script only decides what to ask them.

### Limits

At most 4096 items go into one `parallel()` or `pipeline()`. At most
`min(16, max(2, cpus - 2))` agents run at once; the rest queue. A run starts at
most 1000 agents. `WIZARD_WORKFLOW_MAX_AGENTS` and
`WIZARD_WORKFLOW_MAX_CONCURRENT_AGENTS` change them.

## Pipeline by default

    const results = await pipeline(
      DIMENSIONS,
      d => agent(d.prompt, { label: `review:${d.key}`, phase: 'Review', schema: FINDINGS }),
      review => parallel(review.findings.map(f => () =>
        agent(`Adversarially verify: ${f.title}`, { phase: 'Verify', schema: VERDICT })
          .then(v => ({ ...f, verdict: v })))))
    return results.flat().filter(Boolean).filter(f => f.verdict?.isReal)

A dimension's findings are verified while the other dimensions are still being
reviewed. A barrier (`await parallel(...)` followed by more work) is right only
when a stage needs all of the previous stage's results at once: dedup across the
whole set, an early exit when the total is zero, or a prompt that compares
findings.

## Quality patterns

- Adversarial verify: several skeptics per finding, each told to refute it. Drop
  the finding if most refute.
- Perspective-diverse verify: give each verifier a different lens (correctness,
  security, does-it-reproduce) instead of N identical refuters.
- Judge panel: independent attempts from different angles, scored by judges,
  synthesized from the winner.
- Loop until dry: keep spawning finders until K rounds in a row return nothing new.
  Dedup against everything seen, not only what was confirmed, or it never ends.
  Put a hard cap on the loop.
- Multi-modal sweep: agents searching different ways (by file, by content, by time).
- Completeness critic: a last agent asking what is missing.
- No silent caps: if you bound coverage, `log()` what was dropped.

Scale to the request. "Find any bugs" is a few finders and a single-vote verify;
"audit this thoroughly" is a wider pool, three to five votes and a synthesis stage.

## Resume

The tool result carries a run id. To continue after a stop, a failure or an edit,
call `Workflow({scriptPath, resumeFromRunId})`. The run must not be running (stop
it first). Agent calls are replayed in the order the script makes them. A call
whose prompt and options are unchanged returns its saved result. The first call
that differs, was not finished, or failed runs again, and so does every call
after it, even one that finished. The same script with the same `args` is a full
cache hit. Saved results are in `journal.jsonl` in the run's directory; read it
before assuming a cached result holds something.

## Saved workflows

A file in `.wizard/workflows/` or `.claude/workflows/` (the project or any parent
directory, or the home directory) named like `my-flow.js` is a saved workflow:
call it with `Workflow({name: 'my-flow', args})`. The closest definition of a name
wins, and a project's beats the home directory's.

## Wizard and Claude Code

Run ids, resume, `meta`, the script functions and their error messages follow
Claude Code 2.1.314. Where Wizard differs: a run is a task of the session, so
`TaskStop` is how it stops; resume works across sessions because runs live on disk
under `~/.wizard/workflows/runs/`; there is no `remote` mode, `bashCommandClamp`,
or approval dialog (permission rules apply to the agents' own tool calls).
