//! Subagents: isolated sub-contexts for parallel or decomposed work.
//!
//! Each subagent gets its own message history and tool scope. The result
//! returns to the parent as a single tool result, so a multi-step sub-task
//! costs the parent one turn of context.
//!
//! # What ends a run
//!
//! Three things, and until recently there were none of them. A sub-loop used
//! to default to *no* step ceiling, carry no deadline, and observe no
//! cancellation, which meant nothing in the process could end one: a subagent
//! that talked itself into re-reading the same file forever spent the API
//! budget until the operator killed Wizard, and a foreground one could not be
//! interrupted at all: Esc reached the parent's turn and stopped nothing.
//!
//! So a run now ends on whichever of these comes first:
//!
//! - the model stops calling tools (the ordinary end),
//! - [`SubagentConfig::max_steps`], which defaults to [`DEFAULT_MAX_STEPS`]
//!   rather than to unlimited,
//! - [`SpawnOptions::deadline`], defaulting to [`DEFAULT_DEADLINE`],
//! - [`SpawnOptions::cancel`], the turn's [`CancelHandle`], when the caller is
//!   a foreground run that the user can interrupt,
//! - [`SpawnOptions::breaker`], the endpoint's circuit breaker, when the
//!   provider is down.
//!
//! The last three are enforced by [`spawn`] itself and not by its callers.
//! That is deliberate: `/ultra` used to wrap every candidate in its own biased
//! `select!` because `spawn` had neither of the first two, which put the
//! knowledge of how to abort a run and how to close out its pane in the one
//! caller that happened to need it. A caller that forgot is a run nothing can
//! stop.
//!
//! # What it shares with the parent turn
//!
//! Everything that is not sub-loop-specific, and by construction rather than
//! by resemblance. A sub-run climbs the parent's retry ladder over the
//! parent's breaker ([`crate::agent::retry`]) and steers by the same context
//! reading and the same compactor ([`crate::agent::context`]); what differs is
//! passed *in* — a scoped registry, a step budget, no user to interview, no
//! slash commands, no session file — rather than reimplemented here. Each
//! capability this loop lacked used to be one bug; `/ultra` and `/fusion` fan
//! N of these out per turn, which made each one N.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, StepBudget};
use crate::dispatch::{DispatchOutcome, Dispatcher};
use crate::hooks::HookEngine;
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, Role, ToolCall, ToolSpec};
use crate::tools::subagent_tasks::SubagentRunResult;
use crate::tools::{
    CommandDispatch, Tool, ToolAccess, ToolContext, ToolError, ToolOutput, registry::ToolRegistry,
};

use super::turn::{self, CallOutcome, Host, Policy, Sink, StepUsage};
use super::{CancelHandle, breaker, cancelled, context, prompts};

/// Advertised name of the spawn tool, referenced by the dispatcher's
/// plan-mode gate.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// Step ceiling a definition gets when it does not set one.
///
/// Deliberately finite, where this used to be [`StepBudget::UNLIMITED`]. The
/// parent turn is unlimited for a reason (a human is at the prompt and can
/// interrupt it), and a subagent inherited that reason without inheriting the
/// human: nobody is watching a background run, and a foreground one reports
/// nothing until it is done, so a sub-loop that stops making progress is
/// invisible until the bill arrives.
///
/// Fifty round trips is far past where a self-contained sub-task is still
/// converging and far short of a budget worth reaching for on purpose. A
/// specialist that genuinely needs more says so in its TOML.
pub const DEFAULT_MAX_STEPS: u32 = 50;

/// Wall-clock cap on one run when the caller names none.
///
/// A step budget alone does not bound a run in *time*: a throttled provider
/// parks each step inside the retry ladder below (six attempts, `retry_base`
/// doubling to `retry_max`, over five minutes of sleeping at the shipped
/// defaults), so fifty steps against a rate-limited endpoint is hours during
/// which the run has produced nothing and will keep producing nothing.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// What a subagent reports when its loop ended without any final text — it
/// only ever called tools, or the model returned nothing. Not an error, but
/// not an answer either, which is why a caller that *judges* subagent output
/// ([`crate::agent::ultra`]) has to be able to tell the two apart.
pub const NO_FINAL_TEXT: &str = "(subagent produced no final text)";

/// Session-unique id for one subagent run. Every `AgentEvent::SubagentRun*`
/// event carries it, so a surface can demux concurrent runs — including two
/// runs of the same subagent — into separate panes.
pub fn next_run_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The parent agent's live state that a `spawn_subagent` call has to read when
/// it *runs* rather than when the tool was built: the model `/model` last
/// switched to, the [`CancelHandle`] of the turn the call is happening inside,
/// and the circuit breaker over the endpoint the parent is dialing.
///
/// One slot holding all three, rather than three slots, because they are bound
/// by the same call at the same moment
/// ([`crate::agent::Agent::bind_subagent_model`]) and a tool holding one but
/// not another is a specific bug each time: without the model, subagents
/// answer from the model the session started on however many times `/model`
/// has been used since; without the handle, an interrupted turn leaves its
/// foreground subagent running; without the breaker, a delegated run keeps
/// dialing a provider the parent has already given up on.
///
/// All three are `Option` because the tool is built before the agent that
/// binds it exists. Unbound, a run falls back to the configured model, cannot
/// be interrupted, and carries a breaker of its own, which is what every
/// surface that never binds already gets.
#[derive(Debug, Default)]
pub struct SubagentBinding {
    model: std::sync::RwLock<Option<String>>,
    cancel: std::sync::RwLock<Option<CancelHandle>>,
    breaker: std::sync::RwLock<Option<breaker::LlmBreaker>>,
}

impl SubagentBinding {
    /// Point this slot at a parent agent: the model it is live on, the handle
    /// its surface raises on Esc, and the breaker over its endpoint.
    pub fn bind(&self, model: String, cancel: CancelHandle, breaker: breaker::LlmBreaker) {
        self.set_model(model);
        if let Ok(mut slot) = self.cancel.write() {
            *slot = Some(cancel);
        }
        if let Ok(mut slot) = self.breaker.write() {
            *slot = Some(breaker);
        }
    }

    /// Write a mid-session `/model` switch through, so the next subagent runs
    /// on the model the user just chose.
    pub fn set_model(&self, model: String) {
        if let Ok(mut slot) = self.model.write() {
            *slot = Some(model);
        }
    }

    /// The parent's active model; `None` until bound.
    pub fn model(&self) -> Option<String> {
        self.model.read().ok().and_then(|model| model.clone())
    }

    /// The running turn's cancel handle; `None` until bound.
    pub fn cancel(&self) -> Option<CancelHandle> {
        self.cancel.read().ok().and_then(|cancel| cancel.clone())
    }

    /// The parent's endpoint breaker; `None` until bound.
    pub fn breaker(&self) -> Option<breaker::LlmBreaker> {
        self.breaker.read().ok().and_then(|breaker| breaker.clone())
    }
}

/// The shared [`SubagentBinding`] a surface hands from
/// [`SpawnSubagentTool::model_handle`] to
/// [`crate::agent::Agent::bind_subagent_model`].
///
/// The name is older than the contents (it held only the model once) and is
/// kept because it is what every surface's registry builder returns.
pub type SharedActiveModel = Arc<SubagentBinding>;

/// A named, reusable subagent definition. Built-in defaults exist
/// (a general-purpose worker); `/evolve` can add more as TOML files under
/// `~/.wizard/subagents/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConfig {
    /// Unique name the parent refers to (e.g. `"reviewer"`).
    pub name: String,
    /// What this subagent is for (shown to the parent model).
    pub description: String,
    /// System prompt for the isolated context.
    pub system_prompt: String,
    /// Tool names this subagent may call. `None` = the parent's full set.
    #[serde(default)]
    pub tool_scope: Option<Vec<String>>,
    /// Step ceiling for the sub-loop, defaulting to [`DEFAULT_MAX_STEPS`]. Set
    /// `0` for [`StepBudget::UNLIMITED`] only when a specialist genuinely runs
    /// until it is done and something else is going to end it.
    #[serde(default = "SubagentConfig::default_max_steps")]
    pub max_steps: StepBudget,
}

impl SubagentConfig {
    fn default_max_steps() -> StepBudget {
        StepBudget::new(DEFAULT_MAX_STEPS)
    }
}

/// Why a run ended before its loop did.
///
/// Returned as [`spawn`]'s error rather than folded into its `Ok` value, so a
/// caller that must tell an *interrupted* run from a *broken* one (the
/// council fans several out and one Ctrl-C has to end the whole sitting, not
/// contribute one dead candidate to it) can ask, in the same way the turn
/// loop already asks about [`crate::llm::TruncatedToolCall`]:
///
/// ```ignore
/// match err.downcast_ref::<SubagentStop>() {
///     Some(SubagentStop::Cancelled) => …,
///     _ => …,
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStop {
    /// The user interrupted: [`SpawnOptions::cancel`] was raised.
    Cancelled,
    /// The run outlived [`SpawnOptions::deadline`].
    DeadlineExceeded(Duration),
}

impl std::fmt::Display for SubagentStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::DeadlineExceeded(after) => write!(f, "timed out after {after:?}"),
        }
    }
}

impl std::error::Error for SubagentStop {}

/// Outcome of a subagent run, summarized for the parent.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub name: String,
    /// The subagent's final answer text.
    pub output: String,
    pub steps_used: u32,
    /// False when the sub-loop hit an optional step budget or errored out.
    pub completed: bool,
}

/// Built-in subagent definitions available on every install.
pub fn builtin_configs() -> Vec<SubagentConfig> {
    vec![SubagentConfig {
        name: "worker".to_string(),
        description: "General-purpose worker for self-contained sub-tasks: \
                      investigate, edit, run commands, and report back."
            .to_string(),
        system_prompt: "You are a focused subagent of Wizard, a local agent. Complete \
                        the given sub-task end-to-end using the provided tools, then reply \
                        with a concise final report of what you found or changed. Do not ask \
                        questions; make reasonable decisions and note them in your report."
            .to_string(),
        tool_scope: None,
        max_steps: SubagentConfig::default_max_steps(),
    }]
}

