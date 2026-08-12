//! Native `execute` tool: run shell commands with a timeout.
//!
//! Security note (see `docs/architecture.md`): this is real shell access and
//! cannot be confined to the working directory.
//!
//! # Two ways to run a command, and why there are two
//!
//! [`run_command`] is the path every command in this codebase has always taken:
//! `/dev/null` on fd 0, both output streams read into buffers, one
//! [`tokio::time::timeout`] around [`Child::wait`](tokio::process::Child::wait),
//! and a [`CommandResult`] handed back when it is over. It is a handful of
//! polls for a command that echoes a word and exits, and that is the
//! overwhelmingly common case — the git tools, `search_files`, scripted tools
//! and every subagent's shell run through it unchanged.
//!
//! [`run_command_interactive`] is what `execute` uses when, and only when, the
//! surface has declared [`ConsoleAccess::Interactive`]: a human is watching and
//! can answer a question. It differs in exactly three ways, each of which is
//! one half of the bug it fixes:
//!
//! 1. **fd 0 is a pipe this process holds open**, rather than `/dev/null`. An
//!    installer asking `Do you want to continue? [Y/n]` used to read EOF and
//!    abort or spin; now it blocks on a `read` that somebody can satisfy.
//! 2. **Output is announced as it arrives**
//!    ([`AgentEvent::ConsoleOutput`]), not when the process exits. A prompt
//!    nobody can see is a prompt nobody can answer, which is why fixing only
//!    the first half would have fixed nothing.
//! 3. **The timeout is a budget of *unattended* time.** See
//!    [`run_command_interactive`] for the rule; the short version is that a
//!    command sitting on a question with a human in front of it is not hung,
//!    and killing it because the human went to get coffee is the same bug in a
//!    different costume.
//!
//! What the child sees on fd 0 is a **pipe**, not a pty. `isatty(0)` answered
//! `false` before this change (`/dev/null` is not a terminal) and answers
//! `false` after, so nothing that branches on it — colour, line buffering,
//! progress bars, pagers — changes behaviour. The cost of that choice is
//! written down in `docs/interactive-commands.md`: a program that deliberately
//! bypasses its own stdin and opens `/dev/tty` (`sudo`'s password prompt,
//! `ssh`'s host-key confirmation) is not reachable this way, and never was.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{
    ConsoleAccess, MAX_OUTPUT_BYTES, Tool, ToolContext, ToolError, ToolOutput, parse_args,
    truncate_output,
};
use crate::agent::{AgentEvent, ConsoleGate, ConsoleInput};
use crate::platform::process::ProcessGroupExt;
use crate::platform::shell;

/// Default command timeout when the model does not specify one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Hard upper bound a model-supplied timeout is clamped to.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to keep draining the output pipes after the child exited (or was
/// killed). Bounds a stray descendant holding a pipe open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How quiet a command has to go, after output that did not end in a newline,
/// before an attended console treats it as a question rather than as work.
///
/// The newline is the load-bearing half. `Do you want to continue? [Y/n] ` ends
/// mid-line because the program expects the answer to be typed on that line —
/// that is what a prompt *is*, in every shell, installer and REPL — whereas a
/// build that is grinding away has written whole lines and is between them. The
/// delay only rules out catching the split second inside a `write` of a long
/// line.
///
/// This is a heuristic, and it is deliberately the *conservative* one: getting
/// it wrong by saying "not a prompt" costs a command its wall-clock timeout,
/// which is what it had before. Getting it wrong the other way is impossible to
/// do silently, because pausing the clock also requires a human to be attached.
const PROMPT_IDLE: Duration = Duration::from_millis(400);

/// How long one line may take to reach the child's stdin before the console
/// gives up on it. See the write itself for why it is bounded at all.
const STDIN_GRACE: Duration = Duration::from_secs(2);

/// Captured result of a finished child process.
#[derive(Debug)]
pub(crate) struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    /// Exit code, or `None` when the process was terminated by a signal.
    pub code: Option<i32>,
    /// Set (to the budget in seconds) when the command was killed at the
    /// timeout. `stdout`/`stderr` then carry whatever was produced first.
    pub timed_out: Option<u64>,
}

/// Bytes of one output stream kept while the command runs: the first
/// [`CAPTURE_HEAD_BYTES`] and the most recent [`CAPTURE_TAIL_BYTES`].
///
/// There was no bound at all. The reader appended every 8 KiB chunk into a
/// `Vec` and the cut to [`MAX_OUTPUT_BYTES`] happened *after* the child
/// exited, so `execute({"command": "yes", "timeout_secs": 600})` bought ten
/// minutes of buffering at pipe speed — tens of gigabytes of resident memory
/// in the agent's own process, for output nobody would ever read. The
/// background-task path has been capped ([`super::tasks::OUTPUT_CAP_BYTES`])
/// since it was written; this is the same idea on the foreground path.
///
/// Head *and* tail rather than a plain tail because [`truncate_output`] frames
/// what the model finally sees as head + tail: a build whose first line is the
/// error has to stay readable. Both halves are far above `MAX_OUTPUT_BYTES`,
/// so for anything that reaches the model the truncation that matters is still
/// the one downstream. The callers to check against this cap are the ones that
/// take the capture whole: `git ls-files` in [`super::file`], whose
/// line-by-line parse fits a repository of roughly sixty thousand files inside
/// the head alone, and `search` in the same module, which hands all of
/// `result.stdout` to [`truncate_output`] — so an `rg` run past the cap shows
/// the marker [`CappedBuffer::into_string`] writes, mid-results, rather than a
/// clean cut of its own.
const CAPTURE_HEAD_BYTES: usize = 512 * 1024;
const CAPTURE_TAIL_BYTES: usize = 1536 * 1024;

/// Bounded capture of one stream: the head, the tail, and a count of the
/// bytes that fell out of the middle.
#[derive(Debug, Default)]
struct CappedBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: usize,
}

