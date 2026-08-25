//! The one place a slash command typed in a chat is applied to the gateway's
//! live [`Agent`].
//!
//! # Why this file exists
//!
//! The gateway publishes its command list to Telegram (`setMyCommands`, see
//! [`super::advertised_commands`]), so the client offers `/status`, `/cost`,
//! `/model` and twenty-odd others. Typing one used to *ask the model about the
//! literal text*: [`super::disposition`] matched exactly `/plan` and `/omakase`
//! and sent everything else down the turn path. A menu that advertises a
//! command nothing runs is worse than no menu.
//!
//! # It is the same dispatcher the terminal and the window run
//!
//! [`GatewaySurface`] is a [`CommandSurface`], so what `/model` *means* is
//! written once, in [`crate::commands::surface::dispatch`], for every surface.
//! This file supplies only the verbs — what this surface can do about a
//! command — and the one thing that is genuinely different here: the chat has
//! no transcript to write a notice into, so [`CommandSurface::notice`] and
//! [`CommandSurface::error`] *collect* their text and [`apply_command`] hands
//! it back as the message that answers the command.
//!
//! # What it deliberately does not implement
//!
//! Everything that needs a screen, a picker or a keyboard is
//! [`Execution::Unavailable`](crate::commands::Execution::Unavailable) in the
//! gateway's column of [`crate::commands::COMMANDS`], and `dispatch` refuses it
//! by name before it reaches a verb. The window-owning trait methods are
//! therefore left at their defaults, which answer honestly ("not available in
//! this chat") rather than doing nothing quietly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::agent::{Agent, RewindCandidate, ultra};
use crate::commands::surface::{CommandSurface, PlanState, SessionSnapshot, Surface, dispatch};
use crate::commands::{ServerAction, SlashCommand};
use crate::config::{Config, Mode, ReasoningEffort};
use crate::llm::provider::{LlmProvider, NATIVE_TOOLS_ON_PROBE_FAILURE};
use crate::mcp::McpManager;
use crate::tools::tasks::Task;

/// What the gateway's command executor may reach besides the agent itself.
///
/// Held by [`super::serve`] for the life of the process, because two of these
/// are session state and not config: `fusion` is a toggle with no home on the
/// agent, and `mcp` is *the* connected manager — `/reload` re-registers against
/// it rather than connecting a second set, which would leave the gateway
/// running one copy of every configured MCP server per reload, each a real OS
/// process that nothing later shuts down.
pub struct GatewayCtx<'a> {
    /// The config the gateway's agent was built from (sovereign posture, with
    /// the step budget that follows from it).
    pub config: &'a Config,
    /// The workspace `/goal`, `/memory` and `/doctor` answer about.
    pub project_root: &'a Path,
    /// The MCP servers this process connected once, at startup.
    pub mcp: &'a mut McpManager,
    /// Whether turns currently run through the fusion panel (`/fusion`).
    pub fusion: &'a mut bool,
}

/// Apply one slash command line to the gateway's live agent and return the
/// message that answers it. An empty string means "nothing to say".
///
/// Parsing is [`SlashCommand::parse`] and nothing else, so an argument means
/// here exactly what it means at the terminal's prompt — including the errors
/// it rejects a bad one with. A parse failure is a reply, never a panic: the
/// message it produced is what the chat is told.
///
/// The caller decides what is a command at all; see
/// [`super::command_line`], which must let a path like `/etc/hosts` through to
/// the model rather than answering it here.
pub async fn apply_command(agent: &mut Agent, ctx: &mut GatewayCtx<'_>, line: &str) -> String {
    let command = match SlashCommand::parse(line) {
        Some(Ok(command)) => command,
        // The parser's own words: "unknown command '/x' — try /help",
        // "unknown mode 'sideways' (genie|sovereign)", "usage: /btw <question>".
        Some(Err(message)) => return message,
        // Unreachable through `command_line`, which only builds lines that
        // start with a slash. Answered rather than asserted: this is a daemon,
        // and a panic here would take the whole gateway down with it.
        None => return format!("'{}' is not a slash command", line.trim()),
    };

    // `/help` is derived from the command table on every surface, which is
    // right and is also why it cannot know about `/stop`: that control has no
    // row, because it means nothing anywhere but here. The chat's menu offers
    // it, so the chat's help has to list it too — a help that contradicts the
    // autocomplete is how a user concludes the autocomplete is stale.
    let native = matches!(command, SlashCommand::Help);

    let mut surface = GatewaySurface {
        agent,
        ctx,
        out: String::new(),
    };
    dispatch(command, &mut surface).await;
    let mut reply = surface.out;
    if native {
        reply.push_str(&super::native_help());
    }
    reply
}