/// Load `/evolve`-authored subagent definitions (`*.toml`) from `dir`.
/// Missing directory yields an empty vec.
pub fn load_dir(dir: &Path) -> Result<Vec<SubagentConfig>> {
    let mut configs = Vec::new();
    if !dir.is_dir() {
        return Ok(configs);
    }
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|raw| toml::from_str::<SubagentConfig>(&raw).map_err(anyhow::Error::from));
        match parsed {
            Ok(config) => configs.push(config),
            Err(err) => {
                tracing::warn!("skipping subagent manifest {}: {err}", path.display());
            }
        }
    }
    Ok(configs)
}

/// Built-in subagents plus any user-defined ones from `dir`, plus the active
/// harness bundle's `subagents/` (if any); later sources shadow earlier ones
/// by name, so bundle definitions win over user definitions win over
/// built-ins.
pub fn available_configs(dir: &Path) -> Vec<SubagentConfig> {
    let mut configs = builtin_configs();
    let mut merge_from = |dir: &Path| {
        let loaded = load_dir(dir).unwrap_or_else(|err| {
            tracing::warn!("loading subagents from {} failed: {err}", dir.display());
            Vec::new()
        });
        for config in loaded {
            configs.retain(|existing| existing.name != config.name);
            configs.push(config);
        }
    };
    merge_from(dir);
    if let Some(harness) = crate::config::Config::harness_dir() {
        let bundle = harness.join("subagents");
        if bundle.is_dir() {
            merge_from(&bundle);
        }
    }
    configs
}

/// Build a registry containing the tools of `parent` named in `scope`
/// (`None` = all of them). Unknown names are skipped with a warning.
pub fn scoped_registry(parent: &ToolRegistry, scope: Option<&[String]>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    match scope {
        None => {
            for spec in parent.specs() {
                if let Some(tool) = parent.get(&spec.function.name) {
                    registry.register(Arc::clone(tool));
                }
            }
        }
        Some(names) => {
            for name in names {
                match parent.get(name) {
                    Some(tool) => registry.register(Arc::clone(tool)),
                    None => tracing::warn!("subagent tool scope names unknown tool '{name}'"),
                }
            }
        }
    }
    registry
}

/// Keep only the read-only tools of `parent` (plan-mode delegation: the
/// subagent may explore but not act).
pub fn read_only_registry(parent: &ToolRegistry) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for spec in parent.specs() {
        if let Some(tool) = parent.get(&spec.function.name)
            && tool.access() == ToolAccess::ReadOnly
        {
            registry.register(Arc::clone(tool));
        }
    }
    registry
}

/// Per-run overrides for [`spawn`].
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Model to run the subagent on; `None` falls back to the configured
    /// active model. The parent passes its live model so `/model` switches
    /// apply.
    pub model: Option<String>,
    /// Restrict the subagent to read-only tools (plan mode).
    pub read_only: bool,
    /// When set, seed the run from the parent conversation instead of a fresh
    /// system prompt + bare task. Used by [`spawn_fork`] (`/fork`): the side
    /// quest inherits history, tools, and prompt, then appends its brief.
    pub inherited_history: Option<Vec<ChatMessage>>,
    /// The turn this run belongs to, so a user interrupt ends it.
    ///
    /// `None`, the default, is an *uninterruptible* run, and that is the
    /// right answer for exactly one caller: a backgrounded subagent outlives
    /// the turn that spawned it by design, so wiring the turn's handle into it
    /// would kill the user's detached work the moment they pressed Esc on
    /// something unrelated. Background runs are ended through
    /// [`crate::tools::subagent_tasks::SubagentTaskRegistry::kill`] instead.
    /// Every foreground caller passes a handle.
    pub cancel: Option<CancelHandle>,
    /// Wall-clock cap on the whole run, defaulting to [`DEFAULT_DEADLINE`].
    /// `None` removes the cap, which only a caller enforcing its own should
    /// do.
    pub deadline: Option<Duration>,
    /// Circuit breaker over the endpoint this run will dial.
    ///
    /// Not `Option`, unlike everything above it: a run without a breaker is
    /// the defect this field closes, so the question a caller answers is
    /// *whose* breaker, never whether. A caller that has the parent's passes
    /// it, and then an outage the parent already hit is one its N delegated
    /// runs do not each have to prove; a caller that has none gets a fresh
    /// one, which still bounds that run on its own.
    pub breaker: breaker::LlmBreaker,
}

impl Default for SpawnOptions {
    /// Note that this is *not* `#[derive]`-shaped: `deadline` defaults to
    /// [`DEFAULT_DEADLINE`] and not to `None`. A derived default would make
    /// "the caller said nothing" mean "no deadline", which is the exact
    /// failure this field exists to close.
    fn default() -> Self {
        Self {
            model: None,
            read_only: false,
            inherited_history: None,
            cancel: None,
            deadline: Some(DEFAULT_DEADLINE),
            breaker: breaker::LlmBreaker::new(),
        }
    }
}

/// Built-in name for a `/fork` side-quest run (shown on the subagent rail and
/// in the background-subagent report injected into the parent).
pub const FORK_NAME: &str = "fork";

/// Tools a fork must never call: nesting another spawn would recurse forever,
/// and interactive / surface-bound tools have no user attached to answer them.
const FORK_TOOL_DENYLIST: &[&str] = &[
    SPAWN_SUBAGENT_TOOL_NAME,
    "run_command",
    "exit_plan",
    "interview",
    // Compact is parent-loop only; a fork calling it just gets an error.
    crate::tools::compact::COMPACT_TOOL_NAME,
];

/// System reminder appended as the user message that launches a `/fork`
/// side quest. The parent conversation stays untouched; this brief is only
/// in the fork's own history.
const FORK_BRIEF: &str = "\
This is a forked side quest from the user (\"/fork\"). You inherit the full \
conversation above — history, tools, and system prompt — and run in parallel \
with the main session.\n\
\n\
CRITICAL CONSTRAINTS:\n\
- Complete the side quest end-to-end using your tools, then reply with a \
concise final report of what you found or changed.\n\
- Do not ask the user questions; make reasonable decisions and note them in \
your report.\n\
- Do not try to steer the main conversation or wait on it — you are a \
detached worker. Your report is injected back into the main session when \
you finish.\n\
- Stay focused on the side quest below; ignore unrelated open work unless it \
blocks you.";

/// Parent tool set with the tools a fork must never call stripped (see
/// [`FORK_TOOL_DENYLIST`]).
pub fn fork_registry(parent: &ToolRegistry) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for spec in parent.specs() {
        let name = spec.function.name.as_str();
        if FORK_TOOL_DENYLIST.contains(&name) {
            continue;
        }
        if let Some(tool) = parent.get(name) {
            registry.register(Arc::clone(tool));
        }
    }
    registry
}

/// Config used by every `/fork` run: general-purpose worker, full remaining
/// tool set, no step ceiling.
pub fn fork_config() -> SubagentConfig {
    SubagentConfig {
        name: FORK_NAME.to_string(),
        description: "User-spawned side quest that inherits the full conversation \
                      context and reports back when finished."
            .to_string(),
        // Unused when `inherited_history` is set — the parent's system prompt
        // already sits at history[0]. Kept as a safe fallback if a caller
        // ever spawns a fork without history.
        system_prompt: "You are a focused fork of Wizard. Complete the given side \
                        quest end-to-end, then reply with a concise final report."
            .to_string(),
        tool_scope: None,
        max_steps: SubagentConfig::default_max_steps(),
    }
}

/// Run a `/fork` side quest: same loop as [`spawn`], but seeded with the
/// parent's conversation and a stripped tool set. Streams progress as
/// `SubagentRun*` events and returns one final report for the parent.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_fork(
    run: u64,
    task: &str,
    history: Vec<ChatMessage>,
    options: &SpawnOptions,
    client: &Arc<dyn LlmProvider>,
    registry: &ToolRegistry,
    hooks: &Arc<HookEngine>,
    ctx: &ToolContext,
) -> Result<SubagentResult> {
    let config = fork_config();
    let mut options = options.clone();
    options.inherited_history = Some(history);
    // Forks always scope down from a denylisted snapshot so a parent that
    // still has `spawn_subagent` registered cannot recurse through the fork.
    let scoped = fork_registry(registry);
    spawn(run, &config, task, &options, client, &scoped, hooks, ctx).await
}

/// The run's event channel: where a subagent's progress (and its spend) is
/// streamed for the surface to render as its pane.
type Progress = tokio::sync::mpsc::Sender<crate::agent::AgentEvent>;

/// Run `task` in an isolated context defined by `config`: fresh history,
/// scoped registry, step budget, deadline, and the caller's cancellation.
/// The parent's lifecycle `hooks` apply to the subagent's tool calls too.
///
/// The subagent reports back to the parent model as one tool result, but its
/// step-by-step activity streams to the surface as `AgentEvent::SubagentRun*`
/// events scoped to `run` (see [`next_run_id`]), which the TUI renders as that
/// subagent's own pane. The caller emits `SubagentRunStarted` (it knows the
/// background id); this function emits everything after it, including the
/// terminal `SubagentRunDone`, **on every path**, including the two where the
/// loop never gets to finish, so no pane is ever left sitting at "running".
///
/// # Why the interrupt is here and not around the call
///
/// `biased` checks cancellation before anything else on every poll, so a
/// Ctrl-C is honored mid-stream (the TUI raises the parent's [`CancelHandle`]
/// before it resorts to aborting the turn's task) rather than at the next step
/// boundary. Dropping [`run_loop`]'s future is a clean abort: it holds its own
/// history and nothing else.
///
/// A caller cannot do this for itself correctly. Wrapping the whole thing in
/// its own `select!` was what `/ultra` did, and it left that caller unable to
/// know whether this function had already closed the pane out before its
/// future was dropped, so it guessed, and a second `SubagentRunDone` flips a
/// pane from `Done` to `Failed`. Inside, the question does not arise: the same
/// function that emits the terminal event decides the run is over.
#[allow(clippy::too_many_arguments)]
pub async fn spawn(
    run: u64,
    config: &SubagentConfig,
    task: &str,
    options: &SpawnOptions,
    client: &Arc<dyn LlmProvider>,
    registry: &ToolRegistry,
    hooks: &Arc<HookEngine>,
    ctx: &ToolContext,
) -> Result<SubagentResult> {
    let stop = tokio::select! {
        biased;
        () = cancelled(options.cancel.as_ref()) => SubagentStop::Cancelled,
        () = elapsed(options.deadline) => {
            // `elapsed` only resolves when there *is* a deadline.
            SubagentStop::DeadlineExceeded(options.deadline.unwrap_or_default())
        }
        result = run_loop(run, config, task, options, client, registry, hooks, ctx) => return result,
    };

    // The loop's future has just been dropped mid-run, so it never reached its
    // own terminal event. Close the pane out here or it sits at "running" for
    // the rest of the session. The pane's step count already arrived on
    // `SubagentRunStep`; the loop's own counter went with its dropped future.
    close_pane(&ctx.events, run, 0, &stop.to_string()).await;
    Err(anyhow::Error::new(stop))
}

