//! In-process task manager: one lazily-built [`Agent`] per chat.
//!
//! A task is a wizard session; the manager keeps a keep-warm map of live
//! agents (LRU-bounded — sessions persist on disk, so an evicted agent is
//! rebuilt on demand). Each task runs on a dedicated worker that owns the
//! agent and executes one turn at a time; its [`AgentEvent`]s fan out
//! *unserialized* through [`TaskShared::tap`], which is also where the
//! plan/interview gates are parked until the window answers them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::agent::session::Session;
use crate::agent::{
    Agent, AgentEvent, CancelHandle, DoneReason, FinishedNotification, PlanVerdict,
    build_headless_agent_for_session,
};
use crate::config::{Config, Mode};
use crate::gui::command::{CommandCtx, apply_command};
use crate::gui::settings::ConfigStore;
use crate::llm::provider::NATIVE_TOOLS_ON_PROBE_FAILURE;
use crate::mcp::McpManager;
use crate::session_registry::{self, SessionRecord, SessionState};
use crate::tools::{CommandDispatch, ConsoleAccess};

/// Keep at most this many agents warm; beyond it the least-recently-used
/// idle task is retired (its session persists, so it rebuilds on demand).
const MAX_WARM_TASKS: usize = 4;

/// How often every live task's registry heartbeat is refreshed. Must stay well
/// under [`session_registry::STALE_SECS`], or a task that sits idle between
/// turns ages out of `/dashboard` while it is still there. The TUI refreshes on
/// the same cadence, from its draw loop.
const HEARTBEAT: Duration = Duration::from_secs(3);

/// The slash commands the agent may queue here: the ones this surface runs
/// against the Agent ([`crate::commands::Execution::Agent`]) that
/// [`SlashCommand::agent_runnable`]
/// also allows it. Derived from the one command table, never listed again —
/// `run_command` refuses everything else at *call* time, so a command with
/// nowhere to land here comes back to the model as a tool error rather
/// than a no-op it never hears about.
fn agent_commands() -> &'static [&'static str] {
    crate::commands::agent_commands()
}

/// One todo item, as the window's rail draws it.
#[derive(Debug)]
pub struct TodoRow {
    pub text: String,
    pub done: bool,
    pub active: bool,
}

/// What a managed task is doing: the chat list's dot, and what the task
/// heartbeats as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Working,
    NeedsInput,
    Idle,
    /// The agent could not be built (unreachable provider, bad session) or
    /// the last turn ended in an error. A later message retries.
    Failed,
}

/// Whether a finished turn leaves the task failed.
///
/// The agent maps a mid-turn provider failure to `Error` + `Done{Stopped}` —
/// the same shape as a user cancel — so a stop after an error, with no cancel
/// requested, is a failure where a cancel is not.
fn turn_failed(reason: DoneReason, error_seen: bool, cancel_requested: bool) -> bool {
    match reason {
        DoneReason::Completed | DoneReason::MaxSteps => false,
        DoneReason::Stopped => error_seen && !cancel_requested,
        DoneReason::TimeLimit | DoneReason::CircuitBreaker => true,
    }
}

/// One queued turn: the user text, an optional model override, and the
/// attachments the composer picked up for it.
#[derive(Debug, Default)]
pub struct TurnRequest {
    pub text: String,
    pub model: Option<String>,
    /// Images to attach to the user message (the vision path).
    pub images: Vec<PathBuf>,
    /// Non-image attachments. Appended to the text as `@/abs/path` tokens, so
    /// the `@file` expansion every surface shares is what reads them.
    pub files: Vec<PathBuf>,
}

/// One queued slash command that runs against the chat's agent
/// ([`crate::commands::Execution::Agent`]).
#[derive(Debug)]
pub struct CommandRequest {
    pub name: String,
    pub args: String,
}

/// What the worker takes off its queue. Commands and turns share one channel,
/// and one slot, because both need `&mut Agent`: `/compact` running beside a
/// turn would be two mutable borrows of the same conversation.
#[derive(Debug)]
enum WorkerRequest {
    Turn(TurnRequest),
    Command(CommandRequest),
}

/// State shared between a task's worker and whoever is watching it. All
/// mutation goes through the inner mutex; the async sides never hold it across
/// an await.
pub struct TaskShared {
    pub id: String,
    pub cwd: PathBuf,
    /// When this task went live, for its registry record.
    started_unix: u64,
    /// Where this task heartbeats (`~/.wizard/running/`), so `/dashboard` and
    /// every other Wizard on the machine sees it while it is alive. `None` for a
    /// task no manager owns — it has no session anyone could attach to, and must
    /// not advertise one.
    registry: Option<PathBuf>,
    state: Mutex<SharedState>,
}

struct SharedState {
    task_state: TaskState,
    /// Dashboard label: the first line of the task's first message, or the
    /// workspace name until it has one. The TUI names its session the same way.
    name: String,
    /// The first message has landed, so `name` is the task's own and no later
    /// turn overwrites it.
    named: bool,
    /// The posture the agent runs in, mirrored here for the registry record;
    /// `/mode` moves it.
    mode: String,
    /// One-line summary of what the task is doing, for the dashboard row.
    activity: String,
    /// Slash commands the agent asked for through `run_command` during the
    /// current turn. `run_turn` holds `&mut Agent` for its whole duration, so
    /// nothing can be applied until the borrow ends; the worker drains this the
    /// moment the turn returns, which is where the TUI applies its own queue and
    /// for the same reason.
    pending_commands: Vec<String>,
    /// A turn is queued or running; set by the enqueuer, cleared by the
    /// worker, so "one in-flight turn per task" holds across the gap.
    turn_active: bool,
    /// The attached watcher, if any. See [`TaskShared::tap`].
    watcher: Option<mpsc::UnboundedSender<AgentEvent>>,
    /// Bumped per tap, so a stale window's untap cannot clear its replacement.
    watcher_gen: u64,
    pending_plan: Option<oneshot::Sender<PlanVerdict>>,
    pending_interview: Option<oneshot::Sender<Option<Vec<String>>>>,
    cancel: Option<CancelHandle>,
    /// An [`AgentEvent::Error`] arrived during the current turn; with no cancel
    /// requested, its `Done{Stopped}` is a failure and the task ends failed
    /// rather than idle.
    turn_error_seen: bool,
    /// The user asked to cancel during the current turn, so its stop really is
    /// a cancellation.
    turn_cancel_requested: bool,
    /// The current turn ended in an error, so [`TaskShared::finish_turn`] ends
    /// it failed rather than idle.
    turn_failed: bool,
    model: String,
}

