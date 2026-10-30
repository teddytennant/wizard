//! Background subagent registry, shared across tool calls via
//! [`ToolContext`](super::ToolContext), plus the `subagent_status` and
//! `subagent_kill` tools.
//!
//! `spawn_subagent` with `background: true` detaches the subagent's run as a
//! tokio task and registers it here instead of awaiting it inline. The agent
//! loop calls [`SubagentTaskRegistry::drain_completed`] at the top of every
//! step to notify the model of finished subagents exactly once, mirroring
//! [`super::tasks::TaskRegistry`] for background shell commands.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args};

/// Cap on the output included in `subagent_status` responses.
const STATUS_OUTPUT_CHARS: usize = 20_000;

/// One message from another agent in the session, waiting to be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    /// Who sent it: `main`, or `agent:<id> <name>`.
    pub from: String,
    pub text: String,
}

/// Messages addressed to one agent (`SendMessage`), read by its loop at the
/// top of the next step, the way a finished background task is.
#[derive(Debug, Default)]
pub struct Inbox {
    messages: Mutex<Vec<InboxMessage>>,
}

impl Inbox {
    pub fn push(&self, from: impl Into<String>, text: impl Into<String>) {
        self.messages
            .lock()
            .expect("inbox lock poisoned")
            .push(InboxMessage {
                from: from.into(),
                text: text.into(),
            });
    }

    /// Everything waiting, oldest first; each message is returned once.
    pub fn take(&self) -> Vec<InboxMessage> {
        std::mem::take(&mut *self.messages.lock().expect("inbox lock poisoned"))
    }
}

/// Terminal outcome of one background subagent run.
#[derive(Debug, Clone)]
pub struct SubagentRunResult {
    /// True when the sub-loop finished on its own (not budget, error, kill).
    pub completed: bool,
    /// The subagent's final report (or an error description).
    pub output: String,
    pub steps_used: u32,
    /// Set when the run ended in a hard error or was killed, with the
    /// reason; `None` for a normal finish or a step-budget stop.
    pub error: Option<String>,
}

impl SubagentRunResult {
    fn killed() -> Self {
        Self {
            completed: false,
            output: String::new(),
            steps_used: 0,
            error: Some("killed on request".to_string()),
        }
    }

    /// One-line status label: `completed`, `hit its step budget`,
    /// `failed: <reason>`.
    pub fn describe(&self) -> String {
        match &self.error {
            Some(error) => format!("failed: {error}"),
            None if self.completed => "completed".to_string(),
            None => "hit its step budget".to_string(),
        }
    }
}

/// A finished background subagent run, returned by
/// [`SubagentTaskRegistry::drain_completed`] exactly once.
#[derive(Debug, Clone)]
pub struct SubagentTaskResult {
    pub id: u32,
    pub name: String,
    pub task: String,
    /// False when the sub-loop hit its step budget or errored out.
    pub completed: bool,
    pub output: String,
    pub steps_used: u32,
    /// Set when the run ended in a hard error (see
    /// [`SubagentRunResult::error`]).
    pub error: Option<String>,
}

/// Point-in-time view of one registered run, for `subagent_status`.
#[derive(Debug, Clone)]
pub struct SubagentTaskSnapshot {
    pub id: u32,
    pub name: String,
    pub task: String,
    /// `None` while the run is still in flight.
    pub result: Option<SubagentRunResult>,
    /// The completion note already carried the result into the conversation.
    pub reported: bool,
}

impl SubagentTaskSnapshot {
    /// Short status label: `running`, `completed`, `hit its step budget`,
    /// `failed: <reason>`.
    pub fn describe(&self) -> String {
        match &self.result {
            None => "running".to_string(),
            Some(result) => result.describe(),
        }
    }
}

/// Internal per-run state. `result` is `None` while running.
#[derive(Debug)]
struct Entry {
    name: String,
    task: String,
    result: Option<SubagentRunResult>,
    /// Already returned by [`SubagentTaskRegistry::drain_completed`].
    reported: bool,
    /// Detached driver task; aborted by [`SubagentTaskRegistry::kill`].
    handle: Option<tokio::task::JoinHandle<()>>,
    /// Where `SendMessage` leaves mail for the run.
    inbox: Arc<Inbox>,
}

impl Entry {
    fn snapshot(&self, id: u32) -> SubagentTaskSnapshot {
        SubagentTaskSnapshot {
            id,
            name: self.name.clone(),
            task: self.task.clone(),
            result: self.result.clone(),
            reported: self.reported,
        }
    }
}