/// Emit the terminal `SubagentRunDone` for a run that ended badly.
///
/// One function because there are five ways for a run to end early — the
/// interrupt, the deadline, a permanent provider error, an exhausted retry
/// ladder, an open breaker — and a pane left at "running" is indistinguishable
/// from a run still working, so the failure mode of forgetting one is a rail
/// that never empties.
async fn close_pane(events: &Option<Progress>, run: u64, steps_used: u32, error: &str) {
    if let Some(events) = events {
        super::emit(
            events,
            crate::agent::AgentEvent::SubagentRunDone {
                run,
                completed: false,
                output: String::new(),
                steps_used,
                error: Some(error.to_string()),
            },
        )
        .await;
    }
}

/// Resolves when `deadline` elapses, and never when there is none. See
/// [`cancelled`](super::cancelled).
async fn elapsed(deadline: Option<Duration>) {
    match deadline {
        Some(deadline) => tokio::time::sleep(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// One delegated run, as the step loop sees it.
///
/// Everything here that differs from a turn's [`TurnHost`](super::turn) is a
/// thing that genuinely needs the owner of the history: there is no session to
/// persist to, the compactor cuts under a different anchor, and the tokens are
/// billed as delegated. What the run merely *chooses* — what bounds it, which
/// of the turn's gates it keeps — is [`Policy::sub_run`], not code in here.
struct SubRun<'a> {
    client: &'a Arc<dyn LlmProvider>,
    /// The context nested tools run in: the parent's registries, checkpoint
    /// store and image store, with the surface deliberately unwired (see the
    /// construction in [`run_loop`]).
    ctx: ToolContext,
    /// This run's tool pipeline, over its scoped registry. The same
    /// [`Dispatcher`] a turn uses, built for a sub-run.
    dispatcher: Dispatcher,
    history: Vec<ChatMessage>,
    model: String,
    /// Serialized-history ceiling compaction falls back to when the provider
    /// names no context window, carried from the same config key the parent
    /// reads ([`crate::config::Config::compact_threshold_bytes`]).
    byte_threshold: usize,
    /// The sub-loop's own last reported prompt size, which is what decides
    /// when it compacts.
    ///
    /// It cannot come from `ctx.usage`: a subagent's tokens are recorded there
    /// as *delegated* precisely so they never become the parent's
    /// `last_prompt` (see [`Host::record_usage`]), and the parent's window has
    /// nothing to do with how full this run's context is. Behind a lock
    /// because the retry ladder bills an attempt through `&self`.
    last_prompt: std::sync::Mutex<Option<u64>>,
}

impl SubRun<'_> {
    /// Read the reading, tolerating a poisoned lock: a number describing how
    /// full a prompt was is not worth failing a run over.
    fn reading(&self) -> std::sync::MutexGuard<'_, Option<u64>> {
        self.last_prompt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl Host for SubRun<'_> {
    fn client(&self) -> &Arc<dyn LlmProvider> {
        self.client
    }

    fn ctx(&self) -> &ToolContext {
        &self.ctx
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.dispatcher.registry().specs()
    }

    fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    fn history_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.history
    }

    /// In memory and nowhere else: a sub-run has no session file, and its
    /// record is the one report it hands back. That is the whole economy of
    /// delegation — a multi-step sub-task costs the parent one turn of context.
    fn push(&mut self, message: ChatMessage) {
        self.history.push(message);
    }

    fn last_prompt(&self) -> Option<u64> {
        *self.reading()
    }

    /// A sub-run's tokens are the parent's — one bill, one status bar, and a
    /// council makes N candidate runs the price of every turn — but they are
    /// recorded as *delegated*: they belong on the totals and must never
    /// become the parent's `last_prompt`, which is what decides when the
    /// parent compacts. This run's own reading is kept here instead.
    async fn record_usage(&self, usage: &StepUsage, sink: &Sink) {
        if !usage.reported() {
            return;
        }
        let prompt = usage.prompt.unwrap_or(0);
        let completion = usage.completion.unwrap_or(0);
        if let Some(tracker) = &self.ctx.usage {
            tracker.record_delegated(prompt, completion);
            tracker.record_cache(usage.cache.read, usage.cache.write);
        }
        if usage.prompt.is_some() {
            *self.reading() = usage.prompt;
        }
        sink.usage(prompt, completion).await;
    }

    /// The same cut a turn makes, under [`context::Anchor::SubLoop`] — a
    /// sub-loop's history has exactly one user message, so the conversation
    /// anchor would walk the boundary all the way back onto it and find
    /// nothing it was allowed to cut, and a run that outgrew its window simply
    /// failed.
    ///
    /// The pass is logged rather than announced. There is no run-scoped event
    /// for "the context was managed", and borrowing one that renders as the
    /// subagent's own words would put a sentence it never said into the
    /// transcript the council reads back — which is exactly what [`Sink`] does
    /// with a notice on this arm.
    async fn compact(&mut self, sink: &Sink) {
        let budget = context::Budget {
            window: self.client.context_window(&self.model).await,
            byte_threshold: self.byte_threshold,
        };
        let compacted = context::compact(
            &mut self.history,
            context::Anchor::SubLoop,
            budget,
            self.client,
            &self.model,
        )
        .await;
        // The summarizer's tokens are the parent's, like every other token a
        // sub-run spends: delegated, so they land on the totals without ever
        // becoming anyone's `last_prompt`. A sub-run has no usage log of its
        // own, so this rides the parent turn's record rather than writing a
        // line the way `Agent::compact_now` does.
        if compacted.usage.reported() {
            if let Some(tracker) = &self.ctx.usage {
                tracker.record_delegated(compacted.usage.prompt, compacted.usage.completion);
                tracker.record_cache(compacted.usage.cache_read, compacted.usage.cache_write);
            }
            sink.usage(compacted.usage.prompt, compacted.usage.completion)
                .await;
        }
        if compacted.outcome == context::CompactOutcome::Nothing {
            return;
        }
        // The history just shrank: the last reported prompt size describes a
        // prompt that no longer exists and must not re-trigger a pass with
        // nothing left to cut.
        *self.reading() = None;
        sink.notice(compacted.outcome.describe()).await;
    }

    async fn dispatch(&mut self, call: &ToolCall, sink: &Sink) -> DispatchOutcome {
        self.dispatcher.dispatch(call, &self.ctx, sink).await
    }

    /// Nothing. The one tool a turn answers itself is `compact`, which is
    /// parent-loop only: it is not in a sub-run's scope, and a fork's denylist
    /// strips it outright.
    async fn intercept(&mut self, _call: &ToolCall, _sink: &Sink) -> Option<CallOutcome> {
        None
    }
}

/// The sub-loop proper: everything [`spawn`] does once it is running, minus
/// the interrupt and the deadline that can end it early.
///
/// Which is to say: the setup, [`turn::run`], and the report. The loop itself
/// is the parent's, and has been since criterion 6 — see [`super::turn`].
#[allow(clippy::too_many_arguments)]
async fn run_loop(
    run: u64,
    config: &SubagentConfig,
    task: &str,
    options: &SpawnOptions,
    client: &Arc<dyn LlmProvider>,
    registry: &ToolRegistry,
    hooks: &Arc<HookEngine>,
    ctx: &ToolContext,
) -> Result<SubagentResult> {
    let loaded = Config::load().unwrap_or_default();
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| loaded.active().model);
    let mut scoped = scoped_registry(registry, config.tool_scope.as_deref());
    if options.read_only {
        scoped = read_only_registry(&scoped);
    }
    let native_tools = crate::llm::provider::probe_native_tools(client.as_ref(), &model).await;

    let history = match &options.inherited_history {
        // `/fork`: seed from the parent's conversation, then append the
        // side-quest brief. The parent's system prompt (and any mid-session
        // notes) stay at the front; we only add the fork instruction + task.
        Some(parent_history) => {
            let mut history = parent_history.clone();
            // When the parent is on the JSON tool protocol, refresh the tool
            // list against *this* run's scoped registry so the fork doesn't
            // advertise tools we stripped (spawn_subagent, exit_plan, …).
            if !native_tools
                && let Some(system) = history.first_mut()
                && system.role == Role::System
            {
                let protocol = prompts::render_tool_protocol(&scoped.specs());
                system
                    .content
                    .push(crate::llm::ContentBlock::text(format!("\n\n{protocol}")));
            }
            history.push(ChatMessage::user(format!("{FORK_BRIEF}\n\n{task}")));
            history
        }
        None => {
            let mut system_prompt = config.system_prompt.clone();
            if !native_tools {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&prompts::render_tool_protocol(&scoped.specs()));
            }
            vec![
                ChatMessage::system(system_prompt),
                ChatMessage::user(task.to_string()),
            ]
        }
    };

    // The subagent reports back to the parent model as one tool result, but its
    // step-by-step activity streams to the surface as run-scoped events so the
    // user can open its pane and watch it work. Nested tools run with
    // `events: None` so they don't double-emit (todos, background tasks) or
    // leak into the parent's transcript; the run's own pair goes out on the
    // [`Sink`] below.
    let progress = ctx.events.clone();
    // Forks keep the parent's todo list (shared work, shared status bar);
    // ordinary subagents get a fresh one so their scratch todos never leak.
    let todos = if options.inherited_history.is_some() {
        Arc::clone(&ctx.todos)
    } else {
        Arc::new(std::sync::Mutex::new(crate::tools::todo::TodoList::new()))
    };
    let ctx = ToolContext {
        todos,
        events: None,
        // A subagent has no surface to drive; it must never dispatch the
        // parent's slash commands even if the parent's ctx enabled it.
        command_dispatch: CommandDispatch::None,
        // Nor does it have a human. A subagent's `execute` must keep
        // `/dev/null` on fd 0: its prompt would be announced on a stream the
        // parent's composer is not bound to, and the only party in a position
        // to answer it would be the model that asked. `events: None` above
        // already means nothing is announced; this says why, and holds even if
        // a later change starts forwarding a subagent's raw events.
        console: crate::tools::ConsoleAccess::None,
        ..ctx.clone()
    };

    let sink = Sink::Run {
        run,
        name: config.name.clone(),
        events: progress.clone(),
    };
    let policy = Policy::sub_run(
        config.max_steps.last_step(),
        model.clone(),
        native_tools,
        loaded
            .reasoning_effort
            .map(|effort| effort.as_str().to_string()),
        // The parent's breaker when it shared one: an outage it already hit is
        // one this run does not have to prove again.
        options.breaker.clone(),
        loaded.retry_base_secs,
        loaded.retry_max_secs,
        loaded.compact_threshold_bytes,
    );
    let mut host = SubRun {
        client,
        dispatcher: Dispatcher::sub_run(scoped, Arc::clone(hooks)),
        ctx,
        history,
        model,
        byte_threshold: loaded.compact_threshold_bytes,
        last_prompt: std::sync::Mutex::new(None),
    };

    let ran = match turn::run(&mut host, &policy, &sink).await {
        Ok(ran) => ran,
        Err(err) => {
            let err = err.context(format!("subagent '{}' chat failed", config.name));
            // Close the pane out, or it sits at "running" forever. The step
            // count the pane shows already arrived on `SubagentRunStep`; the
            // loop's own counter went with the error.
            close_pane(&progress, run, 0, &format!("{err:#}")).await;
            return Err(err);
        }
    };
    // Unreachable in practice — this run's policy carries no interrupt, because
    // `spawn` races the whole loop against one — but a `spawn` that grows a
    // per-step interrupt later must not silently turn it into a completed run.
    if ran.reason == crate::agent::DoneReason::Stopped {
        close_pane(
            &progress,
            run,
            ran.steps_used,
            &SubagentStop::Cancelled.to_string(),
        )
        .await;
        return Err(anyhow::Error::new(SubagentStop::Cancelled));
    }

    let completed = ran.reason == crate::agent::DoneReason::Completed;
    let output = if ran.last_text.trim().is_empty() {
        NO_FINAL_TEXT.to_string()
    } else {
        ran.last_text
    };
    if let Some(events) = &progress {
        super::emit(
            events,
            crate::agent::AgentEvent::SubagentRunDone {
                run,
                completed,
                output: output.clone(),
                steps_used: ran.steps_used,
                error: None,
            },
        )
        .await;
    }

    Ok(SubagentResult {
        name: config.name.clone(),
        output,
        steps_used: ran.steps_used,
        completed,
    })
}

