# Scheduler: `wizard schedule` and `wizard scheduler`

Cron-style scheduled runs. Entries live in `~/.wizard/schedule.toml`; the
`wizard scheduler` daemon fires each one as a headless wizard child process
at its cron times. Like `doctor`, the schedule commands are
self-contained: they never load `~/.wizard/config.toml`, never trigger
onboarding, and need no LLM in this process. The spawned jobs load their
own config exactly like a user-invoked `wizard --mode sovereign -p "..."`.

## schedule.toml

```toml
[[entries]]
name = "nightly-cleanup"        # unique key, [a-zA-Z0-9_-]+
cron = "0 3 * * *"              # standard 5-field cron, local time
prompt = "tidy the repo, run tests, commit"
cwd = "/home/user/proj"         # required: where the run executes
mode = "sovereign"              # optional, default "sovereign"; or "continuous"
max_hours = 2.0                 # optional wall-clock cap for the spawned run
enabled = true                  # optional, default true
once = false                    # optional: fire once, then remove the entry
expires_at = "2026-11-06T00:00:00Z"  # optional: remove the entry after this instant
```

Cron expressions are strict 5-field (`minute hour day month weekday`),
no seconds or year fields. Aliases like `MON-FRI` and `@daily` work.

They are validated on `wizard schedule add` and on `wizard schedule run`,
and **not** on load. An entry you hand-edit into an invalid cron is not an
error. The daemon reloads the file, logs one warning naming the entry, the
expression and the parse error, and then never fires it. `wizard schedule
list` is the other place it shows up — the `next` column reads `invalid
cron`.

`max_hours` is treated the same way: it must be a positive, finite number of
hours no greater than 8760 (a year), and an entry whose value is not is warned
about on load and never fired. `--max-hours` on the command line is checked
against the same rule, and a value that fails it is refused before the run
starts.

`max_hours` is passed to the child as `--max-hours`, and the run can see it. Every step's
request carries a line saying how much of the budget is left, which turns into "write the
deliverable now" at 60% spent and "stop changing things and land it" at 85%, naming any
output path the prompt asked for that is still not on disk. So a nightly entry with
`max_hours = 2.0` winds itself down before the daemon's kill at `max_hours + grace` ever
comes up, and a scheduled run that gets cut off is much more likely to leave the file it
was asked for behind. See [The clock](modes.md#the-clock).

`once` and `expires_at` are what the agent's `CronCreate` and `ScheduleWakeup`
tools write. A `once` entry is removed on the pass that fires it. An entry past
its `expires_at` is removed after firing if it was due, and on the first pass
after it otherwise.

## Scheduling from a session

The agent can queue work itself with the Claude Code tool names (load them with
`tool_search`):

- `CronCreate {cron, prompt, recurring?}` writes an entry named `cron-<id>` for
  the session's directory: `recurring: false` makes it `once`, and a recurring
  job expires after 7 days. Each run is capped at an hour. `durable` is accepted
  and changes nothing, because every entry is in `schedule.toml`.
- `CronList` and `CronDelete {id}` show and remove only those `cron-` entries.
- `ScheduleWakeup {delaySeconds, reason, prompt}` writes a one-shot `wakeup-<id>`
  entry 60 to 3600 seconds out, rounded up to a whole minute, replacing the last
  one for the directory. `stop: true` cancels it.

These run as a headless session when `wizard scheduler` is running, not inside
the live session, and the tool result says so when no daemon holds the lock.

## CLI

```bash
# Add an entry (validates the cron, the mode, and that --cwd exists;
# prints the next fire time). --mode defaults to sovereign; pass
# --mode continuous for a perpetual run:
wizard schedule add nightly-cleanup \
    --cron "0 3 * * *" \
    --prompt "tidy the repo, run tests, commit" \
    --cwd ~/proj --max-hours 2

# Table of name, cron, enabled, next fire (absolute + relative), cwd:
wizard schedule list

# Remove by name (error if absent):
wizard schedule remove nightly-cleanup

# Toggle an entry without removing it (enabled = true/false in the file;
# the daemon picks the change up on its next pass):
wizard schedule disable nightly-cleanup
wizard schedule enable nightly-cleanup

# Run one entry's job right now, in the foreground, with inherited stdio:
# the same child command the daemon would spawn. Exits with the child's
# exit code (0 completed, 2 max-steps, 3 circuit breaker, 4 time limit):
wizard schedule run nightly-cleanup
```

You can also edit `~/.wizard/schedule.toml` by hand; the daemon picks up
changes on its next pass. `enabled = false` keeps an entry in the file
without firing it (same effect as `wizard schedule disable`).

## The daemon

```bash
wizard scheduler
```

A long-running foreground process. Exactly one instance runs per user: the
daemon takes an exclusive lock on `~/.wizard/scheduler.lock` at startup, and
a second instance exits immediately with an error naming the lock (the lock
is released by the kernel on any exit, including SIGKILL, so it can never go
stale). Each pass (at least once a minute) it:

1. Reaps finished jobs and kills any past `max_hours` (plus a short grace:
   the child also receives `--max-hours`, so the normal path is a graceful
   self-stop; the kill is the backstop).
2. Reloads `schedule.toml`, so edits apply without a restart.
3. Fires every due entry: `current_exe()` spawned as
   `wizard --mode sovereign -p "<prompt>" --cwd <cwd> [--max-hours H]`
   (`--continuous` for `mode = "continuous"`). Entries due at the same time
   all spawn concurrently; runs are never serialized.
4. Sleeps until the next fire, floored at 1 s and capped at 60 s.

Semantics worth knowing:

- **No backfill.** Each entry's clock starts when the daemon starts (or at
  its last fire within this daemon's lifetime). Occurrences that passed
  while the daemon was down are skipped, and several occurrences missed
  during one sleep collapse into a single fire.
- **Logging.** One timestamped line per fire / finish / timeout to
  `~/.wizard/logs/scheduler.log` (rotated to `.log.old` past ~5 MB) and to
  stdout. Each job's stdout/stderr is captured to
  `~/.wizard/logs/jobs/<name>-<timestamp>.log` (the "finished" line names
  the file), so a job that dies before writing any of its own state still
  leaves evidence. The newest 10 logs per entry are kept; older ones are
  pruned automatically.
- **Shutdown.** Ctrl-C (SIGINT) kills running jobs and exits 0.

## Running it in the background

`wizard scheduler` stays in the foreground on purpose. To have it supervised
instead — surviving logout and starting at boot — install it:

```bash
wizard scheduler install     # systemd user unit, or a launchd agent on macOS
wizard scheduler status
wizard scheduler logs -f     # same lines as scheduler.log
wizard scheduler start       # stop / start / restart an installed service
wizard scheduler stop
wizard scheduler restart     # after replacing the binary
wizard scheduler uninstall
```

`install` on an empty `schedule.toml` says so rather than leaving you with a
daemon that looks broken but is only idle.

The unit points at the absolute path of the running binary and runs the daemon
from your home directory: every entry carries its own `cwd` and each child is
started there, so the daemon's own directory is never a project. On Linux,
`install` also tells you whether lingering is on — without it a user service
stops when you log out. On Termux or a Linux without systemd it refuses and
names the alternative rather than writing a unit nothing will read.

See [Services](services.md) for the unit it writes and what environment it
carries. To write one yourself instead:

```ini
[Unit]
Description=wizard scheduler

[Service]
ExecStart=%h/.local/bin/wizard scheduler
Restart=on-failure

[Install]
WantedBy=default.target
```
