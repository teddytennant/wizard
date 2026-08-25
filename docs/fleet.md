# Fleet: `wizard fleet`

Parallel sovereign workers over git worktrees. One coordinator process plans
a mission, fans the resulting tasks out to up to N headless `wizard` children
(each in its own worktree on its own branch), supervises them, and finally
merges the fleet branches back into the current branch.

```bash
wizard fleet run -n 3 -p "raise test coverage of the parser and document the public API"
wizard fleet status   # task table: queued | running | done
wizard fleet stop     # ask a running fleet to wind down
```

`fleet run` must be invoked inside a git repository with at least one commit,
since workers run in `git worktree`s. It loads `~/.wizard/config.toml` (the
planning and synthesis turns drive a real in-process agent); `status` and
`stop` only touch the project's `.wizard/fleet/` directory.

## Lifecycle

1. **Planning**: one in-process agent turn decomposes the mission into at
   most `2 × N` independent tasks (`{id, title, prompt, files_hint}`). The
   reply is parsed liberally (fenced or prose-wrapped JSON works); one retry
   quotes the parse error back at the model. Each task lands in
   `queue/<id>.json`.
2. **Running**: the coordinator creates one worktree per worker slot
   (`.wizard/fleet/worktrees/<i>`, branch `fleet/<i>-<slug>`; a timestamp
   suffix is appended when a previous run already took the name) and then
   supervises on a 1-second tick:
   - **claiming**: the coordinator claims tasks for workers by atomically
     renaming `queue/<id>.json` into `claimed/` and spawns
     `wizard --mode sovereign -p "<task>" --cwd <worktree> --output-format json`
     with `WIZARD_FLEET=1`, at most N
     children at a time. Worker prompts end with three standing instructions:
     commit your changes with a descriptive message, do not push, and never
     commit anything under `.wizard/`.
   - **reaping**: when a child exits, its exit code, branch, and parsed JSON
     summary (the `--output-format json` object from stdout) are written to
     `results/<id>.json`, and the slot's worktree is reused for the next
     queued task.
   - **watchdog**: a child running past `[fleet] max_minutes` is killed and
     recorded as timed out.
   - **heartbeat**: `.wizard/fleet/heartbeat` is touched every tick.
   - **stop**: a `stop` sentinel (written by `wizard fleet stop`) or ctrl-c
     kills the children, marks `fleet.toml` stopped, and skips synthesis.
3. **Synthesis**: once the queue drains and every child has exited, the
   coordinator runs a second in-process turn in the MAIN checkout (never a
   worktree): merge each fleet branch into the current branch sequentially,
   resolve trivial conflicts, abort and report anything non-trivial. Nothing
   is ever forced; failed merges are left to you, on their branches. Set
   `[fleet] synthesize = false` to skip the merge and just print the branch
   list and results table.
4. **Teardown**: worktrees are removed, branches are kept (also on failure
   or stop, so no work is ever lost), `fleet.toml` flips to `done`, and the
   final task table prints. `fleet run` exits 0 only when the fleet completed
   and every task exited 0; any failed, timed-out, or killed task (or a stop)
   exits 1, so a fleet run can gate CI.

## File layout

Project-local, rooted at the current directory:

```
.wizard/fleet/
├── fleet.toml          # mission, worker count, status, child pids
├── queue/<id>.json     # tasks not yet claimed
├── claimed/<id>.json   # tasks claimed by a worker slot
├── results/<id>.json   # task_id, title, branch, exit, timed_out, summary
├── worktrees/<i>/      # per-slot git worktree (removed at the end)
├── logs/<id>.stdout    # raw child output (the JSON summary)
├── logs/<id>.stderr    # child diagnostics
├── heartbeat           # unix timestamp, touched every supervision tick
└── stop                # sentinel written by `wizard fleet stop`
```

`fleet.toml` status walks `planning → running → synthesizing → done` (or
`stopped`). Each `wizard fleet run` resets `queue/`, `claimed/`, `results/`,
and `logs/` from the previous run.

## status and stop

`wizard fleet status` prints the mission, the fleet status line, live child
pids, and a per-task table:

```
task        state    exit  branch                          title
add-tests   done     0     fleet/0-raise-test-coverage-of  Add parser tests
write-docs  running  -     -                               Document the API
fix-lints   queued   -     -                               Fix clippy lints
```

Columns are sized to their contents; nothing is truncated. The branch slug is
the mission kebab-cased and cut to 24 characters, which is why the branch for
the mission above is `fleet/0-raise-test-coverage-of`.

`exit` is the child's exit code (the headless map: 0 completed/stopped,
1 hard error, 2 max-steps, 3 circuit breaker, 4 time limit), `timeout` for a
watchdog kill, or `killed` for signal death / shutdown.

While the fleet is `running`, `status` also reports the coordinator's
heartbeat age. The heartbeat is touched every supervision tick, so an age
past ~30 s means the coordinator was killed without cleaning up
(`stale (42s old — coordinator likely dead)`); the `running` status in
`fleet.toml` can then no longer be trusted.

`wizard fleet stop` writes the stop sentinel and returns immediately; the
coordinator winds down on its next tick. Ctrl-c on the coordinator behaves
the same way. When no fleet is live, `stop` prints "no fleet is running",
writes no sentinel, and exits 1. It also clears a stale sentinel a previous
no-op stop left behind — on that path only (none ever ran, or the last one
already finished). The other one, where `fleet.toml` still says "running"
but the coordinator's heartbeat has gone stale, refuses without touching the
sentinel.

## Config

```toml
[fleet]
max_minutes = 30   # per-worker wall-clock cap; the watchdog kills past it
synthesize = true  # false: skip the merge turn, just print branches + table
```

## Caveats

- Workers share no state: tasks must be genuinely independent. The planning
  prompt asks for non-overlapping files, but the model can get this wrong;
  overlapping tasks surface as merge conflicts at synthesis, where
  non-trivial ones are reported, not forced.
- Branches are kept on every path (completed, failed, timed out, stopped),
  so partial work is always recoverable with a manual `git merge`.
- A worker slot reuses its worktree (and branch) for consecutive tasks, so
  one `fleet/<i>-<slug>` branch can carry commits from several tasks.
- The coordinator claims tasks; workers never touch the queue. Ending the
  coordinator *gracefully* — ctrl-c, `wizard fleet stop`, a normal exit —
  also kills its workers, because the child handles are `kill_on_drop`. A
  `SIGKILL`ed or OOM-killed coordinator never runs that drop, so its workers
  are orphaned and keep going; the stale heartbeat is the only signal, and
  the pids in `fleet.toml` are how you find them.

## Build

Fleet mode is a plugin, behind `--features fleet`, on by default. Every
published binary has it and nothing needs doing. On a build without it,
`wizard fleet` still parses — the subcommand is part of the CLI either way —
and prints one line naming the flag. `[fleet]` in `config.toml` is still read
and round-tripped there, so a shared config file works on both builds. See
[plugins.md](plugins.md).
