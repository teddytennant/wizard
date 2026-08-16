# Interactive commands

A shell command the agent runs can ask the user a question, and the user can
answer it.

That sounds like it should always have been true. It was not: every command
Wizard spawned got `/dev/null` on file descriptor 0, so an installer asking
`Do you want to continue? [Y/n]` read end-of-file and either aborted or spun,
and its output was buffered until the process exited, so nobody ever saw the
question in the first place. The visible symptom was a command that appeared to
hang and was then killed at the timeout, with a composer whose Enter key did
nothing useful.

Both halves had to be fixed together. Delivering the user's keystrokes to a
prompt nobody can see is worth nothing, and showing a prompt nobody can answer
is worth less.

## What a child process sees

`execute` now has two run paths, and which one it takes is decided by a
declaration from the surface, not by a guess about the command:

| | fd 0 | output | timeout |
|---|---|---|---|
| **`ConsoleAccess::None`** (default) | `/dev/null` | buffered, delivered when the process exits | wall clock, exactly as before |
| **`ConsoleAccess::Interactive`** | a pipe held open by Wizard | streamed as it is produced | a budget of *unattended* time |

The interactive path attaches a **pipe**, not a pseudo-terminal. That is a
deliberate and load-bearing choice:

- **`isatty(0)` does not change answer.** `/dev/null` is not a terminal and
  neither is a pipe, so it returned `false` before and returns `false` now.
  Nothing that branches on it changes behaviour: no colour that was off turns
  on, no progress bar starts animating, no pager starts paging, no line
  buffering flips to block buffering. A pty would have changed the answer for
  *every* command, not just the ones that prompt, and "we fixed installers by
  changing how `git log` behaves" is not a trade worth making.
- **The cost is `/dev/tty`.** A program that deliberately bypasses its own stdin
  and opens the controlling terminal directly is not reachable this way.
  `sudo`'s password prompt does this, and so does `ssh`'s host-key
  confirmation. They did not work before either — this is a limitation the
  change does not lift, not a regression it introduces. `sudo -S` reads the
  password from stdin and does work.
- **One behaviour genuinely does change.** A command that reads stdin and gets
  no answer used to see EOF immediately; on the interactive path it blocks until
  somebody answers or the timeout fires. `cat` with no arguments, or `grep foo`
  with no file, will now sit there instead of returning at once. It is bounded
  by the timeout, and Ctrl-D sends EOF on demand, but it is a real difference and
  it is why the pipe is only attached where a human is watching.

## Which surface does what

`ConsoleAccess` is declared by the surface's agent builder, because only the
surface knows whether there is a person in front of it. It is deliberately
**not** derived from "does this run have an event channel" — a headless run has
one of those too; what it does not have is somebody reading it.

- **TUI** (`wizard`): `Interactive` (`src/app/session.rs:155`). This is the
  surface the bug was reported from and the one that works end to end.
- **The window** (`wizard gui`, a `--features native` build): `Interactive`. It
  is the same process as the child it would be driving, it dies when the agent
  dies, and it has a person in front of it — the same condition the TUI meets —
  so `TaskManager::attended` builds its tasks with `ConsoleAccess::Interactive`
  (`src/gui/tasks.rs`) and the window claims the gate
  (`src/native/console.rs`). See `docs/native-gui.md`.
  `TaskManager::with_registry`, the constructor that does *not* say this, leaves
  `ConsoleAccess::None` in place: a caller that has not claimed a keyboard keeps
  `/dev/null` on fd 0, because announcing a prompt nobody can answer would park
  the turn on a question with no keyboard behind it.
- **Headless** (`wizard -p`), **gateway**, **ACP**, **fleet**: `None`. Same
  reasoning, more obviously.
- **Subagents**, everywhere: `None`, forced, plus `events: None`. A subagent's
  prompt would be announced on a stream the user's composer is not bound to, and
  the only party in a position to answer it would be the model that asked.
- **Background tasks** (`execute` with `run_in_background: true`): `/dev/null`,
  unchanged. A detached task has no composer bound to it by construction.

## The mechanism

The same shape as plan review and the interview tool, which is why it is worth
recognising rather than reading from scratch. The request travels the event
stream as plain data and the channel that answers it waits at a process-wide
desk (`GateDesk` in `src/agent/event.rs`):

1. `execute` opens a `ConsoleGate` and emits `AgentEvent::ConsoleOpened` before
   the child can produce a single byte.
2. A surface with a human claims the gate. **`ConsoleGate::claim` succeeds
   exactly once**, so a stream teed to a renderer, a recorder and a mesh peer
   still has exactly one author of what the child reads.
3. Everything the child writes is announced as `AgentEvent::ConsoleOutput`,
   stdout and stderr interleaved in arrival order, as a terminal would show
   them. The tool result still separates them for the model.
4. What the user types goes back through the claimed `ConsoleWriter` as a
   `ConsoleInput::Line`, with the newline appended.
5. `AgentEvent::ConsoleClosed` when the command ends, and the ticket is voided
   so nothing can type at a dead pipe.

The model never sees an `AgentEvent`, so it cannot claim a console. Over the
mesh the `gate` field is redacted to ticket 0 (`src/mesh/turn.rs`), which is
never issued, so watching a peer's session never becomes typing into a peer's
shell.

## The timeout

A wall clock is the wrong instrument for a command that is waiting on a person.
Two minutes blocked on `[Y/n]` because somebody went to make coffee is not a
hung command, and killing it turns a working prompt into the failure the prompt
was meant to replace. But a pure inactivity timer is wrong in the other
direction: a genuinely wedged command that dribbles a byte a minute would run
forever.

