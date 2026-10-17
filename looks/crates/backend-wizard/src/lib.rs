//! `wizard acp` as an `agent_core::Backend`.
//!
//! The process speaks ACP (JSON-RPC 2.0, one message per line) on stdio.
//! [`core::Core`] holds all the protocol logic with no I/O; this file owns the
//! child process and the three tasks around it (stdout reader, stderr ring,
//! stdin writer) plus the actor loop that joins them to the frontend's channels.
//!
//! Choices that are not obvious from the event model:
//!
//! * **Slash commands.** A prompt that starts with `/` and names a command in
//!   the server's advertised list is a command turn: its message text is held
//!   back and delivered as one `Notice(Info)` just before `TurnEnd`, so the UI
//!   can show it as a result. Tool events inside the turn still stream live.
//!   Any other `/word` (paths, custom commands) goes to the model and streams
//!   as `TextDelta`.
//! * **Usage.** ACP carries no usage. After each ordinary turn (and `/compact`)
//!   the backend runs a hidden `/status`, which the server answers locally
//!   without a model call and without touching the transcript, and parses the
//!   session totals into `Usage { input_tokens, output_tokens, context_tokens }`
//!   (cumulative for the session; cache tokens, window size and cost stay
//!   zero/`None`). `~/.wizard/usage.jsonl` is not used: it is written once at
//!   process exit, with no session id.
//! * **Sessions.** `Ready` is emitted once per session switch, not once per
//!   process: after `NewSession` and `LoadSession` too. A load emits `History`
//!   first (and `Todos` if the replayed transcript had a todo list), then
//!   `Ready` with the loaded session's config. The server resets model, effort
//!   and mode to the config defaults on every load.
//! * **Prompts while busy** are queued and sent when the turn ends; the server
//!   leaves a prompt that arrives mid-turn unanswered forever. `Cancel` drops
//!   the queue. Session switches and option changes mid-turn give a
//!   `Notice(Warn)` and do nothing.
//! * **Interrupts.** `session/cancel` only lands at the next stream or tool
//!   boundary, so a running `sleep 40` would go on for 40 seconds. 300 ms after
//!   a `Cancel` the backend kills the process groups of the `execute` tools that
//!   are still running (wizard starts each as `sh -c <command>` in a group of
//!   its own; MCP servers are not touched), which makes the tool fail and the
//!   turn end. If the turn is still open `OPENW_CANCEL_GRACE_MS` (10 s) after
//!   the cancel, or a second `Cancel` comes three seconds after the first, wizard
//!   is killed and started again on the same session (`History`, `Ready`, then a
//!   `Notice(Warn)` saying so).
//! * **Output bytes** are decoded lossily line by line; a stray non-UTF-8 byte
//!   does not end the session.
//! * **Edits.** wizard sends no diff blocks, so `ToolCall::diff` is rebuilt from
//!   the arguments: `edit_file` gives the replaced snippet (`old_string` to
//!   `new_string`), `write_file` gives the new content with `old: None`.
//! * **`[wizard] ...` chunks** (wizard's own retry and error lines, which it
//!   injects into the message stream) become `Notice(Warn)`.

pub mod core;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use agent_core::{Backend, BackendHandle, Event, Request};
use anyhow::Context;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::core::{Action, Core};

/// Slash commands `wizard acp` refuses by name. The frontend handles these
/// itself; they are never sent. Without the leading slash, like `SlashCommand::name`.
pub const FRONTEND_ONLY: &[&str] = &[
    "clear",
    "resume",
    "resume-claude",
    "login",
    "settings",
    "dashboard",
    "vim",
    "ui",
    "view",
    "quit",
    "exit",
];

pub struct WizardBackend;

const STDERR_LINES: usize = 64;
const STDERR_LINE_CHARS: usize = 400;
/// The longest stdout line kept. A tool result is one JSON line, so this is generous; a line
/// past it is dropped (half a JSON object parses to nothing) and reported.
const STDOUT_LINE_MAX: usize = 32 * 1024 * 1024;
/// stderr lines are only kept 400 characters of, so there is no reason to hold more.
const STDERR_LINE_MAX: usize = 16 * 1024;