/// The chat's half of the one dispatcher: an agent, and a string to answer with.
///
/// Every method is a verb — what to change, what can be seen. What a command
/// *means*, and every line of prose it answers with, belongs to
/// [`dispatch`].
struct GatewaySurface<'a, 'ctx> {
    agent: &'a mut Agent,
    ctx: &'a mut GatewayCtx<'ctx>,
    /// Everything the command has said so far, in arrival order. This is the
    /// reply.
    out: String,
}

impl GatewaySurface<'_, '_> {
    /// Add one thing the command has to say to the reply.
    ///
    /// Blank-line separated, because a command that speaks twice (`/reload`
    /// warning then result, `/fusion` refusal then state) reads as two
    /// paragraphs in a chat, not as one run-on line.
    fn say(&mut self, text: impl Into<String>) {
        let text = text.into();
        let text = text.trim_end();
        if text.trim().is_empty() {
            return;
        }
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
        }
        self.out.push_str(text);
    }

    /// The seats an `/ultra` roster is dealt across, given what is active now.
    ///
    /// Empty unless `/fusion` is on — the same rule as the terminal's and the
    /// window's, so "the two modes compose" means the same thing in a chat.
    fn ultra_seats(config: &Config, fusion_active: bool) -> Result<Vec<ultra::Seat>> {
        if !fusion_active {
            return Ok(Vec::new());
        }
        let Some(fusion) = config.effective_fusion() else {
            return Ok(Vec::new());
        };
        crate::llm::fusion::panel_seats(&fusion, &config.providers)
    }

    /// Re-deal a live `/ultra` roster across the seats the session now offers.
    /// Runs on both edges of the `/fusion` toggle.
    fn reseat_ultra(&mut self) {
        if !self.agent.ultra() {
            return;
        }
        let built = self.ctx.config.build_ultra().and_then(|engine| {
            Ok(engine.with_seats(Self::ultra_seats(self.ctx.config, *self.ctx.fusion)?))
        });
        match built {
            Ok(engine) => self.agent.set_ultra(Some(Arc::new(engine))),
            // Ultra stays on with the roster it had: saying so beats turning
            // it off behind the user's back.
            Err(err) => self.say(format!("ultra roster could not be re-seated: {err:#}")),
        }
    }
}

#[async_trait]
impl CommandSurface for GatewaySurface<'_, '_> {
    fn surface(&self) -> Surface {
        Surface::Gateway
    }

    fn project_root(&self) -> PathBuf {
        self.ctx.project_root.to_path_buf()
    }

