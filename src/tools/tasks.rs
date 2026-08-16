//! Background task registry, shared across tool calls via
//! [`ToolContext`](super::ToolContext), plus the `task_output` and
//! `task_kill` tools.
//!
//! The `execute` tool with `run_in_background: true` spawns a detached child
//! and registers it here. A monitor task
//! captures stdout/stderr into a tail-capped buffer, enforces the
//! [`BACKGROUND_TIMEOUT`], and records the exit. The agent loop calls
//! [`TaskRegistry::drain_completed`] at the top of every step to notify the
//! model of finished tasks exactly once. Surfaces poll [`TaskRegistry::output`]
//! / [`TaskRegistry::output_full`] for a live tail (the bash rail pane).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args};

/// Cap on the output buffered per task; the tail is kept when exceeded.
pub const OUTPUT_CAP_BYTES: usize = 200 * 1024;

/// Output tail included in finished-task notifications to the model.
pub const NOTIFY_TAIL_BYTES: usize = 2 * 1024;

/// Default output tail returned by `task_output` when `tail_bytes` is unset.
const DEFAULT_TAIL_BYTES: usize = 20_000;

/// Largest tail `task_output` returns (stays inside the global tool-output
/// cap together with the status header).
const MAX_TAIL_BYTES: usize = 28_000;

/// Wall-clock limit on a background task; the child is killed when it
/// elapses and the status reflects the timeout.
pub const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Lifecycle state of one background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    /// Exited with the given code (`-1` when terminated by a signal).
    Done(i32),
    /// Terminated on request (`task_kill`, `kill_all`).
    Killed,
    /// Killed at the [`BACKGROUND_TIMEOUT`].
    TimedOut,
}

impl TaskStatus {
    /// Whether the task is no longer running.
    pub fn is_finished(self) -> bool {
        !matches!(self, TaskStatus::Running)
    }

    /// Short human description: `running`, `exit 0`, `killed`, `timed out`.
    pub fn describe(self) -> String {
        match self {
            TaskStatus::Running => "running".to_string(),
            TaskStatus::Done(code) => format!("exit {code}"),
            TaskStatus::Killed => "killed".to_string(),
            TaskStatus::TimedOut => "timed out".to_string(),
        }
    }
}

/// Snapshot of one background command and its state.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: u32,
    pub command: String,
    pub status: TaskStatus,
    /// When the task was registered (for elapsed clocks on the rail).
    pub started: Instant,
    /// When the task finished, if it has.
    pub finished: Option<Instant>,
}

/// A finished task as returned by [`TaskRegistry::drain_completed`] —
/// reported to the model exactly once.
#[derive(Debug, Clone)]
pub struct FinishedTask {
    pub id: u32,
    pub command: String,
    pub status: TaskStatus,
    /// Last [`NOTIFY_TAIL_BYTES`] of combined stdout/stderr.
    pub tail: String,
}

