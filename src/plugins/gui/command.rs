//! The one place a slash command is applied to a chat's live [`Agent`].
//!
//! Split out of [`super::tasks`], which is the file everything needs and was
//! four thousand lines of two separable jobs: **owning** the sessions (the
//! manager, the worker, the event fan-out, the gates) and **executing** a
//! command against one. This is the second job.
//!
//! # It is one executor for two callers
//!
//! Every [`Execution::Agent`](crate::commands::Execution) command typed at the
//! window lands in [`apply_command`] — the window submits it through
//! [`TaskManager::submit_command`](super::tasks::TaskManager::submit_command)
//! rather than growing a handler of its own — and so does every `/…` the agent
//! asked for through `run_command`. [`GuiSurface`] is the [`CommandSurface`]
//! both run against, so what `/model` means is written once.
//!
//! Everything it has to say comes back as an [`AgentEvent`] on the task's tap:
//! the same `Notice` and `Error` a turn carries, folded into the same
//! transcript. A command needs no reply channel of its own.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{RwLock, mpsc};

use crate::agent::{Agent, AgentEvent, RewindCandidate, ultra};
use crate::commands::surface::{CommandSurface, PlanState, SessionSnapshot, Surface, dispatch};
use crate::commands::{ServerAction, SlashCommand};
use crate::config::{Config, Mode, ReasoningEffort};
use crate::llm::provider::LlmProvider;
use crate::mcp::McpManager;

use super::tasks::{CommandRequest, TaskShared, drain_finished, switch_model};

/// What the command executor may reach besides the agent itself.
pub(super) struct CommandCtx<'a> {
    /// The task's config, as its agent was built from.
    pub config: &'a Config,
    pub shared: &'a Arc<TaskShared>,
    /// The process-wide MCP manager. `/reload` re-registers against *this* one:
    /// connecting a second set would leave the GUI running two copies of every
    /// configured server, each a real OS process.
    pub mcp: &'a Arc<RwLock<McpManager>>,
    /// Whether this task's turns currently run through the fusion panel
    /// (`/fusion`), so the toggle knows which way it is toggling. Warm-agent
    /// state, like the mode and the plan flag: an evicted task rebuilds on the
    /// configured provider.
    pub fusion: &'a mut bool,
}

/// Apply one slash command to the live agent. Everything it has to say comes
/// back as an [`AgentEvent`] on the task's tap, so a command needs no reply
/// channel of its own.
///
/// The one executor: a command typed at the window and a `/…` the agent asked
/// for through `run_command` both land here, are parsed by the one parser, and
/// run through the one dispatcher
/// ([`crate::commands::surface::dispatch`]). What each command *means* is
/// written there, once, for every surface; what is written here is only what
/// this half of this surface can do about it.
pub(super) async fn apply_command(
    agent: &mut Agent,
    request: CommandRequest,
    ctx: &mut CommandCtx<'_>,
) {
    let line = match request.args.trim() {
        "" => format!("/{}", request.name),
        args => format!("/{} {args}", request.name),
    };

    // The one parser, so an argument means here exactly what it means at the
    // TUI's prompt — including the errors it rejects a bad one with.
    let command = match SlashCommand::parse(&line) {
        Some(Ok(command)) => command,
        Some(Err(message)) => return error(ctx.shared, message),
        None => return error(ctx.shared, format!("'{line}' is not a slash command")),
    };

    let mut surface = GuiSurface { agent, ctx };
    dispatch(command, &mut surface).await;
}

/// The agent-owning half of the one dispatcher.
///
/// *Half*, deliberately: the window owns the panels, the overlays and the chat
/// list, and this side owns the agent. So the window-owning verbs are left at
/// their defaults, which answer honestly rather than pretending to be a window,
/// and the table's [`Execution::Ui`](crate::commands::Execution::Ui) column
/// routes them to [`crate::plugins::native::command::Native`] before they ever reach
/// here.
struct GuiSurface<'a, 'ctx> {
    agent: &'a mut Agent,
    ctx: &'a mut CommandCtx<'ctx>,
}