    /// No keyboard: the only key bindings here are Telegram's own.
    fn help_keys(&self) -> Option<&'static str> {
        None
    }

    /// One channel. A chat message is a chat message, and a refusal that
    /// arrived as a second kind of message would still be one line of text in
    /// the same bubble.
    fn notice(&mut self, text: String) {
        self.say(text);
    }

    fn error(&mut self, message: String) {
        self.say(message);
    }

    /// Commands run between turns here — the serve loop handles one message at
    /// a time — so the agent is always in hand and nothing has to be reported
    /// as unknown.
    fn snapshot(&self) -> SessionSnapshot {
        let provider = self.ctx.config.active();
        let (prompt_tokens, completion_tokens) = self.agent.usage().session_totals();
        SessionSnapshot {
            model: self.agent.model().to_string(),
            provider_name: provider.name.clone(),
            provider_kind: provider.kind,
            provider_base_url: provider.base_url.clone(),
            mode: self.agent.mode(),
            effort: self.ctx.config.reasoning_effort,
            max_steps: Some(self.ctx.config.max_steps),
            session: Some(self.agent.session().id.clone()),
            prompt_tokens,
            completion_tokens,
            cache_tokens: Some(self.agent.usage().session_cache_totals()),
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

    fn background_tasks(&self) -> Result<Vec<Task>, String> {
        Ok(self.agent.tasks())
    }

    fn rewind_candidates(&self) -> Vec<RewindCandidate> {
        self.agent.rewind_candidates(20)
    }

    /// Swapped in place, as the window does it: the conversation is the point
    /// of a long-lived chat, and rebuilding onto a fresh session would drop it.
    async fn set_model(&mut self, tag: String) {
        let native = match self.ctx.config.active().build() {
            Ok(client) => crate::llm::provider::probe_native_tools(client.as_ref(), &tag).await,
            Err(err) => {
                tracing::warn!(
                    "building a probe client: {err:#}; assuming \
                     native_tools={NATIVE_TOOLS_ON_PROBE_FAILURE}"
                );
                NATIVE_TOOLS_ON_PROBE_FAILURE
            }
        };
        self.agent.set_model(tag.clone(), native);
        self.say(format!("switched to model {tag}"));
    }

    async fn set_mode(&mut self, mode: Mode) -> bool {
        self.agent.set_mode(mode);
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
        self.say(outcome.describe());
    }

    /// `/reload`: skills, scripted tools, and MCP servers, without restarting
    /// the service. The MCP config reloads into the manager this process
    /// already runs (see [`GatewayCtx::mcp`]).
    async fn reload(&mut self) {
        match Config::mcp_config_path().and_then(|path| crate::mcp::McpConfig::load(&path)) {
            Ok(config) => {
                let reloaded = self.ctx.mcp.reload(&config).await;
                if let Err(err) = reloaded {
                    self.say(format!("MCP reload warning: {err:#}"));
                }
            }
            Err(err) => self.say(format!("could not reload MCP config: {err:#}")),
        }
        let hooks = Arc::clone(self.agent.hooks());
        let client = Arc::clone(self.agent.client());
        let built =
            crate::agent::build_tool_registry(self.ctx.config, &client, &hooks, self.ctx.mcp).await;
        match built {
            Ok((registry, subagent_model)) => {
                let tools = registry.len();
                let skills = crate::agent::load_skills();
                let count = skills.len();
                self.agent.set_registry(registry);
                self.agent.bind_subagent_model(subagent_model);
                self.agent.set_skills(skills);
                self.say(format!("reloaded: {tools} tools, {count} skills"));
            }
            Err(err) => self.say(format!("reload failed: {err:#}")),
        }
    }

    async fn rewind(&mut self, turn: u64) {
        let restored = self.agent.rewind_to(turn);
        match restored {
            Ok(files) => {
                let files = match files.is_empty() {
                    true => "no files needed restoring".to_string(),
                    false => format!(
                        "restored {}",
                        files
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                self.say(format!(
                    "rewound to before turn {turn} — {files}; conversation truncated"
                ));
            }
            Err(err) => self.say(format!("rewind failed: {err:#}")),
        }
    }

    /// `/clear`: the gateway's agent is shared by every allow-listed chat and
    /// lives as long as the service, so this is the only way to start a fresh
    /// conversation without restarting it. The new session id is reported
    /// because it is the one thing that identifies the transcript on disk.
    async fn clear(&mut self) {
        let cleared = self.agent.clear();
        match cleared {
            Ok(()) => {
                let id = self.agent.session().id.clone();
                self.say(format!("conversation cleared — new session {id}"));
            }
            Err(err) => self.say(format!("could not clear the conversation: {err:#}")),
        }
    }

    /// Runs against the live agent, between turns. The exchange never enters
    /// history — the same contract as every other surface.
    async fn btw(&mut self, question: String) {
        let answer = self.agent.answer_side_question(&question).await;
        match answer {
            Ok(answer) => self.say(format!("/btw {question}\n{answer}")),
            Err(err) => self.say(format!("/btw failed: {err:#}")),
        }
    }

    /// A side quest that inherits the conversation and outlives this reply.
    ///
    /// No event channel: nobody is watching a stream here, and the fork's
    /// report lands in history on the next turn's top-of-loop drain, which is
    /// where the chat will see it.
    async fn fork(&mut self, task: String) {
        let started = self.agent.spawn_fork(&task, None).await;
        match started {
            Ok(id) => self.say(format!("fork #{id} started: {task}")),
            Err(err) => self.say(format!("/fork failed: {err:#}")),
        }
    }

    /// No composer to put a kickoff turn in: the chat sends the next message,
    /// and [`dispatch`] says so.
    async fn start_goal(&mut self, goal: String) -> bool {
        let _ = goal;
        false
    }

    /// Runs in the message's own slot, so the chat waits and then gets the
    /// outcome. There is no transcript to stream progress into.
    async fn evolve(&mut self, deep: bool, description: String) {
        let tier = match deep {
            true => crate::evolve::EvolveTier::Deep,
            false => crate::evolve::EvolveTier::Runtime,
        };
        let request = crate::evolve::EvolveRequest { description, tier };
        let mut evolver = crate::evolve::Evolver::new(self.ctx.config.clone());
        let outcome = evolver.run(request).await;
        match outcome {
            Ok(outcome) => self.say(crate::evolve::describe_outcome(&outcome)),
            Err(err) => self.say(format!("evolve failed: {err:#}")),
        }
    }

    async fn publish(&mut self, branch: Option<String>) {
        let request = crate::evolve::PublishRequest { branch };
        let outcome = crate::evolve::publish(self.ctx.config, request, false).await;
        match outcome {
            Ok(outcome) => self.say(format!(
                "publish: forked to {}  (branch: {})\n\nInstall one-liner:\n{}",
                outcome.fork_url, outcome.branch, outcome.install_one_liner
            )),
            Err(err) => self.say(format!("publish failed: {err:#}")),
        }
    }

    /// `/fusion`: every turn runs through the panel, or back to the single
    /// configured model. The client is swapped in place and the registry
    /// rebuilt against it, so the conversation survives the toggle.
    async fn toggle_fusion(&mut self) {
        let (client, label) = match *self.ctx.fusion {
            true => match self.ctx.config.active().build() {
                Ok(client) => {
                    let name = self.ctx.config.active().model;
                    (client, format!("fusion off — back to {name}"))
                }
                Err(err) => {
                    return self.say(format!("could not rebuild the provider: {err:#}"));
                }
            },
            false => {
                let Some(fusion) = self.ctx.config.effective_fusion() else {
                    return self.say(
                        "fusion needs at least one configured provider — set [fusion] in \
                         ~/.wizard/config.toml on the machine running the gateway, then /fusion \
                         to turn it on",
                    );
                };
                match self.ctx.config.build_fusion_from(&fusion) {
                    Ok(provider) => {
                        let label = provider.label();
                        (
                            Arc::new(provider) as Arc<dyn LlmProvider>,
                            format!(
                                "{label} — every turn now fuses the panel; /fusion to turn off"
                            ),
                        )
                    }
                    Err(err) => return self.say(format!("could not start fusion: {err:#}")),
                }
            }
        };

        let model = self.agent.model().to_string();
        let native = crate::llm::provider::probe_native_tools(client.as_ref(), &model).await;
        self.agent.set_client(Arc::clone(&client), native);
        // The spawn tool captured the old client: without this, subagents would
        // keep answering from the model the panel just replaced.
        let hooks = Arc::clone(self.agent.hooks());
        let built =
            crate::agent::build_tool_registry(self.ctx.config, &client, &hooks, self.ctx.mcp).await;
        match built {
            Ok((registry, subagent_model)) => {
                self.agent.set_registry(registry);
                self.agent.bind_subagent_model(subagent_model);
            }
            Err(err) => tracing::warn!("rebuilding the registry for /fusion: {err:#}"),
        }
        *self.ctx.fusion = !*self.ctx.fusion;
        // The seats went with the panel, in whichever direction it just moved.
        self.reseat_ultra();
        self.say(label);
    }

    /// `/ultra`: mixture of agents. A plain flag on the live agent — no
    /// rebuild, no session reset, and the conversation survives the toggle.
    async fn toggle_ultra(&mut self) {
        if self.agent.ultra() {
            self.agent.set_ultra(None);
            return self.say("ultra off — one agent per turn again, no pre-phase");
        }
        // `build_ultra` is the sole validation gate for `[ultra]`, so a roster
        // hand-edited into an unusable state surfaces here rather than at the
        // top of the next turn.
        let built = self.ctx.config.build_ultra().and_then(|engine| {
            Ok(engine.with_seats(Self::ultra_seats(self.ctx.config, *self.ctx.fusion)?))
        });
        match built {
            Ok(engine) => {
                let engine = Arc::new(engine);
                let label = engine.label();
                self.agent.set_ultra(Some(engine));
                self.say(format!(
                    "{label} — each turn now drafts on the active model, compares, then acts; \
                     /ultra to turn off"
                ));
            }
            Err(err) => self.say(format!("could not start ultra: {err:#}")),
        }
    }

    /// `/server [status|start|stop]`: a local model server's lifecycle. It is
    /// a process on the machine running the gateway, which is the machine the
    /// chat is driving — the same command, answering into a message.
    ///
    /// A start streams its progress to the journal through the same spinner a
    /// headless build uses (plain lines when stderr is not a terminal, which
    /// under systemd it never is); the chat gets the outcome, because there is
    /// no transcript here to trickle percentages into. The spinner is shared
    /// rather than handed over because the closing tick is this surface's to
    /// draw — see the `Arc<ServerSpinner>` impl in [`crate::progress`].
    async fn server(&mut self, action: ServerAction) {
        let provider = self.ctx.config.active();
        if !provider
            .descriptor()
            .is_some_and(|descriptor| descriptor.manages_local_server())
        {
            return self.say(crate::server::not_managed(
                &provider.name,
                provider.kind.as_str(),
            ));
        }
        let Some(managed) = crate::server::installed() else {
            return self.say(crate::server::absent());
        };
        let text = match action {
            ServerAction::Status => managed.status(&provider).await,
            ServerAction::Start => {
                let wait = std::sync::Arc::new(crate::progress::ServerSpinner::start());
                let outcome = managed
                    .start(provider, Box::new(std::sync::Arc::clone(&wait)))
                    .await;
                wait.finish(outcome.is_ok());
                match outcome {
                    Ok(line) | Err(line) => line,
                }
            }
            ServerAction::Stop => managed.stop(),
        };
        self.say(text);
    }
}