/// Next `\n`-terminated line, holding at most `cap` bytes of it. What is past the cap is read
/// and thrown away, so a line without a newline in it costs `cap`, not its length. Returns the
/// text and how many bytes were dropped; `None` at the end of the stream.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    r: &mut R,
    cap: usize,
) -> std::io::Result<Option<(String, usize)>> {
    let mut line: Vec<u8> = Vec::new();
    let mut dropped = 0usize;
    let mut any = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            if !any {
                return Ok(None);
            }
            break;
        }
        any = true;
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        let payload = if done { take - 1 } else { take };
        let keep = payload.min(cap.saturating_sub(line.len()));
        line.extend_from_slice(&chunk[..keep]);
        dropped += payload - keep;
        r.consume(take);
        if done {
            break;
        }
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some((String::from_utf8_lossy(&line).into_owned(), dropped)))
}
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Ring(VecDeque<String>);

impl Ring {
    fn push(&mut self, line: &str) {
        if self.0.len() == STDERR_LINES {
            self.0.pop_front();
        }
        self.0
            .push_back(line.chars().take(STDERR_LINE_CHARS).collect());
    }

    fn tail(&self, n: usize) -> String {
        let skip = self.0.len().saturating_sub(n);
        self.0
            .iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The session `/ui` hands over when wizard starts this look from another one: `WIZARD_HANDOFF`
/// is JSON, and its `session` is what to resume. wizard reads the model and mode from it itself.
fn handoff_session(value: Option<&str>) -> Option<String> {
    let handoff: serde_json::Value = serde_json::from_str(value?).ok()?;
    handoff["session"].as_str().map(str::to_string)
}

fn wizard_bin() -> OsString {
    match std::env::var_os("OPENW_WIZARD_BIN") {
        Some(v) if !v.is_empty() => v,
        _ => OsString::from("wizard"),
    }
}

/// `wizard --version` prints `wizard 3.7.1`. Empty when it fails or takes longer than two
/// seconds; the core then falls back to the version in the initialize answer. Runs inside the
/// backend task, so a slow binary cannot hold up the frontend's start.
async fn wizard_version(bin: &OsString) -> String {
    let run = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(2), run).await else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        String::new()
    } else if line.starts_with("wizard") {
        line.to_string()
    } else {
        format!("wizard {line}")
    }
}

/// How long an interrupted turn may keep running before wizard is restarted. A cooperative
/// cancel only lands at the next stream or tool boundary, so a stalled provider stream or a
/// tool that ignores its group being killed would otherwise leave the turn open forever.
/// `OPENW_CANCEL_GRACE_MS` overrides it (tests).
fn cancel_grace() -> Duration {
    std::env::var("OPENW_CANCEL_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(10), Duration::from_millis)
}

/// Pause between `session/cancel` and killing the running tool, so wizard has seen the cancel
/// before the tool fails and cannot go on to start the next one.
const TOOL_KILL_DELAY: Duration = Duration::from_millis(300);
/// A second interrupt this long after the first one forces the restart.
const FORCE_AFTER: Duration = Duration::from_secs(3);

/// What `ps` says about one process.
#[derive(Debug, PartialEq)]
pub struct ProcRow {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub args: String,
}

/// Parse `ps -A -o pid=,ppid=,pgid=,args=`. A multi-line argument continues the previous row.
pub fn parse_ps(text: &str) -> Vec<ProcRow> {
    let mut rows: Vec<ProcRow> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let nums = (|| {
            Some((
                it.next()?.parse::<u32>().ok()?,
                it.next()?.parse::<u32>().ok()?,
                it.next()?.parse::<u32>().ok()?,
            ))
        })();
        match nums {
            Some((pid, ppid, pgid)) => {
                let mut rest = line.trim_start();
                for _ in 0..3 {
                    rest = rest
                        .trim_start_matches(|c: char| !c.is_whitespace())
                        .trim_start();
                }
                let args = rest.to_string();
                rows.push(ProcRow {
                    pid,
                    ppid,
                    pgid,
                    args,
                });
            }
            None => {
                if let Some(last) = rows.last_mut() {
                    last.args.push('\n');
                    last.args.push_str(line);
                }
            }
        }
    }
    rows
}