#[async_trait]
impl CommandSurface for GuiSurface<'_, '_> {
    fn surface(&self) -> Surface {
        Surface::Gui
    }

    fn project_root(&self) -> PathBuf {
        self.ctx.shared.cwd.clone()
    }

    fn notice(&mut self, text: String) {
        notice(self.ctx.shared, text);
    }

    fn error(&mut self, message: String) {
        error(self.ctx.shared, message);
    }

    fn snapshot(&self) -> SessionSnapshot {
        let provider = self.ctx.config.active();
        let (prompt_tokens, completion_tokens) = self.agent.usage().session_totals();
        let cache_tokens = self.agent.usage().session_cache_totals();
        SessionSnapshot {
            model: self.ctx.shared.model(),
            provider_name: provider.name.clone(),
            provider_kind: provider.kind,
            provider_base_url: provider.base_url.clone(),
            mode: self.agent.mode(),
            effort: self.ctx.config.reasoning_effort,
            max_steps: Some(self.ctx.config.max_steps),
            session: Some(self.agent.session().id.clone()),
            prompt_tokens,
            completion_tokens,
            cache_tokens: Some(cache_tokens),
            context_tokens: Some(self.agent.context_tokens()),
            background_tasks: Some(self.agent.running_tasks()),
            todos: crate::tools::todo::progress(&self.agent.todos()),
            plan: self.plan(),
            ultra: self.agent.ultra().then(|| "on".to_string()),
            usd_per_mtok_in: provider.usd_per_mtok_in,
            usd_per_mtok_out: provider.usd_per_mtok_out,
        }
    }

    fn plan(&self) -> PlanState {
        PlanState {
            plan: self.agent.plan_mode(),
            omakase: self.agent.omakase(),
        }
    }

    fn background_tasks(&self) -> Result<Vec<crate::tools::tasks::Task>, String> {
        Ok(self.agent.tasks())
    }

    fn rewind_candidates(&self) -> Vec<RewindCandidate> {
        self.agent.rewind_candidates(20)
    }

    async fn set_model(&mut self, tag: String) {
        switch_model(self.agent, self.ctx.config, &tag, self.ctx.shared).await;
    }

    /// Plan mode is a stance on top of a mode, not a property of one, so it
    /// survives the switch, as it does in the TUI.
    async fn set_mode(&mut self, mode: Mode) -> bool {
        self.agent.set_mode(mode);
        self.ctx.shared.set_mode(mode);
        true
    }

    async fn set_effort(&mut self, effort: Option<ReasoningEffort>) -> bool {
        self.agent.set_reasoning_effort(effort);
        true
    }

    async fn set_plan(&mut self, plan: PlanState) -> bool {
        self.agent.set_plan_mode(plan.plan);
        self.agent.set_omakase(plan.omakase);
        true
    }

    async fn compact(&mut self) {
        let outcome = self.agent.compact_now().await;
        let tokens = self.agent.context_tokens();
        notice(self.ctx.shared, outcome.describe());
        self.ctx.shared.push_context(tokens);
    }

    async fn reload(&mut self) {
        reload(self.agent, self.ctx).await;
    }

    async fn rewind(&mut self, turn: u64) {
        rewind(self.agent, turn, self.ctx.shared);
    }

    /// Runs against the live agent (commands wait for turns to finish on this
    /// surface). The exchange never enters history, the same contract as the TUI.
    async fn btw(&mut self, question: String) {
        notice(self.ctx.shared, "answering /btw…".to_string());
        match self.agent.answer_side_question(&question).await {
            Ok(answer) => notice(self.ctx.shared, format!("/btw {question}\n{answer}")),
            Err(err) => error(self.ctx.shared, format!("/btw failed: {err:#}")),
        }
    }