impl CappedBuffer {
    fn append(&mut self, data: &[u8]) {
        let mut data = data;
        if self.head.len() < CAPTURE_HEAD_BYTES {
            let take = (CAPTURE_HEAD_BYTES - self.head.len()).min(data.len());
            self.head.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        if data.is_empty() {
            return;
        }
        self.tail.extend(data.iter().copied());
        if self.tail.len() > CAPTURE_TAIL_BYTES {
            let excess = self.tail.len() - CAPTURE_TAIL_BYTES;
            self.tail.drain(..excess);
            self.dropped += excess;
        }
    }

    /// Everything captured, as lossy UTF-8, with the gap named when there is
    /// one. Consumes the buffer rather than copying it: `finish` used to take
    /// a second full copy of an already-oversized `Vec` on the way out.
    fn into_string(self) -> String {
        let tail: Vec<u8> = self.tail.into();
        if self.dropped == 0 {
            // Decoded as one string: the head/tail boundary can land inside a
            // multi-byte character, and two lossy decodes would turn it into
            // two replacement characters.
            let mut bytes = self.head;
            bytes.extend_from_slice(&tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        format!(
            "{}\n... [output truncated] {} bytes omitted from the middle ...\n{}",
            String::from_utf8_lossy(&self.head),
            self.dropped,
            String::from_utf8_lossy(&tail),
        )
    }
}

/// One piped output stream read incrementally into a shared buffer, so a
/// timeout can still report what the command produced before it was killed.
struct Pipe {
    buf: Arc<Mutex<CappedBuffer>>,
    task: tokio::task::JoinHandle<()>,
}

impl Pipe {
    fn new(stream: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>) -> Self {
        let buf = Arc::new(Mutex::new(CappedBuffer::default()));
        let task = tokio::spawn({
            let buf = Arc::clone(&buf);
            async move {
                let Some(mut stream) = stream else { return };
                let mut chunk = [0u8; 8192];
                loop {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.lock().unwrap().append(&chunk[..n]),
                    }
                }
            }
        });
        Self { buf, task }
    }

    /// Wait up to `grace` for the reader to hit EOF, then take whatever is
    /// buffered.
    async fn finish(self, grace: Duration) -> String {
        let mut task = self.task;
        if tokio::time::timeout(grace, &mut task).await.is_err() {
            task.abort();
        }
        let taken = std::mem::take(&mut *self.buf.lock().unwrap());
        taken.into_string()
    }
}

/// SIGKILL `child`'s whole process group and reap it. Mirrors
/// `tasks::kill_tree`: the shell may fork the command rather than exec it, and
/// killing only the shell would leave grandchildren running.
async fn kill_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        crate::platform::process::kill_group(pid);
    }
    let _ = child.kill().await;
}

/// Spawn `command` with piped stdio, wait for it under `timeout`, and capture
/// its output. On timeout the whole process group is killed and the partial
/// output is returned with `timed_out` set. Shared by `execute`, the git
/// tools, `search_files`, and scripted tools.
pub(crate) async fn run_command(
    tool: &str,
    mut command: Command,
    timeout: Duration,
) -> Result<CommandResult, ToolError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Own process group so a timeout kill reaches the whole tree (see
        // `kill_group`).
        .own_process_group();

    let mut child = command.spawn().map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::Error::new(err).context("failed to spawn process"),
    })?;

    let stdout = Pipe::new(child.stdout.take());
    let stderr = Pipe::new(child.stderr.take());

    let mut timed_out = None;
    let code = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        Ok(Err(err)) => {
            return Err(ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::Error::new(err).context("failed to wait for process"),
            });
        }
        Err(_) => {
            kill_group(&mut child).await;
            timed_out = Some(timeout.as_secs());
            None
        }
    };

    let (stdout, stderr) = tokio::join!(stdout.finish(DRAIN_GRACE), stderr.finish(DRAIN_GRACE));
    Ok(CommandResult {
        stdout,
        stderr,
        code,
        timed_out,
    })
}

/// Which of a child's two output streams a chunk arrived on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stream {
    Out,
    Err,
}