/// Process groups of the tools wizard is running right now. wizard starts each `execute` tool
/// as `sh -c <command>` in a group of its own, a direct child of the `acp` process, while its
/// MCP servers stay in wizard's group. So the tools to stop are the direct children that lead
/// their own group and whose command line carries the command of a running tool call; nothing
/// else (MCP servers, a background task from an earlier turn) is touched.
pub fn tool_groups(rows: &[ProcRow], wizard: u32, commands: &[String]) -> Vec<u32> {
    let own = rows.iter().find(|r| r.pid == wizard).map(|r| r.pgid);
    let mut out: Vec<u32> = rows
        .iter()
        .filter(|r| r.ppid == wizard && r.pgid == r.pid && Some(r.pgid) != own)
        .filter(|r| commands.iter().any(|c| r.args.contains(c.as_str())))
        .map(|r| r.pgid)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// First line of an execute tool's command, the part that shows up in `ps`.
fn command_key(call: &agent_core::ToolCall) -> Option<String> {
    if call.kind != agent_core::ToolKind::Execute {
        return None;
    }
    let c = call.input.get("command")?.as_str()?;
    let first = c.lines().map(str::trim).find(|l| !l.is_empty())?;
    (first.len() >= 3).then(|| first.to_string())
}

fn signal_group(pgid: u32, sig: i32) -> bool {
    // SAFETY: plain kill(2); a negative pid addresses the process group.
    unsafe { libc::kill(-(pgid as i32), sig) == 0 }
}

/// SIGTERM the groups, SIGKILL whatever is left after a moment.
async fn kill_groups(groups: Vec<u32>) {
    for g in &groups {
        signal_group(*g, libc::SIGTERM);
    }
    if groups.is_empty() {
        return;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for g in groups {
        signal_group(g, libc::SIGKILL);
    }
}

async fn running_tool_groups(wizard: Option<u32>, commands: &HashMap<String, String>) -> Vec<u32> {
    let Some(wizard) = wizard else {
        return Vec::new();
    };
    if commands.is_empty() {
        return Vec::new();
    }
    let ps = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid=,args="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(2), ps).await else {
        return Vec::new();
    };
    let rows = parse_ps(&String::from_utf8_lossy(&out.stdout));
    let cmds: Vec<String> = commands.values().cloned().collect();
    tool_groups(&rows, wizard, &cmds)
}

/// The child process and the tasks that feed it.
struct Proc {
    child: Child,
    writer: tokio::task::JoinHandle<()>,
    lines: UnboundedReceiver<Option<String>>,
    ring: Arc<Mutex<Ring>>,
    w_tx: Option<UnboundedSender<String>>,
}

fn launch(bin: &OsString, cwd: &std::path::Path) -> anyhow::Result<Proc> {
    let mut child = Command::new(bin)
        .arg("acp")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "could not start `{} acp` (set OPENW_WIZARD_BIN to the wizard binary)",
                bin.to_string_lossy()
            )
        })?;
    let stdin = child.stdin.take().context("no stdin on wizard acp")?;
    let stdout = child.stdout.take().context("no stdout on wizard acp")?;
    let stderr = child.stderr.take().context("no stderr on wizard acp")?;

    let ring = Arc::new(Mutex::new(Ring::default()));
    let (line_tx, lines) = unbounded_channel::<Option<String>>();
    let ring_o = ring.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stdout);
        // `read_line_capped` decodes lossily, so one stray non-UTF-8 byte (a child that inherited
        // stdout and prints Latin-1) does not end the session, and bounds what one line costs.
        while let Ok(Some((l, dropped))) = read_line_capped(&mut r, STDOUT_LINE_MAX).await {
            if dropped > 0 {
                // half a JSON object parses to nothing; leave a trace for the crash tail
                if let Ok(mut ring) = ring_o.lock() {
                    ring.push(&format!(
                        "dropped a {} MB line from wizard",
                        (l.len() + dropped) >> 20
                    ));
                }
                continue;
            }
            if line_tx.send(Some(l)).is_err() {
                return;
            }
        }
        let _ = line_tx.send(None);
    });
    let ring_w = ring.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stderr);
        while let Ok(Some((l, _))) = read_line_capped(&mut r, STDERR_LINE_MAX).await {
            if let Ok(mut ring) = ring_w.lock() {
                ring.push(&l);
            }
        }
    });
    let (w_tx, mut w_rx) = unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(l) = w_rx.recv().await {
            if stdin.write_all(l.as_bytes()).await.is_err()
                || stdin.write_all(b"\n").await.is_err()
                || stdin.flush().await.is_err()
            {
                return;
            }
        }
        // Dropping stdin here is the EOF that tells wizard to clean up and exit.
    });
    Ok(Proc {
        child,
        writer,
        lines,
        ring,
        w_tx: Some(w_tx),
    })
}