/// Session-wide registry of background subagent runs.
#[derive(Debug, Default)]
pub struct SubagentTaskRegistry {
    entries: Mutex<HashMap<u32, Entry>>,
    next_id: AtomicU32,
}

impl SubagentTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Entry>> {
        self.entries
            .lock()
            .expect("subagent task registry lock poisoned")
    }

    /// Register `name`/`task` as running and detach `fut` as a tokio task;
    /// returns the new id immediately. `fut` resolves to the run's
    /// [`SubagentRunResult`] once the subagent finishes.
    pub fn spawn(
        self: &Arc<Self>,
        name: &str,
        task: &str,
        fut: impl Future<Output = SubagentRunResult> + Send + 'static,
    ) -> u32 {
        let id = self.reserve(name, task);
        self.attach(id, fut);
        id
    }

    /// Register `name`/`task` as running and return its id *without* starting
    /// anything. Pair with [`SubagentTaskRegistry::attach`].
    ///
    /// The split exists so the caller can announce the run (emit
    /// `AgentEvent::SubagentRunStarted`, which carries this id) *before* the
    /// subagent starts emitting into it — `attach` can begin executing the
    /// future on another worker thread the instant it is called.
    pub fn reserve(&self, name: &str, task: &str) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.lock().insert(
            id,
            Entry {
                name: name.to_string(),
                task: task.to_string(),
                result: None,
                reported: false,
                handle: None,
                inbox: Arc::new(Inbox::default()),
            },
        );
        id
    }

    /// The inbox of run `id`, if it is registered.
    pub fn inbox_of(&self, id: u32) -> Option<Arc<Inbox>> {
        self.lock().get(&id).map(|entry| Arc::clone(&entry.inbox))
    }

    /// Detach `fut` as the driver for a run already reserved under `id`.
    pub fn attach(
        self: &Arc<Self>,
        id: u32,
        fut: impl Future<Output = SubagentRunResult> + Send + 'static,
    ) {
        let registry = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let result = fut.await;
            registry.finish(id, result);
        });
        if let Some(entry) = self.lock().get_mut(&id) {
            // A kill can only race this while the entry is unresolved; if it
            // already resolved (kill_all between reserve and here), abort now.
            if entry.result.is_some() {
                handle.abort();
            } else {
                entry.handle = Some(handle);
            }
        }
    }

    fn finish(&self, id: u32, result: SubagentRunResult) {
        if let Some(entry) = self.lock().get_mut(&id) {
            if entry.result.is_none() {
                entry.result = Some(result);
            }
            entry.handle = None;
        }
    }

    /// Count of runs still in flight (registered, no result yet). Read-only
    /// peek; does not consume anything `drain_completed` would return.
    pub fn pending_count(&self) -> usize {
        self.lock().values().filter(|e| e.result.is_none()).count()
    }

    /// Snapshot of every registered run (running and finished), ordered by
    /// id.
    pub fn list(&self) -> Vec<SubagentTaskSnapshot> {
        let mut runs: Vec<SubagentTaskSnapshot> = self
            .lock()
            .iter()
            .map(|(id, entry)| entry.snapshot(*id))
            .collect();
        runs.sort_unstable_by_key(|run| run.id);
        runs
    }

    /// Snapshot of one run, if it exists.
    pub fn get(&self, id: u32) -> Option<SubagentTaskSnapshot> {
        self.lock().get(&id).map(|entry| entry.snapshot(id))
    }

    /// Abort a running subagent. Returns false when the id is unknown or the
    /// run already finished. A killed run is marked reported — the killer
    /// already knows — so it is never drained as a notification.
    pub fn kill(&self, id: u32) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&id) else {
            return false;
        };
        if entry.result.is_some() {
            return false;
        }
        if let Some(handle) = entry.handle.take() {
            handle.abort();
        }
        entry.result = Some(SubagentRunResult::killed());
        entry.reported = true;
        true
    }

    /// Abort every running subagent and mark everything reported, so nothing
    /// leaks a stale notification into a fresh conversation (`/clear`).
    pub fn kill_all(&self) {
        let mut entries = self.lock();
        for entry in entries.values_mut() {
            if let Some(handle) = entry.handle.take() {
                handle.abort();
            }
            if entry.result.is_none() {
                entry.result = Some(SubagentRunResult::killed());
            }
            entry.reported = true;
        }
    }

    /// Finished subagent runs not yet reported, each returned exactly once,
    /// ordered by id. The outcome stays queryable via [`Self::get`] after.
    pub fn drain_completed(&self) -> Vec<SubagentTaskResult> {
        let mut finished = Vec::new();
        for (id, entry) in self.lock().iter_mut() {
            if entry.reported {
                continue;
            }
            if let Some(result) = entry.result.clone() {
                entry.reported = true;
                finished.push(SubagentTaskResult {
                    id: *id,
                    name: entry.name.clone(),
                    task: entry.task.clone(),
                    completed: result.completed,
                    output: result.output,
                    steps_used: result.steps_used,
                    error: result.error,
                });
            }
        }
        finished.sort_unstable_by_key(|task| task.id);
        finished
    }
}