    /// Detach a side quest that inherits the full conversation. Progress
    /// streams through a collector into the same events a background subagent
    /// would; the report lands in history on the next drain (the end of this
    /// command path, or the next turn's top-of-loop).
    async fn fork(&mut self, task: String) {
        notice(self.ctx.shared, format!("forking: {task}"));
        let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
        let collector_shared = Arc::clone(self.ctx.shared);
        let collector = tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                collector_shared.handle_event(event);
            }
        });
        match self.agent.spawn_fork(&task, Some(events_tx)).await {
            Ok(id) => notice(self.ctx.shared, format!("fork #{id} started: {task}")),
            Err(err) => error(self.ctx.shared, format!("/fork failed: {err:#}")),
        }
        // Don't join the collector: the fork outlives this command and keeps
        // streaming. Dropping our end of the join handle is fine; the task ends
        // when the fork drops its last event sender.
        drop(collector);
        // Surface any already-finished background work (unlikely this soon, but
        // keeps the drain path consistent with the TUI idle tick).
        drain_finished(self.agent, self.ctx.shared);
    }

    /// This half owns no composer to put a kickoff turn in: the user sends the
    /// next message, and the dispatcher says so.
    async fn start_goal(&mut self, goal: String) -> bool {
        let _ = goal;
        false
    }

    /// The TUI detaches this and notices when it lands; here it runs in the
    /// command's own slot, so the chat shows the task working until it is done.
    async fn evolve(&mut self, deep: bool, description: String) {
        evolve(deep, description, self.ctx.config, self.ctx.shared).await;
    }

    async fn publish(&mut self, branch: Option<String>) {
        publish(branch, self.ctx.config, self.ctx.shared).await;
    }

    async fn toggle_fusion(&mut self) {
        toggle_fusion(self.agent, self.ctx).await;
    }

    async fn toggle_ultra(&mut self) {
        toggle_ultra(self.agent, self.ctx);
    }

    /// The llama-server is a process on this machine, and the window runs on
    /// that machine: the same command, answering into the chat instead of onto
    /// a status line.
    async fn server(&mut self, action: ServerAction) {
        server_command(action, self.ctx.config, self.ctx.shared).await;
    }
}

fn notice(shared: &TaskShared, text: impl Into<String>) {
    shared.notice(text);
}

fn error(shared: &TaskShared, message: impl Into<String>) {
    shared.error(message);
}

/// `/reload`: pick up skills, scripted tools, and MCP servers edited since the
/// session started, without a restart.
///
/// The MCP config reloads into the *shared* manager — the one every task's agent
/// was built against — so a reload re-registers the servers this process already
/// runs instead of starting a second set beside them.
async fn reload(agent: &mut Agent, ctx: &mut CommandCtx<'_>) {
    let shared = ctx.shared;
    let mut manager = ctx.mcp.write().await;
    match Config::mcp_config_path().and_then(|path| crate::mcp::McpConfig::load(&path)) {
        Ok(config) => {
            if let Err(err) = manager.reload(&config).await {
                notice(shared, format!("MCP reload warning: {err:#}"));
            }
        }
        Err(err) => notice(shared, format!("could not reload MCP config: {err:#}")),
    }
    let hooks = Arc::clone(agent.hooks());
    let client = Arc::clone(agent.client());
    match crate::agent::build_tool_registry(ctx.config, &client, &hooks, &manager).await {
        Ok((registry, subagent_model)) => {
            let tools = registry.len();
            let skills = crate::agent::load_skills();
            let count = skills.len();
            agent.set_registry(registry);
            agent.bind_subagent_model(subagent_model);
            agent.set_skills(skills);
            notice(shared, format!("reloaded: {tools} tools, {count} skills"));
        }
        Err(err) => error(shared, format!("reload failed: {err:#}")),
    }
}