impl Backend for WizardBackend {
    fn spawn(cwd: PathBuf, resume: Option<String>) -> anyhow::Result<BackendHandle> {
        Self::spawn_with(cwd, resume, false, false)
    }
}

impl WizardBackend {
    /// [`Backend::spawn`], with wizard's questions (a finished plan) shown to the frontend as
    /// [`Event::Permission`] when `ask` is set. Without it they are answered "allow" here, so a
    /// frontend with no prompt to show never leaves a turn waiting.
    ///
    /// With `answer_unknown`, wizard answers a `/name` nothing runs as unknown rather than
    /// sending it to the model.
    pub fn spawn_with(
        cwd: PathBuf,
        resume: Option<String>,
        ask: bool,
        answer_unknown: bool,
    ) -> anyhow::Result<BackendHandle> {
        let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
        let resume =
            resume.or_else(|| handoff_session(std::env::var("WIZARD_HANDOFF").ok().as_deref()));
        let bin = wizard_bin();
        let proc = launch(&bin, &cwd)?;
        let (req_tx, req_rx) = unbounded_channel();
        let (ev_tx, ev_rx) = unbounded_channel();
        agent_core::spawn_supervised(
            "the wizard driver",
            ev_tx.clone(),
            run(proc, bin, cwd, resume, req_rx, ev_tx, (ask, answer_unknown)),
        );
        Ok(BackendHandle {
            tx: req_tx,
            rx: ev_rx,
        })
    }
}

/// Between the core and the frontend: turns [`Action`]s into writes and events, and keeps what
/// an interrupt needs to know about the tools that are running.
struct Link {
    ev_tx: UnboundedSender<Event>,
    /// Command of every execute tool that has started and not finished, by call id.
    running: HashMap<String, String>,
    /// A `TurnEnd` went out since this was last cleared.
    turn_ended: bool,
    /// Sent right after the next `Ready`, once the transcript replay has replaced the screen.
    after_ready: Option<String>,
}

impl Link {
    /// Returns false once the frontend is gone or the backend reported Fatal.
    fn dispatch(&mut self, p: &mut Proc, actions: Vec<Action>) -> bool {
        let mut keep = true;
        for a in actions {
            match a {
                Action::Send(v) => {
                    if let Some(tx) = &p.w_tx {
                        let _ = tx.send(v.to_string());
                    }
                }
                Action::CloseStdin => p.w_tx = None,
                Action::Emit(Event::Fatal(msg)) => {
                    let t = p.ring.lock().map(|r| r.tail(6)).unwrap_or_default();
                    let text = if t.is_empty() {
                        msg
                    } else {
                        format!("{msg}\n{t}")
                    };
                    let _ = self.ev_tx.send(Event::Fatal(text));
                    keep = false;
                }
                Action::Emit(e) => {
                    match &e {
                        Event::Tool(call) => match (call.status, command_key(call)) {
                            (
                                agent_core::ToolStatus::Pending | agent_core::ToolStatus::Running,
                                Some(c),
                            ) => {
                                self.running.insert(call.id.clone(), c);
                            }
                            _ => {
                                self.running.remove(&call.id);
                            }
                        },
                        Event::TurnEnd(_) => {
                            self.running.clear();
                            self.turn_ended = true;
                        }
                        _ => {}
                    }
                    let ready = matches!(e, Event::Ready { .. });
                    if self.ev_tx.send(e).is_err() {
                        keep = false;
                    }
                    if ready {
                        if let Some(text) = self.after_ready.take() {
                            let _ = self.ev_tx.send(Event::Notice {
                                level: agent_core::NoticeLevel::Warn,
                                text,
                            });
                        }
                    }
                }
            }
        }
        keep
    }
}

enum End {
    /// The frontend asked for it (or went away).
    Shutdown,
    /// The backend is gone; `Fatal` has been delivered.
    Dead,
    /// An interrupt did not take: start over from the stored session.
    Restart,
}

