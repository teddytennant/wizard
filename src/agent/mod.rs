//! Agent loop: build messages → stream completion → parse tool calls →
//! execute tools → repeat until the model is done (or a configured `max_steps`
//! cap, the time limit, the circuit breaker, or an interrupt ends the turn).
//!
//! The loop is UI-agnostic: it emits [`AgentEvent`]s over a channel that the
//! Ratatui TUI (genie) or the headless runner (sovereign) consumes.

pub mod breaker;
pub mod context;
mod event;
pub mod mission;
pub mod prompts;
mod retry;
pub mod session;
pub mod subagent;
pub(crate) mod turn;
pub mod ultra;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::{Config, Mode, ProviderKind};
use crate::dispatch::Dispatcher;
use crate::hooks::HookEngine;
use crate::images::{ImageRef, ImageStore};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, FunctionCall, Image, Role, ToolCall};
use crate::mcp::{McpConfig, McpManager};
use crate::skills::Skill;
use crate::tools::{CommandDispatch, ToolContext, registry::ToolRegistry};

pub use context::{
    COMPACT_SUMMARY_HEADING, CONTEXT_PRESSURE_HEADING, CompactOutcome, ContextPressure,
    PressureLevel,
};
#[cfg(test)]
pub(crate) use context::{KEEP_RECENT, PRESSURE_ELEVATED_FRACTION};
pub use event::{
    AgentEvent, ConsoleGate, ConsoleHost, ConsoleInput, ConsoleWriter, InterviewGate, PlanGate,
};
use session::Session;

/// Everything a `/btw` side question needs, owned so it can run without
/// borrowing the live [`Agent`]. Surfaces snapshot this before parking the
/// agent in a turn task so a side question can still fire mid-turn.
#[derive(Clone)]
pub struct SideQuestionContext {
    pub client: Arc<dyn LlmProvider>,
    pub model: String,
    /// Conversation the answer is grounded in (system prompt at index 0).
    pub messages: Vec<ChatMessage>,
    /// Reasoning effort forwarded on the forked call, when set.
    pub reasoning_effort: Option<String>,
}

/// Everything a `/fork` side quest needs, owned so it can spawn without
/// borrowing the live [`Agent`]. Surfaces snapshot this before parking the
/// agent in a turn task so a fork can still fire mid-turn — same pattern as
/// [`SideQuestionContext`], but with tools, hooks, and the background-subagent
/// registry so the fork can work and report back.
#[derive(Clone)]
pub struct ForkContext {
    pub client: Arc<dyn LlmProvider>,
    pub model: String,
    /// Snapshot of the parent conversation (system prompt at index 0).
    pub messages: Vec<ChatMessage>,
    /// Parent tool set at snapshot time (shallow `Arc` clone of each tool).
    pub registry: ToolRegistry,
    /// Lifecycle hooks shared with the parent.
    pub hooks: Arc<HookEngine>,
    /// Parent tool context (cwd, tasks, subagents, usage, images, …). The fork
    /// registers on `subagents` so its report drains into the parent history.
    pub ctx: ToolContext,
    /// Restrict the fork to read-only tools (parent was in plan mode).
    pub read_only: bool,
}

/// System reminder prepended to a `/btw` user message. Mirrors Claude Code's
/// side-question constraints: one shot, no tools, answer from context only.
const SIDE_QUESTION_REMINDER: &str = "\
This is a side question from the user (\"/btw\"). Answer it directly in a \
single response.\n\
\n\
CRITICAL CONSTRAINTS:\n\
- You have NO tools — you cannot read files, run commands, search, or take \
any actions.\n\
- This is a one-off response — there will be no follow-up turns.\n\
- Answer only from what you already know in the conversation context and \
your own knowledge.\n\
- NEVER say things like \"Let me try…\", \"I'll now…\", \"Let me check…\", \
or promise to take any action.\n\
- If you don't know, say so — do not offer to look it up or investigate.\n\
\n\
Simply answer the question with the information you have.";

impl SideQuestionContext {
    /// Fork a single tool-less completion over a copy of `messages` plus the
    /// side question. The main conversation is never written.
    pub async fn ask(&self, question: &str) -> Result<String> {
        let question = question.trim();
        anyhow::ensure!(!question.is_empty(), "empty side question");

        let mut messages = self.messages.clone();
        messages.push(ChatMessage::user(format!(
            "{SIDE_QUESTION_REMINDER}\n\n{question}"
        )));

        let request = ChatRequest {
            model: self.model.clone(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                // Slightly cooler than a normal turn: side questions are
                // factual asides, not creative work.
                temperature: Some(0.3),
                num_ctx: None,
                reasoning_effort: self.reasoning_effort.clone(),
            }),
        };

        let mut stream = self
            .client
            .chat_stream(request)
            .await
            .context("starting /btw side question")?;
        let mut answer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading /btw stream")?;
            if let Some(message) = chunk.message
                && !chunk.thinking
            {
                answer.push_str(&message.text());
            }
            if chunk.done {
                break;
            }
        }
        let answer = answer.trim().to_string();
        if answer.is_empty() {
            anyhow::bail!("empty /btw reply");
        }
        Ok(answer)
    }
}

impl ForkContext {
    /// Detach a `/fork` side quest against this snapshot. Registers on the
    /// parent's background-subagent registry and streams progress through
    /// `events` (when provided) so the surface can open a pane. Returns the
    /// background-registry id immediately; the report lands in the parent
    /// history the next time background subagents are drained.
    ///
    /// `events` should be a channel the surface is already listening on
    /// (turn-forwarded or a dedicated idle collector). When `None`, the fork
    /// still runs and still reports via the registry — it just has no pane.
    pub async fn spawn(self, task: &str, events: Option<mpsc::Sender<AgentEvent>>) -> Result<u32> {
        let task = task.trim();
        anyhow::ensure!(!task.is_empty(), "empty fork task");

        let run = subagent::next_run_id();
        let options = subagent::SpawnOptions {
            model: Some(self.model.clone()),
            read_only: self.read_only,
            inherited_history: None, // spawn_fork sets this itself
            // A fork is detached by definition: it runs alongside the main
            // conversation and reports back whenever it is done, so the turn
            // the user pressed Esc on is not the turn the fork belongs to.
            // `/kill` through the subagent registry is how a fork is ended.
            cancel: None,
            ..Default::default()
        };

        let name = subagent::FORK_NAME.to_string();
        let task_owned = task.to_string();
        let client = Arc::clone(&self.client);
        let registry = self.registry;
        let hooks = Arc::clone(&self.hooks);
        let history = self.messages;
        // Carry the surface's event channel into the fork's tool context so
        // SubagentRun* progress streams to the same place the parent does.
        let mut fut_ctx = self.ctx.clone();
        fut_ctx.events = events.clone();
        let fut_options = options;
        let fut_task = task_owned.clone();
        let fut = async move {
            match subagent::spawn_fork(
                run,
                &fut_task,
                history,
                &fut_options,
                &client,
                &registry,
                &hooks,
                &fut_ctx,
            )
            .await
            {
                Ok(result) => crate::tools::subagent_tasks::SubagentRunResult {
                    completed: result.completed,
                    output: result.output,
                    steps_used: result.steps_used,
                    error: None,
                },
                Err(err) => crate::tools::subagent_tasks::SubagentRunResult {
                    completed: false,
                    output: format!("fork failed: {err:#}"),
                    steps_used: 0,
                    error: Some(format!("{err:#}")),
                },
            }
        };

        let id = self.ctx.subagents.reserve(&name, task);
        if let Some(events) = &events {
            emit(
                events,
                AgentEvent::SubagentRunStarted {
                    run,
                    bg: Some(id),
                    name: name.clone(),
                    task: task_owned.clone(),
                },
            )
            .await;
            emit(
                events,
                AgentEvent::SubagentStarted {
                    id,
                    name: name.clone(),
                    task: task_owned.clone(),
                },
            )
            .await;
        }
        self.ctx.subagents.attach(id, fut);
        Ok(id)
    }
}

/// Why an agent turn (or sovereign run) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneReason {
    /// Model finished without requesting more tools.
    Completed,
    /// A configured `max_steps` cap was exhausted. Never reached on the default
    /// unlimited budget.
    MaxSteps,
    /// `--max-hours` elapsed (sovereign).
    TimeLimit,
    /// Stopped via the loop-control file or user interrupt.
    Stopped,
    /// Circuit breaker: the LLM endpoint breaker tripped (provider down), or
    /// repeated identical failures (sovereign), or too many consecutive
    /// failures of one tool.
    CircuitBreaker,
}

/// Verdict on a plan presented via the `exit_plan` tool (plan mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanVerdict {
    /// True when the plan was approved and execution may proceed.
    pub approved: bool,
    /// Reviewer feedback on rejection (empty = a generic rejection).
    pub feedback: String,
}