So `timeout_secs` is a budget of **unattended** time. The clock stops on a
conjunction of two facts, neither of which is a guess about what the child is
doing:

1. **A surface is holding the console's writer** — it claimed the gate and has
   not detached. If nobody ever claimed it, the full wall clock applies from the
   start (`an_unattended_prompt_still_times_out` pins this).
2. **The child's last output did not end in a newline, and it has been quiet for
   400 ms since.** That is the shape of a question in every shell, installer and
   REPL: the cursor is parked at the end of the line the answer goes on. Work in
   progress writes whole lines; `sleep 30` writes none at all. Both keep their
   wall clock.

Answering **restarts** the budget from zero rather than resuming it: a human who
just typed has proved the command is alive, and the next step of an install
deserves the allowance the first one got.

The failure modes are asymmetric on purpose. Reading a prompt as work costs the
command its old wall-clock timeout, which is no worse than before. Reading work
as a prompt cannot silently hang anything, because it also takes a human sitting
there — and Ctrl-C reaches the process group from inside the parked call.

## At the keyboard (TUI)

While a command owns the composer:

| key | does |
|---|---|
| Enter | send the line to the command, newline included. **An empty line is sent** — that is how a person accepts `[Y/n]`. |
| Ctrl-D | close the command's stdin, as in a terminal. Does *not* quit Wizard. |
| Ctrl-B | background it. The child keeps running as a background task, the tool call returns at once with the task id, and the turn carries on. |
| Esc | detach. The command keeps running and goes back on the wall clock; Enter talks to the agent again. |
| Ctrl-C | stop the command — the turn's cancel handle reaches the parked call, which kills the whole process group. |

Ctrl-B works whether or not the command has asked anything, which is the point:
the commands worth backgrounding are the quiet ones — the build, the test suite,
the sync that is going to take four minutes — and those never take the composer
in the first place. The hint appears in the shortcuts bar for as long as a
foreground command is running.

**Under tmux, press it twice.** `Ctrl-B` is tmux's prefix key and never reaches
the application; pressing it again sends a literal one through. The shortcuts
bar detects `$TMUX` and says `Ctrl+b Ctrl+b` there so it names a key that
actually works.

What the task inherits: the output already captured is seeded into its buffer,
so `task_output` shows the whole command and not just the part after the key.
What it loses is stdin — a backgrounded command reads EOF, exactly as one
started with `run_in_background` does. Its clock restarts at the background
timeout (30 minutes) rather than the foreground one it just escaped.

From there it is an ordinary background task: it appears on the rail under
`Tasks`, `↓` reaches it, Enter opens its live output, Ctrl-X stops it, `/bashes`
lists it, and the model is notified when it finishes.

The line goes to the child **verbatim**: no trimming, no `/command` parsing, no
`@file` expansion. An installer asking for a prefix wants `/usr/local`, and
`/usr/local` is not a slash command.

> **What you type at a console is echoed on screen and into the command's
> transcript card.** It is not masked, because "is this a password prompt?" is a
> guess and a wrong guess in the safe-looking direction is the worse one. The
> echoed text is not sent to the model — the live tail is cleared when the call
> is answered, and the model reads the command's own output, not yours — but it
> is on screen and in the session's rendered history. `sudo` reads its password
> from `/dev/tty` and never reaches a console at all; for anything that does
> read a secret from stdin, prefer a file or an environment variable.

Three things say which mode Enter is in, because a composer that silently means
something else would be a worse bug than the one this fixes:

- the rule above the composer becomes a labelled band, `─ ▶ stdin → npm init ──`
- the prompt glyph changes from `❯` to `▶`, in the warning colour
- the status hints read `Enter → command · Ctrl-D end input · Esc detach ·
  Ctrl-C stop · Ctrl-B background`

The command's card in the transcript fills in live, tailed rather than truncated
from the top — the line you have to answer is the last one. What the user types
is echoed into the card with a `❯ ` marker, because a pipe does not echo the way
a tty does and the answer would otherwise leave no trace in the conversation.
When the call finishes, the live tail is replaced by the real result, which is
the same bytes with the exit status folded in.

The native window binds the same console to its own composer, with a smaller
set of keys: Enter sends the line (a blank one included), and Ctrl-D — or the
`end input (Ctrl-D)` button beside the banner — closes stdin. There is no
detach and no per-command stop; the composer's stop control cancels the whole
turn. Everything below about echo and about what the model reads applies there
too.

One consequence worth knowing: **the model does not see what you typed.** It
reads the command's own output, and a pipe does not echo, so the tool result
shows `npm init`'s questions but not your answers. That is deliberate — your
keystrokes are yours, and the model can read the file the command produced — but
it means the model may need to be told what you chose if it matters.

## What is not built

- **No pty.** See above. If a real terminal is ever needed — for `sudo`, for a
  full-screen program, for raw key control — it is a separate feature with a
  separate cost, and it should be opt-in per command rather than the default for
  all of them.
- **No console over a socket.** The browser GUI never grew one, and that was
  right: a page holding the stdin of a live child in another process is a hung
  `apt install` waiting on a tab somebody closed. It was the boundary that made
  it impossible, not the fact of being graphical — the window, which has no such
  boundary, does have a console. That surface is now deleted; the constraint is
  recorded here because the next surface with a socket in it inherits it.
- **No console for background tasks or subagents.** Both are unattended by
  construction.