/// Where an interrupt is: `Cancel` was sent at `at`, the tools were killed if `killed`.
struct Interrupt {
    at: Instant,
    killed: bool,
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

async fn serve(
    p: &mut Proc,
    core: &mut Core,
    link: &mut Link,
    req_rx: &mut UnboundedReceiver<Request>,
    grace: Duration,
) -> End {
    let actions = core.start();
    if !link.dispatch(p, actions) {
        return End::Dead;
    }
    let mut cancel: Option<Interrupt> = None;
    loop {
        let wake = cancel.as_ref().map(|c| {
            if c.killed {
                c.at + grace
            } else {
                c.at + TOOL_KILL_DELAY
            }
        });
        tokio::select! {
            r = req_rx.recv() => match r {
                Some(Request::Shutdown) | None => return End::Shutdown,
                Some(Request::Cancel) => {
                    let active = core.turn_active();
                    let acts = core.on_request(Request::Cancel);
                    if !link.dispatch(p, acts) {
                        return End::Dead;
                    }
                    match &cancel {
                        None if active => {
                            cancel = Some(Interrupt { at: Instant::now(), killed: false });
                        }
                        Some(c) if c.killed && c.at.elapsed() >= FORCE_AFTER => {
                            return End::Restart;
                        }
                        _ => {}
                    }
                }
                Some(r) => {
                    let acts = core.on_request(r);
                    if !link.dispatch(p, acts) {
                        return End::Dead;
                    }
                }
            },
            l = p.lines.recv() => match l {
                Some(Some(line)) => {
                    let acts = core.on_line(&line);
                    if !link.dispatch(p, acts) {
                        return End::Dead;
                    }
                }
                _ => {
                    let status = tokio::time::timeout(Duration::from_secs(1), p.child.wait()).await;
                    let how = match status {
                        Ok(Ok(s)) => format!("wizard acp exited ({s})"),
                        _ => "wizard acp closed its output".to_string(),
                    };
                    let acts = core.on_exit(&how);
                    link.dispatch(p, acts);
                    return End::Dead;
                }
            },
            _ = sleep_until_opt(wake) => match cancel.as_mut() {
                Some(c) if !c.killed => {
                    c.killed = true;
                    let groups = running_tool_groups(p.child.id(), &link.running).await;
                    tokio::spawn(kill_groups(groups));
                }
                Some(_) => return End::Restart,
                None => {}
            },
        }
        if link.turn_ended {
            link.turn_ended = false;
            cancel = None;
        }
    }
}

async fn run(
    mut p: Proc,
    bin: OsString,
    cwd: PathBuf,
    resume: Option<String>,
    mut req_rx: UnboundedReceiver<Request>,
    ev_tx: UnboundedSender<Event>,
    (ask, answer_unknown): (bool, bool),
) {
    let version = wizard_version(&bin).await;
    let grace = cancel_grace();
    let mut link = Link {
        ev_tx,
        running: HashMap::new(),
        turn_ended: false,
        after_ready: None,
    };
    let cwd_s = cwd.to_string_lossy().into_owned();
    let mut core = Core::new(cwd_s.clone(), resume, version.clone());
    core.set_asking(ask);
    core.set_answer_unknown(answer_unknown);
    let graceful = loop {
        match serve(&mut p, &mut core, &mut link, &mut req_rx, grace).await {
            End::Shutdown => break true,
            End::Dead => break false,
            End::Restart => {
                let groups = running_tool_groups(p.child.id(), &link.running).await;
                kill_groups(groups).await;
                let _ = p.child.kill().await;
                let session = core.session_id().map(str::to_string);
                match launch(&bin, &cwd) {
                    Ok(np) => {
                        p = np;
                        link.running.clear();
                        link.turn_ended = false;
                        link.after_ready = Some(
                            "wizard did not stop after the interrupt, so it was restarted and the session reloaded"
                                .to_string(),
                        );
                        core = Core::new(cwd_s.clone(), session, version.clone());
                        core.set_asking(ask);
                        core.set_answer_unknown(answer_unknown);
                    }
                    Err(e) => {
                        let _ = link.ev_tx.send(Event::Fatal(format!(
                            "wizard did not stop after the interrupt and could not be restarted: {e:#}"
                        )));
                        break false;
                    }
                }
            }
        }
    };

    // Close stdin, give wizard time to remove its empty sessions, then kill. A wizard that was
    // never ready has no session to clean up.
    p.w_tx = None;
    let _ = tokio::time::timeout(Duration::from_secs(1), &mut p.writer).await;
    let wait = if !graceful {
        Duration::from_secs(1)
    } else if core.session_id().is_none() {
        Duration::from_millis(200)
    } else {
        SHUTDOWN_GRACE
    };
    if tokio::time::timeout(wait, p.child.wait()).await.is_err() {
        let _ = p.child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "    1     0     1 /sbin/init\n 100     1   100 wizard acp\n 110   100   100 npm exec @playwright/mcp@latest\n 120   100   120 sh -c set -o pipefail; sleep 55; echo finished\n 121   120   120 sleep 55\n 130   100   130 sh -c set -o pipefail; cargo build\n 140     1   140 sh -c set -o pipefail; sleep 55; echo finished\n";

    fn cmds(c: &[&str]) -> Vec<String> {
        c.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_the_running_tools_group_is_picked() {
        let rows = parse_ps(PS);
        assert_eq!(rows.len(), 7);
        assert_eq!(
            rows[3].args,
            "sh -c set -o pipefail; sleep 55; echo finished"
        );
        // the tool of the running call, not the MCP server, not a stranger's identical command
        assert_eq!(
            tool_groups(&rows, 100, &cmds(&["sleep 55; echo finished"])),
            vec![120]
        );
        // a command that is not running matches nothing
        assert!(tool_groups(&rows, 100, &cmds(&["sleep 99"])).is_empty());
        // nothing known to be running, nothing killed
        assert!(tool_groups(&rows, 100, &[]).is_empty());
        // the MCP server shares wizard's group and is never a candidate
        assert!(tool_groups(&rows, 100, &cmds(&["npm exec"])).is_empty());
    }

    #[test]
    fn a_handoff_names_the_session_to_resume() {
        let json =
            r#"{"session":"2026-10-14T20-45-38","provider":"beta","model":"m","mode":"genie"}"#;
        assert_eq!(
            handoff_session(Some(json)).as_deref(),
            Some("2026-10-14T20-45-38")
        );
        assert_eq!(
            handoff_session(Some(r#"{"session":null,"provider":"beta"}"#)),
            None
        );
        assert_eq!(handoff_session(Some("not json")), None);
        assert_eq!(handoff_session(None), None);
    }

    #[test]
    fn multi_line_arguments_stay_on_their_row() {
        let rows = parse_ps(" 7 1 7 sh -c echo a\necho b\n 8 1 8 true\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].args, "sh -c echo a\necho b");
    }

    #[test]
    fn command_key_is_the_first_nonblank_line_of_an_execute_call() {
        let mut c = agent_core::ToolCall {
            kind: agent_core::ToolKind::Execute,
            input: serde_json::json!({"command": "\n  cargo test\nsleep 3"}),
            ..Default::default()
        };
        assert_eq!(command_key(&c).as_deref(), Some("cargo test"));
        c.kind = agent_core::ToolKind::Read;
        assert_eq!(command_key(&c), None);
    }
}

#[cfg(test)]
mod capped_line_tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_split_and_a_long_one_is_cut_not_buffered() {
        let mut data = b"one\r\ntwo\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', 1000));
        data.extend_from_slice(b"\nlast");
        let mut r = BufReader::with_capacity(16, &data[..]);
        let mut got = Vec::new();
        while let Some(l) = read_line_capped(&mut r, 8).await.unwrap() {
            got.push(l);
        }
        assert_eq!(
            got,
            vec![
                ("one".to_string(), 0),
                ("two".to_string(), 0),
                ("xxxxxxxx".to_string(), 992),
                ("last".to_string(), 0),
            ]
        );
    }

    /// Finding 22: one stderr line with no newline was held whole (400 MB in the report).
    #[tokio::test]
    async fn a_line_with_no_newline_is_held_at_the_cap() {
        let big = vec![b'e'; 5 * 1024 * 1024];
        let mut r = BufReader::new(&big[..]);
        let (l, dropped) = read_line_capped(&mut r, STDERR_LINE_MAX)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.len(), STDERR_LINE_MAX);
        assert_eq!(dropped, big.len() - STDERR_LINE_MAX);
        assert!(read_line_capped(&mut r, 8).await.unwrap().is_none());
    }
}