/// Arguments for [`SubagentStatusTool`].
#[derive(Debug, Deserialize)]
struct SubagentStatusArgs {
    /// Omit to list every background subagent.
    #[serde(default)]
    id: Option<u32>,
    /// Return the report even when its completion note already delivered it.
    #[serde(default)]
    full: bool,
}

/// `subagent_status` — state (and report, once finished) of background
/// subagents, mirroring `task_output` for background shell tasks.
pub struct SubagentStatusTool;

#[async_trait]
impl Tool for SubagentStatusTool {
    fn name(&self) -> &str {
        "subagent_status"
    }

    fn description(&self) -> &str {
        "Check background subagents started with spawn_subagent background: true. \
         Without an id, lists them all with their state; with an id, returns that \
         run's state and (once finished) its report."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Background subagent id; omit to list all" },
                "full": { "type": "boolean", "description": "Repeat a report the completion note already delivered" }
            }
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::ReadOnly
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SubagentStatusArgs = parse_args(self.name(), args)?;
        if let Some(id) = args.id {
            let Some(run) = ctx.subagents.get(id) else {
                return Ok(ToolOutput::error(format!("no background subagent #{id}")));
            };
            let mut content = format!(
                "Background subagent #{} '{}' [{}]: {}",
                run.id,
                run.name,
                run.describe(),
                run.task
            );
            let delivered = run.reported && crate::token_profile::current().safe() && !args.full;
            if delivered && run.result.is_some() {
                content.push_str(
                    "\n(report already delivered in its completion note; pass full: true to repeat it)",
                );
            } else if let Some(result) = &run.result
                && !result.output.trim().is_empty()
            {
                let mut output = result.output.trim_end().to_string();
                if output.chars().count() > STATUS_OUTPUT_CHARS {
                    output = output.chars().take(STATUS_OUTPUT_CHARS).collect::<String>() + " …";
                }
                content.push('\n');
                content.push_str(&output);
            }
            return Ok(ToolOutput::ok(content));
        }
        let runs = ctx.subagents.list();
        if runs.is_empty() {
            return Ok(ToolOutput::ok("no background subagents"));
        }
        let lines: Vec<String> = runs
            .iter()
            .map(|run| {
                let task = run.task.lines().next().unwrap_or_default();
                format!("#{} '{}' [{}]: {}", run.id, run.name, run.describe(), task)
            })
            .collect();
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

/// Arguments for [`SubagentKillTool`].
#[derive(Debug, Deserialize)]
struct SubagentKillArgs {
    id: u32,
}

/// `subagent_kill` — abort a running background subagent.
pub struct SubagentKillTool;

#[async_trait]
impl Tool for SubagentKillTool {
    fn name(&self) -> &str {
        "subagent_kill"
    }