/// Read one of the child's streams into `sink`, chunk by chunk, tagged with
/// which stream it was.
///
/// One task per stream feeding *one* channel, rather than two buffers read at
/// the end: the merge is what puts a prompt on the screen in the order the
/// child wrote it, and plenty of programs prompt on stderr. The tool result
/// still separates them, because the model reads
/// [`render_command_result`]'s `stderr:` section as a signal.
fn pump(
    stream: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    which: Stream,
    sink: mpsc::Sender<(Stream, Vec<u8>)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(mut stream) = stream else { return };
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send((which, chunk[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Why the interactive loop woke up. The select produces one of these and the
/// body acts on it *after* the block, because the arms cannot touch `child`
/// while [`Child::wait`](tokio::process::Child::wait) is borrowing it.
enum Wake {
    /// The child exited on its own.
    Exit(std::process::ExitStatus),
    /// The turn was cancelled (Ctrl-C).
    Cancelled,
    /// The surface sent something, or went away (`None`).
    Input(Option<ConsoleInput>),
    /// The child wrote something, or both its streams closed (`None`).
    Chunk(Option<(Stream, Vec<u8>)>),
    /// A timer expired: either the unattended budget or the prompt threshold.
    /// Which one is worked out from the clock, not from the timer.
    Timer,
}

/// Sleep for `wake`, or never when the clock is stopped.
///
/// A future that is always pending rather than a second copy of the `select!`
/// for the paused case — the same trick, and the same justification, as
/// [`crate::agent::cancelled`].
async fn wake_after(wake: Option<Duration>) {
    match wake {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

/// Run `command` with its stdin held open for a human, streaming output to the
/// surface as it is produced.
///
/// Used by `execute` when the surface declared [`ConsoleAccess::Interactive`],
/// and nowhere else. See the module docs for what the child sees on fd 0.
///
/// # The timeout is a budget of unattended time
///
/// The wall clock is the wrong instrument here. A command blocked on
/// `Do you want to continue? [Y/n]` for two minutes because the person it asked
/// went to make coffee has not hung; killing it at 120 seconds turns a working
/// prompt into the same failure the prompt was supposed to replace. But an idle
/// timer is the wrong instrument too, because then a genuinely wedged command
/// that dribbles a byte a minute runs forever.
///
/// So `timeout` is spent only while the command is *unattended*, and the clock
/// stops on a conjunction of two facts, neither of which is a guess about what
/// the child is doing:
///
/// 1. **A surface is holding the console's writer.** Not "a surface was told
///    about it" — it claimed the gate (`ConsoleHost::attended`) and has not
///    dropped the writer since. Detaching with Esc drops it, and the clock
///    starts again, because "unattended" is once more the truth. If nobody ever
///    claimed it, the full wall clock applies from the start.
/// 2. **The child's last output did not end in a newline, and it has been quiet
///    for [`PROMPT_IDLE`] since.** That is the shape of a question in every
///    shell, installer and REPL: the cursor is parked at the end of the line the
///    answer goes on. Work in progress writes whole lines; `sleep 30` writes
///    none at all. Both keep their wall clock.
///
/// Answering restarts the budget from zero rather than resuming it: a human who
/// just typed has proved the command is alive, and the next step of an install
/// deserves the same allowance the first one got.
///
/// The failure modes are asymmetric on purpose. Reading a prompt as work costs
/// the command its old wall-clock timeout — no worse than before. Reading work
/// as a prompt cannot silently hang anything, because it also takes a human
/// sitting there, and Ctrl-C reaches the process group from inside this loop
/// (the run loop only checks cancellation between tool calls, which is far too
/// late for a call that is deliberately parked).
pub(crate) async fn run_command_interactive(
    tool: &str,
    mut command: Command,
    timeout: Duration,
    label: &str,
    events: &mpsc::Sender<AgentEvent>,
    cancel: Option<&crate::agent::CancelHandle>,
) -> Result<CommandResult, ToolError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Same reason as `run_command`: the shell may fork rather than exec,
        // and both the timeout kill and Ctrl-C have to reach the whole tree.
        .own_process_group();

    let mut child = command.spawn().map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::Error::new(err).context("failed to spawn process"),
    })?;

    // Held for the life of the command; dropping it is what sends EOF.
    let mut stdin = child.stdin.take();
    let (sink, mut chunks) = mpsc::channel::<(Stream, Vec<u8>)>(64);
    let out_pump = pump(child.stdout.take(), Stream::Out, sink.clone());
    let err_pump = pump(child.stderr.take(), Stream::Err, sink);

    let (gate, mut host) = ConsoleGate::open();
    // Announced before the first byte can be, so a surface that claims the gate
    // holds the writer before anything it might have to answer shows up.
    let _ = events
        .send(AgentEvent::ConsoleOpened {
            command: label.to_string(),
            gate,
        })
        .await;

    // Bounded for the same reason `Pipe` is: this loop reads the child at
    // pipe speed for as long as the command is allowed to run, and a human
    // watching a wall of output scroll past is no reason to hold all of it.
    let mut stdout = CappedBuffer::default();
    let mut stderr = CappedBuffer::default();
    // Unattended time already charged, plus when the current charged stretch
    // began. `None` is the clock stopped.
    let mut spent = Duration::ZERO;
    let mut running_since = Some(Instant::now());
    // When the current quiet stretch began, and whether the child's last write
    // left the cursor mid-line. Together they are the prompt test.
    let mut idle_since = Instant::now();
    let mut open_line = false;
    // Whether `ConsoleWaiting` has gone out. Once, per command: see the event's
    // docs for why a composer that flipped back on the next line of output
    // would be worse than one that stayed.
    let mut announced_waiting = false;
    let mut inputs_open = true;
    let mut chunks_open = true;

    let mut code = None;
    let mut timed_out = None;
    let mut exited = false;

    loop {
        let wake_in = running_since.map(|since| {
            let left = timeout.saturating_sub(spent + since.elapsed());
            if open_line {
                left.min(PROMPT_IDLE.saturating_sub(idle_since.elapsed()))
            } else {
                left
            }
        });

        let wake = tokio::select! {
            biased;
            () = crate::agent::cancelled(cancel) => Wake::Cancelled,
            status = child.wait() => match status {
                Ok(status) => Wake::Exit(status),
                Err(err) => {
                    return Err(ToolError::Execution {
                        tool: tool.to_string(),
                        source: anyhow::Error::new(err).context("failed to wait for process"),
                    });
                }
            },
            got = host.receive.recv(), if inputs_open => Wake::Input(got),
            got = chunks.recv(), if chunks_open => Wake::Chunk(got),
            () = wake_after(wake_in) => Wake::Timer,
        };

        match wake {
            Wake::Exit(status) => {
                code = status.code();
                exited = true;
                break;
            }
            Wake::Cancelled => break,
            // The surface dropped its writer (Esc, or the TUI went away):
            // nobody is attending this command any more, so it goes back on the
            // wall clock — including right now, if the clock was stopped
            // waiting for the person who just left.
            Wake::Input(None) => {
                inputs_open = false;
                if running_since.is_none() {
                    running_since = Some(Instant::now());
                }
            }
            Wake::Input(Some(input)) => {
                match input {
                    ConsoleInput::Line(mut line) => {
                        line.push('\n');
                        if let Some(pipe) = stdin.as_mut() {
                            // Bounded, because this await is outside the
                            // `select!` and so out of reach of Ctrl-C. A child
                            // that stopped reading normally makes the write
                            // fail at once (`EPIPE`), and the sixteen-line
                            // queue cannot fill a 64 KiB pipe buffer from a
                            // person's typing, so this should be unreachable —
                            // but the alternative to bounding it is an
                            // interrupt that does nothing, and that is the
                            // exact failure this whole change is about.
                            let wrote = tokio::time::timeout(STDIN_GRACE, async {
                                pipe.write_all(line.as_bytes()).await?;
                                pipe.flush().await
                            })
                            .await;
                            if !matches!(wrote, Ok(Ok(()))) {
                                // Drop our end so a later line is refused
                                // rather than silently swallowed.
                                stdin = None;
                            }
                        }
                    }
                    ConsoleInput::Eof => stdin = None,
                }
                // A human just acted, so the command is demonstrably alive and
                // the budget starts over. Whatever prompt was on screen has
                // been answered, so it is no longer one.
                spent = Duration::ZERO;
                running_since = Some(Instant::now());
                idle_since = Instant::now();
                open_line = false;
            }
            Wake::Chunk(None) => chunks_open = false,
            Wake::Chunk(Some((which, bytes))) => {
                open_line = !bytes.ends_with(b"\n");
                idle_since = Instant::now();
                // Output is progress: whatever the pause was for is over.
                if running_since.is_none() {
                    running_since = Some(Instant::now());
                }
                match which {
                    Stream::Out => stdout.append(&bytes),
                    Stream::Err => stderr.append(&bytes),
                }
                let _ = events
                    .send(AgentEvent::ConsoleOutput {
                        gate,
                        chunk: String::from_utf8_lossy(&bytes).into_owned(),
                    })
                    .await;
            }
            Wake::Timer => {
                // The timer only runs while the clock does, so this cannot be
                // reached with a stopped clock.
                let Some(since) = running_since else { continue };
                if spent + since.elapsed() >= timeout {
                    timed_out = Some(timeout.as_secs());
                    break;
                }
                // Not the budget, so it was the prompt threshold: the child's
                // last write left the cursor mid-line and nothing has followed
                // it. Tell the surface once, so a composer that has been left
                // alone for every `ls` and `cargo build` switches over for the
                // one command in a hundred that is actually asking something.
                if open_line && !announced_waiting {
                    announced_waiting = true;
                    let _ = events.send(AgentEvent::ConsoleWaiting { gate }).await;
                }
                if host.attended() && inputs_open {
                    spent += since.elapsed();
                    running_since = None;
                } else {
                    // Nobody to ask. Start another quiet stretch rather than
                    // re-arming a timer that already expired, which would spin.
                    idle_since = Instant::now();
                }
            }
        }
    }

    // Close our end of stdin before draining: a child still reading it would
    // otherwise outlive the loop that was feeding it.
    drop(stdin);
    if !exited {
        kill_group(&mut child).await;
    }
    // Whatever the pipes still hold belongs both on screen and in the result,
    // so the user and the model do not end up with different readings of what
    // the command said. Bounded, for the same reason `Pipe::finish` is.
    let _ = tokio::time::timeout(DRAIN_GRACE, async {
        while let Some((which, bytes)) = chunks.recv().await {
            match which {
                Stream::Out => stdout.append(&bytes),
                Stream::Err => stderr.append(&bytes),
            }
            let _ = events
                .send(AgentEvent::ConsoleOutput {
                    gate,
                    chunk: String::from_utf8_lossy(&bytes).into_owned(),
                })
                .await;
        }
    })
    .await;
    out_pump.abort();
    err_pump.abort();

    // Void the ticket before announcing the close, so a surface that reacts to
    // `ConsoleClosed` by claiming (a bug, but a cheap one to make impossible)
    // finds nothing rather than a writer into a dead pipe.
    gate.cancel();
    let _ = events.send(AgentEvent::ConsoleClosed { gate }).await;

    Ok(CommandResult {
        stdout: stdout.into_string(),
        stderr: stderr.into_string(),
        code,
        timed_out,
    })
}

/// Render a [`CommandResult`] as the model-facing tool output: stdout, then a
/// labelled stderr section, then the exit code when non-zero. `is_error`
/// mirrors the exit status. A timed-out result is an error carrying the
/// partial output.
pub(crate) fn render_command_result(result: &CommandResult) -> ToolOutput {
    let stdout = result.stdout.trim_end();
    let stderr = result.stderr.trim_end();

    let mut content = String::new();
    if !stdout.is_empty() {
        content.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str("stderr:\n");
        content.push_str(stderr);
    }

    if let Some(secs) = result.timed_out {
        let note = if content.is_empty() {
            format!("command timed out after {secs}s and was killed (no output produced)")
        } else {
            content.push('\n');
            format!("command timed out after {secs}s and was killed; output above is partial")
        };
        content.push_str(&note);
        return ToolOutput::error(truncate_output(content, MAX_OUTPUT_BYTES));
    }

    match result.code {
        Some(0) => {
            if content.is_empty() {
                content.push_str("(command succeeded with no output)");
            }
            ToolOutput::ok(truncate_output(content, MAX_OUTPUT_BYTES))
        }
        Some(code) => {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&format!("exit code: {code}"));
            ToolOutput::error(truncate_output(content, MAX_OUTPUT_BYTES))
        }
        None => {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str("terminated by signal");
            ToolOutput::error(truncate_output(content, MAX_OUTPUT_BYTES))
        }
    }
}

/// Arguments for [`ExecuteTool`].
#[derive(Debug, Deserialize)]
pub struct ExecuteArgs {
    /// Shell command line, run through the platform shell in the project root.
    pub command: String,
    /// Timeout in seconds (default 120, clamped to 600). Ignored for
    /// background tasks, which use the fixed background timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Run detached as a background task: returns immediately with a task
    /// id; the agent is notified when the command finishes.
    #[serde(default)]
    pub run_in_background: bool,
}

/// `execute` — run a shell command, capturing stdout, stderr, and exit code.
pub struct ExecuteTool;

#[async_trait]
impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        "execute"
    }

    fn description(&self) -> &str {
        r#"Run a shell command in the project root and return its stdout, stderr, and exit code. Killed on timeout. With run_in_background, detaches as an agent-managed background task (task_output / task_kill); you are notified when it finishes.

Tips:
- Prefer compact output: summaries, `head`/`tail`/`wc`; put bulky intermediates in `/tmp`.
- Non-zero exit is diagnostic signal — read stderr and adapt.
- A command that prompts on stdin is answered by the **user**, not by you, and only in an interactive session; elsewhere stdin is /dev/null and the prompt reads EOF. Prefer non-interactive flags (`-y`, `--yes`, `--non-interactive`) when the run may be unattended.
- Durable services (HTTP, QEMU, anything a later verifier must reach): `nohup <cmd> > log 2>&1 &`, then `curl`/`ss`/`pgrep`. Do **not** use `run_in_background=true` for those — that mode does not outlive the agent.
- Use `run_in_background=true` only for agent-scoped jobs you will poll or cancel (long builds).
- After system-installing a package with native extensions, verify from `cd /tmp` so a local checkout cannot mask a bad install; reinstall after later source edits.
- Before finishing: `ls` required deliverable paths; for JSON/JSONL, parse and assert task-listed tokens/IDs."#
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command line (run via sh -c)" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120, max 600); ignored for background tasks" },
                "run_in_background": { "type": "boolean", "description": "Detach as a background task and return immediately (default false)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ExecuteArgs = parse_args(self.name(), args)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "command must not be empty".to_string(),
            });
        }

        let timeout = match args.timeout_secs {
            Some(0) => {
                return Err(ToolError::InvalidArgs {
                    tool: self.name().to_string(),
                    message: "timeout_secs must be at least 1".to_string(),
                });
            }
            Some(secs) => Duration::from_secs(secs).min(MAX_TIMEOUT),
            None => DEFAULT_TIMEOUT,
        };

        let mut command = shell::tokio_command(&args.command);
        command.current_dir(&ctx.cwd);

        if args.run_in_background {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                // Own process group so task_kill and the timeout reach the
                // whole tree: the shell may fork the command rather than exec
                // it, and a surviving grandchild would hold the output pipes
                // open.
                .own_process_group();
            let child = command.spawn().map_err(|err| ToolError::Execution {
                tool: self.name().to_string(),
                source: anyhow::Error::new(err).context("failed to spawn background process"),
            })?;
            let id = ctx.tasks.spawn(&args.command, child);
            // Mirror the new task to the UI dashboard (TaskFinished follows
            // when it ends). Surfaces that don't care just drop it.
            if let Some(events) = &ctx.events {
                let _ = events
                    .send(crate::agent::AgentEvent::TaskStarted {
                        id,
                        command: args.command.clone(),
                    })
                    .await;
            }
            return Ok(ToolOutput::ok(format!(
                "Background task #{id} started: {}\nYou will be notified when it finishes; \
                 use task_output to inspect it or task_kill to stop it.",
                args.command
            )));
        }

        // A console is opened only when the surface said there is a human to
        // answer it. Everything else — headless, the gateway, ACP, fleet runs,
        // the browser GUI, every subagent — takes the path below unchanged,
        // `/dev/null` on fd 0 included, because a child blocked on a pipe
        // nobody will ever write to is strictly worse than one that reads EOF.
        let result = match (ctx.console, &ctx.events) {
            (ConsoleAccess::Interactive, Some(events)) => {
                run_command_interactive(
                    self.name(),
                    command,
                    timeout,
                    &args.command,
                    events,
                    ctx.cancel.as_ref(),
                )
                .await?
            }
            _ => run_command(self.name(), command, timeout).await?,
        };
        Ok(render_command_result(&result))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The capture is bounded, and it keeps both ends.
    ///
    /// The reader used to append every chunk into a plain `Vec` and truncate
    /// only once the child was gone, so a command that writes forever —
    /// `execute({"command": "yes", "timeout_secs": 600})` — bought its whole
    /// timeout worth of memory at pipe speed, inside the agent process.
    /// Bounding it with a *tail* alone would have been the other bug: the
    /// model is shown head + tail, and a failure usually announces itself on
    /// the first line.
    #[test]
    fn a_command_that_writes_forever_cannot_grow_the_buffer_forever() {
        let mut buf = CappedBuffer::default();
        let chunk = vec![b'y'; 8192];
        // Twice the total cap, in the 8 KiB reads the pipe actually delivers.
        let rounds = 2 * (CAPTURE_HEAD_BYTES + CAPTURE_TAIL_BYTES) / chunk.len();
        for _ in 0..rounds {
            buf.append(&chunk);
        }
        assert_eq!(buf.head.len(), CAPTURE_HEAD_BYTES);
        assert_eq!(buf.tail.len(), CAPTURE_TAIL_BYTES);
        assert!(buf.dropped > 0, "the middle is what gets dropped");

        // Under the cap nothing is touched, and the two halves join without a
        // marker or a lost byte.
        let mut small = CappedBuffer::default();
        small.append(b"first line\n");
        small.append("second line \u{2014} with a dash\n".as_bytes());
        assert_eq!(
            small.into_string(),
            "first line\nsecond line \u{2014} with a dash\n"
        );

        // Over it, both ends survive and the gap says how much did not.
        let mut split = CappedBuffer::default();
        split.append(b"HEAD");
        split.append(&vec![b'.'; CAPTURE_HEAD_BYTES + CAPTURE_TAIL_BYTES]);
        split.append(b"TAIL");
        let text = split.into_string();
        assert!(text.starts_with("HEAD"), "the head is kept");
        assert!(text.ends_with("TAIL"), "the tail is kept");
        assert!(
            text.contains("bytes omitted from the middle"),
            "the gap is named rather than silently closed"
        );
    }

    #[tokio::test]
    async fn execute_captures_stdout() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "echo spellbook" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "spellbook");
    }

    #[tokio::test]
    async fn execute_runs_in_project_root() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "pwd" }), &tmp.ctx())
            .await
            .unwrap();
        let reported = std::fs::canonicalize(out.content.trim()).unwrap();
        let expected = std::fs::canonicalize(&tmp.0).unwrap();
        assert_eq!(reported, expected);
    }

    #[tokio::test]
    async fn execute_times_out_and_reports_seconds() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(
                json!({ "command": "sleep 5", "timeout_secs": 1 }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("timed out after 1s"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn execute_timeout_returns_partial_output() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(
                json!({ "command": "echo started; echo warn >&2; sleep 5", "timeout_secs": 1 }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("started"), "{}", out.content);
        assert!(out.content.contains("stderr:\nwarn"), "{}", out.content);
        assert!(
            out.content.contains("output above is partial"),
            "{}",
            out.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_timeout_kills_the_whole_process_group() {
        let tmp = TempDir::new();
        // The subshell is a grandchild of the shell we spawned; without the
        // group kill it would survive the timeout and write the marker file.
        let out = ExecuteTool
            .execute(
                json!({
                    "command": "(sleep 2 && touch grandchild-survived) & echo spawned; sleep 30",
                    "timeout_secs": 1
                }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("spawned"), "{}", out.content);
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(
            !tmp.0.join("grandchild-survived").exists(),
            "grandchild must be killed with the group"
        );
    }

    #[tokio::test]
    async fn execute_nonzero_exit_is_tool_output_error() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "echo oops >&2; exit 3" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("stderr:\noops"));
        assert!(out.content.contains("exit code: 3"));
    }

    #[tokio::test]
    async fn execute_success_with_no_output_says_so() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "true" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "(command succeeded with no output)");
    }

    #[tokio::test]
    async fn execute_rejects_empty_command() {
        let tmp = TempDir::new();
        let err = ExecuteTool
            .execute(json!({ "command": "   " }), &tmp.ctx())
            .await
            .expect_err("blank command must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn execute_rejects_zero_timeout() {
        let tmp = TempDir::new();
        let err = ExecuteTool
            .execute(json!({ "command": "true", "timeout_secs": 0 }), &tmp.ctx())
            .await
            .expect_err("zero timeout must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn execute_rejects_missing_command_argument() {
        let tmp = TempDir::new();
        let err = ExecuteTool
            .execute(json!({}), &tmp.ctx())
            .await
            .expect_err("missing command must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "execute"));
    }

    #[tokio::test]
    async fn execute_run_in_background_registers_a_task_and_returns_immediately() {
        let tmp = TempDir::new();
        let ctx = tmp.ctx();
        let out = ExecuteTool
            .execute(
                json!({ "command": "echo bg-marker", "run_in_background": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains("Background task #1 started: echo bg-marker"),
            "{}",
            out.content
        );

        // The task runs to completion in the registry and its output is
        // captured for the finished-task notification.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = ctx.tasks.status(1).expect("task registered");
            if status.is_finished() {
                assert_eq!(status, crate::tools::tasks::TaskStatus::Done(0));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "task finished");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let drained = ctx.tasks.drain_completed();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].tail.contains("bg-marker"), "{}", drained[0].tail);
    }

    #[test]
    fn render_merges_stdout_and_stderr_sections() {
        let result = CommandResult {
            stdout: "out line\n".to_string(),
            stderr: "err line\n".to_string(),
            code: Some(0),
            timed_out: None,
        };
        let out = render_command_result(&result);
        assert!(!out.is_error);
        assert_eq!(out.content, "out line\nstderr:\nerr line");
    }

    #[test]
    fn render_signal_termination_is_an_error() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            timed_out: None,
        };
        let out = render_command_result(&result);
        assert!(out.is_error);
        assert_eq!(out.content, "terminated by signal");
    }

    // -- Consoles ---------------------------------------------------------
    //
    // Every test below drives a **real child process** through a real shell.
    // A mock cannot answer the question these are asking, which is whether a
    // program blocked in `read(0)` gets what a person typed: that is a fact
    // about pipes and process groups, not about our own types.

    /// `script` through the platform shell, in `dir`.
    fn sh(script: &str, dir: &std::path::Path) -> Command {
        let mut command = shell::tokio_command(script);
        command.current_dir(dir);
        command
    }

    /// Run `script` on the interactive path with `surface` consuming its event
    /// stream concurrently — which is the shape the TUI is in, and the only
    /// shape in which a console can be claimed at all.
    async fn interactive<T, Fut>(
        dir: &std::path::Path,
        script: &str,
        timeout: Duration,
        cancel: Option<crate::agent::CancelHandle>,
        surface: impl FnOnce(tokio::sync::mpsc::Receiver<crate::agent::AgentEvent>) -> Fut,
    ) -> (CommandResult, T)
    where
        Fut: std::future::Future<Output = T>,
    {
        let (events, stream) = tokio::sync::mpsc::channel(256);
        let run = run_command_interactive(
            "execute",
            sh(script, dir),
            timeout,
            script,
            &events,
            cancel.as_ref(),
        );
        let (result, out) = tokio::join!(run, surface(stream));
        (result.expect("the command ran"), out)
    }

    /// A surface that claims the console and answers `reply` once it sees
    /// `cue` in the command's output, after thinking for `think` first.
    /// Returns every chunk it was shown, in order.
    async fn answering(
        mut stream: tokio::sync::mpsc::Receiver<crate::agent::AgentEvent>,
        cue: &'static str,
        reply: &'static str,
        think: Duration,
    ) -> Vec<String> {
        let mut writer = None;
        let mut seen = Vec::new();
        let mut answered = false;
        while let Some(event) = stream.recv().await {
            match event {
                AgentEvent::ConsoleOpened { gate, .. } => {
                    writer = Some(gate.claim().expect("the surface claims the console"));
                }
                AgentEvent::ConsoleOutput { chunk, .. } => {
                    seen.push(chunk.clone());
                    if !answered && chunk.contains(cue) {
                        answered = true;
                        tokio::time::sleep(think).await;
                        assert!(
                            writer.as_ref().expect("claimed").line(reply),
                            "the child is still reading"
                        );
                    }
                }
                AgentEvent::ConsoleClosed { .. } => break,
                _ => {}
            }
        }
        seen
    }

    /// A surface that renders and never claims: a watcher, a recorder, a peer.
    ///
    /// It returns on `ConsoleClosed` rather than on the channel closing,
    /// because the run's sender outlives the run — waiting for the channel
    /// would be waiting for the very future this one is joined with.
    async fn render_only(mut stream: tokio::sync::mpsc::Receiver<crate::agent::AgentEvent>) {
        while let Some(event) = stream.recv().await {
            if matches!(event, AgentEvent::ConsoleClosed { .. }) {
                break;
            }
        }
    }

    /// The user's whole side of a prompt: the answer is only sent once the
    /// question has actually been shown, which is what the bug report says
    /// never happened.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_interactive_command_gets_the_line_the_user_typed() {
        let tmp = TempDir::new();
        let (result, seen) = interactive(
            &tmp.0,
            r#"printf 'name? '; read answer; echo "hello $answer""#,
            Duration::from_secs(20),
            None,
            |stream| answering(stream, "name?", "wizard", Duration::ZERO),
        )
        .await;

        assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
        assert!(
            result.stdout.contains("hello wizard"),
            "the child read the typed line: {:?}",
            result.stdout
        );
        assert!(
            seen.iter().any(|chunk| chunk.contains("name?")),
            "the prompt reached the surface: {seen:?}"
        );
    }

    /// An empty line is an answer. Pressing Enter at `[Y/n]` is how a person
    /// accepts the default, and it is the exact keystroke the bug report says
    /// did nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_bare_enter_answers_a_prompt_with_its_default() {
        let tmp = TempDir::new();
        let (result, _) = interactive(
            &tmp.0,
            r#"printf 'continue? [Y/n] '; read reply; echo "reply=[${reply:-Y}]""#,
            Duration::from_secs(20),
            None,
            |stream| answering(stream, "[Y/n]", "", Duration::ZERO),
        )
        .await;
        assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
        assert!(
            result.stdout.contains("reply=[Y]"),
            "an empty line reached the child and took the default: {:?}",
            result.stdout
        );
    }

    /// Output before exit. The command cannot exit until it is answered, so a
    /// surface that has been shown the prompt has been shown output from a
    /// process that is still running — which the old buffer-until-`wait()`
    /// path could not do in any amount of time.
    #[cfg(unix)]
    #[tokio::test]
    async fn output_reaches_the_surface_before_the_command_exits() {
        let tmp = TempDir::new();
        let (result, prompt_seen) = interactive(
            &tmp.0,
            r#"printf 'ready? '; read x; echo "got $x""#,
            Duration::from_secs(8),
            None,
            |mut stream| async move {
                let mut writer = None;
                // Bounded: buffering until exit deadlocks here (the child is
                // waiting on us, we are waiting on the child), and the timeout
                // is what turns that deadlock into a failed assertion.
                let waited = tokio::time::timeout(Duration::from_secs(5), async {
                    while let Some(event) = stream.recv().await {
                        match event {
                            AgentEvent::ConsoleOpened { gate, .. } => writer = gate.claim(),
                            AgentEvent::ConsoleOutput { chunk, .. } if chunk.contains("ready?") => {
                                return true;
                            }
                            _ => {}
                        }
                    }
                    false
                })
                .await;
                let seen = waited.unwrap_or(false);
                if seen {
                    writer.expect("claimed").line("late");
                } else {
                    // Never shown the question. Let go, so the command stops
                    // counting as attended and its timeout can end the run —
                    // otherwise a build that buffered output until exit would
                    // hang this test instead of failing it.
                    drop(writer);
                }
                // Drain until the console *closes*, which is the run saying
                // it is done. Draining until the channel closes would
                // deadlock: the run's sender outlives the run itself.
                while let Some(event) = stream.recv().await {
                    if matches!(event, AgentEvent::ConsoleClosed { .. }) {
                        break;
                    }
                }
                seen
            },
        )
        .await;

        assert!(
            prompt_seen,
            "the prompt must be on screen while the command is still blocked on it"
        );
        assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
        assert!(result.stdout.contains("got late"), "{:?}", result.stdout);
    }

    /// The clock stops for a human. One second of budget, two and a half
    /// seconds of thinking, and the command still completes.
    #[cfg(unix)]
    #[tokio::test]
    async fn time_spent_waiting_for_the_user_is_not_spent_on_the_timeout() {
        let tmp = TempDir::new();
        let (result, _) = interactive(
            &tmp.0,
            r#"printf 'go? '; read x; echo "got $x""#,
            Duration::from_secs(1),
            None,
            |stream| answering(stream, "go?", "yes", Duration::from_millis(2_500)),
        )
        .await;
        assert_eq!(
            result.timed_out, None,
            "a command waiting on a person is not a hung command"
        );
        assert!(result.stdout.contains("got yes"), "{:?}", result.stdout);
    }

    /// And it stops only for a human. Nobody claims the console here, so there
    /// is nothing to wait for and the wall clock is the whole rule — which is
    /// what keeps this from being a way to make the timeout disappear.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unattended_prompt_still_times_out() {
        let tmp = TempDir::new();
        let (result, _) = interactive(
            &tmp.0,
            r#"printf 'go? '; read x; echo "got $x""#,
            Duration::from_secs(1),
            None,
            render_only,
        )
        .await;
        assert_eq!(result.timed_out, Some(1), "{result:?}");
    }

    /// Answering restarts the budget rather than resuming it. One and a half
    /// budgets' worth of work either side of a prompt: three seconds of
    /// unattended time in total, which one budget cannot cover and two can.
    #[cfg(unix)]
    #[tokio::test]
    async fn answering_restarts_the_timeout_budget() {
        let tmp = TempDir::new();
        let (result, _) = interactive(
            &tmp.0,
            r#"sleep 1; printf 'go? '; read x; sleep 1; echo "got $x""#,
            Duration::from_millis(1_600),
            None,
            |stream| answering(stream, "go?", "yes", Duration::ZERO),
        )
        .await;
        assert_eq!(result.timed_out, None, "{result:?}");
        assert!(result.stdout.contains("got yes"), "{:?}", result.stdout);
    }

    /// Ctrl-C reaches the process group from inside the parked call. The
    /// subshell is a grandchild of the shell we spawned; killing only the
    /// direct child would leave it to write its marker.
    #[cfg(unix)]
    #[tokio::test]
    async fn ctrl_c_during_an_interactive_command_kills_the_process_group() {
        let tmp = TempDir::new();
        let cancel = crate::agent::CancelHandle::default();
        let (result, _) = interactive(
            &tmp.0,
            "(sleep 2 && touch grandchild-survived) & printf 'go? '; read x",
            Duration::from_secs(30),
            Some(cancel.clone()),
            |mut stream| async move {
                let mut writer = None;
                while let Some(event) = stream.recv().await {
                    match event {
                        AgentEvent::ConsoleOpened { gate, .. } => writer = gate.claim(),
                        AgentEvent::ConsoleOutput { chunk, .. } if chunk.contains("go?") => {
                            // The user hits Ctrl-C instead of answering.
                            cancel.cancel();
                        }
                        AgentEvent::ConsoleClosed { .. } => break,
                        _ => {}
                    }
                }
                drop(writer);
            },
        )
        .await;

        assert_eq!(
            result.code, None,
            "killed by signal, not exited: {result:?}"
        );
        assert_eq!(result.timed_out, None, "an interrupt is not a timeout");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(
            !tmp.0.join("grandchild-survived").exists(),
            "the whole process group must die with the interrupt"
        );
    }

    /// The overwhelmingly common case: a command that reads nothing, ends its
    /// output with a newline, and exits. A console costs it a pipe on fd 0 and
    /// nothing else — in particular not [`PROMPT_IDLE`], which only ever
    /// applies to output that stopped mid-line.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_reads_nothing_is_not_slowed_by_having_a_console() {
        let tmp = TempDir::new();
        let started = Instant::now();
        let (result, _) = interactive(
            &tmp.0,
            "echo spellbook",
            Duration::from_secs(20),
            None,
            render_only,
        )
        .await;
        assert_eq!(result.code, Some(0));
        assert_eq!(result.stdout.trim_end(), "spellbook");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "took {:?}",
            started.elapsed()
        );
    }

    /// A surface that records whether the command ever said it was waiting.
    /// Returns `true` if `ConsoleWaiting` arrived before the console closed.
    async fn waited(mut stream: tokio::sync::mpsc::Receiver<crate::agent::AgentEvent>) -> bool {
        let mut waiting = false;
        while let Some(event) = stream.recv().await {
            match event {
                AgentEvent::ConsoleWaiting { .. } => waiting = true,
                AgentEvent::ConsoleClosed { .. } => break,
                _ => {}
            }
        }
        waiting
    }

    /// The reported bug, at the tool: a command that finishes its line and
    /// exits is working, not asking. Announcing it as waiting is what took the
    /// composer away from the agent for the whole of every `ls`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_ends_its_line_never_says_it_is_waiting() {
        let tmp = TempDir::new();
        let (result, waiting) = interactive(
            &tmp.0,
            "echo spellbook",
            Duration::from_secs(20),
            None,
            waited,
        )
        .await;
        assert_eq!(result.code, Some(0));
        assert!(!waiting, "a whole line and an exit is not a question");
    }

    /// And the case the console exists for: output that stops mid-line and
    /// stays there is a question, and the surface is told so.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_parked_mid_line_says_it_is_waiting() {
        let tmp = TempDir::new();
        let (_result, waiting) = interactive(
            &tmp.0,
            r#"printf 'continue? [Y/n] '; read answer"#,
            Duration::from_secs(2),
            None,
            waited,
        )
        .await;
        assert!(
            waiting,
            "a cursor parked at the end of a line is the shape of a prompt"
        );
    }

    /// A run with no human keeps `/dev/null` on fd 0: the command reads EOF at
    /// once and finishes, rather than parking on a pipe nobody will write to
    /// until the timeout kills it. This is the headless shape — an event
    /// channel, but nobody reading it — and it is deliberately *not* derived
    /// from `events.is_some()`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_with_no_human_reads_eof_instead_of_blocking() {
        let tmp = TempDir::new();
        let (events, mut stream) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(events);
        assert_eq!(ctx.console, ConsoleAccess::None, "the default is no human");

        let started = Instant::now();
        let out = ExecuteTool
            .execute(
                json!({
                    "command": r#"printf 'go? '; read x; echo "[$x]""#,
                    "timeout_secs": 20
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("[]"), "{}", out.content);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it must not sit out the budget: {:?}",
            started.elapsed()
        );
        assert!(
            stream.try_recv().is_err(),
            "no console may be announced where nobody can answer it"
        );
    }

    /// `ConsoleAccess::Interactive` is not enough on its own: without an event
    /// channel there is nobody the console could be announced *to*, and a
    /// command that opened one would park on a question only the model could
    /// answer. That is the case a subagent is in, and the reason its context
    /// forces both fields.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_declared_console_with_no_event_stream_still_runs_unattended() {
        let tmp = TempDir::new();
        let ctx = ToolContext::new(&tmp.0).with_console(ConsoleAccess::Interactive);
        let out = ExecuteTool
            .execute(
                json!({ "command": r#"read x; echo "[$x]""#, "timeout_secs": 20 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.contains("[]"), "{}", out.content);
    }

    /// The boundary that keeps the agent out of its own prompts: a console is
    /// claimable exactly once, by whoever gets there first, and the model never
    /// sees an [`AgentEvent`] at all.
    #[test]
    fn a_console_can_be_claimed_exactly_once() {
        let (gate, _host) = ConsoleGate::open();
        assert!(gate.claim().is_some(), "the first consumer gets the writer");
        assert!(
            gate.claim().is_none(),
            "a teed stream must not produce a second author of the child's input"
        );
    }

    /// A ticket that came off a wire (the mesh voids `gate` to 0 on the way
    /// out) claims nothing, so watching a peer never becomes typing into a
    /// peer's shell.
    #[test]
    fn a_voided_console_ticket_claims_nothing() {
        let voided: ConsoleGate = serde_json::from_str("0").expect("a ticket is a number");
        assert!(voided.claim().is_none());
    }

    #[test]
    fn render_timeout_without_output_says_so() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            timed_out: Some(30),
        };
        let out = render_command_result(&result);
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "command timed out after 30s and was killed (no output produced)"
        );
    }
}