impl TaskShared {
    pub(crate) fn new(
        id: String,
        cwd: PathBuf,
        model: String,
        mode: String,
        registry: Option<PathBuf>,
    ) -> Arc<Self> {
        let name = cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_string());
        Arc::new(Self {
            id,
            cwd,
            started_unix: now_unix(),
            registry,
            state: Mutex::new(SharedState {
                task_state: TaskState::Idle,
                name,
                named: false,
                mode,
                activity: "idle".to_string(),
                pending_commands: Vec::new(),
                turn_active: false,
                watcher: None,
                watcher_gen: 0,
                pending_plan: None,
                pending_interview: None,
                cancel: None,
                turn_error_seen: false,
                turn_cancel_requested: false,
                turn_failed: false,
                model,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().expect("gui task state lock poisoned")
    }

    pub fn state(&self) -> TaskState {
        self.lock().task_state
    }

    pub fn model(&self) -> String {
        self.lock().model.clone()
    }

    pub(super) fn set_model(&self, model: &str) {
        self.lock().model = model.to_string();
    }

    pub(super) fn set_mode(&self, mode: Mode) {
        self.lock().mode = mode.to_string();
    }

    /// Name the task after its first message, as the TUI names a session after
    /// the prompt it launched with. Only the first one: later turns are the
    /// conversation, not its title.
    fn name_after_first_message(&self, text: &str) {
        let Some(line) = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(48).collect::<String>())
        else {
            return;
        };
        let mut state = self.lock();
        if !state.named {
            state.named = true;
            state.name = line;
        }
    }

    /// This task's heartbeat record. A window's chat is a running Wizard session
    /// like any other: it belongs in `/dashboard` and in every other instance's
    /// task list, not just in the window that opened it.
    fn record(&self) -> SessionRecord {
        let state = self.lock();
        SessionRecord {
            id: self.id.clone(),
            name: state.name.clone(),
            cwd: self.cwd.display().to_string(),
            model: state.model.clone(),
            mode: state.mode.clone(),
            state: registry_state(state.task_state),
            activity: state.activity.clone(),
            pid: std::process::id(),
            started_unix: self.started_unix,
            updated_unix: 0, // stamped by session_registry::write
        }
    }

    /// Publish (or refresh) this task's heartbeat.
    fn publish(&self) {
        let Some(dir) = &self.registry else { return };
        session_registry::write_to(dir, &self.record());
    }

    /// Take the commands the agent queued during the turn that just ended.
    pub(super) fn take_pending_commands(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().pending_commands)
    }

    /// Tell the watcher what the next model call will load.
    ///
    /// A reading the agent did not emit as an event of its own: after a turn
    /// against a provider that reports no token counts, after a compaction, and
    /// after a rewind, the history changed size without an
    /// [`AgentEvent::ContextSize`] going by. Synthesizing one here is what keeps
    /// the meter from showing the size of a conversation that no longer exists.
    pub(super) fn push_context(&self, tokens: u64) {
        self.relay(&AgentEvent::ContextSize { tokens });
    }

    pub(super) fn set_cancel(&self, cancel: CancelHandle) {
        self.lock().cancel = Some(cancel);
    }

    /// Interrupt the running turn cooperatively.
    pub fn cancel_turn(&self) {
        let cancel = {
            let mut state = self.lock();
            state.turn_cancel_requested = true;
            state.cancel.clone()
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }

    /// Claim the task's single turn slot. False when a turn is already
    /// queued or running.
    fn try_begin_turn(&self) -> bool {
        let mut state = self.lock();
        if state.turn_active {
            return false;
        }
        state.turn_active = true;
        true
    }

    /// Whether a turn is currently claimed (running or about to run).
    fn is_turn_active(&self) -> bool {
        self.lock().turn_active
    }

    /// Ensure the turn slot is claimed. The worker calls this at the start of
    /// every request so a turn that was queued behind another still holds the
    /// slot through `finish_turn` (submit may have left `turn_active` false in
    /// the brief gap after the previous turn ended).
    fn ensure_turn_active(&self) {
        self.lock().turn_active = true;
    }

    /// Release a claimed turn slot without running it (the worker is gone).
    fn abandon_turn(&self) {
        self.lock().turn_active = false;
    }

    /// Say something in this task's stream that no model produced: a queued
    /// message, a switched model, a refused workspace. It reaches the watcher
    /// as the [`AgentEvent::Notice`] a TUI turn would carry.
    pub(super) fn notice(&self, text: impl Into<String>) {
        self.handle_event(AgentEvent::Notice(text.into()));
    }

    /// The same, for something that went wrong.
    pub(super) fn error(&self, message: impl Into<String>) {
        self.handle_event(AgentEvent::Error(message.into()));
    }

    /// Turn start: reset the per-turn flags and go working.
    fn begin_turn(&self) {
        {
            let mut state = self.lock();
            state.turn_error_seen = false;
            state.turn_cancel_requested = false;
            state.turn_failed = false;
            state.task_state = TaskState::Working;
            state.activity = "working".to_string();
        }
        self.publish();
    }

    /// Turn end: release the turn slot and go idle — or failed, when the
    /// agent could not be built or the turn ended in an error.
    fn finish_turn(&self, failed: bool) {
        {
            let mut state = self.lock();
            state.turn_active = false;
            state.task_state = if failed || state.turn_failed {
                TaskState::Failed
            } else {
                TaskState::Idle
            };
            state.activity = "idle".to_string();
            // run_turn resolves its own gates before returning; drop any
            // leftovers defensively so a stale sender can never pin needs_input.
            state.pending_plan = None;
            state.pending_interview = None;
        }
        self.publish();
    }

    /// Answer a plan gate. False when no plan is awaiting one.
    pub fn resolve_plan(&self, verdict: PlanVerdict) -> bool {
        {
            let mut state = self.lock();
            let Some(respond) = state.pending_plan.take() else {
                return false;
            };
            let _ = respond.send(verdict);
            resume_after_gate(&mut state);
        }
        self.publish();
        true
    }

    /// Answer an interview (`None` = declined). False when no interview is
    /// pending.
    pub fn resolve_interview(&self, answers: Option<Vec<String>>) -> bool {
        {
            let mut state = self.lock();
            let Some(respond) = state.pending_interview.take() else {
                return false;
            };
            let _ = respond.send(answers);
            resume_after_gate(&mut state);
        }
        self.publish();
        true
    }

    /// Watch this task's raw [`AgentEvent`] stream, in process.
    ///
    /// Nothing is serialized on the way to the screen, and that is the whole
    /// design: [`AgentEvent`] is `Clone`, it is what
    /// [`TranscriptModel::apply`](crate::transcript::TranscriptModel::apply)
    /// folds, and turning it into JSON so a widget could parse it back would be
    /// a round trip through a wire that is not there. (There used to be such a
    /// wire — a WebSocket, a `Frame` enum and a replay buffer, for a browser.
    /// It is gone; see `docs/native-gui.md`.)
    ///
    /// One watcher per task: a new tap replaces the old one, which is what
    /// happens when the window switches which chat it is showing.
    ///
    /// # The gates stay here
    ///
    /// The events that cross this channel include
    /// [`AgentEvent::PlanReady`] and [`AgentEvent::Interview`], and each
    /// carries a gate ticket that [`PlanGate::claim`](crate::agent::PlanGate)
    /// hands over **exactly once**. The watcher must never claim one:
    /// [`TaskShared::handle_event`] claims it a few lines below this call and
    /// parks the reply channel in `pending_plan`, which is what
    /// [`TaskShared::resolve_plan`] answers through. A watcher that claimed the
    /// ticket first would take the channel away from that bookkeeping and park
    /// the turn forever, with `resolve_plan` returning false and nothing to say
    /// why. Answer through `resolve_plan` / `resolve_interview`; the ticket on
    /// the event is for reading, not for taking.
    ///
    /// Unbounded, because the producer is [`TaskShared::handle_event`], which is
    /// synchronous and called from the turn's event drain: it cannot await a
    /// full queue and must not drop from one either — a dropped `TextDelta` is a
    /// hole in the conversation nothing later repairs. Back pressure lives
    /// upstream, on the agent's own bounded channel.
    ///
    /// Returns a generation token for [`TaskShared::untap`].
    pub(crate) fn tap(&self, tx: mpsc::UnboundedSender<AgentEvent>) -> u64 {
        let mut state = self.lock();
        state.watcher = Some(tx);
        state.watcher_gen += 1;
        state.watcher_gen
    }

    /// Detach the watcher identified by `generation`; a newer tap wins.
    ///
    /// Deliberately resolves no gates. A window that stopped listening has not
    /// necessarily gone away — it re-taps whenever it switches which task it is
    /// showing — and auto-approving a plan because the user looked at another
    /// chat would be a decision nobody made.
    pub(crate) fn untap(&self, generation: u64) {
        let mut state = self.lock();
        if state.watcher_gen == generation {
            state.watcher = None;
        }
    }

    /// Whether anything is currently watching this task.
    fn has_watcher(&self) -> bool {
        self.lock().watcher.is_some()
    }

    /// Forward one event to the watcher, if there is one.
    fn relay(&self, event: &AgentEvent) {
        let mut state = self.lock();
        if let Some(tx) = &state.watcher
            && tx.send(event.clone()).is_err()
        {
            state.watcher = None;
        }
    }

    /// Fan one [`AgentEvent`] out to the watcher and fold what the task itself
    /// has to remember about it.
    ///
    /// Almost every event is pure relay: the window's
    /// [`TranscriptModel`](crate::transcript::TranscriptModel) and its rail
    /// know what to do with a delta, a tool call or a subagent run, and this
    /// side would only be a second, staler copy of them. What is folded here is
    /// the handful of things that outlive the screen — the dashboard row, the
    /// gates, the queued commands, and whether the turn failed.
    pub(super) fn handle_event(&self, event: AgentEvent) {
        // Before the match, because the match consumes the event — and before
        // the gate claims inside it, so the order in which the watcher and this
        // bookkeeping see a `PlanReady` is fixed rather than incidental. See
        // [`TaskShared::tap`] on why the watcher still must not claim it.
        self.relay(&event);
        match event {
            AgentEvent::ToolStarted { name, .. } => {
                // The newest in-flight tool call is what the dashboard row shows
                // while the task works, exactly as in the TUI.
                self.lock().activity = name;
            }
            AgentEvent::PlanReady { gate, .. } => {
                // The user answers this gate, minutes later; claim the verdict
                // channel now so nothing else can, and so the turn's one answer
                // belongs to this task.
                let Some(respond) = gate.claim() else {
                    return;
                };
                {
                    let mut state = self.lock();
                    state.pending_plan = Some(respond);
                    state.task_state = TaskState::NeedsInput;
                    state.activity = "waiting for plan approval".to_string();
                }
                self.publish();
            }
            AgentEvent::Interview { gate, .. } => {
                let Some(respond) = gate.claim() else {
                    return;
                };
                {
                    let mut state = self.lock();
                    state.pending_interview = Some(respond);
                    state.task_state = TaskState::NeedsInput;
                    state.activity = "waiting for interview answers".to_string();
                }
                self.publish();
            }
            // The agent asked for one of its own slash commands. It cannot run
            // now — `run_turn` holds `&mut Agent`, and a request already in
            // flight cannot be reconfigured — so it queues, and the worker
            // applies it the moment the turn's borrow ends. The tool already
            // refused anything this surface does not run ([`agent_commands`]),
            // so what lands here is a command the executor has.
            AgentEvent::CommandRequested(line) => {
                let mut state = self.lock();
                state.pending_commands.push(line);
            }
            AgentEvent::Error(_) => {
                self.lock().turn_error_seen = true;
            }
            AgentEvent::Done { reason } => {
                let mut state = self.lock();
                state.turn_failed =
                    turn_failed(reason, state.turn_error_seen, state.turn_cancel_requested);
            }
            _ => {}
        }
    }
}

/// A resolved gate: back to working.
fn resume_after_gate(state: &mut SharedState) {
    state.task_state = TaskState::Working;
    state.activity = "working".to_string();
}

/// The registry state a task state heartbeats as.
///
/// A failed turn leaves the task live and waiting for the user, so it publishes
/// as idle — not as the registry's `Failed`, which marks a *finished* background
/// run and is retained for a day after its process is gone. Working, needs-input
/// and idle are the three states the TUI publishes, and a GUI chat is the same
/// kind of thing.
fn registry_state(state: TaskState) -> SessionState {
    match state {
        TaskState::Working => SessionState::Working,
        TaskState::NeedsInput => SessionState::NeedsInput,
        TaskState::Idle | TaskState::Failed => SessionState::Idle,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The in-process registry of managed tasks, keyed by session id.
pub struct TaskManager {
    /// Read at agent-build time rather than cloned at startup, so a provider
    /// added on the Settings page is live for the very next turn.
    config: Arc<ConfigStore>,
    /// The window's MCP servers, connected once at startup and handed
    /// to every task's agent build. One process-wide manager, as the TUI keeps:
    /// connecting per build would leave four warm tasks running four copies of
    /// every configured server, each a real OS process.
    mcp: Arc<RwLock<McpManager>>,
    /// `~/.wizard/running/`, where every task this manager owns heartbeats.
    /// `None` only when the wizard directory cannot be resolved at all, in which
    /// case no session on the machine is registered anyway.
    registry: Option<PathBuf>,
    /// Whether a foreground shell command may prompt the user
    /// ([`ConsoleAccess`]), which decides whether `execute` keeps a pipe or
    /// `/dev/null` on fd 0.
    ///
    /// [`ConsoleAccess::None`] by default, because a caller that takes the
    /// default has not said it has anywhere to type an answer: announcing a
    /// prompt nobody can answer would park the turn on a question with no
    /// keyboard behind it. The native window ([`TaskManager::attended`]) is the
    /// caller that turns it on, because it *is* the process the command runs in
    /// and it has a person in front of it.
    console: ConsoleAccess,
    tasks: Mutex<HashMap<String, ManagedTask>>,
}

struct ManagedTask {
    shared: Arc<TaskShared>,
    turn_tx: mpsc::UnboundedSender<WorkerRequest>,
    last_used: Instant,
}

impl TaskManager {
    /// A manager for a surface with a person at a keyboard: its tasks declare
    /// [`ConsoleAccess::Interactive`], so a command that asks a question
    /// announces it ([`AgentEvent::ConsoleOpened`]) instead of reading
    /// `/dev/null` and dying at its timeout.
    ///
    /// Only the window calls this, and only because it is in-process with the
    /// child it would be driving. See `src/native/console.rs`.
    pub(crate) fn attended(config: Arc<ConfigStore>, mcp: Arc<RwLock<McpManager>>) -> Self {
        Self::attended_with_registry(config, mcp, session_registry::running_dir())
    }

    /// [`TaskManager::attended`] heartbeating into an explicit directory, for
    /// the same reason [`TaskManager::with_registry`] exists.
    pub(crate) fn attended_with_registry(
        config: Arc<ConfigStore>,
        mcp: Arc<RwLock<McpManager>>,
        registry: Option<PathBuf>,
    ) -> Self {
        Self {
            console: ConsoleAccess::Interactive,
            ..Self::with_registry(config, mcp, registry)
        }
    }

    /// A manager that leaves [`ConsoleAccess::None`] in place, heartbeating
    /// into an explicit directory (tests use a temp dir, so a test run never
    /// advertises itself as a live session).
    pub(crate) fn with_registry(
        config: Arc<ConfigStore>,
        mcp: Arc<RwLock<McpManager>>,
        registry: Option<PathBuf>,
    ) -> Self {
        Self {
            config,
            mcp,
            registry,
            console: ConsoleAccess::None,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ManagedTask>> {
        self.tasks.lock().expect("gui task map lock poisoned")
    }

    /// Create the session for `cwd`, spawn its worker, and queue the first turn
    /// when the caller supplied a prompt. Without one the chat opens empty and
    /// the first message starts it. Returns the new task id.
    pub fn create_task(
        &self,
        cwd: &Path,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<String> {
        let sessions_dir = Config::sessions_dir()?;
        let session = Session::create_in(&sessions_dir, cwd)?;
        let id = session.id.clone();
        self.spawn(id.clone(), cwd.to_path_buf(), session);
        if let Some(text) = prompt {
            self.submit_turn(
                &id,
                TurnRequest {
                    text,
                    model,
                    ..TurnRequest::default()
                },
            )
            .map_err(|message| anyhow::anyhow!(message))?;
        }
        Ok(id)
    }

    /// The managed task for `id`, spawning a worker over the on-disk
    /// session when it is not live yet (opening an old chat).
    pub fn ensure(&self, id: &str) -> Result<Arc<TaskShared>> {
        if let Some(shared) = self.get(id) {
            return Ok(shared);
        }
        let sessions_dir = Config::sessions_dir()?;
        let session = Session::open_by_id(&sessions_dir, id)?
            .with_context(|| format!("no session '{id}'"))?;
        let cwd = session
            .cwd()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        Ok(self.spawn(id.to_string(), cwd, session))
    }

    /// The managed task for `id`, when live.
    pub fn get(&self, id: &str) -> Option<Arc<TaskShared>> {
        let mut tasks = self.lock();
        let task = tasks.get_mut(id)?;
        task.last_used = Instant::now();
        Some(task.shared.clone())
    }

    /// Queue one turn on task `id`. User messages stack FIFO behind any
    /// in-flight turn — the worker runs them one at a time — so a second
    /// message mid-turn is accepted and announced rather than refused.
    /// Commands still take the exclusive slot (see [`TaskManager::submit_command`]).
    pub fn submit_turn(&self, id: &str, request: TurnRequest) -> Result<(), String> {
        self.submit(id, WorkerRequest::Turn(request))
    }

    /// Queue one agent-side slash command on task `id`. It takes the same
    /// slot a turn does — it mutates the same conversation — so a command
    /// arriving mid-turn is refused rather than queued behind it: the user
    /// asked for something to happen now, and "in four minutes" is not that.
    pub fn submit_command(&self, id: &str, request: CommandRequest) -> Result<(), String> {
        self.submit(id, WorkerRequest::Command(request))
    }

    fn submit(&self, id: &str, request: WorkerRequest) -> Result<(), String> {
        let mut tasks = self.lock();
        let Some(task) = tasks.get_mut(id) else {
            return Err(format!("task '{id}' is not live"));
        };
        let is_turn = matches!(request, WorkerRequest::Turn(_));
        let already_active = task.shared.is_turn_active();
        if already_active && !is_turn {
            // Commands still refuse mid-turn: they want to reconfigure *now*.
            return Err("turn in progress".to_string());
        }
        if !already_active && !task.shared.try_begin_turn() {
            // Lost a race with another submit; for a turn, still queue — the
            // worker serializes. For a command, refuse.
            if !is_turn {
                return Err("turn in progress".to_string());
            }
        }
        if task.turn_tx.send(request).is_err() {
            // Worker is gone: only abandon if we were the ones holding the
            // slot and nothing is running. If a turn was already active, leave
            // its bookkeeping alone (it will fail its own channel ops).
            if !already_active {
                task.shared.abandon_turn();
            }
            return Err("task worker exited".to_string());
        }
        if already_active && is_turn {
            // The worker will run this after the current turn. Surface it so
            // the user isn't left wondering where their message went.
            task.shared.notice("queued — will send after this turn");
        }
        task.last_used = Instant::now();
        Ok(())
    }

    /// Live task states in the registry's vocabulary, for
    /// [`session_registry::chats`].
    ///
    /// [`TaskState::Failed`] becomes [`SessionState::Failed`] rather than the
    /// `Idle` a task *heartbeats* as ([`registry_state`]): the heartbeat
    /// answers "is this session still there", and a picker's dot answers "did
    /// the last turn break", which is a different question with a different
    /// right answer.
    pub fn registry_states(&self) -> HashMap<String, SessionState> {
        self.lock()
            .iter()
            .map(|(id, task)| {
                let state = match task.shared.state() {
                    TaskState::Working => SessionState::Working,
                    TaskState::NeedsInput => SessionState::NeedsInput,
                    TaskState::Idle => SessionState::Idle,
                    TaskState::Failed => SessionState::Failed,
                };
                (id.clone(), state)
            })
            .collect()
    }

    /// The model a live task runs on, if managed.
    pub fn model_of(&self, id: &str) -> Option<String> {
        self.lock().get(id).map(|task| task.shared.model())
    }

    /// Drop every task's heartbeat, so a closed window leaves no session behind
    /// claiming to be running. Called on a graceful shutdown; a hard kill leaves
    /// the records to age out ([`session_registry::STALE_SECS`]).
    pub fn shutdown(&self) {
        let Some(dir) = &self.registry else { return };
        for id in self.lock().keys() {
            session_registry::remove_from(dir, id);
        }
    }

    fn spawn(&self, id: String, cwd: PathBuf, session: Session) -> Arc<TaskShared> {
        let mut tasks = self.lock();
        if let Some(existing) = tasks.get(&id) {
            // Raced with another request; keep the first worker.
            return existing.shared.clone();
        }
        evict_lru(&mut tasks, self.registry.as_deref());
        let config = self.config.current();
        let shared = TaskShared::new(
            id.clone(),
            cwd,
            config.active().model,
            config.mode.to_string(),
            self.registry.clone(),
        );
        // Live from here, not from the first turn: a chat opened and left empty
        // is a session somebody may come back to, and every other Wizard on the
        // machine should be able to see it.
        shared.publish();
        spawn_heartbeat(&shared);
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_worker(
            Arc::clone(&self.config),
            Arc::clone(&self.mcp),
            Arc::clone(&shared),
            session,
            self.console,
            turn_rx,
        ));
        tasks.insert(
            id,
            ManagedTask {
                shared: Arc::clone(&shared),
                turn_tx,
                last_used: Instant::now(),
            },
        );
        shared
    }
}

/// Refresh a task's registry heartbeat for as long as it is live.
///
/// Its own task, rather than a tick on the worker's loop: the worker sits inside
/// `run_turn` for however long a turn takes, and a working session that stops
/// heartbeating is pruned as crashed — precisely when the dashboard most wants
/// to see it. Holds a `Weak`, so the beat stops when the task is evicted.
fn spawn_heartbeat(shared: &Arc<TaskShared>) {
    let shared = Arc::downgrade(shared);
    tokio::spawn(async move {
        let mut beat = tokio::time::interval(HEARTBEAT);
        beat.tick().await; // the first tick is immediate; `spawn` just published
        loop {
            beat.tick().await;
            let Some(shared) = shared.upgrade() else {
                return;
            };
            shared.publish();
        }
    });
}

/// Retire least-recently-used tasks that are safe to drop (no turn queued
/// or running, nothing watching it) until the map is under the keep-warm
/// cap. Dropping the turn sender ends the worker, which fires the
/// session-end hooks and releases the agent.
///
/// The heartbeat goes here, under the map lock, and not in the worker's exit
/// path: a task re-spawned for the same session id publishes again immediately,
/// and the outgoing worker — which is still finishing its session-end hooks —
/// must not then delete the newcomer's record.
fn evict_lru(tasks: &mut HashMap<String, ManagedTask>, registry: Option<&Path>) {
    while tasks.len() >= MAX_WARM_TASKS {
        let candidate = tasks
            .iter()
            .filter(|(_, task)| {
                task.shared.state() != TaskState::Working
                    && task.shared.state() != TaskState::NeedsInput
                    && !task.shared.has_watcher()
            })
            .min_by_key(|(_, task)| task.last_used)
            .map(|(id, _)| id.clone());
        match candidate {
            Some(id) => {
                tasks.remove(&id);
                if let Some(dir) = registry {
                    session_registry::remove_from(dir, &id);
                }
            }
            // Everything is busy or watched: let the map grow.
            None => break,
        }
    }
}

/// Per-task agent config: the user's own config, unchanged — same mode, same
/// step budget as the TUI, because the GUI is that agent on another surface and
/// not a reduced one. The only per-task edit is the model override: a configured
/// provider name switches the active provider, anything else is a model tag on
/// the active provider.
fn agent_config(base: &Config, model: Option<&str>) -> Config {
    let mut config = base.clone();
    if let Some(want) = model {
        if config.providers.iter().any(|p| p.name == want) {
            config.active_provider = Some(want.to_string());
        } else {
            let active = config.active().name;
            match config.providers.iter_mut().find(|p| p.name == active) {
                Some(provider) => provider.model = want.to_string(),
                // No configured providers: the synthesized local provider
                // reads the legacy `model` field.
                None => config.model = want.to_string(),
            }
        }
    }
    config
}

/// Say once, in this task's own stream, that its workspace's project hooks are
/// not going to run.
///
/// A window has no terminal to ask a trust question in — the process may not
/// even have one behind it. So it declares nothing (the agent build loads hooks
/// through `crate::trust::Console::Unavailable`, which refuses instead of
/// asking) and an undecided workspace contributes no hooks at all. A foreground
/// `wizard gui` *would* pass an `isatty` probe, which is exactly why the probe
/// is not what decides: the task would otherwise park on a prompt in a terminal
/// nobody is watching, holding the trust lock and a tokio worker while it
/// waited.
///
/// Refusing is right; refusing silently is what makes "my hooks stopped firing"
/// unanswerable. A workspace with nothing executable says nothing.
fn report_trust_refusal(shared: &TaskShared) {
    if let Some(why) = crate::trust::unattended_refusal(&shared.cwd) {
        tracing::warn!("{why}");
        shared.notice(format!("wizard: {why}"));
    }
}

/// The dedicated worker for one task: owns the agent (built on the first
/// turn so startup never needs a reachable provider) and runs queued
/// turns and commands one at a time, draining each turn's events into `shared`.
/// Ends when the manager drops the turn sender (eviction or shutdown).
async fn run_worker(
    store: Arc<ConfigStore>,
    mcp: Arc<RwLock<McpManager>>,
    shared: Arc<TaskShared>,
    session: Session,
    console: ConsoleAccess,
    mut requests: mpsc::UnboundedReceiver<WorkerRequest>,
) {
    let mut agent: Option<Agent> = None;
    let mut task_config: Option<Config> = None;
    // Warm-agent state, like the mode and the plan flag: a task rebuilt after
    // eviction comes back on its configured provider.
    let mut fusion = false;

    while let Some(request) = requests.recv().await {
        if let WorkerRequest::Turn(turn) = &request {
            shared.name_after_first_message(&turn.text);
        }
        // A turn that was queued while another ran may arrive after
        // `finish_turn` cleared the flag — re-claim so mid-turn submits still
        // see an active slot.
        shared.ensure_turn_active();
        shared.begin_turn();
        // Read the config per turn: a build that failed for want of a provider
        // must succeed on the next turn once Settings has configured one.
        let base_config = store.current();
        let model_override = match &request {
            WorkerRequest::Turn(turn) => turn.model.clone(),
            WorkerRequest::Command(_) => None,
        };

        // The agent is taken out for the turn and put back when it ends: an
        // agent that has not been built yet is built here (the session is
        // retained, so a failed build retries on the next turn), and a model
        // override on a *later* turn switches the live agent in place — the
        // first turn's override is already baked into its config.
        let mut agent_for_turn = match agent.take() {
            Some(mut live) => {
                if let Some(model) = model_override.as_deref()
                    && model != shared.model()
                {
                    let config = task_config.as_ref().unwrap_or(&base_config);
                    switch_model(&mut live, config, model, &shared).await;
                }
                live
            }
            None => {
                let config = agent_config(&base_config, model_override.as_deref());
                report_trust_refusal(&shared);
                let built = {
                    let manager = mcp.read().await;
                    build_headless_agent_for_session(
                        &config,
                        &shared.cwd,
                        session.clone(),
                        Some(&manager),
                    )
                    .await
                };
                match built {
                    Ok(mut built) => {
                        if config.plan_first {
                            built.set_plan_mode(true);
                        }
                        if config.omakase {
                            built.set_omakase(true);
                        }
                        // The GUI drains the commands the agent queues — but only
                        // the ones its executor implements, so `run_command`
                        // refuses the rest to the model instead of accepting work
                        // that would never run.
                        built.set_command_dispatch(CommandDispatch::Only(agent_commands()));
                        // Whether a prompting command may announce itself. See
                        // the field on [`TaskManager`]: the window gets
                        // `Interactive`, a caller that took the default `None`.
                        built.set_console_access(console);
                        shared.set_cancel(built.cancel_handle());
                        shared.set_model(&config.active().model);
                        shared.set_mode(config.mode);
                        fire_start_hooks(&mut built, &shared).await;
                        task_config = Some(config);
                        built
                    }
                    Err(err) => {
                        shared.error(format!("could not start the agent: {err:#}"));
                        shared.handle_event(AgentEvent::Done {
                            reason: DoneReason::Stopped,
                        });
                        shared.finish_turn(true);
                        continue;
                    }
                }
            }
        };

        let config = task_config.clone().unwrap_or_else(|| base_config.clone());
        let mut ctx = CommandCtx {
            config: &config,
            shared: &shared,
            mcp: &mcp,
            fusion: &mut fusion,
        };
        match request {
            WorkerRequest::Turn(turn) => {
                run_turn(&mut agent_for_turn, turn, &shared).await;
                // The turn is over and its borrow of the agent with it, so the
                // commands it queued can finally be applied — through the one
                // executor a typed command goes through, saying the same things.
                for line in shared.take_pending_commands() {
                    apply_command(&mut agent_for_turn, parse_command_line(&line), &mut ctx).await;
                }
                // Surface any background subagents/forks that finished during
                // the turn (or between this turn and the last) so their reports
                // land in history without waiting for another user message.
                drain_finished(&mut agent_for_turn, &shared);
                shared.finish_turn(false);
            }
            WorkerRequest::Command(command) => {
                apply_command(&mut agent_for_turn, command, &mut ctx).await;
                drain_finished(&mut agent_for_turn, &shared);
                shared.finish_turn(false);
            }
        }
        agent = Some(agent_for_turn);
    }

    if let Some(agent) = &agent {
        agent.fire_session_end(None).await;
    }
}

/// Split a slash line the agent queued (`/model claude-sonnet-5`) into the name
/// and arguments the executor takes. The tool has already validated the line, so
/// this only has to cut it.
fn parse_command_line(line: &str) -> CommandRequest {
    let line = line.trim().trim_start_matches('/');
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    CommandRequest {
        name: name.to_string(),
        args: args.trim().to_string(),
    }
}

/// Drain finished background tasks and subagents (including `/fork` side
/// quests) into history and announce them. Same path the TUI runs on its idle
/// tick and the agent loop runs at the top of every step.
pub(super) fn drain_finished(agent: &mut Agent, shared: &TaskShared) {
    for notification in agent.drain_finished_notifications() {
        match notification {
            FinishedNotification::Task(task) => {
                shared.handle_event(AgentEvent::TaskFinished {
                    id: task.id,
                    command: task.command,
                    status: task.status,
                });
            }
            FinishedNotification::Subagent(task) => {
                shared.handle_event(AgentEvent::SubagentFinished {
                    id: task.id,
                    name: task.name,
                    task: task.task,
                    completed: task.completed,
                    output: task.output,
                });
            }
        }
    }
}

/// Run one user turn on `agent`, fanning its events out to whoever is watching.
///
/// The text goes through [`crate::commands::preprocess`] first — the one
/// pipeline every surface shares — so a message typed here gets the same `@file`
/// references and the same custom `.wizard/commands/*.md` commands a TUI
/// message does. Non-image attachments join it as `@`-tokens, which is how
/// their contents reach the model: no second file-reading path.
async fn run_turn(agent: &mut Agent, request: TurnRequest, shared: &TaskShared) {
    let prompt = turn_prompt(request, &shared.cwd);

    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
    // Drain events concurrently with the turn: the turn owns the sender
    // (dropped on completion, ending the collector), the collector owns
    // the receiver — disjoint borrows, same pattern as the gateway.
    let collector = async {
        while let Some(event) = events_rx.recv().await {
            shared.handle_event(event);
        }
    };
    let (result, ()) = tokio::join!(
        agent.run_turn_with_images(&prompt.text, prompt.images, events_tx),
        collector
    );
    if let Err(err) = result {
        // The turn already emitted its `Error` and `Done` events; the task
        // itself stays usable.
        tracing::warn!("gui task {}: turn failed: {err:#}", shared.id);
    }
    // Not every provider reports token counts, and `Usage` is the only thing
    // that otherwise moves the meter — so a turn against one that stays quiet
    // would leave it blank. `context_tokens` falls back to an estimate of the
    // history, which is what the TUI status bar shows in the same situation.
    shared.push_context(agent.context_tokens());
}

/// What the agent is actually asked: the message with its attached files as
/// `@/abs/path` tokens, run through the shared preprocessing pipeline (custom
/// `/command` expansion, then `@file` references), and the images to attach.
///
/// A path with whitespace in it would not survive `@`-tokenization, which is why
/// the upload route sanitizes names before writing.
fn turn_prompt(request: TurnRequest, cwd: &Path) -> crate::commands::Preprocessed {
    let mut input = request.text;
    for file in &request.files {
        input.push_str(&format!(" @{}", file.display()));
    }
    let custom = crate::commands::load(cwd);
    let mut prompt = crate::commands::preprocess(&input, &custom, cwd);
    // Uploaded images first: they are what the user attached to *this* message,
    // where an `@`-referenced one is context they pointed at.
    let mut images = request.images;
    images.append(&mut prompt.images);
    prompt.images = images;
    prompt
}

/// Fire the `session_start` hooks once per built agent, surfacing their
/// activity as notices (mirrors the gateway's console lines).
async fn fire_start_hooks(agent: &mut Agent, shared: &TaskShared) {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    agent.fire_session_start(&tx).await;
    drop(tx);
    while let Some(event) = rx.recv().await {
        shared.handle_event(event);
    }
}

/// `/model`-style switch on a live agent: probe the new tag's tool-calling
/// support on a fresh client of the active provider (switching providers
/// mid-session is not supported) and swap the model in place, context
/// preserved.
pub(super) async fn switch_model(
    agent: &mut Agent,
    config: &Config,
    model: &str,
    shared: &TaskShared,
) {
    let native = match config.active().build() {
        Ok(client) => crate::llm::provider::probe_native_tools(client.as_ref(), model).await,
        Err(err) => {
            tracing::warn!(
                "building a probe client: {err:#}; assuming \
                 native_tools={NATIVE_TOOLS_ON_PROBE_FAILURE}"
            );
            NATIVE_TOOLS_ON_PROBE_FAILURE
        }
    };
    agent.set_model(model.to_string(), native);
    shared.set_model(model);
    shared.notice(format!("switched to model {model}"));
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::{InterviewGate, PlanGate};
    use crate::commands::Execution;
    use crate::commands::SlashCommand;
    use crate::commands::surface::Surface;
    use crate::config::ProviderKind;
    use crate::gui::command::ultra_seats;
    use crate::tools::ToolOutput;

    /// An unmanaged task: it heartbeats nowhere, so a test run never advertises
    /// itself in the real registry.
    fn shared() -> Arc<TaskShared> {
        TaskShared::new(
            "2026-07-11T00-00-00".to_string(),
            PathBuf::from("/tmp/project"),
            "test-model".to_string(),
            "genie".to_string(),
            None,
        )
    }

    /// A task whose workspace is a real directory: `/goal`, `/memory` and
    /// `/doctor` all read and write one.
    fn shared_in(cwd: &Path) -> Arc<TaskShared> {
        TaskShared::new(
            "2026-07-11T00-00-00".to_string(),
            cwd.to_path_buf(),
            "test-model".to_string(),
            "genie".to_string(),
            None,
        )
    }

    /// Watch a task, the way the window does.
    ///
    /// Always *before* the thing under test: a tap carries no backlog. There
    /// used to be a replay buffer here, for a browser that could reconnect
    /// mid-turn with nothing on its screen; a window holds its whole
    /// conversation in a [`crate::transcript::TranscriptModel`] and needs none.
    fn tap(shared: &Arc<TaskShared>) -> mpsc::UnboundedReceiver<AgentEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        shared.tap(tx);
        rx
    }

    /// Everything the tap has for us right now.
    fn drain(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn the_window_seats_an_ultra_roster_the_same_way_the_tui_does() {
        // The drift this pins: this surface used to refuse `/ultra` on top of
        // `/fusion` outright while the TUI dealt the roster across the panel,
        // so the same two commands meant different things depending on which
        // surface you typed them into. Both now answer with these seats.
        let provider = |name: &str, model: &str| crate::config::ProviderConfig {
            name: name.to_string(),
            // Ollama builds a client without a key or a reachable endpoint,
            // which is all a seat has to prove here.
            kind: crate::config::ProviderKind::OLLAMA,
            base_url: "http://127.0.0.1:11434".to_string(),
            model: model.to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        let mut config = Config::default();
        config.providers = vec![provider("alice", "m-alice"), provider("bob", "m-bob")];
        config.fusion = Some(crate::config::FusionConfig {
            panel: vec!["alice".to_string(), "bob".to_string()],
            synthesizer: "alice".to_string(),
            rounds: 1,
        });

        let off = ultra_seats(&config, false).expect("seats build");
        assert!(
            off.is_empty(),
            "with fusion off the roster runs on the session's own client"
        );

        let on = ultra_seats(&config, true).expect("seats build");
        assert_eq!(on.len(), 2, "one seat per panel member, panel order");
        assert_eq!(on[0].provider.as_deref(), Some("alice"));
        assert_eq!(on[0].model.as_deref(), Some("m-alice"));
        assert_eq!(on[1].provider.as_deref(), Some("bob"));
    }

    /// The notice texts a stream carried, in order.
    fn notices(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_workspace_whose_hooks_are_refused_says_so_in_the_task_stream() {
        // Nobody can be asked here, so the hooks do not load. What must not
        // happen is that they vanish with no trace but a line in
        // ~/.wizard/logs, which nobody browsing a task is reading.
        let cwd = std::env::temp_dir().join(format!("wizard-gui-trust-{}", uuid::Uuid::new_v4()));
        let hooks = cwd.join(".wizard").join("hooks.toml");
        std::fs::create_dir_all(hooks.parent().expect("has parent")).expect("mkdir");
        std::fs::write(
            &hooks,
            "[[hooks]]\nevent = \"session_start\"\ncommand = \"true\"\n",
        )
        .expect("write hooks.toml");
        crate::trust::record(&cwd, crate::trust::Decision::Deny).expect("record the refusal");

        let shared = shared_in(&cwd);
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let mut rx = tap(&shared);
        report_trust_refusal(&shared);
        let said = notices(&drain(&mut rx));
        assert_eq!(said.len(), 1, "exactly one notice: {said:?}");
        assert!(said[0].contains("not running project hooks"), "{said:?}");

        // A workspace that ships nothing executable is never mentioned: that
        // is every ordinary project, and the notice would be noise in all of
        // them.
        let plain = cwd.join("plain");
        std::fs::create_dir_all(&plain).expect("mkdir");
        let shared = shared_in(&plain);
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let mut rx = tap(&shared);
        report_trust_refusal(&shared);
        assert!(notices(&drain(&mut rx)).is_empty());

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// The tap is the whole fan-out, and it is the event itself.
    ///
    /// There is no `Frame` enum here any more, no JSON, no replay buffer and no
    /// protocol doc to keep in step with them: [`AgentEvent`] is `Clone`, it is
    /// what the window's transcript folds, and a round trip through a wire that
    /// is not there was the browser GUI's cost, not this one's.
    #[test]
    fn the_tap_carries_the_event_itself_not_a_serialization_of_it() {
        let shared = shared();
        let mut rx = tap(&shared);
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: json!({ "path": "src/app.rs" }),
        });
        shared.handle_event(AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            output: ToolOutput::ok("line\n"),
        });

        let got = drain(&mut rx);
        assert!(
            matches!(&got[0], AgentEvent::ToolStarted { name, args }
                if name == "read_file" && args["path"] == "src/app.rs"),
            "the arguments arrive as the `Value` they were: {got:?}"
        );
        assert!(
            matches!(&got[1], AgentEvent::ToolFinished { output, .. } if !output.is_error),
            "{got:?}"
        );
    }

    /// One watcher per task: a second tap replaces the first, which is what
    /// happens when the window switches which chat it is showing.
    #[test]
    fn a_new_tap_replaces_the_old_one() {
        let shared = shared();
        let mut first = tap(&shared);
        let mut second = tap(&shared);
        shared.handle_event(AgentEvent::TextDelta("hello".to_string()));
        assert!(drain(&mut first).is_empty(), "the retired tap is silent");
        assert_eq!(drain(&mut second).len(), 1);
    }

    /// A tap released after it was already replaced must not take the
    /// replacement's channel with it — iced drops a retired subscription's
    /// stream whenever it feels like it, including after the new one is up.
    #[test]
    fn a_stale_untap_does_not_clobber_a_newer_tap() {
        let shared = shared();
        let _old = tap(&shared);
        let stale = 1;
        let mut new_rx = tap(&shared);

        shared.untap(stale);
        shared.handle_event(AgentEvent::TextDelta("still streaming".to_string()));
        let got = drain(&mut new_rx);
        assert!(
            matches!(got.first(), Some(AgentEvent::TextDelta(text)) if text == "still streaming"),
            "the newer tap keeps receiving: {got:?}"
        );
    }

    /// Untapping resolves no gates, and that is the difference between a
    /// window and the socket this replaced.
    ///
    /// A closed socket really was a reviewer who left, so it auto-approved the
    /// plan rather than hanging the turn. A window that stopped listening has
    /// not necessarily gone away — it re-taps every time the user looks at
    /// another chat — and approving a plan because of that is a decision nobody
    /// made.
    #[test]
    fn looking_at_another_chat_does_not_answer_a_held_gate() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let generation = {
            let (tx, _rx) = mpsc::unbounded_channel();
            shared.tap(tx)
        };

        let (gate, mut verdict) = PlanGate::open();
        shared.handle_event(AgentEvent::PlanReady {
            plan: "plan".to_string(),
            gate,
        });
        assert_eq!(shared.state(), TaskState::NeedsInput);

        shared.untap(generation);
        assert!(
            verdict.try_recv().is_err(),
            "the plan is still waiting for a person"
        );
        assert_eq!(shared.state(), TaskState::NeedsInput);

        // And it is still answerable afterwards, from the tap that came next.
        assert!(shared.resolve_plan(PlanVerdict::approve()));
        assert!(verdict.blocking_recv().expect("verdict delivered").approved);
    }

    #[test]
    fn one_turn_slot_per_task() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        assert!(!shared.try_begin_turn(), "second claim is refused");
        shared.finish_turn(false);
        assert!(shared.try_begin_turn(), "slot frees on turn end");
    }

    #[test]
    fn ensure_turn_active_reclaims_after_finish() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.finish_turn(false);
        assert!(!shared.is_turn_active());
        // A queued-behind turn arrives after the previous finish_turn: the
        // worker re-claims before begin_turn so mid-turn submits still see an
        // active slot.
        shared.ensure_turn_active();
        assert!(shared.is_turn_active());
        assert!(!shared.try_begin_turn(), "already claimed");
    }

    #[test]
    fn plan_gate_resolves_when_the_window_answers() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (gate, verdict) = PlanGate::open();
        shared.handle_event(AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            gate,
        });
        assert_eq!(shared.state(), TaskState::NeedsInput);

        assert!(shared.resolve_plan(PlanVerdict::reject("smaller steps")));
        assert_eq!(shared.state(), TaskState::Working);
        let got = verdict.blocking_recv().expect("verdict delivered");
        assert!(!got.approved);
        assert_eq!(got.feedback, "smaller steps");
        assert!(
            !shared.resolve_plan(PlanVerdict::approve()),
            "gate is spent"
        );
    }

    #[test]
    fn an_interview_is_answerable_and_declinable_exactly_once() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (gate, answers) = InterviewGate::open();
        shared.handle_event(AgentEvent::Interview {
            questions: Vec::new(),
            gate,
        });
        assert_eq!(shared.state(), TaskState::NeedsInput);

        assert!(shared.resolve_interview(None));
        assert_eq!(shared.state(), TaskState::Working);
        assert_eq!(answers.blocking_recv().expect("resolved"), None);
        assert!(!shared.resolve_interview(None), "gate is spent");
    }