    fn description(&self) -> &str {
        "Abort a running background subagent started with spawn_subagent background: true."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Background subagent id" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SubagentKillArgs = parse_args(self.name(), args)?;
        if ctx.subagents.kill(args.id) {
            return Ok(ToolOutput::ok(format!(
                "background subagent #{} killed",
                args.id
            )));
        }
        Ok(match ctx.subagents.get(args.id) {
            None => ToolOutput::error(format!("no background subagent #{}", args.id)),
            Some(run) => ToolOutput::error(format!(
                "background subagent #{} already finished ({})",
                args.id,
                run.describe()
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn done(output: &str, steps: u32) -> SubagentRunResult {
        SubagentRunResult {
            completed: true,
            output: output.to_string(),
            steps_used: steps,
            error: None,
        }
    }

    async fn wait_drained(registry: &Arc<SubagentTaskRegistry>) -> Vec<SubagentTaskResult> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let drained = registry.drain_completed();
            if !drained.is_empty() {
                return drained;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no subagent task finished in time"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn wait_pending_at_most(registry: &Arc<SubagentTaskRegistry>, n: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while registry.pending_count() > n {
            assert!(
                std::time::Instant::now() < deadline,
                "subagent tasks did not settle in time"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn spawn_returns_immediately_and_drains_exactly_once() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        let id = registry.spawn("worker", "investigate X", async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            done("found X", 3)
        });
        assert_eq!(id, 1);
        // Not finished yet — `spawn` must not block on the future.
        assert!(registry.drain_completed().is_empty());

        let drained = wait_drained(&registry).await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, 1);
        assert_eq!(drained[0].name, "worker");
        assert_eq!(drained[0].task, "investigate X");
        assert!(drained[0].completed);
        assert_eq!(drained[0].output, "found X");
        assert_eq!(drained[0].steps_used, 3);
        assert!(drained[0].error.is_none());

        assert!(
            registry.drain_completed().is_empty(),
            "finished subagent runs are reported exactly once"
        );
        // The outcome stays queryable after the drain.
        let snapshot = registry.get(1).expect("still listed");
        assert_eq!(snapshot.describe(), "completed");
    }

    #[tokio::test]
    async fn drain_skips_runs_still_in_flight() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        let quick = registry.spawn("worker", "quick", async { done("done", 1) });
        let _slow = registry.spawn("worker", "slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            done("done", 1)
        });

        let drained = wait_drained(&registry).await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, quick);
    }

    #[tokio::test]
    async fn errors_carry_through_drain() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        registry.spawn("worker", "doomed", async {
            SubagentRunResult {
                completed: false,
                output: "subagent failed: connection refused".to_string(),
                steps_used: 0,
                error: Some("connection refused".to_string()),
            }
        });
        let drained = wait_drained(&registry).await;
        assert_eq!(drained[0].error.as_deref(), Some("connection refused"));
        assert!(!drained[0].completed);
        assert_eq!(
            registry.get(drained[0].id).unwrap().describe(),
            "failed: connection refused"
        );
    }

    #[tokio::test]
    async fn kill_aborts_a_running_subagent_without_a_notification() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        let id = registry.spawn("worker", "slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            done("never", 1)
        });
        assert!(registry.kill(id));
        assert_eq!(registry.pending_count(), 0);
        assert!(
            registry.drain_completed().is_empty(),
            "a killed run is not drained as a notification"
        );
        assert_eq!(
            registry.get(id).unwrap().describe(),
            "failed: killed on request"
        );
        assert!(!registry.kill(id), "kill of a finished run reports false");
        assert!(!registry.kill(999), "kill of an unknown id reports false");
    }

    #[tokio::test]
    async fn kill_all_aborts_everything_and_silences_drains() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        registry.spawn("worker", "finished", async { done("report", 1) });
        let running = registry.spawn("worker", "slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            done("never", 1)
        });
        // Let the quick one land its result first.
        wait_pending_at_most(&registry, 1).await;

        registry.kill_all();
        assert_eq!(registry.pending_count(), 0);
        assert!(registry.drain_completed().is_empty());
        assert_eq!(
            registry.get(running).unwrap().describe(),
            "failed: killed on request"
        );
    }

    #[tokio::test]
    async fn status_tool_lists_and_details_runs() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        let ctx = ToolContext {
            subagents: Arc::clone(&registry),
            ..ToolContext::new(std::env::temp_dir())
        };

        let out = SubagentStatusTool.execute(json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("no background subagents"));

        let id = registry.spawn("worker", "investigate X\nmore detail", async {
            done("the report", 2)
        });
        wait_pending_at_most(&registry, 0).await;

        let out = SubagentStatusTool.execute(json!({}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains("#1 'worker' [completed]: investigate X"),
            "{}",
            out.content
        );

        let out = SubagentStatusTool
            .execute(json!({ "id": id }), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("the report"), "{}", out.content);

        let out = SubagentStatusTool
            .execute(json!({ "id": 999 }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no background subagent #999"));
    }

    #[tokio::test]
    async fn kill_tool_kills_and_reports_state() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        let ctx = ToolContext {
            subagents: Arc::clone(&registry),
            ..ToolContext::new(std::env::temp_dir())
        };
        let id = registry.spawn("worker", "slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            done("never", 1)
        });

        let out = SubagentKillTool
            .execute(json!({ "id": id }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("killed"), "{}", out.content);

        let out = SubagentKillTool
            .execute(json!({ "id": id }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("already finished"), "{}", out.content);

        let out = SubagentKillTool
            .execute(json!({ "id": 7 }), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no background subagent #7"));
    }
}