/// `/rewind <turn>`: restore every file snapshot from `turn` onward and drop the
/// rewound turns from the conversation.
///
/// The turns are gone from the session file, and the transcript already drawn
/// still shows them. Saying so in a notice is the honest floor: the rewound
/// turns are announced, and the next turn is answered against the truncated
/// conversation the model actually has.
fn rewind(agent: &mut Agent, turn: u64, shared: &TaskShared) {
    match agent.rewind_to(turn) {
        Ok(restored) => {
            let files = match restored.is_empty() {
                true => "no files needed restoring".to_string(),
                false => format!(
                    "restored {}",
                    restored
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            notice(
                shared,
                format!("rewound to before turn {turn} — {files}; conversation truncated"),
            );
            shared.push_context(agent.context_tokens());
        }
        Err(err) => error(shared, format!("rewind failed: {err:#}")),
    }
}

/// `/fusion`: every turn runs through the panel — several providers answer, a
/// synthesizer fuses them — or back to the single configured model.
///
/// The client is swapped in place and the registry rebuilt against it, rather
/// than the agent being rebuilt onto a fresh session as the TUI does: a task
/// *is* its session file, and rotating that would strand the window on a session
/// nothing writes to any more (the same reason `/clear` opens a new chat).
async fn toggle_fusion(agent: &mut Agent, ctx: &mut CommandCtx<'_>) {
    let shared = ctx.shared;
    let config = ctx.config;
    let (client, label) = match *ctx.fusion {
        true => match config.active().build() {
            Ok(client) => {
                let name = config.active().model;
                (client, format!("fusion off — back to {name}"))
            }
            Err(err) => {
                return error(shared, format!("could not rebuild the provider: {err:#}"));
            }
        },
        false => {
            let Some(fusion) = config.effective_fusion() else {
                return error(
                    shared,
                    "fusion needs at least one configured provider — add one on the Settings \
                     page, then set [fusion] in ~/.wizard/config.toml",
                );
            };
            match config.build_fusion_from(&fusion) {
                Ok(provider) => {
                    let label = provider.label();
                    (
                        Arc::new(provider) as Arc<dyn LlmProvider>,
                        format!("{label} — every turn now fuses the panel; /fusion to turn off"),
                    )
                }
                Err(err) => return error(shared, format!("could not start fusion: {err:#}")),
            }
        }
    };

    let model = shared.model();
    let native = crate::llm::provider::probe_native_tools(client.as_ref(), &model).await;
    agent.set_client(Arc::clone(&client), native);
    // The spawn tool captured the old client: without this, subagents would keep
    // answering from the model the panel just replaced.
    let hooks = Arc::clone(agent.hooks());
    let manager = ctx.mcp.read().await;
    match crate::agent::build_tool_registry(config, &client, &hooks, &manager).await {
        Ok((registry, subagent_model)) => {
            agent.set_registry(registry);
            agent.bind_subagent_model(subagent_model);
        }
        Err(err) => tracing::warn!("rebuilding the registry for /fusion: {err:#}"),
    }
    *ctx.fusion = !*ctx.fusion;
    // The seats went with the panel, in whichever direction it just moved.
    reseat_ultra(agent, config, *ctx.fusion, shared);
    notice(shared, label);
}

/// The seats an `/ultra` roster is dealt across, given what is active now.
///
/// Empty unless `/fusion` is on, which is the whole of what "the two modes
/// compose" means: `[ultra]` names no provider and never will, because which
/// providers exist is a question about the session and the answer changes when
/// `/fusion` is toggled without a line of `[ultra]` changing.
///
/// This is deliberately the same rule as the TUI's `ultra_seats`. The two
/// surfaces used to answer differently here, and this one's answer was a flat
/// refusal to run ultra on top of fusion at all.
pub(super) fn ultra_seats(config: &Config, fusion_active: bool) -> Result<Vec<ultra::Seat>> {
    if !fusion_active {
        return Ok(Vec::new());
    }
    let Some(fusion) = config.effective_fusion() else {
        return Ok(Vec::new());
    };
    crate::llm::fusion::panel_seats(&fusion, &config.providers)
}

/// Re-deal a live `/ultra` roster across the seats the session now offers.
///
/// Runs on both edges of the `/fusion` toggle. The roster does not change;
/// where each candidate runs does, and an engine left holding the wrong seats
/// is either a panel debate per candidate or a draft from a provider the user
/// just switched away from.
fn reseat_ultra(agent: &mut Agent, config: &Config, fusion_active: bool, shared: &Arc<TaskShared>) {
    if !agent.ultra() {
        return;
    }
    let built = config
        .build_ultra()
        .and_then(|engine| Ok(engine.with_seats(ultra_seats(config, fusion_active)?)));
    match built {
        Ok(engine) => agent.set_ultra(Some(Arc::new(engine))),
        // The roster stays as it was rather than being silently dropped: ultra
        // is still on, and saying so is more useful than turning it off behind
        // the user's back.
        Err(err) => notice(
            shared,
            format!("ultra roster could not be re-seated: {err:#}"),
        ),
    }
}

/// Toggle `/ultra`: mixture of agents. Where `/fusion` swaps the client and
/// therefore rebuilds against it, ultra changes nothing about *which* model
/// answers: the candidates fan out over the client and model already active,
/// or over the fusion panel's providers when that is also on. So this is a
/// plain flag on the live agent: no rebuild, no session reset, and the
/// conversation in front of the user survives the toggle.
fn toggle_ultra(agent: &mut Agent, ctx: &mut CommandCtx<'_>) {
    let shared = ctx.shared;
    if agent.ultra() {
        agent.set_ultra(None);
        return notice(shared, "ultra off — one agent per turn again, no pre-phase");
    }
    // `build_ultra` is the sole validation gate for `[ultra]`, so a roster the
    // user hand-edited into an unusable state surfaces here, at the toggle,
    // instead of at the top of their next turn.
    match ctx
        .config
        .build_ultra()
        .and_then(|engine| Ok(engine.with_seats(ultra_seats(ctx.config, *ctx.fusion)?)))
    {
        Ok(engine) => {
            let engine = Arc::new(engine);
            let label = engine.label();
            agent.set_ultra(Some(engine));
            notice(
                shared,
                format!(
                    "{label} — each turn now drafts on the active model, compares, then acts; \
                     /ultra to turn off"
                ),
            );
        }
        Err(err) => error(shared, format!("could not start ultra: {err:#}")),
    }
}

/// `/server [status|start|stop]`: the local llama-server's lifecycle. It is a
/// process on this machine, and the window runs on that machine — the same
/// command, answering into the chat instead of onto a status line.
async fn server_command(action: ServerAction, config: &Config, shared: &Arc<TaskShared>) {
    let provider = config.active();
    if !provider
        .descriptor()
        .is_some_and(|descriptor| descriptor.manages_local_server())
    {
        return error(
            shared,
            format!(
                "'/server' manages the local llama-server; the active provider '{}' is {}",
                provider.name, provider.kind
            ),
        );
    }
    match action {
        ServerAction::Status => {
            let spawned = crate::server::spawned_pid()
                .map(|pid| format!(" (PID {pid}, started by wizard)"))
                .unwrap_or_default();
            let text = match crate::server::probe(&provider.base_url).await {
                crate::server::Health::Ready => {
                    format!("llama-server at {}: ready{spawned}", provider.base_url)
                }
                crate::server::Health::Loading => format!(
                    "llama-server at {}: loading its model{spawned}",
                    provider.base_url
                ),
                crate::server::Health::Down => format!(
                    "llama-server at {}: not running — start it with /server start",
                    provider.base_url
                ),
            };
            notice(shared, text);
        }
        ServerAction::Start => {
            if crate::server::probe(&provider.base_url).await == crate::server::Health::Ready {
                return notice(
                    shared,
                    format!("llama-server at {} is already running", provider.base_url),
                );
            }
            notice(
                shared,
                format!("starting llama-server at {}…", provider.base_url),
            );
            let progress = NoticeProgress {
                shared: Arc::clone(shared),
            };
            match crate::server::ensure_running(&provider, &progress).await {
                Ok(()) => notice(
                    shared,
                    format!("llama-server at {} is ready", provider.base_url),
                ),
                Err(err) => error(shared, format!("could not start llama-server: {err:#}")),
            }
        }
        ServerAction::Stop => {
            let text = match crate::server::stop() {
                Ok(crate::server::StopOutcome::Stopped(pid)) => {
                    format!("stopped llama-server (PID {pid})")
                }
                Ok(crate::server::StopOutcome::NotRecorded) => {
                    "wizard has not started a llama-server — nothing to stop".to_string()
                }
                Ok(crate::server::StopOutcome::NotRunning(pid)) => {
                    format!("llama-server (PID {pid}) already exited")
                }
                Ok(crate::server::StopOutcome::NotOurs { pid, name }) => {
                    format!("refusing to stop PID {pid}: it is '{name}', not llama-server")
                }
                Err(err) => format!("could not stop llama-server: {err:#}"),
            };
            notice(shared, text);
        }
    }
}

/// [`crate::server::Progress`] adapter for `/server start`: the server's status
/// lines and download milestones arrive in the chat as notices.
/// The byte guard outlives the borrow that opened it (`Box<dyn ByteProgress>` is
/// `'static`), which is why this holds the task by handle rather than by
/// reference.
struct NoticeProgress {
    shared: Arc<TaskShared>,
}

impl crate::server::Progress for NoticeProgress {
    fn status(&self, line: &str) {
        notice(&self.shared, line.to_string());
    }

    fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn crate::server::ByteProgress> {
        Box::new(NoticeBytes {
            shared: Arc::clone(&self.shared),
            label: label.to_string(),
            total: total.filter(|total| *total > 0),
            written: std::sync::atomic::AtomicU64::new(0),
            last_percent: std::sync::atomic::AtomicU64::new(0),
        })
    }
}

/// Byte-progress guard for [`NoticeProgress`]. Throttled to whole-percent steps,
/// as the TUI's is: a multi-GB model pull would otherwise flood the chat with a
/// notice per chunk.
struct NoticeBytes {
    shared: Arc<TaskShared>,
    label: String,
    total: Option<u64>,
    written: std::sync::atomic::AtomicU64,
    last_percent: std::sync::atomic::AtomicU64,
}

impl crate::server::ByteProgress for NoticeBytes {
    fn inc(&self, n: u64) {
        use std::sync::atomic::Ordering;
        let written = self.written.fetch_add(n, Ordering::Relaxed) + n;
        let Some(total) = self.total else { return };
        let percent = written * 100 / total;
        if percent > self.last_percent.swap(percent, Ordering::Relaxed) {
            notice(
                &self.shared,
                format!(
                    "{} — {percent}% of {:.1} GB",
                    self.label,
                    total as f64 / 1e9
                ),
            );
        }
    }

    fn finish(self: Box<Self>, msg: &str) {
        if !msg.is_empty() {
            notice(&self.shared, msg.to_string());
        }
    }
}

/// `/evolve <what to add>`: Wizard extends itself — a skill, a scripted tool, an
/// MCP server. The TUI detaches it and notices when it lands; here it runs in
/// the command's own slot, so the chat shows the task working until it is done.
async fn evolve(deep: bool, description: String, config: &Config, shared: &TaskShared) {
    notice(
        shared,
        format!(
            "evolving ({}): {description}",
            if deep { "deep" } else { "runtime" }
        ),
    );
    let tier = match deep {
        true => crate::evolve::EvolveTier::Deep,
        false => crate::evolve::EvolveTier::Runtime,
    };
    let request = crate::evolve::EvolveRequest { description, tier };
    let mut evolver = crate::evolve::Evolver::new(config.clone());
    match evolver.run(request).await {
        Ok(outcome) => notice(shared, crate::evolve::describe_outcome(&outcome)),
        Err(err) => error(shared, format!("evolve failed: {err:#}")),
    }
}

/// `/publish [branch]`: fork Wizard and hand back a one-line installer.
async fn publish(branch: Option<String>, config: &Config, shared: &TaskShared) {
    notice(
        shared,
        format!(
            "publishing Wizard{}…",
            branch
                .as_deref()
                .map(|branch| format!(" (branch: {branch})"))
                .unwrap_or_default()
        ),
    );
    let request = crate::evolve::PublishRequest { branch };
    match crate::evolve::publish(config, request, false).await {
        Ok(outcome) => notice(
            shared,
            format!(
                "publish: forked to {}  (branch: {})\n\nInstall one-liner:\n{}",
                outcome.fork_url, outcome.branch, outcome.install_one_liner
            ),
        ),
        Err(err) => error(shared, format!("publish failed: {err:#}")),
    }
}