/// Byte buffer that keeps only the most recent `cap` bytes.
#[derive(Debug)]
struct TailBuffer {
    buf: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl TailBuffer {
    fn with_cap(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn append(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.drain(..excess);
            self.truncated = true;
        }
    }

    /// Last `bytes` of the buffer, lossily decoded.
    fn tail(&self, bytes: usize) -> String {
        let start = self.buf.len().saturating_sub(bytes);
        String::from_utf8_lossy(&self.buf[start..]).into_owned()
    }

    /// Full buffer as lossy UTF-8 (already capped at [`OUTPUT_CAP_BYTES`]).
    fn as_str(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

/// Internal per-task state (snapshot exposed as [`Task`]).
#[derive(Debug)]
struct TaskEntry {
    command: String,
    status: TaskStatus,
    output: TailBuffer,
    /// Already returned by [`TaskRegistry::drain_completed`].
    reported: bool,
    /// Signals the monitor to kill the child. Consumed on first kill.
    kill: Option<oneshot::Sender<()>>,
    started: Instant,
    finished: Option<Instant>,
    /// Generation counter bumped on every output append, so a surface can
    /// poll cheaply ("has anything new landed since I last rendered?").
    output_gen: u64,
}

impl TaskEntry {
    fn snapshot(&self, id: u32) -> Task {
        Task {
            id,
            command: self.command.clone(),
            status: self.status,
            started: self.started,
            finished: self.finished,
        }
    }
}

/// Session-wide registry of background tasks.
#[derive(Debug, Default)]
pub struct TaskRegistry {
    tasks: Mutex<HashMap<u32, TaskEntry>>,
    next_id: AtomicU32,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, TaskEntry>> {
        self.tasks.lock().expect("task registry lock poisoned")
    }

    /// Register a new running task and return its id (1-based).
    pub fn add(&self, command: impl Into<String>) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.lock().insert(
            id,
            TaskEntry {
                command: command.into(),
                status: TaskStatus::Running,
                output: TailBuffer::with_cap(OUTPUT_CAP_BYTES),
                reported: false,
                kill: None,
                started: Instant::now(),
                finished: None,
                output_gen: 0,
            },
        );
        id
    }

    /// Take ownership of an already-spawned child: register it, capture its
    /// stdout/stderr into the tail buffer, enforce the
    /// [`BACKGROUND_TIMEOUT`], and record the exit. Returns the task id.
    pub fn spawn(self: &Arc<Self>, command_line: &str, child: tokio::process::Child) -> u32 {
        self.spawn_with_timeout(command_line, child, BACKGROUND_TIMEOUT)
    }

    /// [`spawn`](Self::spawn) with an explicit timeout (tests).
    pub fn spawn_with_timeout(
        self: &Arc<Self>,
        command_line: &str,
        mut child: tokio::process::Child,
        timeout: Duration,
    ) -> u32 {
        let id = self.add(command_line);
        let readers = vec![
            tokio::spawn(read_into(Arc::clone(self), id, child.stdout.take())),
            tokio::spawn(read_into(Arc::clone(self), id, child.stderr.take())),
        ];
        self.monitor(id, child, timeout, readers);
        id
    }

    /// Take over a command that has been running in the foreground: register
    /// it, seed its buffer with the output already captured, and monitor it
    /// from here as an ordinary background task. Returns the task id.
    ///
    /// This is the Ctrl-B path. The caller keeps its own readers — by the time
    /// a command is backgrounded its pipes are already taken and pumped
    /// somewhere else — so it hands over the task draining them, which the
    /// monitor awaits before the task becomes drainable, exactly as
    /// [`spawn_with_timeout`](Self::spawn_with_timeout) awaits its own.
    ///
    /// The [`BACKGROUND_TIMEOUT`] runs from adoption rather than from the
    /// command's start: the foreground timeout the user just escaped is not the
    /// one that should apply to a job they explicitly asked to keep running.
    pub fn adopt(
        self: &Arc<Self>,
        command_line: &str,
        child: tokio::process::Child,
        seed: &[u8],
        drain: impl FnOnce(u32) -> tokio::task::JoinHandle<()>,
    ) -> u32 {
        let id = self.add(command_line);
        if !seed.is_empty() {
            self.append_output(id, seed);
        }
        // The drain is started against the id it writes to, which is why it
        // arrives as a closure rather than a handle: the caller cannot spawn it
        // before the id exists, and the registry cannot spawn it at all without
        // knowing how the caller's pipes are already plumbed.
        let drain = drain(id);
        self.monitor(id, child, BACKGROUND_TIMEOUT, vec![drain]);
        id
    }

    /// Wait on a registered task's child: record the exit, honour a kill
    /// request, and enforce `timeout`. `readers` are the tasks capturing the
    /// child's output; they are awaited before the task is finished so nothing
    /// they still hold is lost from the buffer.
    fn monitor(
        self: &Arc<Self>,
        id: u32,
        mut child: tokio::process::Child,
        timeout: Duration,
        readers: Vec<tokio::task::JoinHandle<()>>,
    ) {
        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        if let Some(entry) = self.lock().get_mut(&id) {
            entry.kill = Some(kill_tx);
        }

        let registry = Arc::clone(self);
        tokio::spawn(async move {
            // Wait for exit, kill request, or timeout. The wait future is
            // pinned in an inner scope so `child` is free again for `kill`.
            let waited = {
                let wait = tokio::time::timeout(timeout, child.wait());
                tokio::pin!(wait);
                tokio::select! {
                    _ = &mut kill_rx => None,
                    res = &mut wait => Some(res),
                }
            };
            let status = match waited {
                // Kill requested (task_kill / kill_all).
                None => {
                    kill_tree(&mut child).await;
                    TaskStatus::Killed
                }
                Some(Ok(Ok(exit))) => TaskStatus::Done(exit.code().unwrap_or(-1)),
                Some(Ok(Err(err))) => {
                    tracing::warn!("waiting on background task #{id} failed: {err}");
                    TaskStatus::Done(-1)
                }
                // Timeout elapsed.
                Some(Err(_)) => {
                    kill_tree(&mut child).await;
                    TaskStatus::TimedOut
                }
            };
            // Let the readers capture whatever output is still buffered
            // before the task becomes drainable.
            for reader in readers {
                let _ = reader.await;
            }
            registry.finish(id, status);
        });
    }

    /// Record the final status of a task and drop its kill handle.
    fn finish(&self, id: u32, status: TaskStatus) {
        if let Some(entry) = self.lock().get_mut(&id) {
            if entry.status == TaskStatus::Running {
                entry.status = status;
                entry.finished = Some(Instant::now());
            }
            entry.kill = None;
        }
    }

    /// Mark a task finished `ago` in the past, for surfaces tests that need a
    /// row old enough to have aged off the rail. There is no other way to get
    /// one: retirement is a comparison against `finished`, and a test cannot
    /// wait out the linger.
    #[cfg(test)]
    pub fn finish_in_the_past(&self, id: u32, status: TaskStatus, ago: Duration) {
        if let Some(entry) = self.lock().get_mut(&id) {
            entry.status = status;
            entry.finished = Some(Instant::now() - ago);
            entry.kill = None;
        }
    }

    /// Append captured output to a task's tail buffer.
    pub fn append_output(&self, id: u32, data: &[u8]) {
        if let Some(entry) = self.lock().get_mut(&id) {
            entry.output.append(data);
            entry.output_gen = entry.output_gen.saturating_add(1);
        }
    }

    /// Snapshot of all tasks, ordered by id.
    pub fn list(&self) -> Vec<Task> {
        let mut tasks: Vec<Task> = self
            .lock()
            .iter()
            .map(|(id, entry)| entry.snapshot(*id))
            .collect();
        tasks.sort_unstable_by_key(|task| task.id);
        tasks
    }

    /// Status of one task, if it exists.
    pub fn status(&self, id: u32) -> Option<TaskStatus> {
        self.lock().get(&id).map(|entry| entry.status)
    }

    /// Snapshot plus the last `tail_bytes` of buffered output of one task.
    pub fn output(&self, id: u32, tail_bytes: usize) -> Option<(Task, String)> {
        self.lock()
            .get(&id)
            .map(|entry| (entry.snapshot(id), entry.output.tail(tail_bytes)))
    }

    /// Snapshot plus the full buffered output and the current generation
    /// counter. The TUI bash pane uses the generation to skip re-renders when
    /// nothing new has landed.
    pub fn output_full(&self, id: u32) -> Option<(Task, String, u64)> {
        self.lock()
            .get(&id)
            .map(|entry| (entry.snapshot(id), entry.output.as_str(), entry.output_gen))
    }

    /// Request termination of a running task. Returns false when the task is
    /// unknown or already finished (or a kill is already in flight).
    pub fn kill(&self, id: u32) -> bool {
        let sender = self.lock().get_mut(&id).and_then(|entry| entry.kill.take());
        match sender {
            Some(sender) => sender.send(()).is_ok(),
            None => false,
        }
    }

    /// Kill every running task (agent shutdown). The children also carry
    /// `kill_on_drop`, this makes the teardown explicit and immediate.
    pub fn kill_all(&self) {
        let senders: Vec<oneshot::Sender<()>> = self
            .lock()
            .values_mut()
            .filter_map(|entry| entry.kill.take())
            .collect();
        for sender in senders {
            let _ = sender.send(());
        }
    }

    /// Finished tasks not yet reported, each returned exactly once, ordered
    /// by id. The tail carries the last [`NOTIFY_TAIL_BYTES`] of output.
    pub fn drain_completed(&self) -> Vec<FinishedTask> {
        let mut finished: Vec<FinishedTask> = self
            .lock()
            .iter_mut()
            .filter(|(_, entry)| entry.status.is_finished() && !entry.reported)
            .map(|(id, entry)| {
                entry.reported = true;
                FinishedTask {
                    id: *id,
                    command: entry.command.clone(),
                    status: entry.status,
                    tail: entry.output.tail(NOTIFY_TAIL_BYTES),
                }
            })
            .collect();
        finished.sort_unstable_by_key(|task| task.id);
        finished
    }
}

/// Kill a background task's whole process tree and reap the child.
///
/// Background children are spawned as their own process-group leaders, so the
/// SIGKILL goes to the whole group: the shell may fork the command rather than
/// exec it (dash does), and killing only the shell would leave a grandchild
/// running, holding the output pipes open, which blocks the monitor until the
/// orphan exits.
async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        crate::platform::process::kill_group(pid);
    }
    let _ = child.kill().await;
}