    #[test]
    fn a_turn_that_ended_in_an_error_leaves_the_task_failed() {
        assert!(!turn_failed(DoneReason::Completed, false, false));
        assert!(!turn_failed(DoneReason::Stopped, false, false));
        assert!(!turn_failed(DoneReason::MaxSteps, false, false));
        assert!(turn_failed(DoneReason::CircuitBreaker, false, false));
        assert!(turn_failed(DoneReason::TimeLimit, false, false));
        // A stop after an error is a failed turn — unless the user asked for
        // the stop, which stays a cancellation.
        assert!(turn_failed(DoneReason::Stopped, true, false));
        assert!(!turn_failed(DoneReason::Stopped, true, true));
        assert!(!turn_failed(DoneReason::Stopped, false, true));
        // An error the turn recovered from does not taint its completion.
        assert!(!turn_failed(DoneReason::Completed, true, false));
    }

    #[test]
    fn provider_error_ends_the_turn_failed_not_cancelled() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        // The agent's provider-failure shape: `Error` then `Done{Stopped}`.
        shared.handle_event(AgentEvent::Error("provider exploded".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Stopped,
        });
        shared.finish_turn(false);
        assert_eq!(shared.state(), TaskState::Failed);
    }

    #[test]
    fn a_cancelled_turn_goes_idle_not_failed() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();

        shared.cancel_turn();
        // Even an error emitted while unwinding stays a cancellation.
        shared.handle_event(AgentEvent::Error("interrupted".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Stopped,
        });
        shared.finish_turn(false);
        assert_eq!(shared.state(), TaskState::Idle);

        // The flags are per-turn: an errored stop on the next turn is not
        // masked by the previous turn's cancel.
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::Error("provider exploded".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Stopped,
        });
        shared.finish_turn(false);
        assert_eq!(shared.state(), TaskState::Failed);
    }

    #[test]
    fn agent_config_resolves_provider_names_and_model_tags() {
        let base = Config {
            providers: vec![
                crate::config::ProviderConfig {
                    name: "local".to_string(),
                    kind: crate::config::ProviderKind::LLAMACPP,
                    base_url: "http://127.0.0.1:11435".to_string(),
                    model: "qwen3.6:27b".to_string(),
                    api_key_env: None,
                    gguf_path: None,
                    usd_per_mtok_in: None,
                    usd_per_mtok_out: None,
                },
                crate::config::ProviderConfig {
                    name: "claude".to_string(),
                    kind: crate::config::ProviderKind::ANTHROPIC,
                    base_url: "https://api.anthropic.com".to_string(),
                    model: "claude-fable-5".to_string(),
                    api_key_env: None,
                    gguf_path: None,
                    usd_per_mtok_in: None,
                    usd_per_mtok_out: None,
                },
            ],
            ..Default::default()
        };

        // No override: the config is the user's own, untouched.
        let config = agent_config(&base, None);
        assert_eq!(config.mode, base.mode);
        assert_eq!(config.active().name, "local");

        // A provider name switches the active provider.
        let config = agent_config(&base, Some("claude"));
        assert_eq!(config.active().name, "claude");
        assert_eq!(config.active().model, "claude-fable-5");

        // Anything else is a model tag on the active provider.
        let config = agent_config(&base, Some("qwen3.6:32b"));
        assert_eq!(config.active().name, "local");
        assert_eq!(config.active().model, "qwen3.6:32b");
    }

    // --- the preprocessing seam ---

    #[test]
    fn a_gui_message_gets_the_file_refs_and_custom_commands_every_surface_has() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("notes.md"), "the note\n").unwrap();
        std::fs::create_dir_all(cwd.path().join(".wizard/commands")).unwrap();
        std::fs::write(
            cwd.path().join(".wizard/commands/review.md"),
            "Review $ARGUMENTS against @notes.md",
        )
        .unwrap();

        // An `@file` reference is read into the prompt.
        let prompt = turn_prompt(
            TurnRequest {
                text: "explain @notes.md".to_string(),
                ..TurnRequest::default()
            },
            cwd.path(),
        );
        assert!(prompt.text.contains("the note"), "got: {}", prompt.text);

        // A project custom command expands to its template — arguments and the
        // `@file` refs inside it included.
        let prompt = turn_prompt(
            TurnRequest {
                text: "/review src/app.rs".to_string(),
                ..TurnRequest::default()
            },
            cwd.path(),
        );
        assert!(
            prompt.text.starts_with("Review src/app.rs against"),
            "got: {}",
            prompt.text
        );
        assert!(prompt.text.contains("the note"), "got: {}", prompt.text);
    }

    #[test]
    fn attachments_reach_the_turn_as_images_and_file_refs() {
        let cwd = tempfile::tempdir().unwrap();
        let spec = cwd.path().join("spec.txt");
        std::fs::write(&spec, "the spec\n").unwrap();
        let shot = cwd.path().join("shot.png");
        std::fs::write(&shot, [0x89, b'P', b'N', b'G']).unwrap();

        let prompt = turn_prompt(
            TurnRequest {
                text: "what is wrong here?".to_string(),
                images: vec![shot.clone()],
                files: vec![spec],
                ..TurnRequest::default()
            },
            cwd.path(),
        );
        // The file is pulled in by the `@file` expansion, not by a second
        // file-reading path.
        assert!(prompt.text.starts_with("what is wrong here?"));
        assert!(prompt.text.contains("the spec"), "got: {}", prompt.text);
        // The image rides along as an attachment for the vision path.
        assert_eq!(prompt.images, vec![shot]);
    }

    // --- the context meter ---

    /// A reading nothing emitted an event for still reaches the meter.
    ///
    /// The events the agent produces on its own — [`AgentEvent::Usage`] and
    /// [`AgentEvent::ContextSize`] — are pure relay here, and the window folds
    /// them in [`crate::native::rail::Meter`]. This is the other half: after a
    /// turn against a provider that reports no token counts, after a compaction
    /// and after a rewind, the history changed size with no event to say so, and
    /// [`TaskShared::push_context`] is what says it.
    #[test]
    fn a_reading_no_event_carried_is_synthesized_onto_the_tap() {
        let shared = shared();
        let mut rx = tap(&shared);
        assert!(shared.try_begin_turn());
        shared.begin_turn();

        // The agent's own events pass straight through, unfolded.
        shared.handle_event(AgentEvent::Usage {
            prompt_tokens: 1_600,
            completion_tokens: 30,
        });
        // Compaction: the history shrank, and no event announced it.
        shared.push_context(300);

        let got = drain(&mut rx);
        assert!(
            matches!(
                got[0],
                AgentEvent::Usage {
                    prompt_tokens: 1_600,
                    ..
                }
            ),
            "{got:?}"
        );
        assert!(
            matches!(got[1], AgentEvent::ContextSize { tokens: 300 }),
            "the synthesized reading is the same event a turn would carry: {got:?}"
        );
        assert_eq!(got.len(), 2, "and nothing is said twice: {got:?}");
    }

    // --- server-side slash commands ---

    /// A provider that answers nothing: the command tests drive the agent's own
    /// state (history, session, usage), which needs no model call.
    #[derive(Debug)]
    struct SilentProvider;

    #[async_trait::async_trait]
    impl crate::llm::provider::LlmProvider for SilentProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(
            &self,
            _request: crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatStream> {
            anyhow::bail!("no model behind this test")
        }
        fn label(&self) -> String {
            "silent".to_string()
        }
    }

    fn test_agent(cwd: &Path) -> Agent {
        let sessions = Config::sessions_dir().expect("sessions dir");
        let session = Session::create_in(&sessions, cwd).expect("session");
        let hooks = Arc::new(crate::hooks::HookEngine::new(
            Vec::new(),
            cwd.to_path_buf(),
            session.id.clone(),
        ));
        Agent::new(
            Arc::new(SilentProvider),
            crate::tools::registry::ToolRegistry::new(),
            Config::default(),
            Vec::new(),
            cwd.to_path_buf(),
            session,
            true,
            hooks,
        )
        .expect("agent")
    }

    /// Run one command against `agent`, as either a client `command` frame or a
    /// line the agent queued: the executor is the same for both.
    async fn command(agent: &mut Agent, shared: &Arc<TaskShared>, name: &str, args: &str) {
        command_in(agent, shared, &Config::default(), name, args).await;
    }

    async fn command_in(
        agent: &mut Agent,
        shared: &Arc<TaskShared>,
        config: &Config,
        name: &str,
        args: &str,
    ) {
        let request = CommandRequest {
            name: name.to_string(),
            args: args.to_string(),
        };
        apply(agent, shared, config, request).await;
    }

    async fn apply(
        agent: &mut Agent,
        shared: &Arc<TaskShared>,
        config: &Config,
        request: CommandRequest,
    ) {
        let mcp = Arc::new(RwLock::new(McpManager::empty()));
        let mut fusion = false;
        apply_command(
            agent,
            request,
            &mut CommandCtx {
                config,
                shared,
                mcp: &mcp,
                fusion: &mut fusion,
            },
        )
        .await;
    }

    /// The first `notice` a command produced, or a panic naming what came out
    /// instead — a command that errors where a notice was expected is a failure
    /// worth reading.
    fn notice_text(events: &[AgentEvent]) -> String {
        notices(events)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("expected a notice, got: {events:?}"))
    }

    /// The error messages a stream carried, in order.
    fn errors(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Error(message) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// The first `error` a command produced, or a panic naming what came out
    /// instead.
    fn error_text(events: &[AgentEvent]) -> String {
        errors(events)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("expected an error, got: {events:?}"))
    }

    /// `Agent::clear` rotates the session file, and a task is keyed by its
    /// session id: clearing against the agent would leave the chat pointing at a
    /// session the agent had just stopped writing to, and the sidebar row would
    /// go quiet while the conversation carried on somewhere else. So `/clear`
    /// opens a new chat instead, in the window.
    #[tokio::test]
    async fn clear_is_not_an_agent_command_because_it_would_strand_the_session() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);
        let before = agent.session().id.clone();

        command(&mut agent, &shared, "clear", "").await;

        // Refused like any other command this half does not run, and the session
        // the task is keyed by is left alone.
        assert_eq!(agent.session().id, before);
        let got = drain(&mut rx);
        assert!(
            !errors(&got).is_empty(),
            "clear is not dispatched against the agent: {got:?}"
        );

        assert_eq!(
            crate::commands::spec("clear").map(|spec| spec.gui),
            Some(Execution::Ui),
            "and the palette routes it to the window"
        );
    }

    #[tokio::test]
    async fn compact_summarizes_and_reports_the_new_context_size() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        // A history with nothing between the system prompt and the recent tail
        // has nothing to compact — and says so, rather than failing silently.
        command(&mut agent, &shared, "compact", "").await;
        let got = drain(&mut rx);
        assert!(notice_text(&got).contains("compact"), "got: {got:?}");
        assert!(
            got.iter()
                .any(|event| matches!(event, AgentEvent::ContextSize { .. })),
            "the meter is refreshed either way: {got:?}"
        );
    }

    #[tokio::test]
    async fn cost_reports_the_lifetime_totals_and_unknown_commands_error() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        let _ = drain(&mut rx); // the attach snapshot

        agent.usage().record(Some(1_200), Some(300));
        command(&mut agent, &shared, "cost", "").await;
        let text = notice_text(&drain(&mut rx));
        assert!(
            text.contains("1200 prompt + 300 completion"),
            "the session total, not the last call: {text}"
        );

        // `/mode` switches the posture the turn runs in.
        command(&mut agent, &shared, "mode", "sovereign").await;
        assert_eq!(agent.mode(), Mode::Sovereign);
        assert!(!notices(&drain(&mut rx)).is_empty());

        // The one parser: a bad argument is rejected here exactly as at the
        // TUI's prompt, in the same words.
        command(&mut agent, &shared, "mode", "yolo").await;
        assert!(
            error_text(&drain(&mut rx)).contains("unknown mode 'yolo'"),
            "the parser's own words"
        );

        // `/model` without the model it needs.
        command(&mut agent, &shared, "model", "").await;
        assert!(!errors(&drain(&mut rx)).is_empty());

        // A word that is no command at all.
        command(&mut agent, &shared, "frobnicate", "").await;
        assert!(error_text(&drain(&mut rx)).contains("frobnicate"));
    }

    /// `/mode` moves the posture; plan mode is a stance held *on top* of a mode,
    /// so it survives the switch. The TUI does the same, and a plan the user set
    /// up should not evaporate because they moved to sovereign.
    #[tokio::test]
    async fn switching_mode_keeps_the_plan_stance() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();

        command(&mut agent, &shared, "plan", "").await;
        assert!(agent.plan_mode());
        command(&mut agent, &shared, "mode", "sovereign").await;
        assert_eq!(agent.mode(), Mode::Sovereign);
        assert!(agent.plan_mode(), "the stance is not the mode's to drop");
    }

    // --- the commands the GUI grew ---

    /// The one the user noticed was missing. `/goal` is the standing mission in
    /// `<cwd>/.wizard/mission.toml` — the same file the TUI writes, so a goal set
    /// in the browser drives a sovereign run started from the terminal.
    #[tokio::test]
    async fn goal_reports_the_mission_sets_it_and_notes_the_change() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared_in(cwd.path());
        let mut rx = tap(&shared);

        command(&mut agent, &shared, "goal", "").await;
        assert!(
            notice_text(&drain(&mut rx)).contains("no standing goal set"),
            "an unset goal says so"
        );

        command(&mut agent, &shared, "goal", "ship the release").await;
        assert!(notice_text(&drain(&mut rx)).contains("ship the release"));
        let mission = crate::agent::mission::Mission::load(cwd.path())
            .expect("load")
            .expect("a mission on disk");
        assert_eq!(mission.goal, "ship the release");

        // Setting it again keeps the mission's history rather than replacing it.
        command(&mut agent, &shared, "goal", "cut a patch release").await;
        let _ = drain(&mut rx);
        let text = {
            command(&mut agent, &shared, "goal", "").await;
            notice_text(&drain(&mut rx))
        };
        assert!(text.contains("goal: cut a patch release"), "got: {text}");
        assert!(
            text.contains("goal changed to: cut a patch release"),
            "the change is noted, not silently overwritten: {text}"
        );
    }

    #[tokio::test]
    async fn effort_plan_and_omakase_toggle_the_agent_they_name() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        command(&mut agent, &shared, "effort", "high").await;
        assert!(notice_text(&drain(&mut rx)).contains("high"));
        command(&mut agent, &shared, "effort", "turbo").await;
        assert!(!errors(&drain(&mut rx)).is_empty());

        command(&mut agent, &shared, "plan", "").await;
        assert!(agent.plan_mode());
        assert!(notice_text(&drain(&mut rx)).contains("plan mode on"));
        command(&mut agent, &shared, "plan", "").await;
        assert!(!agent.plan_mode());
        let _ = drain(&mut rx);

        // Omakase implies plan mode: it is the flavor of it where the agent
        // approves its own plan.
        command(&mut agent, &shared, "omakase", "").await;
        assert!(agent.omakase());
        assert!(agent.plan_mode(), "omakase is plan mode, chef's choice");
        assert!(notice_text(&drain(&mut rx)).contains("omakase on"));
    }

    /// `/genie` and `/sovereign` are the `/mode` aliases, and reach the same
    /// executor through the same parser.
    #[tokio::test]
    async fn the_mode_aliases_switch_the_mode() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();

        command(&mut agent, &shared, "sovereign", "").await;
        assert_eq!(agent.mode(), Mode::Sovereign);
        command(&mut agent, &shared, "genie", "").await;
        assert_eq!(agent.mode(), Mode::Genie);
    }

    #[tokio::test]
    async fn status_reports_the_session_and_bashes_and_agents_report_theirs() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        agent.usage().record(Some(10), Some(4));
        command(&mut agent, &shared, "status", "").await;
        let text = notice_text(&drain(&mut rx));
        for expected in [
            "model: test-model",
            "mode: genie",
            "effort: default",
            "10 prompt + 4 completion",
            "plan mode: off",
            "todos: none",
        ] {
            assert!(
                text.contains(expected),
                "status is missing {expected}: {text}"
            );
        }
        assert!(
            text.contains(&agent.session().id),
            "and names the session it is talking about: {text}"
        );

        // No background `execute` has run, and the report says that rather than
        // printing an empty list.
        command(&mut agent, &shared, "bashes", "").await;
        assert_eq!(notice_text(&drain(&mut rx)), "background tasks: none");

        // The roster is whatever is installed; it must at least come back as a
        // notice rather than an error.
        command(&mut agent, &shared, "agents", "").await;
        assert!(!notices(&drain(&mut rx)).is_empty());
    }

    #[tokio::test]
    async fn memory_and_doctor_answer_with_a_notice() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared_in(cwd.path());
        let mut rx = tap(&shared);

        command(&mut agent, &shared, "memory", "").await;
        assert!(
            notice_text(&drain(&mut rx)).contains("no memories saved yet"),
            "an empty store says so, with the directory it looked in"
        );

        command(&mut agent, &shared, "doctor", "").await;
        assert!(notice_text(&drain(&mut rx)).starts_with("doctor:"));
    }

    /// `/reload` recomposes the tool set against the *shared* MCP manager. It
    /// must come back with a live registry — a reload that dropped the agent's
    /// tools would be worse than no reload at all.
    #[tokio::test]
    async fn reload_recomposes_the_tools_and_skills() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        command(&mut agent, &shared, "reload", "").await;
        let text = notice_text(&drain(&mut rx));
        assert!(text.starts_with("reloaded: "), "got: {text}");
        assert!(
            !text.contains("reloaded: 0 tools"),
            "the reloaded agent has its tools: {text}"
        );
    }

    /// Bare `/rewind` lists what there is to go back to; `/rewind <turn>`
    /// restores the files and truncates the conversation, and says so.
    #[tokio::test]
    async fn rewind_lists_candidates_then_restores_the_files() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        command(&mut agent, &shared, "rewind", "").await;
        assert_eq!(
            notice_text(&drain(&mut rx)),
            "nothing to rewind yet",
            "a session with no turns has nothing to offer"
        );

        // A turn to go back to, and a file the turn changed.
        let file = cwd.path().join("edited.txt");
        std::fs::write(&file, "before").unwrap();
        let turn = agent.checkpoints().begin_turn();
        agent
            .checkpoints()
            .snapshot(turn, "write_file", &file)
            .unwrap();
        std::fs::write(&file, "after").unwrap();

        command(&mut agent, &shared, "rewind", "").await;
        let text = notice_text(&drain(&mut rx));
        assert!(text.contains("edited.txt"), "the turn is listed: {text}");

        command(&mut agent, &shared, "rewind", &turn.to_string()).await;
        let got = drain(&mut rx);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "before",
            "the file is restored"
        );
        assert!(notice_text(&got).contains(&format!("rewound to before turn {turn}")));
        assert!(
            got.iter()
                .any(|event| matches!(event, AgentEvent::ContextSize { .. })),
            "and the meter follows the conversation that shrank: {got:?}"
        );
    }

    /// `/fusion` with nothing to fuse refuses and says what is missing, rather
    /// than reporting a panel that is not there.
    #[tokio::test]
    async fn fusion_without_providers_refuses_honestly() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        command(&mut agent, &shared, "fusion", "").await;
        assert!(error_text(&drain(&mut rx)).contains("fusion needs"));

        // The panel *editor* is a TUI picker; the refusal points at the file to
        // change instead of pretending to open one.
        command(&mut agent, &shared, "fusion", "config").await;
        assert!(error_text(&drain(&mut rx)).contains("config.toml"));
    }

    /// `/server` manages the local llama-server. On a provider that is not
    /// llama.cpp there is no server to manage, and it says so instead of probing
    /// a URL that answers for something else.
    #[tokio::test]
    async fn server_refuses_when_the_provider_is_not_llamacpp() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        let mut config = Config::default();
        config.providers.push(crate::config::ProviderConfig {
            name: "openai".to_string(),
            kind: ProviderKind::OPENAI,
            base_url: "https://example.test/v1".to_string(),
            model: "m".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        });

        command_in(&mut agent, &shared, &config, "server", "status").await;
        assert!(error_text(&drain(&mut rx)).contains("local llama-server"));
    }

    /// A command the *window* owns, submitted here by mistake, is answered —
    /// not swallowed. Same for one that only ever runs in a terminal: the
    /// refusal says what the command is, rather than "unknown command".
    #[tokio::test]
    async fn window_and_terminal_commands_are_refused_by_name() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let mut rx = tap(&shared);

        // Derived from the table, not listed here: a new window-owned command is
        // covered the moment its row says so, which is the whole point of the
        // column.
        let window_owned: Vec<&str> = crate::commands::commands_for(Surface::Gui, Execution::Ui)
            .map(|spec| spec.name)
            .collect();
        assert!(window_owned.contains(&"diff") && window_owned.contains(&"resume"));
        for name in window_owned {
            // `/login` is the one window-owned row whose bare form is a usage
            // error at the parser, before any surface sees it.
            let args = if name == "login" { "xai" } else { "" };
            command(&mut agent, &shared, name, args).await;
            let got = drain(&mut rx);
            assert!(
                error_text(&got).contains("part of the window"),
                "/{name} is the window's own: {got:?}"
            );
        }

        // And the terminal-only rows say what the command *is*, by name.
        command(&mut agent, &shared, "vim", "").await;
        assert!(
            error_text(&drain(&mut rx)).contains("modal editing"),
            "the refusal says what /vim is"
        );

        command(&mut agent, &shared, "quit", "").await;
        assert!(
            error_text(&drain(&mut rx)).contains("close it"),
            "and what /quit would have meant"
        );
    }

    // --- the agent's own slash commands ---

    /// The set the tool gates on is the set the executor implements, filtered by
    /// the gate the TUI applies to the same request. Let them drift and the agent
    /// is either refused a command this surface runs, or told "queued" for one it
    /// will answer with an error after the turn.
    #[test]
    fn the_commands_the_agent_may_queue_are_the_ones_the_executor_runs() {
        for name in agent_commands() {
            let spec = crate::commands::spec(name).expect("a table entry");
            assert_eq!(
                spec.gui,
                Execution::Agent,
                "/{name} is offered to the agent, so this surface must execute it"
            );
            let line = format!("/{} {}", spec.name, spec.agent_arg);
            let command = SlashCommand::parse(&line)
                .unwrap_or_else(|| panic!("/{name} parses"))
                .unwrap_or_else(|err| panic!("/{name} parses: {err}"));
            assert!(
                command.agent_runnable().is_ok(),
                "/{name} is offered to the agent, but the gate refuses it"
            );
        }

        // And nothing the executor runs is withheld from the agent unless the
        // gate is what withholds it.
        for spec in crate::commands::commands_for(crate::commands::Surface::Gui, Execution::Agent) {
            if agent_commands().contains(&spec.name) {
                continue;
            }
            let line = format!("/{} {}", spec.name, spec.agent_arg);
            let refused = match SlashCommand::parse(&line) {
                Some(Ok(command)) => command.agent_runnable().is_err(),
                _ => true,
            };
            assert!(
                refused,
                "/{} runs on this surface and the gate allows it — so the agent should have it",
                spec.name
            );
        }
    }

    /// The user's `/rewind` restores checkpoints; the agent's does not. It is the
    /// sharpest case of a command this surface runs that the model still may not.
    #[test]
    fn the_agent_may_not_rewind_a_session_the_window_can() {
        assert_eq!(
            crate::commands::spec("rewind").map(|spec| spec.gui),
            Some(Execution::Agent)
        );
        assert!(
            !agent_commands().contains(&"rewind"),
            "the executor runs it for the user, never for the model"
        );
    }

    #[test]
    fn a_queued_line_splits_into_the_name_and_arguments_the_executor_takes() {
        let request = parse_command_line("/model claude-sonnet-5");
        assert_eq!(request.name, "model");
        assert_eq!(request.args, "claude-sonnet-5");

        let request = parse_command_line("/compact");
        assert_eq!(request.name, "compact");
        assert_eq!(request.args, "");
    }

    /// The turn holds `&mut Agent`, so a command the agent calls for is queued
    /// and applied the moment the borrow ends — the TUI's `pending_agent_commands`
    /// for the same reason. What the model is told is *not* deferred: the tool
    /// refuses anything this surface cannot run before it ever gets here.
    #[tokio::test]
    async fn a_command_the_agent_calls_for_runs_when_the_turn_releases_the_agent() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let mut rx = tap(&shared);

        // Mid-turn: the tool's event lands, and says so — but nothing is applied
        // while the turn owns the agent.
        shared.handle_event(AgentEvent::CommandRequested("/mode sovereign".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Completed,
        });
        assert_ne!(agent.mode(), Mode::Sovereign, "not while the turn runs");
        let queued = drain(&mut rx);
        assert!(
            queued.iter().any(|event| matches!(
                event,
                AgentEvent::CommandRequested(line) if line == "/mode sovereign"
            )),
            "the request reaches the window as the event it is: {queued:?}"
        );

        // The worker drains the queue the moment the turn returns.
        for line in shared.take_pending_commands() {
            apply(
                &mut agent,
                &shared,
                &Config::default(),
                parse_command_line(&line),
            )
            .await;
        }
        shared.finish_turn(false);

        assert_eq!(agent.mode(), Mode::Sovereign, "it took effect");
        let applied = drain(&mut rx);
        assert!(
            notices(&applied).contains(&"switched to sovereign mode".to_string()),
            "and its effect is reported, exactly as a typed command's is: {applied:?}"
        );
        assert!(
            shared.take_pending_commands().is_empty(),
            "the queue is drained, not replayed on the next turn"
        );
    }

    // --- MCP servers ---

    /// One connected manager for the process, shared by every task. Connecting
    /// inside each agent build would give a window with four warm chats four
    /// copies of every configured MCP server — four filesystem servers, four
    /// browser servers, each a real OS process. The workers hold the manager the
    /// window connected, which is what the strong count says.
    #[tokio::test]
    async fn every_task_shares_the_one_connected_mcp_manager() {
        let cwd = tempfile::tempdir().unwrap();
        let sessions = cwd.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mcp = Arc::new(RwLock::new(McpManager::empty()));
        let tasks = TaskManager::with_registry(
            Arc::new(ConfigStore::new(Config::default())),
            Arc::clone(&mcp),
            None,
        );
        assert_eq!(Arc::strong_count(&mcp), 2, "ours and the manager's");

        for _ in 0..2 {
            let session = Session::create_in(&sessions, cwd.path()).expect("session");
            tasks.spawn(session.id.clone(), cwd.path().to_path_buf(), session);
        }
        assert_eq!(
            Arc::strong_count(&mcp),
            4,
            "each worker holds the manager the server connected, not one it \
             connected for itself"
        );
    }

    // --- the session registry ---

    #[test]
    fn a_failed_turn_still_heartbeats_as_a_live_idle_session() {
        // The registry's `Failed` marks a *finished* background run and is kept
        // for a day; a task whose turn errored is live, and the next message
        // retries it. The TUI publishes the same three states.
        assert_eq!(registry_state(TaskState::Working), SessionState::Working);
        assert_eq!(
            registry_state(TaskState::NeedsInput),
            SessionState::NeedsInput
        );
        assert_eq!(registry_state(TaskState::Idle), SessionState::Idle);
        assert_eq!(registry_state(TaskState::Failed), SessionState::Idle);
    }

    /// A window's chat is a running Wizard session: `/dashboard` — in any other
    /// instance on the machine — must see it while it is alive, and must not see
    /// it once it is gone.
    #[tokio::test]
    async fn a_live_task_is_in_the_session_registry_until_it_ends() {
        let cwd = tempfile::tempdir().unwrap();
        let sessions = cwd.path().join("sessions");
        let running = cwd.path().join("running");
        std::fs::create_dir_all(&sessions).unwrap();
        let tasks = TaskManager::with_registry(
            Arc::new(ConfigStore::new(Config::default())),
            Arc::new(RwLock::new(McpManager::empty())),
            Some(running.clone()),
        );

        let session = Session::create_in(&sessions, cwd.path()).expect("session");
        let id = session.id.clone();
        let shared = tasks.spawn(id.clone(), cwd.path().to_path_buf(), session);

        // Live from the moment the task exists, idle and named for its workspace.
        let listed = session_registry::list_from(&running);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].state, SessionState::Idle);
        assert_eq!(listed[0].cwd, cwd.path().display().to_string());
        assert_eq!(listed[0].pid, std::process::id());

        // The turn's transitions are published as they happen, so a dashboard
        // does not wait a heartbeat to see the task go to work.
        shared.name_after_first_message("fix the parser\nand the tests");
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: json!({ "path": "src/app.rs" }),
        });
        shared.publish();
        let listed = session_registry::list_from(&running);
        assert_eq!(listed[0].state, SessionState::Working);
        assert_eq!(listed[0].name, "fix the parser");
        assert_eq!(listed[0].activity, "read_file");

        // A gate is a session that needs its user — the state `/dashboard` sorts
        // to the top — and resolving it goes straight back to working.
        let (gate, _verdict) = PlanGate::open();
        shared.handle_event(AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            gate,
        });
        let listed = session_registry::list_from(&running);
        assert_eq!(listed[0].state, SessionState::NeedsInput);
        assert_eq!(listed[0].activity, "waiting for plan approval");
        assert!(shared.resolve_plan(PlanVerdict::approve()));
        assert_eq!(
            session_registry::list_from(&running)[0].state,
            SessionState::Working
        );

        shared.finish_turn(false);
        assert_eq!(
            session_registry::list_from(&running)[0].state,
            SessionState::Idle
        );

        // And it leaves the registry when the window closes, rather than sitting
        // there claiming to be running until it ages out.
        tasks.shutdown();
        assert!(session_registry::list_from(&running).is_empty());
    }

    // --- the watcher ---

    /// The gate stays with `TaskShared`. A watcher that claimed the ticket
    /// itself would take the reply channel out of `pending_plan`, and
    /// `resolve_plan` — the only thing the window has to answer with — would
    /// then refuse and park the turn forever.
    #[test]
    fn a_watcher_sees_a_plan_gate_but_does_not_own_it() {
        let shared = shared();
        let mut events_rx = tap(&shared);

        let (gate, mut wait) = PlanGate::open();
        shared.handle_event(AgentEvent::PlanReady {
            plan: "do the thing".to_string(),
            gate,
        });

        assert!(
            matches!(events_rx.try_recv(), Ok(AgentEvent::PlanReady { plan, .. }) if plan == "do the thing"),
            "the watcher has to see the request"
        );
        // The ticket it received is spent: the claim happened inside
        // `handle_event`, which is what `resolve_plan` answers through.
        assert!(shared.resolve_plan(PlanVerdict::approve()));
        assert!(wait.try_recv().expect("a verdict").approved);
    }

    /// A window that closed while a turn was running must not wedge the task:
    /// the send fails, the watcher is dropped, and the turn carries on writing
    /// its session file.
    #[test]
    fn a_dropped_watcher_does_not_stop_the_turn() {
        let shared = shared();
        {
            let (tx, _rx) = mpsc::unbounded_channel();
            shared.tap(tx);
        }
        assert!(shared.has_watcher());
        shared.handle_event(AgentEvent::TextDelta("hello".to_string()));
        assert!(!shared.has_watcher(), "the dead channel is let go");
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Completed,
        });
    }
}