impl PlanVerdict {
    /// Approve the plan: plan mode ends and the model executes it.
    pub fn approve() -> Self {
        Self {
            approved: true,
            feedback: String::new(),
        }
    }

    /// Reject the plan with `feedback`; plan mode stays on.
    pub fn reject(feedback: impl Into<String>) -> Self {
        Self {
            approved: false,
            feedback: feedback.into(),
        }
    }
}

/// One clarifying question asked via the `interview` tool (plan mode). The
/// surface collects an answer for each; an empty option list means a
/// free-text answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterviewQuestion {
    /// The question text shown to the user.
    pub question: String,
    /// Suggested answers the user can pick from; empty for free-text only.
    /// The user may always type their own answer instead of picking.
    pub options: Vec<String>,
}

/// Where an image came from, on an [`AgentEvent::Images`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSource {
    /// The model produced it inline in its reply
    /// ([`ChatChunk::images`](crate::llm::ChatChunk::images)).
    Assistant,
    /// A tool returned it ([`ToolOutput::images`](crate::tools::ToolOutput::images));
    /// the name of the tool.
    Tool(String),
}

impl ImageSource {
    /// Stable tag for the structured surfaces (`stream-json`, the GUI's
    /// protocol frames).
    pub fn as_str(&self) -> &'static str {
        match self {
            ImageSource::Assistant => "assistant",
            ImageSource::Tool(_) => "tool",
        }
    }

    /// The tool that produced the image, if a tool did.
    pub fn tool(&self) -> Option<&str> {
        match self {
            ImageSource::Assistant => None,
            ImageSource::Tool(name) => Some(name),
        }
    }
}

/// Sovereign-mode run control, read from `.wizard/loop-control` in the
/// project between steps (see `docs/modes.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopControl {
    /// Graceful shutdown after the current step.
    Stop,
    /// Wait until the file is removed or set to `resume`.
    Pause,
    /// Skip the current sub-task.
    Skip,
}

/// Read and parse `.wizard/loop-control` under `project_root`.
/// `None` when the file is absent, unreadable, or holds `resume`/unknown
/// content.
pub fn read_loop_control(project_root: &Path) -> Option<LoopControl> {
    let path = loop_control_path(project_root);
    let raw = std::fs::read_to_string(path).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "stop" => Some(LoopControl::Stop),
        "pause" => Some(LoopControl::Pause),
        "skip" => Some(LoopControl::Skip),
        _ => None,
    }
}

fn loop_control_path(project_root: &Path) -> PathBuf {
    project_root.join(".wizard").join("loop-control")
}

/// Remove the loop-control file after consuming a one-shot command
/// (`stop`/`skip`), so it does not re-trigger on the next run.
pub(crate) fn clear_loop_control(project_root: &Path) {
    let path = loop_control_path(project_root);
    if let Err(err) = std::fs::remove_file(&path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("could not remove {}: {err}", path.display());
    }
}