/// `spawn_subagent` — the tool the parent model calls to fan out work.
pub struct SpawnSubagentTool {
    /// Available subagent definitions, by name.
    pub configs: Vec<SubagentConfig>,
    /// Model client shared with the parent loop.
    client: Arc<dyn LlmProvider>,
    /// Parent tool set subagents scope down from. Built without the spawn
    /// tool itself, so subagents cannot recurse.
    registry: Arc<ToolRegistry>,
    /// The parent's lifecycle hooks, applied to subagent tool calls too.
    hooks: Arc<HookEngine>,
    /// Tool description, including the roster of available subagents.
    description: String,
    /// The parent's live model and cancel handle (bound via
    /// [`Self::model_handle`] + `Agent::bind_subagent_model`). Unbound, runs
    /// read the configured model and cannot be interrupted.
    binding: SharedActiveModel,
}

impl SpawnSubagentTool {
    pub fn new(
        configs: Vec<SubagentConfig>,
        client: Arc<dyn LlmProvider>,
        registry: Arc<ToolRegistry>,
        hooks: Arc<HookEngine>,
    ) -> Self {
        let roster = configs
            .iter()
            .map(|c| {
                let scope = match &c.tool_scope {
                    None => "all tools".to_string(),
                    Some(names) => names.join(", "),
                };
                format!(
                    "\n  - `{}` — {} (tools: {}; {})",
                    c.name, c.description, scope, c.max_steps
                )
            })
            .collect::<String>();
        let description = format!(
            "Delegate a self-contained sub-task to an isolated subagent. It runs its own \
             loop with a fresh context and scoped tools, then returns one final report — \
             intermediate steps never enter your context.\n\n\
             Default to `background: true` (returns immediately; progress streams; report \
             lands when done). Use synchronous only when you need the report in this same \
             turn. Don't delegate trivial one-tool actions, work that needs the user \
             mid-flight, or a task you can't fully describe.\n\n\
             `task` is the ONLY context the subagent gets besides its own prompt — goal, \
             paths, constraints, and exactly what to report back. One well-scoped task \
             beats a chain of follow-ups.\n\n\
             Available subagents:{roster}"
        );
        Self {
            configs,
            client,
            registry,
            hooks,
            description,
            binding: Arc::new(SubagentBinding::default()),
        }
    }

    /// Handle the parent agent binds (see `Agent::bind_subagent_model`) so
    /// mid-session `/model` switches and user interrupts reach subagent runs.
    /// Unbound, runs fall back to the configured active model and cannot be
    /// cancelled.
    pub fn model_handle(&self) -> SharedActiveModel {
        Arc::clone(&self.binding)
    }