/// Stream a child's stdout or stderr into the task's tail buffer.
async fn read_into(
    registry: Arc<TaskRegistry>,
    id: u32,
    stream: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
) {
    let Some(mut stream) = stream else { return };
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => registry.append_output(id, &buf[..n]),
        }
    }
}

/// Arguments for [`TaskOutputTool`].
#[derive(Debug, Deserialize)]
struct TaskOutputArgs {
    id: u32,
    /// How many bytes of the output tail to return (default 20000).
    #[serde(default)]
    tail_bytes: Option<usize>,
}

/// `task_output` — buffered output and status of a background task.
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Return the status and buffered output (stdout+stderr tail) of a background \
         task started with execute run_in_background. For services that must outlive \
         the agent, use `nohup ... &` and ordinary shell log checks instead."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Background task id" },
                "tail_bytes": { "type": "integer", "description": "How many bytes of the output tail to return (default 20000)" }
            },
            "required": ["id"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: TaskOutputArgs = parse_args(self.name(), args)?;
        let tail_bytes = args
            .tail_bytes
            .unwrap_or(DEFAULT_TAIL_BYTES)
            .min(MAX_TAIL_BYTES);
        let Some((task, output)) = ctx.tasks.output(args.id, tail_bytes) else {
            return Ok(ToolOutput::error(format!(
                "no background task #{}",
                args.id
            )));
        };
        let mut content = format!(
            "Background task #{} [{}]: {}",
            task.id,
            task.status.describe(),
            task.command
        );
        if output.trim().is_empty() {
            content.push_str("\n(no output)");
        } else {
            content.push('\n');
            content.push_str(output.trim_end());
        }
        Ok(ToolOutput::ok(content))
    }
}