/// Parse a prompt-protocol tool call (`{"tool": ..., "arguments": {...}}`)
/// out of assistant text, for models without native tool calling. Lenient:
/// accepts the whole message, a fenced ```json block, or any line that is a
/// JSON object with a `tool` field.
pub(crate) fn parse_json_tool_call(text: &str) -> Option<ToolCall> {
    #[derive(serde::Deserialize)]
    struct ProtocolCall {
        tool: String,
        #[serde(default)]
        arguments: Value,
    }

    fn try_parse(candidate: &str) -> Option<ToolCall> {
        let call: ProtocolCall = serde_json::from_str(candidate).ok()?;
        let arguments = if call.arguments.is_null() {
            json!({})
        } else {
            call.arguments
        };
        // A JSON-in-text call has no provider id: it never went through a
        // native tool-calling API. One is minted here so the result that
        // answers it can be correlated the same way every other call's is.
        Some(ToolCall {
            id: crate::llm::synthetic_tool_call_id(),
            function: FunctionCall {
                name: call.tool,
                arguments,
            },
        })
    }

    let trimmed = text.trim();
    if let Some(call) = try_parse(trimmed) {
        return Some(call);
    }
    // Fenced ```json block.
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```")
            && let Some(call) = try_parse(after[..end].trim())
        {
            return Some(call);
        }
    }
    // Any single line that is a JSON object.
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('{')
            && let Some(call) = try_parse(line)
        {
            return Some(call);
        }
    }
    None
}

/// Normalize model-provided tool arguments to a JSON object: `null` becomes
/// `{}`, and stringified JSON (some models double-encode) is parsed.
pub(crate) fn normalize_args(args: &Value) -> Value {
    match args {
        Value::Null => json!({}),
        Value::String(raw) => serde_json::from_str(raw).unwrap_or_else(|_| args.clone()),
        other => other.clone(),
    }
}

/// Send an event, reporting whether the receiver is still listening.
pub(crate) async fn emit(events: &mpsc::Sender<AgentEvent>, event: AgentEvent) -> bool {
    events.send(event).await.is_ok()
}

/// Take custody of images produced during a turn — the one seam every image
/// passes through, from either direction: a tool's
/// [`ToolOutput::images`](crate::tools::ToolOutput::images) or the model's own
/// [`ChatChunk::images`](crate::llm::ChatChunk::images).
///
/// Images over [`crate::llm::MAX_IMAGE_BYTES`] are dropped here with a notice:
/// an absurd image must not reach history, where it would melt the context
/// window and bloat the session file. The rest are written to the session's
/// image store and announced to the surfaces as `announce(refs)` — an event
/// carrying paths, never base64. Persistence is best-effort (see
/// [`ImageStore::save_all`]); the model's copy is the base64 this returns, for
/// the caller to attach to the message it is about to push.
pub(crate) async fn absorb_images(
    images: Vec<Image>,
    store: Option<&Arc<ImageStore>>,
    events: Option<&mpsc::Sender<AgentEvent>>,
    announce: impl FnOnce(Vec<ImageRef>) -> AgentEvent,
) -> Vec<Image> {
    if images.is_empty() {
        return images;
    }
    let (kept, dropped) = crate::images::split_oversized(images);
    if !dropped.is_empty() {
        let notice = crate::images::oversized_notice(&dropped);
        tracing::warn!("{notice}");
        if let Some(events) = events {
            emit(events, AgentEvent::Notice(notice)).await;
        }
    }
    let Some(store) = store else {
        // No store (a registry driven directly, outside an agent): the images
        // still reach the model, they just land nowhere for the surfaces.
        return kept;
    };
    // Each surviving image comes back tagged with its path, so the session file
    // records where it went and a replayed transcript needs no re-derivation.
    let (kept, saved) = store.save_all(kept);
    if !saved.is_empty()
        && let Some(events) = events
    {
        emit(events, announce(saved)).await;
    }
    kept
}

/// Whether an LLM error is worth retrying after backoff. Typed provider
/// errors classify themselves; unknown errors (mid-stream drops surface as
/// plain `anyhow` context chains) stay transient for robustness.
pub(crate) fn error_is_transient(err: &anyhow::Error) -> bool {
    if let Some(provider) = err.downcast_ref::<crate::llm::ProviderError>() {
        return provider.is_transient();
    }
    if let Some(ollama) = err.downcast_ref::<crate::llm::ollama::OllamaError>() {
        return ollama.is_transient();
    }
    true
}

/// Cooperative cancellation handle for a running turn. Cloneable and
/// thread-safe: the surface keeps a clone (see [`Agent::cancel_handle`]) and
/// calls [`CancelHandle::cancel`] (e.g. on Esc); the run loop observes it in
/// the stream loop and between tool calls, synthesizes results for the tool
/// calls it skips, and ends the turn with [`DoneReason::Stopped`] — without
/// the agent (or its background tasks) being torn down. The flag auto-resets
/// at the start of the next turn.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<CancelState>);

impl std::fmt::Debug for CancelHandle {
    /// A handle is a flag and a notify list; neither is worth printing, and
    /// the flag would be stale by the time anyone read it. What a `{:?}` of a
    /// struct holding one needs is that the field is there at all.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CancelHandle")
    }
}

#[derive(Default)]
struct CancelState {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelHandle {
    /// Request cancellation of the turn currently running (if any).
    pub fn cancel(&self) {
        self.0.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Whether cancellation has been requested for the current turn.
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves once cancellation is requested (immediately if it already
    /// was).
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register interest before checking the flag so a concurrent
            // `cancel` cannot slip between the check and the await.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Arm for a new turn (a stale request must not cancel it).
    fn clear(&self) {
        self.0
            .flag
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Resolves when the turn is cancelled, and never when there is no handle.
///
/// A future that never resolves rather than a second copy of every `select!`
/// for each combination of "has a handle" and "has whatever else": an arm that
/// is always pending is exactly what "this cannot be cancelled" means, and it
/// costs one poll.
pub(crate) async fn cancelled(handle: Option<&CancelHandle>) {
    match handle {
        Some(handle) => handle.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// A background completion surfaced to the model, returned by
/// [`Agent::drain_finished_notifications`] so surfaces can render it.
#[derive(Debug)]
pub enum FinishedNotification {
    /// A background shell task (`execute` with `run_in_background`).
    Task(crate::tools::tasks::FinishedTask),
    /// A backgrounded subagent (`spawn_subagent` with `background: true`).
    Subagent(crate::tools::subagent_tasks::SubagentTaskResult),
}

/// History/system note announcing a finished background task.
fn task_note(task: &crate::tools::tasks::FinishedTask) -> String {
    let mut note = format!(
        "[background task #{} finished ({})] {}",
        task.id,
        task.status.describe(),
        task.command
    );
    let tail = task.tail.trim();
    if !tail.is_empty() {
        note.push('\n');
        note.push_str(tail);
    }
    note
}

/// History/system note announcing a finished background subagent.
fn subagent_note(task: &crate::tools::subagent_tasks::SubagentTaskResult) -> String {
    let status = match &task.error {
        Some(error) => format!("failed: {error}"),
        None if task.completed => "completed".to_string(),
        None => "hit its step budget".to_string(),
    };
    // `/fork` side quests share the background-subagent drain path; label them
    // so the main model (and the user reading the transcript) can tell a
    // user-spawned fork from a `spawn_subagent` delegation at a glance.
    let kind = if task.name == subagent::FORK_NAME {
        "fork"
    } else {
        "background subagent"
    };
    format!(
        "[{kind} #{} '{}' {} after {} step(s)] {}\n\n{}",
        task.id, task.name, status, task.steps_used, task.task, task.output
    )
}

/// The tool-calling agent. Owns the conversation history, the model client,
/// the tool dispatcher, and session persistence.
pub struct Agent {
    client: Arc<dyn LlmProvider>,
    /// Circuit breaker over the model endpoint (see [`breaker`]): bounds the
    /// streaming retry loop when a provider is down instead of retrying it
    /// forever, and recovers on its own. Reset on a provider switch.
    llm_breaker: breaker::LlmBreaker,
    /// Active model tag (from `config.active().model`); switched by
    /// [`Agent::set_model`].
    model: String,
    /// Tool-call pipeline; owns the registry and the failure breakers.
    dispatcher: Dispatcher,
    /// Lifecycle hooks; the dispatcher and the subagent spawner share it.
    hooks: Arc<HookEngine>,
    config: Config,
    mode: Mode,
    /// Full conversation including the system prompt at index 0.
    history: Vec<ChatMessage>,
    session: Session,
    ctx: ToolContext,
    /// Whether the model supports native tool calling; when false the loop
    /// uses the prompt-based JSON tool protocol.
    native_tools: bool,
    /// Skills baked into the system prompt (kept for `/mode` rebuilds).
    skills: Vec<Skill>,
    /// Assembled instruction hierarchy (`WIZARD.md` / `AGENTS.md` /
    /// `CLAUDE.md` from the project root up, plus `~/.wizard/WIZARD.md` —
    /// see [`crate::instructions`]), if any file exists.
    agents_md: Option<String>,
    /// Persistent memory index (MEMORY.md) for this project, if any
    /// memories are saved. Re-read on every system prompt refresh so
    /// `/reload` picks up changes.
    memory_index: Option<String>,
    /// Wall-clock deadline for sovereign runs (`--max-hours`).
    deadline: Option<Instant>,
    /// Warning from session resume (corrupt/unreadable file), emitted on
    /// the next turn so the UI can surface it.
    load_warning: Option<String>,
    /// Plan-mode flag, shared with the dispatcher (read-only gate) and the
    /// `exit_plan` tool (cleared on approval).
    plan_mode: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the plan-mode instruction block is currently baked into the
    /// system prompt; [`Agent::sync_plan_prompt`] refreshes on mismatch.
    plan_prompt_on: bool,
    /// Omakase (chef's-choice) flag, shared with the `exit_plan` and
    /// `interview` tools. While set, `exit_plan` auto-approves the plan and
    /// `interview` declines to ask — the agent decides and proceeds.
    /// Implies plan mode (the read-only exploration phase).
    omakase: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the omakase instruction block is currently baked into the
    /// system prompt; refreshed on mismatch alongside the plan block.
    omakase_prompt_on: bool,
    /// Token counters fed from `ChatChunk` eval counts during streaming.
    /// Shared into the tool context (`ToolContext::usage`) so a subagent's
    /// model calls — `spawn_subagent`, and every `/ultra` candidate and judge —
    /// bill this agent instead of vanishing from the totals.
    usage: Arc<crate::usage::UsageTracker>,
    /// Where per-turn usage records are appended
    /// (`~/.wizard/usage.jsonl`); `None` disables the log.
    usage_log: Option<PathBuf>,
    /// Per-file checkpoint store (`.wizard/checkpoints/` in the project).
    /// Shared with the tool context so the dispatcher and subagents snapshot
    /// `Edit`-class targets into it; `/rewind` and perpetual rollback
    /// restore from it.
    checkpoints: Arc<crate::checkpoint::CheckpointStore>,
    /// Cooperative cancellation of the running turn (see
    /// [`Agent::cancel_handle`]). Cleared at the start of every turn.
    cancel: CancelHandle,
    /// The spawn tool's shared model slot, when bound
    /// ([`Agent::bind_subagent_model`]): `/model` switches write through so
    /// subagents run on the parent's active model.
    subagent_model: Option<subagent::SharedActiveModel>,
    /// The `/ultra` engine while mixture-of-agents mode is on: each turn first
    /// fans candidate subagents out on *this* client and model, has judges
    /// compare their drafts, and injects the verdict — then runs normally.
    /// `None` (the default, and what every non-TUI surface gets) is an ordinary
    /// turn.
    ///
    /// Session state, not config: a rebuilt agent (`/model`, a provider switch,
    /// `/resume`) starts without it, so every rebuild path must re-arm it.
    ultra: Option<Arc<ultra::UltraEngine>>,
}

/// One row of the `/rewind` picker: a turn, the prompt that started it, and
/// the files its tool calls snapshotted.
#[derive(Debug, Clone)]
pub struct RewindCandidate {
    pub turn: u64,
    /// First line of the turn's user prompt (empty when unknown).
    pub prompt: String,
    /// Files the turn snapshotted before editing.
    pub files: Vec<PathBuf>,
}

/// Prefix of the system note carrying `session_start` hook output.
///
/// The note is context for the model, not conversation: surfaces that replay a
/// session from disk (the GUI transcript) match on this to drop it, the way the
/// TUI drops every system message when it reloads a transcript. Hook *events*
/// are still reported, as one-line [`AgentEvent::HookFired`] notices.
pub const SESSION_START_HOOK_NOTE: &str = "[session_start hook]";

/// User-role nudge injected (in memory only) when a completion comes back
/// with no visible text and no tool calls.
const EMPTY_COMPLETION_NUDGE: &str = "(continue: reply to the user with your findings)";

/// User-role nudge injected (in memory only) after the provider cut a reply off
/// at its output-token ceiling while the model was still writing a tool call.
///
/// The `{tool}` placeholder is filled with the name of the call that was cut
/// off, because that is the only part of it that survived decoding and it is
/// usually enough for the model to recognize what it was in the middle of.
///
/// Naming a concrete way to be smaller matters more than it looks. Told only
/// "that was too long", a model tends to re-emit the same call with the same
/// arguments and get cut off at the same byte; told to split the write or
/// narrow the hunk, it produces something that fits.
const TRUNCATED_TOOL_CALL_NUDGE: &str = "\
(your previous reply hit the output-token limit while you were still writing the arguments for \
`{tool}`, so it was discarded and never ran — nothing you asked for has happened)\n\
Send that step again, smaller: one tool call, with the least argument text that still does the \
job. Write a long file in successive appends rather than one call; edit a narrow hunk rather than \
a whole file; run a short command rather than a long script. Do not repeat the explanation you \
already gave.";

/// User-role nudge injected (in memory only) after a reply was cut off because
/// the *conversation* no longer fit the context window, rather than because the
/// reply hit its own output ceiling.
///
/// Deliberately says nothing about writing smaller tool calls, which is the
/// advice the other cutoff wants and is actively misleading here: the reply's
/// length was never the problem, and a model that acts on it spends the next
/// request being wrong in a smaller way. By the time this is sent the history
/// has already been compacted, so what the model needs is to know its last step
/// did not happen and that the transcript behind it has changed shape.
const CONTEXT_OVERFLOW_NUDGE: &str = "\
(your previous reply was cut off because the conversation had outgrown the model's context \
window, so it was discarded and the tool call in it never ran — nothing you asked for has \
happened)\n\
The history above has just been compacted, so older detail is now a summary. Re-read what is \
there, then carry on from where you were.";

/// True when a completed assistant message has neither visible content nor
/// tool calls (e.g. a reasoning model that thought and then just stopped).
pub(crate) fn completion_is_empty(content: &str, tool_calls: &[ToolCall]) -> bool {
    content.trim().is_empty() && tool_calls.is_empty()
}

impl Agent {
    /// Build an agent: compose the system prompt from `mode`, `skills`, and
    /// any project `AGENTS.md`; seed history from `session` (resumed
    /// sessions replay their persisted messages under a fresh system
    /// prompt). `hooks` is loaded by the builders (`crate::hooks::load`) and
    /// injected so tests can supply their own definitions.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<dyn LlmProvider>,
        registry: ToolRegistry,
        config: Config,
        skills: Vec<Skill>,
        project_root: PathBuf,
        session: Session,
        native_tools: bool,
        hooks: Arc<HookEngine>,
    ) -> Result<Self> {
        let agents_md = crate::instructions::load(&project_root);
        let memory_index = read_memory_index(&project_root);
        let model = config.active().model;
        let mut load_warning = None;
        // load_history replays persisted system notes, drops stale system
        // prompts, and repairs dangling tool calls from interrupted runs.
        let prior = session.load_history().unwrap_or_else(|err| {
            tracing::warn!("could not load session {}: {err}", session.path().display());
            load_warning = Some(format!(
                "previous session {} could not be read ({err}); starting fresh",
                session.path().display()
            ));
            Vec::new()
        });

        // Plan mode: one flag shared by the dispatcher (read-only gate) and
        // the always-registered exit_plan tool (cleared on approval).
        let plan_mode = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Omakase: chef's-choice flavor of plan mode, shared with exit_plan
        // (auto-approve) and interview (decline to ask).
        let omakase = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let web = config.web.clone();

        // Checkpoints: per-file snapshots of everything this agent edits.
        // Old turns are garbage-collected once per session, here.
        let checkpoints = Arc::new(crate::checkpoint::CheckpointStore::open(
            &project_root,
            config.checkpoints.keep_turns,
        ));
        match checkpoints.gc() {
            Ok(dropped) if dropped > 0 => {
                tracing::debug!("checkpoint gc dropped {dropped} old turn(s)");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("checkpoint gc failed: {err:#}"),
        }

        let mut registry = registry;
        registry.register(Arc::new(crate::tools::plan::ExitPlanTool::new(
            Arc::clone(&plan_mode),
            Arc::clone(&omakase),
        )));
        registry.register(Arc::new(crate::tools::interview::InterviewTool::new(
            Arc::clone(&omakase),
        )));

        // The agent's token counters, shared into the tool context so a
        // subagent's model calls bill the parent (see `ToolContext::usage`).
        let usage = Arc::new(crate::usage::UsageTracker::new());

        // Images produced this session (by a tool or by the model) land under
        // `~/.wizard/images/<session>/`, so every surface has a real file to
        // render or link to.
        // One cancel handle, shared between the run loop and the tool context.
        // The loop checks it between calls; a tool that can park on a human
        // (`execute` with a claimed console) checks it while it is parked, so
        // Ctrl-C reaches the child's process group instead of waiting for the
        // turn task to be aborted around it.
        let cancel = CancelHandle::default();
        let mut ctx = ToolContext::new(project_root)
            .with_web(web)
            .with_checkpoints(Arc::clone(&checkpoints))
            .with_usage(Arc::clone(&usage))
            .with_cancel(cancel.clone());
        if let Some(images) = open_image_store(&session.id) {
            ctx = ctx.with_images(images);
        }

        let mut agent = Self {
            client,
            llm_breaker: breaker::LlmBreaker::new(),
            model,
            dispatcher: Dispatcher::new(
                registry,
                config.mode,
                Arc::clone(&hooks),
                Arc::clone(&plan_mode),
            ),
            hooks,
            mode: config.mode,
            config,
            history: Vec::new(),
            session,
            ctx,
            native_tools,
            skills,
            agents_md,
            memory_index,
            deadline: None,
            load_warning,
            plan_mode,
            plan_prompt_on: false,
            omakase,
            omakase_prompt_on: false,
            usage,
            usage_log: crate::usage::default_log_path(),
            checkpoints,
            cancel,
            subagent_model: None,
            ultra: None,
        };
        agent
            .history
            .push(ChatMessage::system(agent.compose_system_prompt()));
        agent.history.extend(prior);
        Ok(agent)
    }

    /// Current personality mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Switch mode mid-session (`/mode`): swaps the system prompt and
    /// circuit-breaker behavior for subsequent turns.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.config.mode = mode;
        self.dispatcher.set_mode(mode);
        self.refresh_system_prompt();
    }

    /// Set the reasoning effort (`/effort`) forwarded on subsequent turns.
    /// `None` leaves the provider default. Only reaches models that accept a
    /// `reasoning_effort` request field; others ignore it.
    pub fn set_reasoning_effort(&mut self, effort: Option<crate::config::ReasoningEffort>) {
        self.config.reasoning_effort = effort;
    }

    /// Declare which slash commands the surface behind this agent will run when
    /// the agent queues one via `run_command` (see [`CommandDispatch`]). Only a
    /// surface that drains the queue makes the tool useful; headless and gateway
    /// runs leave it at `None` so the tool refuses rather than report success for
    /// a command nothing applies. Preserved across per-turn context clones
    /// (`ToolContext::with_events`).
    pub fn set_command_dispatch(&mut self, dispatch: CommandDispatch) {
        self.ctx.command_dispatch = dispatch;
    }

    /// Declare whether a human is watching this agent and can answer a shell
    /// command that prompts on stdin (see
    /// [`ConsoleAccess`](crate::tools::ConsoleAccess)).
    ///
    /// Only the interactive TUI says yes. Everything else leaves it at
    /// `None`, which keeps `/dev/null` on every child's fd 0 — the behaviour
    /// every surface had before consoles existed, and the only honest one when
    /// there is nobody to type an answer. Preserved across per-turn context
    /// clones (`ToolContext::with_events`).
    pub fn set_console_access(&mut self, console: crate::tools::ConsoleAccess) {
        self.ctx.console = console;
    }

    /// Conversation history (system prompt included).
    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    /// Tokens that will load into the next model call: the backend's last
    /// reported prompt size when known, otherwise a char/4 estimate of the
    /// current history (used right after `/clear` or compaction, when the
    /// real count is stale). This is the number the TUI status bar shows —
    /// *not* the session-lifetime sum of every past prompt.
    pub fn context_tokens(&self) -> u64 {
        match self.usage.last_prompt_tokens() {
            Some(n) => n,
            None => crate::llm::estimate_history_tokens(&self.history),
        }
    }

    /// Live fill of the next model call against the provider window, measured
    /// by [`context::pressure`] — the same reading a subagent takes of its own
    /// history, from the same numbers.
    pub async fn context_pressure(&self) -> ContextPressure {
        context::pressure(context::Measured {
            tokens: self.context_tokens(),
            window: self.client.context_window(&self.model).await,
            bytes: self.history.iter().map(|msg| msg.text().len()).sum(),
            byte_threshold: self.config.compact_threshold_bytes,
            last_prompt: self.usage.last_prompt_tokens(),
        })
    }

    /// Session this agent persists to.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The per-file checkpoint store (snapshots powering `/rewind` and
    /// perpetual rollback).
    pub fn checkpoints(&self) -> &Arc<crate::checkpoint::CheckpointStore> {
        &self.checkpoints
    }

    /// Recent turns `/rewind` can return to, newest first: this session's
    /// turn markers (prompt snippets) joined with the checkpoint store's
    /// per-turn file lists. Turns from before this session are listed only
    /// when the session has no markers at all (old-format resume), since
    /// only marked turns can also truncate the conversation.
    pub fn rewind_candidates(&self, limit: usize) -> Vec<RewindCandidate> {
        let markers = self.session.turn_markers().unwrap_or_default();
        let first_marked = markers.first().map(|marker| marker.turn);
        let mut by_turn: std::collections::BTreeMap<u64, RewindCandidate> = markers
            .into_iter()
            .map(|marker| {
                (
                    marker.turn,
                    RewindCandidate {
                        turn: marker.turn,
                        prompt: marker.prompt,
                        files: Vec::new(),
                    },
                )
            })
            .collect();
        for turn_files in self.checkpoints.recent_turns(usize::MAX) {
            if first_marked.is_some_and(|first| turn_files.turn < first) {
                continue;
            }
            by_turn
                .entry(turn_files.turn)
                .or_insert_with(|| RewindCandidate {
                    turn: turn_files.turn,
                    prompt: String::new(),
                    files: Vec::new(),
                })
                .files = turn_files.files;
        }
        by_turn.into_values().rev().take(limit).collect()
    }

    /// `/rewind`: restore every file snapshot from `turn` onward, drop the
    /// rewound turns from the session file, and reload the in-memory
    /// conversation to match. Returns the restored file paths.
    pub fn rewind_to(&mut self, turn: u64) -> Result<Vec<PathBuf>> {
        let restored = self
            .checkpoints
            .restore_turns_from(turn)
            .context("restoring checkpoints")?;
        self.session
            .truncate_after(turn)
            .context("truncating session history")?;
        let prior: Vec<ChatMessage> = self
            .session
            .load_history()
            .context("reloading session history")?;
        self.history.truncate(1);
        self.history.extend(prior);
        self.dispatcher.reset_failures();
        Ok(restored)
    }

    /// Set (or clear) the wall-clock deadline for this run (`--max-hours`).
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    /// `/clear`: drop everything but the system prompt and start a fresh
    /// session file. Background work from the old conversation is killed
    /// and detached (fresh registries; late monitors hold the old ones, so
    /// their notes can never reach the new conversation) and the todo list
    /// is reset. Session token counters go to zero with the history so the
    /// TUI context meter and `/cost` do not keep the wiped conversation.
    pub fn clear(&mut self) -> Result<()> {
        self.ctx.tasks.kill_all();
        self.ctx.subagents.kill_all();
        self.ctx.tasks = Arc::new(crate::tools::tasks::TaskRegistry::new());
        self.ctx.subagents = Arc::new(crate::tools::subagent_tasks::SubagentTaskRegistry::new());
        self.ctx.todos = Arc::new(std::sync::Mutex::new(Vec::new()));
        self.session = Session::create(&Config::sessions_dir()?)?;
        // Images follow the session: the fresh conversation writes into its own
        // directory, and the old one's files stay where its transcript points.
        self.ctx.images = open_image_store(&self.session.id);
        self.hooks.set_session_id(self.session.id.clone());
        self.history.truncate(1);
        self.dispatcher.reset_failures();
        self.usage.clear_session();
        Ok(())
    }

    /// Handle for cancelling the running turn cooperatively. The surface
    /// clones it before spawning `run_turn` and calls
    /// [`CancelHandle::cancel`] to interrupt: the turn stops at the next
    /// stream chunk or tool boundary, answers skipped tool calls with
    /// "(not executed — interrupted by user)", emits
    /// [`AgentEvent::Done`] with [`DoneReason::Stopped`], and returns —
    /// no task aborts, no agent rebuild, background work keeps running.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Shared handle on the background-shell-task registry, so a surface can
    /// read it while a turn holds the agent — same pattern as
    /// [`Self::subagent_registry`], and taken by `App::tasks` so `/bashes`
    /// answers mid-turn, which is the only time there is anything to list.
    ///
    /// Read, specifically. The registry can also kill (`TaskRegistry::kill`),
    /// and this accessor claimed a surface used it for that; none does. The
    /// only kill paths today are the model's own `kill_task` tool and
    /// `kill_all` on teardown, so a human cannot stop a backgrounded command
    /// from any surface. That gap is real and is not closed here.
    pub fn task_registry(&self) -> Arc<crate::tools::tasks::TaskRegistry> {
        Arc::clone(&self.ctx.tasks)
    }

    /// Bind the spawn tool's shared slot (see
    /// [`subagent::SpawnSubagentTool::model_handle`]) to this agent: subagents
    /// run on its active model, including after `/model` switches, and a
    /// foreground subagent ends when the user interrupts the turn it is
    /// running inside.
    ///
    /// The name is older than what it binds. It stayed because every surface's
    /// registry builder hands the slot back through it, and renaming the entry
    /// point would touch each of them for nothing.
    pub fn bind_subagent_model(&mut self, handle: subagent::SharedActiveModel) {
        handle.bind(
            self.model.clone(),
            self.cancel.clone(),
            self.llm_breaker.clone(),
        );
        self.subagent_model = Some(handle);
    }

    /// The lifecycle-hook engine this agent fires (shared for `/reload`
    /// registry rebuilds).
    pub fn hooks(&self) -> &Arc<HookEngine> {
        &self.hooks
    }

    /// Fire the `session_start` hooks. Hook stdout is appended to the
    /// session as system context, visible to the model on every turn.
    pub async fn fire_session_start(&mut self, events: &mpsc::Sender<AgentEvent>) {
        if let Some(extra) = self.hooks.session_start(self.mode, Some(events)).await {
            self.push(ChatMessage::system(format!(
                "{SESSION_START_HOOK_NOTE}\n{extra}"
            )));
        }
    }

    /// Fire the `session_end` hooks. `events` is `None` when the surface is
    /// already torn down (e.g. the TUI terminal was restored).
    pub async fn fire_session_end(&self, events: Option<&mpsc::Sender<AgentEvent>>) {
        self.hooks.session_end(self.mode, events).await;
    }

    /// Swap the tool registry (after `/reload` or `/evolve`). Re-registers
    /// the always-present `exit_plan` and `interview` tools (sharing this
    /// agent's plan-mode and omakase flags) and refreshes the system prompt so
    /// the JSON tool protocol's tool list stays current.
    pub fn set_registry(&mut self, mut registry: ToolRegistry) {
        registry.register(Arc::new(crate::tools::plan::ExitPlanTool::new(
            Arc::clone(&self.plan_mode),
            Arc::clone(&self.omakase),
        )));
        registry.register(Arc::new(crate::tools::interview::InterviewTool::new(
            Arc::clone(&self.omakase),
        )));
        self.dispatcher.set_registry(registry);
        self.refresh_system_prompt();
    }

    /// Session token counters (prompt/completion totals, last prompt size).
    pub fn usage(&self) -> &crate::usage::UsageTracker {
        &self.usage
    }

    /// Number of background tasks (`execute` with `run_in_background`)
    /// still running, for `/status`.
    pub fn running_tasks(&self) -> usize {
        self.ctx
            .tasks
            .list()
            .iter()
            .filter(|task| !task.status.is_finished())
            .count()
    }

    /// Snapshot of every background task this session has spawned (running
    /// and finished), oldest first, for `/bashes`.
    pub fn tasks(&self) -> Vec<crate::tools::tasks::Task> {
        self.ctx.tasks.list()
    }

    /// The agent's working todo list, for `/status` on a surface that does not
    /// mirror the `TodoUpdated` events itself.
    pub fn todos(&self) -> Vec<crate::tools::todo::TodoItem> {
        // Poison is recovered, not propagated, for the same reason
        // `GateDesk::lock` recovers it: the list is owned data that nothing
        // reads while the lock is held, so a panic elsewhere cannot have left
        // it half-written. `map(...).unwrap_or_default()` used to swallow a
        // poisoned lock as an *empty* todo list, which reads on the status bar
        // as "the agent has no work left", a wrong answer where the honest
        // one was available all along.
        self.ctx
            .todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The model client this agent talks to. A surface rebuilding the tool
    /// registry (`/reload`) has to hand the subagent spawner the same client
    /// the parent runs on, or its subagents answer from a different model than
    /// the one `/model` and `/fusion` last set.
    pub fn client(&self) -> &Arc<dyn LlmProvider> {
        &self.client
    }

    /// Active model tag (what the next completion will call).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Snapshot of everything a [`SideQuestionContext`] needs to answer a
    /// `/btw` without borrowing the agent. Surfaces clone this before parking
    /// the agent in a turn task so a side question can run *while* the turn is
    /// in flight — the whole point of `/btw`.
    pub fn side_question_context(&self) -> SideQuestionContext {
        SideQuestionContext {
            client: Arc::clone(&self.client),
            model: self.model.clone(),
            messages: self.history.clone(),
            reasoning_effort: self
                .config
                .reasoning_effort
                .map(|effort| effort.as_str().to_string()),
        }
    }

    /// Snapshot of everything a [`ForkContext`] needs to spawn a `/fork` side
    /// quest without borrowing the agent. Same mid-turn pattern as
    /// [`Self::side_question_context`]: surfaces clone this before the agent
    /// leaves its slot so a fork can still fire while a turn is running.
    pub fn fork_context(&self) -> ForkContext {
        ForkContext {
            client: Arc::clone(&self.client),
            model: self.model.clone(),
            messages: self.history.clone(),
            registry: self.dispatcher.registry().snapshot(),
            hooks: Arc::clone(&self.hooks),
            ctx: self.ctx.clone(),
            read_only: self.plan_mode(),
        }
    }

    /// Answer a one-shot side question (`/btw`) against the live history. Does
    /// **not** push the exchange into history or the session file — that is
    /// the feature. Prefer [`SideQuestionContext`] when the agent is out of
    /// its slot mid-turn.
    pub async fn answer_side_question(&self, question: &str) -> Result<String> {
        self.side_question_context().ask(question).await
    }

    /// Spawn a `/fork` side quest against the live history. Detaches into the
    /// background-subagent registry and returns its id immediately; the report
    /// is injected into history the next time background subagents drain.
    /// Prefer [`ForkContext`] when the agent is out of its slot mid-turn.
    pub async fn spawn_fork(
        &self,
        task: &str,
        events: Option<mpsc::Sender<AgentEvent>>,
    ) -> Result<u32> {
        self.fork_context().spawn(task, events).await
    }

    /// Swap the model client mid-session (`/fusion`: the panel answers every
    /// turn; toggling back restores the configured provider). The conversation
    /// and the session file are untouched — on a surface whose chat *is* its
    /// session file (the GUI), rotating either to change what answers would
    /// strand the page on a session nothing writes to any more.
    ///
    /// The caller must also rebuild the tool registry against the new client
    /// ([`build_tool_registry`]), or subagents keep spawning on the old one.
    pub fn set_client(&mut self, client: Arc<dyn LlmProvider>, native_tools: bool) {
        self.client = client;
        self.native_tools = native_tools;
        // A new endpoint starts with a clean breaker — don't inherit the old
        // provider's failure history.
        self.llm_breaker = breaker::LlmBreaker::new();
        self.refresh_system_prompt();
    }

    /// Shared handle on the background-subagent registry, so a surface can
    /// kill a detached run. Cloned out rather than borrowed through the agent
    /// because the TUI parks the whole `Agent` elsewhere while a turn is in
    /// flight — which is exactly when you want to kill a runaway subagent.
    pub fn subagent_registry(&self) -> Arc<crate::tools::subagent_tasks::SubagentTaskRegistry> {
        Arc::clone(&self.ctx.subagents)
    }

    /// Redirect (or disable) the per-turn usage JSONL log. Defaults to
    /// `~/.wizard/usage.jsonl`; tests point it into a temp dir.
    pub fn set_usage_log(&mut self, path: Option<PathBuf>) {
        self.usage_log = path;
    }

    /// Whether plan mode is active.
    pub fn plan_mode(&self) -> bool {
        self.plan_mode.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Turn plan mode on or off (`/plan`, `--plan`, `plan_each_cycle`).
    /// While on, the dispatcher blocks every non-read-only tool except
    /// `exit_plan`, and the system prompt instructs the model to plan.
    pub fn set_plan_mode(&mut self, on: bool) {
        self.plan_mode
            .store(on, std::sync::atomic::Ordering::SeqCst);
        // Leaving plan mode also leaves omakase (omakase is a flavor of plan
        // mode; there is no omakase without the read-only exploration phase).
        if !on {
            self.omakase
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        self.sync_plan_prompt();
    }

    /// Whether omakase (chef's-choice) mode is active.
    pub fn omakase(&self) -> bool {
        self.omakase.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Turn omakase mode on or off (`/omakase`, `--omakase`). Omakase implies
    /// plan mode, so enabling it enables plan mode too; the agent explores
    /// read-only, then auto-approves its own plan and proceeds.
    pub fn set_omakase(&mut self, on: bool) {
        self.omakase.store(on, std::sync::atomic::Ordering::SeqCst);
        if on {
            self.plan_mode
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.sync_plan_prompt();
    }

    /// Turn `/ultra` on with a built engine, or off. Unlike `/fusion` this swaps
    /// neither the client nor the registry — the candidates run on this agent's
    /// own client and model — so the toggle is instant and the conversation
    /// survives it.
    pub fn set_ultra(&mut self, engine: Option<Arc<ultra::UltraEngine>>) {
        self.ultra = engine;
    }

    /// Whether `/ultra` is on for this session.
    pub fn ultra(&self) -> bool {
        self.ultra.is_some()
    }

    /// Re-compose the system prompt when the plan-mode or omakase flag changed
    /// since it was last baked in. Either flag can flip mid-turn (exit_plan
    /// approval clears plan mode), so the turn loop calls this before every
    /// completion.
    fn sync_plan_prompt(&mut self) {
        let plan = self.plan_mode();
        let omakase = self.omakase();
        if plan != self.plan_prompt_on || omakase != self.omakase_prompt_on {
            self.plan_prompt_on = plan;
            self.omakase_prompt_on = omakase;
            self.refresh_system_prompt();
        }
    }

    /// Switch models mid-session (`/model`) without resetting conversation
    /// context. `native_tools` is the new model's tool-calling capability
    /// (probe with [`OllamaClient::supports_native_tools`]); the system
    /// prompt is recomposed so the JSON tool protocol section matches.
    pub fn set_model(&mut self, model: String, native_tools: bool) {
        self.config.model = model.clone();
        if let Some(handle) = &self.subagent_model {
            handle.set_model(model.clone());
        }
        self.model = model;
        self.native_tools = native_tools;
        self.refresh_system_prompt();
    }

    /// Replace the skill set mid-session (`/reload`) and rebuild the system
    /// prompt so the new skills apply to subsequent turns.
    pub fn set_skills(&mut self, skills: Vec<Skill>) {
        self.skills = skills;
        self.refresh_system_prompt();
    }

    fn compose_system_prompt(&self) -> String {
        let mut prompt = prompts::build_system_prompt(
            self.mode,
            &self.skills,
            self.agents_md.as_deref(),
            self.memory_index.as_deref(),
        );
        if !self.native_tools {
            prompt.push_str("\n\n");
            prompt.push_str(&prompts::render_tool_protocol(
                &self.dispatcher.registry().specs(),
            ));
        }
        if self
            .dispatcher
            .registry()
            .get(crate::tools::todo::TODO_TOOL_NAME)
            .is_some()
        {
            prompt.push_str("\n\n");
            prompt.push_str(prompts::TODO_PROMPT);
        }
        // Always teach context stewardship: auto-compaction + session JSONL
        // are always on, and the agent should compact / reset deliberately
        // rather than wait for the window to overflow.
        prompt.push_str("\n\n");
        prompt.push_str(prompts::CONTEXT_PROMPT);
        if self.plan_mode() {
            prompt.push_str("\n\n");
            prompt.push_str(prompts::PLAN_MODE_PROMPT);
            if self.omakase() {
                prompt.push_str("\n\n");
                prompt.push_str(prompts::OMAKASE_PROMPT);
            }
        }
        prompt
    }

    fn refresh_system_prompt(&mut self) {
        self.memory_index = read_memory_index(&self.ctx.cwd);
        let prompt = self.compose_system_prompt();
        match self.history.first_mut() {
            Some(first) if first.role == Role::System => *first = ChatMessage::system(prompt),
            _ => self.history.insert(0, ChatMessage::system(prompt)),
        }
    }

    /// Append to history and persist. Injected system messages (background
    /// notes, subagent reports, hook context) persist as flagged system
    /// notes that replay on resume; the system prompt itself is never
    /// pushed (it lives at history[0] and is recomposed fresh).
    fn push(&mut self, message: ChatMessage) {
        let result = if message.role == Role::System {
            self.session.append_system_note(&message)
        } else {
            self.session.append(&message)
        };
        if let Err(err) = result {
            tracing::warn!("session append failed: {err}");
        }
        self.history.push(message);
    }

    /// Drop the guidance `/ultra` injected for the turn that just ended.
    ///
    /// Guidance is turn-scoped by nature: it is N drafts and a verdict about
    /// *one* request, and that request has now been answered. Left in history it
    /// would be re-sent on every subsequent turn and accumulate one block per
    /// ultra turn — tens of KB each — until it filled a large fraction of the
    /// window with stale advice, and (because a guidance block sits immediately
    /// after its user message) it would also stall the compactor, whose kept
    /// tail must start at a `Role::User` message.
    ///
    /// It is only ever in `self.history`, never in the session file, so nothing
    /// has to be un-persisted. Compaction may have folded it into a summary
    /// mid-turn, in which case there is nothing left to find and this is a
    /// no-op — as it is on every turn of the ordinary single-agent path, which
    /// is why this is unconditional rather than gated on the ultra flag: the
    /// flag can be turned off between turns, and the block it left behind still
    /// has to go.
    fn drop_ultra_guidance(&mut self) {
        self.history.retain(|message| !ultra::is_guidance(message));
    }

    /// Append this turn's token usage to the JSONL log. Compaction is not
    /// part of it: it bills itself through
    /// [`record_compaction_usage`](Self::record_compaction_usage).
    fn record_turn_usage(&self) {
        let (prompt_tokens, completion_tokens) = self.usage.turn_totals();
        let (cache_read_tokens, cache_write_tokens) = self.usage.turn_cache_totals();
        self.append_usage_record(crate::usage::TurnTokens {
            prompt: prompt_tokens,
            completion: completion_tokens,
            cache_read: cache_read_tokens,
            cache_write: cache_write_tokens,
        });
    }

    /// Bill one compaction pass ([`context::compact`]'s summarization calls).
    ///
    /// Its own line in the log rather than a share of the turn's, because a
    /// pass does not always happen inside a turn: `/compact` between turns is
    /// a model call like any other, and the per-turn counters are reset at the
    /// top of the next turn, so anything parked in them would simply vanish.
    /// Writing the line here also keeps the two paths from double-counting,
    /// which is why the tracker is told
    /// [`record_side_call`](crate::usage::UsageTracker::record_side_call)
    /// rather than the turn totals: the session counters behind `/cost` see
    /// the tokens, the turn record does not.
    fn record_compaction_usage(&self, usage: &context::CompactUsage) {
        if !usage.reported() {
            return;
        }
        self.usage.record_side_call(
            usage.prompt,
            usage.completion,
            usage.cache_read,
            usage.cache_write,
        );
        self.append_usage_record(crate::usage::TurnTokens {
            prompt: usage.prompt,
            completion: usage.completion,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
        });
    }

    /// Append one priced record to the JSONL usage log (when the backend
    /// reported counts and the log is enabled). Best-effort: failures are
    /// logged, never surfaced.
    fn append_usage_record(&self, tokens: crate::usage::TurnTokens) {
        if tokens.prompt == 0 && tokens.completion == 0 {
            return;
        }
        let Some(path) = &self.usage_log else {
            return;
        };
        let crate::usage::TurnTokens {
            prompt: prompt_tokens,
            completion: completion_tokens,
            cache_read: cache_read_tokens,
            cache_write: cache_write_tokens,
        } = tokens;
        let provider = self.config.active();
        // Cost is settled here, at write time, because this is the only place
        // that holds all three inputs at once: the counts, the model that
        // produced them, and the provider's configured rates. Whoever reads
        // usage.jsonl months from now has the counts and neither of the
        // others, and by then the config may name a different model entirely.
        let priced = crate::usage::estimate_cost(
            crate::usage::TurnTokens {
                prompt: prompt_tokens,
                completion: completion_tokens,
                cache_read: cache_read_tokens,
                cache_write: cache_write_tokens,
            },
            &crate::usage::PriceInputs {
                model: &self.model,
                // The seller, for the open-weight ids several hosts serve at
                // different prices: `gpt-oss-120b` costs one thing on Groq
                // and more than twice that on Cerebras, and the model id
                // alone cannot tell them apart.
                endpoint: &provider.base_url,
                usd_per_mtok_in: provider.usd_per_mtok_in,
                usd_per_mtok_out: provider.usd_per_mtok_out,
                self_hosted: crate::usage::self_hosted(provider.kind),
            },
        );
        let record = crate::usage::UsageRecord {
            ts: crate::usage::unix_now(),
            project: self.ctx.cwd.display().to_string(),
            model: self.model.clone(),
            provider: provider.name,
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cost_usd: Some(priced.usd),
            price_source: priced.source,
            mode: self.mode.to_string(),
        };
        if let Err(err) = crate::usage::append(path, &record) {
            tracing::warn!("could not append usage record: {err:#}");
        }
    }

    /// Compact the conversation unconditionally — the `/compact` command's
    /// force path, the `compact` tool, and the shared core of
    /// the step loop's own pass. The cut itself is [`context::compact`]'s, under
    /// [`context::Anchor::Conversation`]; what is added here is the part that
    /// needs an [`Agent`]: persisting the note, and invalidating the reading
    /// that triggered the pass.
    ///
    /// On success the progress note is appended to the session as a system note
    /// so resume and the model both see that stewardship happened (the full
    /// pre-compact transcript remains earlier in the JSONL).
    pub async fn compact_now(&mut self) -> CompactOutcome {
        let budget = context::Budget {
            window: self.client.context_window(&self.model).await,
            byte_threshold: self.config.compact_threshold_bytes,
        };
        let compacted = context::compact(
            &mut self.history,
            context::Anchor::Conversation,
            budget,
            &self.client,
            &self.model,
        )
        .await;
        self.record_compaction_usage(&compacted.usage);
        // Persist the note so resume replays the stewardship breadcrumb; the
        // in-memory middle span is already replaced (the full transcript stays
        // earlier in the append-only JSONL).
        if let Some(note) = &compacted.note
            && let Err(err) = self.session.append_system_note(note)
        {
            tracing::warn!("session append failed for compact note: {err}");
        }
        if compacted.outcome != CompactOutcome::Nothing {
            // The history just shrank: the last reported prompt size is stale
            // and must not re-trigger compaction on the next step. A pass that
            // cut nothing leaves it alone — the reading is still true, and
            // dropping it would put the status bar back on an estimate.
            self.usage.clear_last_prompt();
        }
        compacted.outcome
    }

    /// Drain background tasks and subagents that finished since the last check
    /// (each reported exactly once): inject a notification into history so the
    /// model sees it on its next completion, and emit the matching event for
    /// the surfaces.
    ///
    /// The same drain the loop runs at the top of every step (it *is* that
    /// drain — see [`turn::drain_background`]), exposed for the perpetual
    /// runner, which has to surface finished work between cycles too.
    pub(crate) async fn drain_background(&mut self, events: &mpsc::Sender<AgentEvent>) {
        let sink = turn::Sink::Turn(events.clone());
        let mut host = turn::TurnHost { agent: self };
        turn::drain_background(&mut host, &sink).await;
    }

    /// Collect background tasks and subagents that finished since the last
    /// check, injecting each note into history (persisted, exactly once) and
    /// returning the batch. For surfaces to poll on their idle tick — the
    /// same drain the turn loop runs at the top of every step — so finished
    /// work surfaces while the agent sits between turns. Cheap when nothing
    /// finished (two mutex-guarded scans, no I/O).
    pub fn drain_finished_notifications(&mut self) -> Vec<FinishedNotification> {
        let mut notifications = Vec::new();
        for task in self.ctx.tasks.drain_completed() {
            self.push(ChatMessage::system(task_note(&task)));
            notifications.push(FinishedNotification::Task(task));
        }
        for task in self.ctx.subagents.drain_completed() {
            self.push(ChatMessage::system(subagent_note(&task)));
            notifications.push(FinishedNotification::Subagent(task));
        }
        notifications
    }
}

impl Drop for Agent {
    /// Kill everything this agent detached: background shell tasks, and
    /// background subagent runs.
    ///
    /// Both, and the second one was missing. A shell task's child carries
    /// `kill_on_drop`, so killing the registry is mostly making the teardown
    /// explicit and immediate. A background subagent has no such backstop: its
    /// driver is a detached `tokio::spawn` holding its own `Arc` clones of the
    /// client, the registry and the tool context, so dropping the agent freed
    /// none of it. The run kept calling the provider, kept billing the user's
    /// key, and kept writing into a registry nothing would ever drain, once for
    /// every rebuild of the agent, which is every `/model`, every provider
    /// switch, every `/fusion` toggle and every `/reload`.
    fn drop(&mut self) {
        self.ctx.tasks.kill_all();
        self.ctx.subagents.kill_all();
    }
}

/// The image store for session `id` (`~/.wizard/images/<id>/`). A store that
/// cannot be opened (no home directory) costs the surfaces their copy of an
/// image, never the turn — the model still gets the base64 — so the failure is
/// logged, not fatal.
fn open_image_store(id: &str) -> Option<Arc<ImageStore>> {
    match ImageStore::open(id) {
        Ok(store) => Some(Arc::new(store)),
        Err(err) => {
            tracing::warn!("could not open the session image store: {err:#}");
            None
        }
    }
}

/// Read the persistent memory index (MEMORY.md) for `project_root`, if any
/// memories are saved. Failures are logged, not fatal — memory is an
/// enhancement, never a reason a session cannot start.
fn read_memory_index(project_root: &Path) -> Option<String> {
    let store = match crate::memory::MemoryStore::open(project_root) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!("could not open memory store: {err:#}");
            return None;
        }
    };
    match store.index() {
        Ok(index) => index,
        Err(err) => {
            tracing::warn!("could not read memory index: {err:#}");
            None
        }
    }
}

/// Build a fully wired headless [`Agent`]: construct the active provider's
/// client, health-check it, probe native tool support, assemble the tool
/// registry (native + scripted + MCP + subagent spawner + evolve + publish),
/// load skills, and open/create the session.
///
/// This is the shared agent-construction path used by the sovereign headless
/// runner ([`crate::headless::run`]), the ACP server ([`crate::acp`]) and the
/// messaging gateway ([`crate::gateway`]). `resume` reopens the latest session
/// instead of starting a new one. Each builds exactly one agent, so each lets
/// this path connect the MCP servers for it.
pub async fn build_headless_agent(
    config: &Config,
    project_root: &Path,
    resume: bool,
) -> Result<Agent> {
    build_headless_agent_inner(config, project_root, resume, None, None).await
}

/// [`build_headless_agent`] with an explicit session instead of the
/// latest-or-new resolution — the GUI server manages one session per task
/// (created for a chosen workspace, or reopened by id) and hands it in.
///
/// `mcp` is the caller's already-connected manager. A process that builds more
/// than one agent — the GUI, one per warm task — must connect its servers once
/// and pass them here: connecting per build would run one copy of every
/// configured MCP server *per agent*, each a real OS process. `None` connects a
/// manager for this agent alone.
pub async fn build_headless_agent_for_session(
    config: &Config,
    project_root: &Path,
    session: Session,
    mcp: Option<&McpManager>,
) -> Result<Agent> {
    build_headless_agent_inner(config, project_root, false, Some(session), mcp).await
}

/// The agent's whole tool set, freshly composed: native tools, scripted tools
/// (`~/.wizard/tools`), the MCP tools `manager` is connected to, the subagent
/// spawner, and the config-dependent `evolve` / `publish` tools.
///
/// Returns the registry and the spawn tool's shared model slot, which the caller
/// must hand to [`Agent::bind_subagent_model`] — a fresh spawn tool reads the
/// *configured* model until it is bound, and would quietly ignore `/model`.
///
/// This is what a build composes and what `/reload` recomposes, so a reloaded
/// session has exactly the tools a fresh one does — no more (a second copy of
/// every MCP server) and no fewer (`evolve` and `publish` silently dropped).
pub async fn build_tool_registry(
    config: &Config,
    client: &Arc<dyn LlmProvider>,
    hooks: &Arc<HookEngine>,
    manager: &McpManager,
) -> Result<(ToolRegistry, subagent::SharedActiveModel)> {
    let mut base = ToolRegistry::with_native_tools();
    match Config::scripted_tools_dir() {
        Ok(dir) => {
            if let Err(err) = base.load_scripted(&dir) {
                tracing::warn!("loading scripted tools failed: {err}");
            }
        }
        Err(err) => tracing::warn!("scripted tools dir unavailable: {err}"),
    }
    if let Err(err) = base.attach_mcp(manager).await {
        tracing::warn!("attaching MCP tools failed: {err}");
    }
    base.apply_harness_overrides();

    let subagents_dir = Config::subagents_dir()?;
    let subagent_configs = subagent::available_configs(&subagents_dir);
    let base = Arc::new(base);
    let mut registry = subagent::scoped_registry(&base, None);
    let spawn_tool = Arc::new(subagent::SpawnSubagentTool::new(
        subagent_configs,
        Arc::clone(client),
        Arc::clone(&base),
        Arc::clone(hooks),
    ));
    let subagent_model = spawn_tool.model_handle();
    registry.register(spawn_tool);
    registry.register(Arc::new(crate::tools::evolve::EvolveTool::new(
        config.clone(),
    )));
    registry.register(Arc::new(crate::tools::publish::PublishTool::new(
        config.clone(),
    )));
    Ok((registry, subagent_model))
}

/// Skills from the repo/bundled roots plus `~/.wizard/skills` (user shadowing).
/// A skill tree that will not load costs its skills, never the session.
pub fn load_skills() -> Vec<Skill> {
    let roots = crate::skills::default_roots();
    crate::skills::load_skills(&roots).unwrap_or_else(|err| {
        tracing::warn!("loading skills failed: {err}");
        Vec::new()
    })
}

/// Connect every server in `~/.wizard/mcp.toml`. Never hard-fails: a missing or
/// broken config, or a server that will not come up, costs its tools — not the
/// session.
pub async fn connect_mcp() -> McpManager {
    let config = match Config::mcp_config_path().and_then(|path| McpConfig::load(&path)) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!("could not load mcp.toml: {err:#}");
            return McpManager::empty();
        }
    };
    match McpManager::connect_all(&config).await {
        Ok(manager) => manager,
        Err(err) => {
            tracing::warn!("MCP startup failed: {err:#}");
            McpManager::empty()
        }
    }
}

async fn build_headless_agent_inner(
    config: &Config,
    project_root: &Path,
    resume: bool,
    session: Option<Session>,
    mcp: Option<&McpManager>,
) -> Result<Agent> {
    let active = config.active();
    let model = active.model.clone();
    let client = active
        .build()
        .with_context(|| format!("building provider '{}'", active.name))?;
    // llama.cpp gets a lifecycle hand: when nothing answers, Wizard starts
    // the server itself, showing spawn/load progress on a spinner (plain
    // stderr lines when stderr is not a terminal).
    if active.kind == ProviderKind::LlamaCpp {
        let wait = crate::progress::ServerSpinner::start();
        let outcome = crate::server::ensure_running(&active, &wait).await;
        wait.finish(outcome.is_ok());
        outcome?;
    }
    // Ollama's analog: a configured tag that is not pulled yet is pulled now
    // (loopback hosts only — never download onto a remote server).
    if active.kind == ProviderKind::Ollama && crate::server::local_port(&active.base_url).is_some()
    {
        let wait =
            crate::progress::ServerSpinner::start_with("Checking the local model…", "model ready");
        let outcome = crate::llm::ollama::OllamaClient::new(active.base_url.clone())
            .ensure_model(&model, &wait)
            .await;
        wait.finish(outcome.is_ok());
        outcome?;
    }
    client
        .health()
        .await
        .with_context(|| format!("LLM health check failed for {}", client.label()))?;

    let native_tools = crate::llm::provider::probe_native_tools(client.as_ref(), &model).await;
    if !native_tools {
        // Never `println!`. Every surface reaches this path, and two of them
        // own stdout as a protocol transport: the ACP server frames JSON-RPC
        // on it (`crate::acp`) and `--output-format json` frames the run
        // there. A bare line on stdout corrupts both, so the notice goes to
        // stderr, which no surface parses.
        eprintln!("using the JSON tool protocol for '{model}'");
    }

    // Session first: the hook engine carries its id in every payload. An
    // explicit session (GUI) wins; otherwise resolve latest-or-new here.
    let session = match session {
        Some(session) => session,
        None => {
            let sessions_dir = Config::sessions_dir()?;
            if resume {
                match Session::open_latest(&sessions_dir)? {
                    Some(session) => session,
                    None => Session::create(&sessions_dir)?,
                }
            } else {
                Session::create(&sessions_dir)?
            }
        }
    };

    // Lifecycle hooks, shared by the agent's dispatcher and the subagent
    // spawner so subagent tool calls fire the same hooks.
    let hooks = Arc::new(HookEngine::new(
        crate::hooks::load(project_root),
        project_root.to_path_buf(),
        session.id.clone(),
    ));

    // Tools: natives + scripted + MCP, then the subagent spawner on top.
    let connected;
    let manager = match mcp {
        Some(manager) => manager,
        None => {
            connected = connect_mcp().await;
            &connected
        }
    };
    let (registry, subagent_model) = build_tool_registry(config, &client, &hooks, manager).await?;

    let skills = load_skills();

    let mut agent = Agent::new(
        client,
        registry,
        config.clone(),
        skills,
        project_root.to_path_buf(),
        session,
        native_tools,
        hooks,
    )?;
    agent.bind_subagent_model(subagent_model);
    Ok(agent)
}

#[cfg(test)]
mod tests;

/// Teardown: what a dropped [`Agent`] takes with it.
///
/// Its own module rather than a case in [`tests`], because the thing under
/// test is a `Drop` impl and every assertion has to happen *after* the value
/// is gone, which means the harness is a scope and not a fixture.
#[cfg(test)]
mod teardown_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::llm::ChatStream;

    /// A provider that is never called; the agent under test only has to
    /// exist.
    struct IdleProvider;

    #[async_trait]
    impl LlmProvider for IdleProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, _request: ChatRequest) -> Result<ChatStream> {
            anyhow::bail!("the teardown tests never talk to a model")
        }

        fn label(&self) -> String {
            "idle:test".to_string()
        }
    }

    /// A background subagent must not outlive the agent that spawned it.
    ///
    /// It used to. `Drop` killed the shell-task registry and not the subagent
    /// one, so every rebuild of the agent (`/model`, a provider switch, a
    /// `/fusion` toggle, `/reload`) left the previous agent's detached runs
    /// calling the provider on a key nobody was watching, writing into a
    /// registry nothing would ever drain.
    #[tokio::test]
    async fn dropping_the_agent_kills_its_background_subagents() {
        let dir = std::env::temp_dir().join(format!("wizard-teardown-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let session = Session::create(&dir).expect("create session");
        let hooks = Arc::new(HookEngine::new(Vec::new(), dir.clone(), session.id.clone()));

        // Set by the detached run if it is ever allowed to finish.
        let survived = Arc::new(AtomicBool::new(false));
        let registry = {
            let agent = Agent::new(
                Arc::new(IdleProvider),
                ToolRegistry::new(),
                Config::default(),
                Vec::new(),
                dir.clone(),
                session,
                true,
                hooks,
            )
            .expect("build agent");

            let registry = agent.subagent_registry();
            let flag = Arc::clone(&survived);
            registry.spawn("worker", "outlive my parent", async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                flag.store(true, Ordering::SeqCst);
                crate::tools::subagent_tasks::SubagentRunResult {
                    completed: true,
                    output: "still here".to_string(),
                    steps_used: 1,
                    error: None,
                }
            });
            assert_eq!(registry.pending_count(), 1, "the run is detached and live");
            registry
        };

        // Give the aborted driver a chance to be scheduled, so a failure here
        // is "it kept running", not "it had not started yet".
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.pending_count(),
            0,
            "the agent went away and its detached runs went with it"
        );
        assert!(
            !survived.load(Ordering::SeqCst),
            "the run was aborted, not merely marked as finished"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