    fn active_model(&self) -> Option<String> {
        self.binding.model()
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subagent": { "type": "string", "description": "Name of the subagent to use" },
                "task": { "type": "string", "description": "Self-contained task description with all needed context" },
                "background": {
                    "type": "boolean",
                    "description": "Run detached and return immediately instead of waiting for \
                        the report. Default false. Set true for self-contained, non-blocking \
                        delegation — the common case — so the user isn't stuck waiting on you."
                }
            },
            "required": ["subagent", "task"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            subagent: String,
            task: String,
            #[serde(default)]
            background: bool,
            /// Injected by the dispatcher while plan mode is on (not
            /// advertised in the schema): the subagent runs read-only.
            #[serde(default)]
            plan_mode: bool,
        }
        let args: Args = serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs {
            tool: SPAWN_SUBAGENT_TOOL_NAME.to_string(),
            message: err.to_string(),
        })?;
        // A foreground run happens *inside* the parent's turn, so Esc must end
        // it; a background one outlives that turn on purpose and is killed
        // through the subagent registry instead. Deciding here rather than in
        // `spawn` is what keeps that distinction visible at the place it is
        // actually made.
        let options = SpawnOptions {
            model: self.active_model(),
            read_only: args.plan_mode,
            cancel: if args.background {
                None
            } else {
                self.binding.cancel()
            },
            // The breaker, unlike the cancel handle, is shared by background
            // runs too: detaching a run from the *turn* is not detaching it
            // from the endpoint, and an outage is an outage whoever noticed.
            breaker: self.binding.breaker().unwrap_or_default(),
            ..Default::default()
        };

        let config = self
            .configs
            .iter()
            .find(|c| c.name == args.subagent)
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: SPAWN_SUBAGENT_TOOL_NAME.to_string(),
                message: format!(
                    "unknown subagent '{}'; available: {}",
                    args.subagent,
                    self.configs
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })?;

        let run = next_run_id();

        if args.background {
            let name = config.name.clone();
            let config = config.clone();
            let task = args.task.clone();
            let client = Arc::clone(&self.client);
            let registry = Arc::clone(&self.registry);
            let hooks = Arc::clone(&self.hooks);
            let fut_ctx = ctx.clone();
            let fut_options = options.clone();
            let fut = async move {
                match spawn(
                    run,
                    &config,
                    &task,
                    &fut_options,
                    &client,
                    &registry,
                    &hooks,
                    &fut_ctx,
                )
                .await
                {
                    Ok(result) => SubagentRunResult {
                        completed: result.completed,
                        output: result.output,
                        steps_used: result.steps_used,
                        error: None,
                    },
                    Err(err) => SubagentRunResult {
                        completed: false,
                        output: format!("subagent failed: {err:#}"),
                        steps_used: 0,
                        error: Some(format!("{err:#}")),
                    },
                }
            };
            // Reserve the id and announce the run *before* attaching the
            // driver, so the pane exists by the time the subagent's first
            // event lands in it.
            let id = ctx.subagents.reserve(&name, &args.task);
            if let Some(events) = &ctx.events {
                super::emit(
                    events,
                    crate::agent::AgentEvent::SubagentRunStarted {
                        run,
                        bg: Some(id),
                        name: name.clone(),
                        task: args.task.clone(),
                    },
                )
                .await;
                super::emit(
                    events,
                    crate::agent::AgentEvent::SubagentStarted {
                        id,
                        name: name.clone(),
                        task: args.task.clone(),
                    },
                )
                .await;
            }
            ctx.subagents.attach(id, fut);
            return Ok(ToolOutput::ok(format!(
                "Delegated to subagent '{name}' (#{id}): {}.\nRunning in the background — \
                 you'll see its progress as it works, and the report lands in your context \
                 once it's done.",
                args.task
            )));
        }

        if let Some(events) = &ctx.events {
            super::emit(
                events,
                crate::agent::AgentEvent::SubagentRunStarted {
                    run,
                    bg: None,
                    name: config.name.clone(),
                    task: args.task.clone(),
                },
            )
            .await;
        }

        let result = spawn(
            run,
            config,
            &args.task,
            &options,
            &self.client,
            &self.registry,
            &self.hooks,
            ctx,
        )
        .await
        .map_err(|err| ToolError::Execution {
            tool: SPAWN_SUBAGENT_TOOL_NAME.to_string(),
            source: err,
        })?;

        let summary = format!(
            "Subagent '{}' {} after {} step(s).\n\n{}",
            result.name,
            if result.completed {
                "completed"
            } else {
                "hit its step budget"
            },
            result.steps_used,
            result.output
        );
        Ok(if result.completed {
            ToolOutput::ok(summary)
        } else {
            ToolOutput::error(summary)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use futures_util::stream;

    use super::*;
    use crate::llm::{CacheTokens, ChatChunk, ChatRequest, ChatStream, Image};

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Provider that replays canned chunk sequences (or scripted failures)
    /// and records the requests it received.
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<ChatChunk>>>,
        requests: Mutex<Vec<ChatRequest>>,
        /// Upcoming chat_stream calls that fail with `fail_status` before the
        /// scripted responses resume; `u32::MAX` fails every call.
        fail: Mutex<u32>,
        fail_status: u16,
        /// `Retry-After` the scripted failures carry, as a real provider
        /// attaches it (under the `ProviderError`, not on it).
        fail_retry_after: Option<std::time::Duration>,
        /// What `supports_native_tools` reports.
        native_tools: bool,
        /// What `context_window` reports, which is what the pressure bands —
        /// and therefore compaction — are measured against.
        window: Option<u32>,
        /// Leading calls that hang forever before the scripted responses
        /// start, so a test can interrupt a run mid-call and still have the
        /// provider answer the *next* one.
        stall: Mutex<u32>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Self::build(responses, 0, 0, true)
        }

        fn failing(status: u16) -> Arc<Self> {
            Self::build(Vec::new(), u32::MAX, status, true)
        }

        fn flaky(status: u16, failures: u32, responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Self::build(responses, failures, status, true)
        }

        fn without_native_tools(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Self::build(responses, 0, 0, false)
        }

        fn build(
            responses: Vec<Vec<ChatChunk>>,
            fail: u32,
            fail_status: u16,
            native_tools: bool,
        ) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                fail: Mutex::new(fail),
                fail_status,
                fail_retry_after: None,
                native_tools,
                window: None,
                stall: Mutex::new(0),
            })
        }

        /// Serves `responses` under a provider that reports `window` tokens of
        /// context, which is the only way a sub-run can know it is too full.
        fn windowed(window: u32, responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            let mut built = Self::build(responses, 0, 0, true);
            Arc::get_mut(&mut built).expect("sole owner").window = Some(window);
            built
        }

        /// Hangs on the first `calls` requests, then serves `responses`.
        fn stalling(calls: u32, responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            let built = Self::build(responses, 0, 0, true);
            *built.stall.lock().unwrap() = calls;
            built
        }

        /// Fails `failures` times with `status` **and** a server-stated
        /// `Retry-After`, then serves `responses`.
        fn rate_limited(
            failures: u32,
            retry_after: std::time::Duration,
            responses: Vec<Vec<ChatChunk>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                fail: Mutex::new(failures),
                fail_status: 429,
                fail_retry_after: Some(retry_after),
                native_tools: true,
                window: None,
                stall: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(self.native_tools)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
            self.requests.lock().unwrap().push(request);
            {
                let stall = {
                    let mut stall = self.stall.lock().unwrap();
                    let hang = *stall > 0;
                    *stall = stall.saturating_sub(1);
                    hang
                };
                if stall {
                    // Longer than any test's patience: a stalled run must end
                    // on its own terms, never on the provider relenting.
                    tokio::time::sleep(Duration::from_secs(3_600)).await;
                    unreachable!("the stall outlasts the test");
                }
            }
            {
                let mut fail = self.fail.lock().unwrap();
                if *fail > 0 {
                    if *fail != u32::MAX {
                        *fail -= 1;
                    }
                    return Err(crate::llm::http_error_with_retry_after(
                        self.fail_status,
                        "scripted failure",
                        self.fail_retry_after,
                    ));
                }
            }
            let chunks = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response available");
            Ok(futures_util::StreamExt::boxed(stream::iter(
                chunks.into_iter().map(Ok),
            )))
        }

        async fn context_window(&self, _model: &str) -> Option<u32> {
            self.window
        }

        fn label(&self) -> String {
            "scripted:test".to_string()
        }
    }

    fn chunk(content: &str, thinking: bool, done: bool) -> ChatChunk {
        ChatChunk {
            message: Some(ChatMessage::assistant(content)),
            images: Vec::new(),
            thinking,
            done,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }
    }

    /// Minimal tool with a configurable access class.
    struct FakeTool {
        name: &'static str,
        access: ToolAccess,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "fake tool for subagent tests"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        fn access(&self) -> ToolAccess {
            self.access
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("ok"))
        }
    }

    fn worker() -> SubagentConfig {
        builtin_configs()
            .into_iter()
            .next()
            .expect("builtin worker")
    }

    #[test]
    fn read_only_registry_keeps_only_read_only_tools() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: "mutate",
            access: ToolAccess::Edit,
        }));
        parent.register(Arc::new(FakeTool {
            name: "run",
            access: ToolAccess::Execute,
        }));

        let filtered = read_only_registry(&parent);
        assert!(filtered.get("probe").is_some());
        assert!(filtered.get("mutate").is_none());
        assert!(filtered.get("run").is_none());
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn scoped_registry_selects_named_tools_and_skips_unknown() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: "run",
            access: ToolAccess::Execute,
        }));

        let all = scoped_registry(&parent, None);
        assert_eq!(all.len(), 2);
        let scoped = scoped_registry(&parent, Some(&["probe".to_string(), "missing".to_string()]));
        assert!(scoped.get("probe").is_some());
        assert!(scoped.get("missing").is_none());
        assert_eq!(scoped.len(), 1);
    }

    #[test]
    fn user_configs_shadow_builtins_by_name() {
        let tmp = TempDir::new();
        std::fs::write(
            tmp.0.join("worker.toml"),
            "name = \"worker\"\ndescription = \"custom\"\nsystem_prompt = \"be custom\"\n",
        )
        .unwrap();
        let configs = available_configs(&tmp.0);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].description, "custom");
    }

    /// The spawn tool reads the parent's live model out of the slot handed to
    /// `Agent::bind_subagent_model`. A surface that builds the tool and drops
    /// the handle strands its subagents on the *configured* model, silently
    /// ignoring `/model` — which is what the TUI did until its registry was
    /// made to hand the handle back.
    #[tokio::test]
    async fn a_bound_model_handle_is_what_subagents_run_on() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![vec![chunk("done", false, true)]]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let tool = SpawnSubagentTool::new(
            vec![worker()],
            Arc::clone(&client),
            Arc::new(ToolRegistry::new()),
            Arc::new(HookEngine::new(Vec::new(), tmp.0.clone(), "test".into())),
        );

        // Nothing bound: the slot is empty and the sub-loop falls back to the
        // configured model.
        assert!(tool.active_model().is_none());

        // Bound, then written through by a `/model` switch.
        let handle = tool.model_handle();
        handle.set_model("switched-model".to_string());

        tool.execute(
            serde_json::json!({ "subagent": worker().name, "task": "report" }),
            &ToolContext::new(&tmp.0),
        )
        .await
        .expect("spawn ok");

        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests[0].model, "switched-model",
            "the subagent ran on the parent's switched model, not the configured one"
        );
    }

    #[tokio::test]
    async fn spawn_skips_thinking_chunks_and_uses_the_model_override() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![vec![
            chunk("secret reasoning", true, false),
            chunk("the actual report", false, true),
        ]]);
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            "test".to_string(),
        ));
        let ctx = ToolContext::new(&tmp.0);
        let client: Arc<dyn LlmProvider> = provider.clone();

        let options = SpawnOptions {
            model: Some("parent-active-model".to_string()),
            read_only: false,
            ..Default::default()
        };
        let result = spawn(
            next_run_id(),
            &worker(),
            "report",
            &options,
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");

        assert!(result.completed);
        assert_eq!(result.output, "the actual report", "thinking never leaks");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].model, "parent-active-model");
    }

    #[tokio::test]
    async fn images_inside_a_subagent_run_are_persisted_and_announced_on_the_run() {
        // A tool inside a run returns an image: it must reach the subagent's
        // model (following user message), land in the session's image store,
        // and be announced on the run's own events — not lost between panes.
        struct ShotTool;
        #[async_trait]
        impl Tool for ShotTool {
            fn name(&self) -> &str {
                "generate_image"
            }
            fn description(&self) -> &str {
                "Generate an image."
            }
            fn parameters(&self) -> Value {
                json!({ "type": "object", "properties": {} })
            }
            async fn execute(
                &self,
                _args: Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
                bytes.extend_from_slice(b"pixels");
                Ok(ToolOutput::ok_with_images(
                    "rendered",
                    vec![Image::from_bytes(&bytes).expect("a PNG")],
                ))
            }
        }

        let tmp = TempDir::new();
        let mut call = ChatMessage::assistant("");
        call.push_tool_call(ToolCall::new("generate_image".to_string(), json!({})));
        let provider = ScriptedProvider::new(vec![
            vec![ChatChunk {
                message: Some(call),
                images: Vec::new(),
                thinking: false,
                done: true,
                done_reason: None,
                eval_count: None,
                prompt_eval_count: None,
                cache: CacheTokens::NONE,
            }],
            vec![chunk("done", false, true)],
        ]);
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            "test".to_string(),
        ));
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0)
            .with_images(Arc::new(crate::images::ImageStore::in_dir(
                tmp.0.join("images"),
            )))
            .with_events(tx);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ShotTool));

        let run = next_run_id();
        let result = spawn(
            run,
            &worker(),
            "make a picture",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert!(result.completed);

        // The image reached the subagent's model on a following user message.
        let second = provider.requests.lock().unwrap()[1].messages.clone();
        let carried = second
            .iter()
            .find(|message| !message.images().is_empty())
            .expect("a message carrying the image");
        assert_eq!(carried.role, crate::llm::Role::User);
        assert_eq!(carried.images()[0].mime, "image/png");

        // And it was announced on this run, with a path on disk.
        let mut announced = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::agent::AgentEvent::SubagentRunImages {
                run: id,
                source,
                images,
            } = event
            {
                assert_eq!(id, run, "scoped to the run that produced it");
                announced.push((source, images));
            }
        }
        assert_eq!(announced.len(), 1);
        assert_eq!(
            announced[0].0,
            crate::agent::ImageSource::Tool("generate_image".to_string())
        );
        assert!(announced[0].1[0].path.is_file(), "written to disk");
    }

    #[tokio::test]
    async fn a_subagents_tokens_bill_the_parent_and_reach_the_surface() {
        let tmp = TempDir::new();
        // Two model calls: one that asks for a tool, then the report. Both
        // report counts, and both have to be accounted for — an ultra turn is
        // N of these runs, and the status bar shows one number.
        //
        // Both also report a cache split, which is the shape that actually
        // occurs: the second call re-sends the first one's prefix, so a
        // subagent's steps are cache hits almost by construction, and `/ultra`
        // multiplies that by the size of its roster. A run whose split never
        // reaches the parent bills that prefix at the full input rate every
        // time.
        let provider = ScriptedProvider::new(vec![
            vec![ChatChunk {
                message: Some(ChatMessage::assistant_turn(
                    "",
                    Vec::new(),
                    vec![ToolCall::new("probe".to_string(), json!({}))],
                )),
                prompt_eval_count: Some(100),
                cache: CacheTokens { read: 0, write: 80 },
                eval_count: Some(20),
                ..chunk("", false, true)
            }],
            vec![ChatChunk {
                prompt_eval_count: Some(300),
                cache: CacheTokens {
                    read: 240,
                    write: 0,
                },
                eval_count: Some(40),
                ..chunk("the report", false, true)
            }],
        ]);
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            "test".to_string(),
        ));
        let usage = Arc::new(crate::usage::UsageTracker::new());
        let (events, mut drain) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0)
            .with_usage(Arc::clone(&usage))
            .with_events(events);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));

        let result = spawn(
            next_run_id(),
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert_eq!(result.output, "the report");

        assert_eq!(
            usage.session_totals(),
            (400, 60),
            "the parent paid for both of the subagent's model calls, so both land on its totals \
             (and therefore in /cost)"
        );
        assert_eq!(
            usage.last_prompt_tokens(),
            None,
            "but never on last_prompt: that is the parent's own prompt size, and it decides when \
             to compact"
        );
        assert_eq!(
            usage.session_cache_totals(),
            (240, 80),
            "the cache split is delegated the same way the counts are: it lands on the parent's \
             record, which is where the turn is priced. Without it the 240 cached tokens bill at \
             the full input rate and the saving disappears from the cost column"
        );

        // What that is worth. The second figure is the same turn with the
        // split dropped, which is what this priced before the threading.
        let (prompt, completion) = usage.session_totals();
        let (cache_read, cache_write) = usage.session_cache_totals();
        let inputs = crate::usage::PriceInputs {
            model: "claude-opus-5",
            endpoint: "https://api.anthropic.com",
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
            self_hosted: false,
        };
        let billed = crate::usage::estimate_cost(
            crate::usage::TurnTokens {
                prompt,
                completion,
                cache_read,
                cache_write,
            },
            &inputs,
        );
        let as_all_fresh = crate::usage::estimate_cost(
            crate::usage::TurnTokens {
                prompt,
                completion,
                cache_read: 0,
                cache_write: 0,
            },
            &inputs,
        );
        assert!(
            billed.usd < as_all_fresh.usd,
            "a delegated cache hit has to move the number it is supposed to move: \
             {billed:?} vs {as_all_fresh:?}"
        );

        let mut reported = Vec::new();
        while let Ok(event) = drain.try_recv() {
            if let crate::agent::AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } = event
            {
                reported.push((prompt_tokens, completion_tokens));
            }
        }
        assert_eq!(
            reported,
            [(100, 20), (300, 40)],
            "one Usage event per model call, so the status bar counts the fan-out it advertises"
        );
    }

    /// A subagent runs the same models through the same providers as its
    /// parent, so it cannot answer a parallel batch in a shape Anthropic
    /// rejects either: all of a turn's results go back on ONE message, and
    /// the images a tool returned ride after the whole batch rather than
    /// between two results.
    #[tokio::test]
    async fn a_subagent_answers_a_parallel_batch_on_one_message() {
        /// First call of the batch, so a per-call image push would land its
        /// user message in the middle of the results.
        struct ShotTool;
        #[async_trait]
        impl Tool for ShotTool {
            fn name(&self) -> &str {
                "generate_image"
            }
            fn description(&self) -> &str {
                "Generate an image."
            }
            fn parameters(&self) -> Value {
                json!({ "type": "object", "properties": {} })
            }
            async fn execute(
                &self,
                _args: Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
                bytes.extend_from_slice(b"pixels");
                Ok(ToolOutput::ok_with_images(
                    "rendered",
                    vec![Image::from_bytes(&bytes).expect("a PNG")],
                ))
            }
        }

        let tmp = TempDir::new();
        let mut batch = ChatMessage::assistant("");
        batch.push_tool_call(ToolCall::new("generate_image", json!({})));
        batch.push_tool_call(ToolCall::new("probe", json!({ "n": 2 })));
        let ids: Vec<String> = batch
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        let provider = ScriptedProvider::new(vec![
            vec![ChatChunk {
                message: Some(batch),
                ..chunk("", false, true)
            }],
            vec![chunk("the report", false, true)],
        ]);
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            "test".to_string(),
        ));
        let ctx = ToolContext::new(&tmp.0).with_images(Arc::new(
            crate::images::ImageStore::in_dir(tmp.0.join("images")),
        ));
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ShotTool));
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));

        spawn(
            next_run_id(),
            &worker(),
            "draw and probe",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");

        // The second request carries the answered batch.
        let second = provider.requests.lock().unwrap()[1].messages.clone();
        let answered = second
            .iter()
            .position(|message| message.role == Role::Tool)
            .expect("the batch was answered");
        assert_eq!(
            second
                .iter()
                .filter(|message| message.role == Role::Tool)
                .count(),
            1,
            "one message answers the whole batch"
        );
        let blocks = second[answered].tool_results();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].tool_use_id, ids[0]);
        assert_eq!(blocks[1].tool_use_id, ids[1]);

        // The images payload follows the batch, never splits it.
        let follow_up = &second[answered + 1];
        assert_eq!(follow_up.role, Role::User);
        assert!(follow_up.text().contains("generate_image"));
        assert_eq!(follow_up.images().len(), 1);
    }

    #[tokio::test]
    async fn spawn_fails_fast_on_permanent_provider_errors() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::failing(401);
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            "test".to_string(),
        ));
        let ctx = ToolContext::new(&tmp.0);
        let client: Arc<dyn LlmProvider> = provider.clone();

        let err = spawn(
            next_run_id(),
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect_err("permanent error fails the run");
        assert!(format!("{err:#}").contains("scripted failure"), "{err:#}");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "a 401 is never retried"
        );
    }

    fn test_hooks(tmp: &TempDir) -> Arc<HookEngine> {
        Arc::new(HookEngine::new(Vec::new(), tmp.0.clone(), "test".into()))
    }

    /// A tool that counts how often it was actually dispatched.
    struct CountingTool(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "probe"
        }

        fn description(&self) -> &str {
            "counts its dispatches"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": { "path": { "type": "string" } } })
        }

        fn access(&self) -> ToolAccess {
            ToolAccess::ReadOnly
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput::ok("ok"))
        }
    }

    #[tokio::test]
    async fn a_truncated_tool_call_is_refused_by_a_subagent_too() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // What a provider emits when the output-token ceiling lands in the
        // middle of the *second* call's arguments: both names made it out,
        // the truncated arguments decoded to `{}`, and the finish reason says
        // why. The parent turn refuses this batch whole; a subagent running
        // the same dispatcher and the same hook pipeline must too, and it
        // must refuse the complete first call along with it, or the model's
        // own message is left half-answered.
        let mut message = ChatMessage::assistant("");
        for _ in 0..2 {
            message.push_tool_call(ToolCall::new("probe".to_string(), json!({})));
        }
        let provider = ScriptedProvider::new(vec![vec![ChatChunk {
            message: Some(message),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: Some("length".to_string()),
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }]]);
        let client: Arc<dyn LlmProvider> = provider.clone();

        let tmp = TempDir::new();
        let dispatches = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(CountingTool(Arc::clone(&dispatches))));
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);
        let run = next_run_id();

        let err = spawn(
            run,
            &worker(),
            "delete the temp files",
            &SpawnOptions::default(),
            &client,
            &registry,
            &test_hooks(&tmp),
            &ctx,
        )
        .await
        .expect_err("a truncated tool call fails the run");
        let chain = format!("{err:#}");
        assert!(chain.contains("output-token limit"), "got: {chain}");
        assert!(chain.contains("probe"), "got: {chain}");

        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            0,
            "neither call in the batch runs with the arguments that survived truncation"
        );
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "a truncated reply is not an outage: retrying re-bills the prompt for the same cut"
        );

        // The pane is closed out rather than left at "running" forever.
        let mut done = None;
        while let Ok(event) = rx.try_recv() {
            if let crate::agent::AgentEvent::SubagentRunDone { run: id, error, .. } = event {
                done = Some((id, error));
            }
        }
        let (id, error) = done.expect("the pane is closed out");
        assert_eq!(id, run);
        assert!(
            error
                .expect("error carried on the terminal event")
                .contains("output-token limit")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_subagent_waits_the_server_stated_retry_after() {
        // `/ultra` fans N candidates at one endpoint; on a shared 429 they
        // used to sleep an identical `retry_base * 2^attempt` and re-storm it
        // in lockstep, ignoring the deadline the endpoint had just named. The
        // shipped ladder's first rung is 5s, so a wait past 30s can only have
        // come from the header.
        let tmp = TempDir::new();
        let provider = ScriptedProvider::rate_limited(
            1,
            std::time::Duration::from_secs(30),
            vec![vec![chunk("recovered", false, true)]],
        );
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let ctx = ToolContext::new(&tmp.0);

        let started = tokio::time::Instant::now();
        let result = spawn(
            next_run_id(),
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect("the run recovers after honouring the wait");
        let waited = started.elapsed();

        assert!(result.completed);
        assert_eq!(result.output, "recovered");
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert!(
            waited >= std::time::Duration::from_secs(30),
            "the subagent retried before the server's own deadline: {waited:?}"
        );
    }

    /// `done: true` chunk carrying one tool call alongside `content`.
    fn tool_call_chunk(name: &str, content: &str) -> ChatChunk {
        let mut message = ChatMessage::assistant(content);
        message.push_tool_call(ToolCall::new(name.to_string(), json!({})));
        ChatChunk {
            message: Some(message),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
            cache: CacheTokens::NONE,
        }
    }

    #[test]
    fn invalid_manifests_are_skipped_and_the_rest_load() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("bad.toml"), "name = \"broken").unwrap();
        std::fs::write(
            tmp.0.join("good.toml"),
            "name = \"helper\"\ndescription = \"d\"\nsystem_prompt = \"p\"\n",
        )
        .unwrap();
        std::fs::write(tmp.0.join("ignored.txt"), "not toml").unwrap();

        let configs = load_dir(&tmp.0).expect("load ok");
        assert_eq!(configs.len(), 1, "the bad manifest costs itself only");
        assert_eq!(configs[0].name, "helper");
        assert_eq!(
            configs[0].max_steps,
            crate::config::StepBudget::new(DEFAULT_MAX_STEPS),
            "an omitted budget is the finite default, never unlimited: a manifest that says \
             nothing must not produce a run nothing can end"
        );
    }

    #[tokio::test]
    async fn unknown_subagent_is_rejected_with_the_roster() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(Vec::new());
        let client: Arc<dyn LlmProvider> = provider.clone();
        let tool = SpawnSubagentTool::new(
            vec![worker()],
            client,
            Arc::new(ToolRegistry::new()),
            test_hooks(&tmp),
        );

        let err = tool
            .execute(
                json!({ "subagent": "nope", "task": "anything" }),
                &ToolContext::new(&tmp.0),
            )
            .await
            .expect_err("unknown name is invalid args");
        let message = err.to_string();
        assert!(message.contains("unknown subagent 'nope'"), "{message}");
        assert!(
            message.contains("worker"),
            "the roster is listed: {message}"
        );
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "no model call for a bad name"
        );
    }

    #[tokio::test]
    async fn a_run_that_exhausts_its_step_budget_reports_an_error_summary() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("probe", "")],
            vec![tool_call_chunk("probe", "still digging")],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        let mut config = worker();
        config.max_steps = crate::config::StepBudget::new(2);
        let tool =
            SpawnSubagentTool::new(vec![config], client, Arc::new(registry), test_hooks(&tmp));

        let output = tool
            .execute(
                json!({ "subagent": "worker", "task": "dig" }),
                &ToolContext::new(&tmp.0),
            )
            .await
            .expect("tool output");
        assert!(output.is_error, "a budget stop is an error result");
        assert!(
            output.content.contains("hit its step budget"),
            "{}",
            output.content
        );
        assert!(output.content.contains("2 step(s)"), "{}", output.content);
        assert!(
            output.content.contains("still digging"),
            "the last text the subagent produced is the report: {}",
            output.content
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transient_errors_back_off_and_retry_until_the_stream_recovers() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::flaky(429, 2, vec![vec![chunk("recovered", false, true)]]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let ctx = ToolContext::new(&tmp.0);

        let result = spawn(
            next_run_id(),
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect("run recovers");
        assert!(result.completed);
        assert_eq!(result.output, "recovered");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            3,
            "two transient failures, then the success"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_budget_bounds_transient_failures_and_closes_the_pane() {
        use crate::agent::AgentEvent;

        let tmp = TempDir::new();
        let provider = ScriptedProvider::failing(503);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        let run = next_run_id();
        let err = spawn(
            run,
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect_err("the budget bounds a persistent outage");
        assert!(format!("{err:#}").contains("chat failed"), "{err:#}");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            (turn::RETRY_ATTEMPTS + 1) as usize,
            "initial attempt plus the retry budget"
        );

        let mut done = None;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::SubagentRunDone {
                run: id,
                completed,
                error,
                ..
            } = event
            {
                done = Some((id, completed, error));
            }
        }
        let (id, completed, error) = done.expect("the pane is closed out");
        assert_eq!(id, run);
        assert!(!completed);
        assert!(
            error
                .expect("error carried on the terminal event")
                .contains("scripted failure")
        );
    }

    #[tokio::test]
    async fn plan_mode_restricts_a_spawned_run_to_read_only_tools() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("mutate", "")],
            vec![chunk("gave up on writing", false, true)],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        registry.register(Arc::new(FakeTool {
            name: "mutate",
            access: ToolAccess::Edit,
        }));
        let tool =
            SpawnSubagentTool::new(vec![worker()], client, Arc::new(registry), test_hooks(&tmp));

        let output = tool
            .execute(
                json!({ "subagent": "worker", "task": "explore", "plan_mode": true }),
                &ToolContext::new(&tmp.0),
            )
            .await
            .expect("tool output");
        assert!(!output.is_error);

        let requests = provider.requests.lock().unwrap();
        let advertised: Vec<&str> = requests[0]
            .tools
            .iter()
            .map(|spec| spec.function.name.as_str())
            .collect();
        assert_eq!(advertised, ["probe"], "only read-only tools are offered");
        let feedback = requests[1]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .expect("tool feedback");
        assert!(
            feedback.text().contains("unknown tool: mutate"),
            "the write tool does not exist inside the run: {}",
            feedback.text()
        );
    }

    #[tokio::test]
    async fn json_protocol_runs_tools_for_models_without_native_calling() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::without_native_tools(vec![
            vec![chunk(r#"{"tool": "probe", "arguments": {}}"#, false, true)],
            vec![chunk("all done", false, true)],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let ctx = ToolContext::new(&tmp.0);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));

        let result = spawn(
            next_run_id(),
            &worker(),
            "look around",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert!(result.completed);
        assert_eq!(result.output, "all done");
        assert_eq!(result.steps_used, 2);

        let requests = provider.requests.lock().unwrap();
        assert!(requests[0].tools.is_empty(), "no native tool specs sent");
        let system = requests[0].messages[0].text();
        assert!(
            system.contains("do not have native function calling"),
            "the JSON protocol is taught: {system}"
        );
        assert!(
            system.contains("`probe`"),
            "the roster is rendered: {system}"
        );
        let feedback = requests[1]
            .messages
            .last()
            .expect("second request has messages");
        assert_eq!(feedback.role, Role::User, "results ride user messages");
        assert!(
            feedback.text().contains("Tool result for `probe`"),
            "{}",
            feedback.text()
        );
    }

    #[tokio::test]
    async fn a_foreground_run_streams_run_scoped_events_in_order() {
        use crate::agent::AgentEvent;

        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("probe", "scouting")],
            vec![chunk("the report", false, true)],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));

        let run = next_run_id();
        let result = spawn(
            run,
            &worker(),
            "scout",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert!(result.completed);
        assert_eq!(result.steps_used, 2);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 5, "run-scoped events only: {events:?}");
        assert!(matches!(
            &events[0],
            AgentEvent::SubagentRunText { run: id, text } if *id == run && text == "scouting"
        ));
        assert!(matches!(
            &events[1],
            AgentEvent::SubagentRunToolStarted { run: id, name, .. }
                if *id == run && name == "probe"
        ));
        assert!(matches!(
            &events[2],
            AgentEvent::SubagentRunToolFinished { run: id, name, output }
                if *id == run && name == "probe" && !output.is_error
        ));
        assert!(matches!(
            &events[3],
            AgentEvent::SubagentRunStep { run: id, step: 1 } if *id == run
        ));
        assert!(matches!(
            &events[4],
            AgentEvent::SubagentRunDone { run: id, completed: true, steps_used: 2, error: None, output }
                if *id == run && output == "the report"
        ));
    }

    #[tokio::test]
    async fn spawn_fork_inherits_parent_history_and_strips_nested_spawn() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![vec![chunk("forked report", false, true)]]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let ctx = ToolContext::new(&tmp.0);

        // Parent tool set includes spawn_subagent and a normal tool; the fork
        // must keep the normal one and drop spawn.
        let mut parent_registry = ToolRegistry::new();
        parent_registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent_registry.register(Arc::new(SpawnSubagentTool::new(
            builtin_configs(),
            Arc::clone(&client),
            Arc::new(ToolRegistry::new()),
            Arc::clone(&hooks),
        )));

        let parent_history = vec![
            ChatMessage::system("you are the parent".to_string()),
            ChatMessage::user("we were discussing auth".to_string()),
            ChatMessage::assistant("right, the login flow"),
        ];

        let result = spawn_fork(
            next_run_id(),
            "summarize the auth discussion",
            parent_history.clone(),
            &SpawnOptions::default(),
            &client,
            &parent_registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("fork ok");
        assert!(result.completed);
        assert_eq!(result.name, FORK_NAME);
        assert_eq!(result.output, "forked report");

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one model call");
        let messages = &requests[0].messages;
        // Parent system + user + assistant + fork brief.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text(), "you are the parent");
        assert!(
            messages[3].text().contains("summarize the auth discussion"),
            "fork brief carries the task: {}",
            messages[3].text()
        );
        assert!(
            messages[3].text().contains("/fork"),
            "fork brief identifies itself: {}",
            messages[3].text()
        );
        // Tools advertised to the fork must exclude spawn_subagent.
        let tool_names: Vec<_> = requests[0]
            .tools
            .iter()
            .map(|t| t.function.name.as_str())
            .collect();
        assert!(
            tool_names.contains(&"probe"),
            "parent tools kept: {tool_names:?}"
        );
        assert!(
            !tool_names.contains(&SPAWN_SUBAGENT_TOOL_NAME),
            "spawn stripped: {tool_names:?}"
        );
    }

    #[test]
    fn fork_registry_strips_the_denylist() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: "run_command",
            access: ToolAccess::Execute,
        }));
        parent.register(Arc::new(FakeTool {
            name: "exit_plan",
            access: ToolAccess::Execute,
        }));
        parent.register(Arc::new(FakeTool {
            name: "interview",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: SPAWN_SUBAGENT_TOOL_NAME,
            access: ToolAccess::Execute,
        }));

        let scoped = fork_registry(&parent);
        let names: Vec<_> = scoped
            .specs()
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert_eq!(names, vec!["probe".to_string()]);
    }

    /// A provider that never answers, so the only thing that can end a run
    /// against it is the run's own interrupt or its deadline.
    struct StallingProvider;

    #[async_trait]
    impl LlmProvider for StallingProvider {
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
            // Longer than any test's patience: a stalled run must die on its
            // own terms, never on the provider relenting.
            tokio::time::sleep(Duration::from_secs(3_600)).await;
            unreachable!("the stall outlasts the test")
        }

        fn label(&self) -> String {
            "stalling:test".to_string()
        }
    }

    /// Every `SubagentRunDone` on `events`, drained without waiting for the
    /// channel to close (the sender is still alive).
    fn done_events(rx: &mut tokio::sync::mpsc::Receiver<crate::agent::AgentEvent>) -> Vec<String> {
        let mut closed = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::agent::AgentEvent::SubagentRunDone { error, .. } = event {
                closed.push(error.unwrap_or_default());
            }
        }
        closed
    }

    /// An interrupt ends a foreground run *while it is inside a model call*,
    /// not at the next step boundary, and closes its pane on the way out.
    /// Before the interrupt lived in `spawn`, Esc reached the parent's turn and
    /// left the subagent it was blocked on running.
    #[tokio::test]
    async fn a_cancelled_run_ends_at_once_and_closes_its_pane() {
        let tmp = TempDir::new();
        let client: Arc<dyn LlmProvider> = Arc::new(StallingProvider);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);
        let cancel = CancelHandle::default();

        let options = SpawnOptions {
            cancel: Some(cancel.clone()),
            ..Default::default()
        };
        let raised = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            raised.cancel();
        });

        let started = std::time::Instant::now();
        let err = spawn(
            next_run_id(),
            &worker(),
            "stall forever",
            &options,
            &client,
            &ToolRegistry::new(),
            &test_hooks(&tmp),
            &ctx,
        )
        .await
        .expect_err("a cancelled run is not a result");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the interrupt ends the run, not the provider"
        );
        assert_eq!(
            err.downcast_ref::<SubagentStop>(),
            Some(&SubagentStop::Cancelled),
            "and says so in a way a fan-out can tell from a dead candidate: {err:#}"
        );
        assert_eq!(
            done_events(&mut rx),
            vec!["cancelled".to_string()],
            "exactly one terminal event, or the pane is left running"
        );
    }

    /// The deadline is `spawn`'s own, so a caller that passed one does not have
    /// to wrap the call to get it, and the pane is closed out by the same
    /// function that opened everything else on it.
    #[tokio::test]
    async fn a_run_past_its_deadline_is_ended_and_named() {
        let tmp = TempDir::new();
        let client: Arc<dyn LlmProvider> = Arc::new(StallingProvider);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        let options = SpawnOptions {
            deadline: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let err = spawn(
            next_run_id(),
            &worker(),
            "stall forever",
            &options,
            &client,
            &ToolRegistry::new(),
            &test_hooks(&tmp),
            &ctx,
        )
        .await
        .expect_err("a run past its deadline is not a result");

        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            matches!(
                err.downcast_ref::<SubagentStop>(),
                Some(SubagentStop::DeadlineExceeded(_))
            ),
            "{err:#}"
        );
        let closed = done_events(&mut rx);
        assert_eq!(closed.len(), 1);
        assert!(closed[0].contains("timed out"), "{}", closed[0]);
    }

    /// A run that outgrows its window compacts, exactly as the parent turn
    /// does, rather than climbing until the provider refuses the prompt.
    ///
    /// This is the case a fifty-step budget makes *likelier*, not rarer:
    /// fifty round trips of tool output is precisely what fills a window, and
    /// before the sub-loop shared the parent's compactor there was nothing in
    /// it that could give any of that back.
    #[tokio::test]
    async fn a_sub_run_that_outgrows_its_window_compacts_instead_of_failing() {
        let tmp = TempDir::new();
        // Twenty messages of inherited conversation (`/fork`'s shape), which
        // is a history whose only user message is at index 1 — the shape the
        // conversation anchor could never find a cut in.
        let mut inherited = vec![
            ChatMessage::system("you are the parent"),
            ChatMessage::user("the original request"),
        ];
        for step in 0..9 {
            let mut assistant = ChatMessage::assistant(format!("parent step {step}"));
            assistant.push_tool_call(ToolCall::new("probe", json!({})));
            inherited.push(assistant);
            inherited.push(ChatMessage::tool_result("id", "probe", "some tool output"));
        }

        // Step one reports a prompt filling 90% of a 1,000-token window, which
        // is past the auto-compact trigger; the summary lands next; then the
        // report.
        let provider = ScriptedProvider::windowed(
            1_000,
            vec![
                vec![ChatChunk {
                    prompt_eval_count: Some(900),
                    cache: CacheTokens::NONE,
                    ..tool_call_chunk("probe", "still working")
                }],
                vec![chunk("a terse progress note", false, true)],
                vec![chunk("the report", false, true)],
            ],
        );
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        let options = SpawnOptions {
            inherited_history: Some(inherited),
            ..Default::default()
        };

        let result = spawn(
            next_run_id(),
            &worker(),
            "carry on",
            &options,
            &client,
            &registry,
            &test_hooks(&tmp),
            &ToolContext::new(&tmp.0),
        )
        .await
        .expect("the run survives its own context");
        assert!(result.completed);
        assert_eq!(result.output, "the report");

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "the step, the summary, the step");
        assert!(
            requests[1].tools.is_empty(),
            "the summarization pass is a bare completion"
        );
        assert!(
            requests[1].messages[1].text().contains("parent step 0"),
            "the span handed to the summarizer is the middle of the history"
        );

        let (before, after) = (&requests[0].messages, &requests[2].messages);
        assert!(
            after.len() < before.len(),
            "the history shrank: {} -> {}",
            before.len(),
            after.len()
        );
        let note = after
            .iter()
            .find(|message| {
                message
                    .text()
                    .starts_with(crate::agent::COMPACT_SUMMARY_HEADING)
            })
            .expect("the summary replaced the span");
        assert!(note.text().contains("a terse progress note"));
        assert_eq!(
            note.role,
            Role::User,
            "a sub-loop's tail starts on an assistant turn, so a system note would leave the \
             request opening on one — which Anthropic rejects outright"
        );
        assert_eq!(
            after
                .iter()
                .find(|message| message.role != Role::System)
                .map(|message| message.role),
            Some(Role::User),
            "and the request still opens on a user message"
        );
    }

    /// The breaker a parent shares is the breaker its delegated runs answer
    /// to, so an outage is proved once rather than N times — which is the
    /// difference between a council noticing a dead endpoint and a council
    /// spending N × 7 requests on it.
    #[tokio::test(start_paused = true)]
    async fn a_shared_breaker_ends_a_subagents_run_and_refuses_the_next_one() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::failing(503);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);
        let shared = breaker::LlmBreaker::new();
        let options = SpawnOptions {
            breaker: shared.clone(),
            ..Default::default()
        };
        let registry = ToolRegistry::new();
        // An `async fn` rather than a closure: the future borrows `options`,
        // and a closure has no way to say its return type outlives its own
        // parameter.
        async fn run(
            options: &SpawnOptions,
            client: &Arc<dyn LlmProvider>,
            registry: &ToolRegistry,
            hooks: &Arc<HookEngine>,
            ctx: &ToolContext,
        ) -> Result<SubagentResult> {
            spawn(
                next_run_id(),
                &worker(),
                "report",
                options,
                client,
                registry,
                hooks,
                ctx,
            )
            .await
        }

        // One run's retry budget is seven attempts, which is under the trip
        // threshold: it ends on the provider's own error, as it always did.
        let err = run(&options, &client, &registry, &hooks, &ctx)
            .await
            .expect_err("the outage fails the run");
        assert!(
            !err.is::<breaker::LlmBreakerOpen>(),
            "one run's worth of failures is not yet an outage: {err:#}"
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 7);
        assert!(!shared.is_open());

        // The next run inherits that streak instead of starting a fresh one,
        // so its very first attempt trips the breaker and ends it.
        let err = run(&options, &client, &registry, &hooks, &ctx)
            .await
            .expect_err("the breaker ends the run");
        assert!(err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            8,
            "one more attempt, then the trip"
        );

        // And once open it refuses without dialing at all.
        let err = run(&options, &client, &registry, &hooks, &ctx)
            .await
            .expect_err("an open breaker refuses");
        assert!(err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            8,
            "a run against an open breaker costs the endpoint nothing"
        );

        // A run without the shared breaker is unaffected: the field is about
        // *whose* breaker, never whether there is one.
        let err = run(&SpawnOptions::default(), &client, &registry, &hooks, &ctx)
            .await
            .expect_err("a fresh breaker still lets the run try");
        assert!(!err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert_eq!(provider.requests.lock().unwrap().len(), 15);

        assert_eq!(
            done_events(&mut rx).len(),
            4,
            "every one of those runs closed its pane, including the two that never dialed"
        );
    }

    /// Interrupting a foreground run ends that run and nothing else: the
    /// parent is free to delegate again on the same tool and the same context,
    /// and the endpoint's breaker has not been told an outage happened.
    ///
    /// The second half is the part worth stating. An interrupt that counted as
    /// a provider failure would mean a user who presses Esc eight times has
    /// taken their own endpoint offline for thirty seconds.
    #[tokio::test]
    async fn a_cancelled_run_leaves_the_parent_free_to_delegate_again() {
        let tmp = TempDir::new();
        // The first call hangs; the second answers. So only the interrupt can
        // end run one, and only a genuinely unwedged parent gets run two.
        let provider = ScriptedProvider::stalling(1, vec![vec![chunk("the report", false, true)]]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let tool = SpawnSubagentTool::new(
            vec![worker()],
            client,
            Arc::new(ToolRegistry::new()),
            test_hooks(&tmp),
        );
        let ctx = ToolContext::new(&tmp.0);
        let shared = breaker::LlmBreaker::new();

        let cancel = CancelHandle::default();
        let handle = tool.model_handle();
        handle.bind("a-model".to_string(), cancel.clone(), shared.clone());
        let raised = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            raised.cancel();
        });

        let started = std::time::Instant::now();
        let err = tool
            .execute(
                json!({ "subagent": "worker", "task": "stall forever" }),
                &ctx,
            )
            .await
            .expect_err("a cancelled run is not a tool result");
        let ToolError::Execution { source, .. } = &err else {
            panic!("a cancelled run is an execution failure, not bad arguments: {err}");
        };
        assert_eq!(
            source.downcast_ref::<SubagentStop>(),
            Some(&SubagentStop::Cancelled),
            "and the parent can tell an interrupt from a broken run: {source:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the interrupt ends the run, not the provider"
        );
        assert_eq!(
            shared.state(),
            breaker::BreakerState::Closed,
            "an interrupt is the user's decision, not the provider's"
        );

        // A new turn arms a fresh handle, exactly as `run_turn` does, and the
        // very next delegation runs to completion through the same tool.
        handle.bind(
            "a-model".to_string(),
            CancelHandle::default(),
            shared.clone(),
        );
        let output = tool
            .execute(json!({ "subagent": "worker", "task": "carry on" }), &ctx)
            .await
            .expect("the parent delegates again");
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("the report"), "{}", output.content);
    }

    /// The default is a run that *can* be ended: unlimited steps plus no
    /// deadline was a run nothing in the process could stop.
    #[test]
    fn the_default_run_is_bounded_in_steps_and_in_time() {
        assert_eq!(
            SubagentConfig::default_max_steps(),
            crate::config::StepBudget::new(DEFAULT_MAX_STEPS)
        );
        assert_eq!(SpawnOptions::default().deadline, Some(DEFAULT_DEADLINE));
        assert!(
            SpawnOptions::default().cancel.is_none(),
            "cancellation is opt-in: a backgrounded run outlives the turn that spawned it"
        );
    }
}