/// Arguments for [`TaskKillTool`].
#[derive(Debug, Deserialize)]
struct TaskKillArgs {
    id: u32,
}

/// `task_kill` — terminate a running background task.
pub struct TaskKillTool;

#[async_trait]
impl Tool for TaskKillTool {
    fn name(&self) -> &str {
        "task_kill"
    }

    fn description(&self) -> &str {
        "Kill a running background task started with execute run_in_background. \
         Agent-managed jobs only — long-lived services should use `nohup ... &`, \
         not run_in_background."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Background task id" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: TaskKillArgs = parse_args(self.name(), args)?;
        if ctx.tasks.kill(args.id) {
            return Ok(ToolOutput::ok(format!(
                "kill signal sent to background task #{}",
                args.id
            )));
        }
        Ok(match ctx.tasks.status(args.id) {
            None => ToolOutput::error(format!("no background task #{}", args.id)),
            Some(status) if status.is_finished() => ToolOutput::error(format!(
                "background task #{} already finished ({})",
                args.id,
                status.describe()
            )),
            Some(_) => ToolOutput::error(format!(
                "background task #{} is already being killed",
                args.id
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;
    use crate::platform::process::ProcessGroupExt;

    /// Spawn `script` through the platform shell with piped stdio, ready for
    /// the registry: the same configuration as the `execute` tool's background
    /// branch, including the own process group that `kill_tree` targets.
    fn spawn_sh(script: &str) -> tokio::process::Child {
        let mut command = crate::platform::shell::tokio_command(script);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .own_process_group();
        command.spawn().expect("spawn test child")
    }

    /// Poll until task `id` is finished (10s deadline).
    async fn wait_finished(registry: &TaskRegistry, id: u32) -> TaskStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = registry.status(id).expect("task exists");
            if status.is_finished() {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "task #{id} did not finish in time"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn ctx_with(registry: &Arc<TaskRegistry>) -> ToolContext {
        ToolContext {
            tasks: Arc::clone(registry),
            ..ToolContext::new(std::env::temp_dir())
        }
    }

    #[test]
    fn add_assigns_sequential_ids_and_list_is_ordered() {
        let registry = TaskRegistry::new();
        assert!(registry.list().is_empty());

        let first = registry.add("cargo build");
        let second = registry.add("cargo test");
        assert_eq!(first, 1);
        assert_eq!(second, 2);

        let tasks = registry.list();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].command, "cargo build");
        assert_eq!(tasks[0].status, TaskStatus::Running);
        assert_eq!(tasks[1].id, 2);
    }

    #[test]
    fn tail_buffer_keeps_the_tail_when_over_cap() {
        let mut buffer = TailBuffer::with_cap(10);
        buffer.append(b"0123456789");
        assert!(!buffer.truncated);
        assert_eq!(buffer.tail(100), "0123456789");

        buffer.append(b"abcdef");
        assert!(buffer.truncated);
        assert_eq!(buffer.buf.len(), 10);
        assert_eq!(buffer.tail(100), "6789abcdef");
        assert_eq!(buffer.tail(4), "cdef", "tail respects the byte count");

        // One oversized append keeps only the newest cap bytes.
        let mut buffer = TailBuffer::with_cap(4);
        buffer.append(b"abcdefgh");
        assert_eq!(buffer.tail(100), "efgh");
        assert!(buffer.truncated);
    }

    #[tokio::test]
    async fn spawn_records_exit_and_output_and_drains_exactly_once() {
        let registry = Arc::new(TaskRegistry::new());
        let id = registry.spawn("echo demo", spawn_sh("echo out; echo err >&2; exit 3"));
        assert_eq!(id, 1);

        let status = wait_finished(&registry, id).await;
        assert_eq!(status, TaskStatus::Done(3));
        assert_eq!(status.describe(), "exit 3");

        let drained = registry.drain_completed();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, 1);
        assert_eq!(drained[0].command, "echo demo");
        assert_eq!(drained[0].status, TaskStatus::Done(3));
        assert!(drained[0].tail.contains("out"), "{}", drained[0].tail);
        assert!(drained[0].tail.contains("err"), "{}", drained[0].tail);

        assert!(
            registry.drain_completed().is_empty(),
            "finished tasks are reported exactly once"
        );
    }

    #[tokio::test]
    async fn drain_skips_running_tasks() {
        let registry = Arc::new(TaskRegistry::new());
        let quick = registry.spawn("quick", spawn_sh("echo hi"));
        let slow = registry.spawn("slow", spawn_sh("sleep 30"));
        wait_finished(&registry, quick).await;

        let drained = registry.drain_completed();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, quick);
        assert_eq!(registry.status(slow), Some(TaskStatus::Running));

        registry.kill_all();
        wait_finished(&registry, slow).await;
    }

    #[tokio::test]
    async fn kill_terminates_a_running_task() {
        let registry = Arc::new(TaskRegistry::new());
        let id = registry.spawn("sleep", spawn_sh("sleep 30"));
        assert!(registry.kill(id));
        let status = wait_finished(&registry, id).await;
        assert_eq!(status, TaskStatus::Killed);
        assert!(!registry.kill(id), "kill of a finished task reports false");
        assert!(!registry.kill(999), "kill of an unknown id reports false");
    }

    #[tokio::test]
    async fn kill_all_terminates_every_running_task() {
        let registry = Arc::new(TaskRegistry::new());
        let a = registry.spawn("a", spawn_sh("sleep 30"));
        let b = registry.spawn("b", spawn_sh("sleep 30"));
        registry.kill_all();
        assert_eq!(wait_finished(&registry, a).await, TaskStatus::Killed);
        assert_eq!(wait_finished(&registry, b).await, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn timeout_kills_the_task_and_marks_it_timed_out() {
        let registry = Arc::new(TaskRegistry::new());
        let id =
            registry.spawn_with_timeout("sleep", spawn_sh("sleep 30"), Duration::from_millis(50));
        let status = wait_finished(&registry, id).await;
        assert_eq!(status, TaskStatus::TimedOut);
        assert_eq!(status.describe(), "timed out");
        let drained = registry.drain_completed();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].status, TaskStatus::TimedOut);
    }

    #[tokio::test]
    async fn task_output_tool_returns_status_and_tail() {
        let registry = Arc::new(TaskRegistry::new());
        let id = registry.spawn("echo", spawn_sh("printf 'abcdefgh'"));
        wait_finished(&registry, id).await;
        let ctx = ctx_with(&registry);

        let out = TaskOutputTool
            .execute(json!({ "id": id }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("[exit 0]"), "{}", out.content);
        assert!(out.content.contains("abcdefgh"), "{}", out.content);

        // tail_bytes limits the returned output.
        let out = TaskOutputTool
            .execute(json!({ "id": id, "tail_bytes": 4 }), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("efgh"), "{}", out.content);
        assert!(!out.content.contains("abcd"), "{}", out.content);

        // Unknown ids are tool-level errors.
        let out = TaskOutputTool
            .execute(json!({ "id": 999 }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no background task #999"));
    }

    #[tokio::test]
    async fn task_kill_tool_kills_and_reports_state() {
        let registry = Arc::new(TaskRegistry::new());
        let id = registry.spawn("sleep", spawn_sh("sleep 30"));
        let ctx = ctx_with(&registry);

        let out = TaskKillTool
            .execute(json!({ "id": id }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("kill signal sent"), "{}", out.content);
        wait_finished(&registry, id).await;

        let out = TaskKillTool
            .execute(json!({ "id": id }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("already finished"), "{}", out.content);

        let out = TaskKillTool
            .execute(json!({ "id": 7 }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no background task #7"));
    }
}
