//! The one dispatcher every surface runs its built-in slash commands through,
//! and the interface a surface implements to get them.
//!
//! [`dispatch`] owns every match arm and every line of prose a built-in command
//! answers with. A surface supplies the *verbs* (set the model, toggle the
//! panel, open the picker) through [`CommandSurface`], and gets the semantics
//! back rather than writing its own copy of them. That is the whole point:
//! `/goal` used to work in the terminal and do nothing on the second surface,
//! `/model` meant two different things, and `/help` was a hand-written const on
//! one surface and derived from the table on the other. All three were the same
//! bug, which is that a command lived in two places.
//!
//! What a surface cannot do a built-in declares in [`crate::commands::COMMANDS`] as an
//! [`Execution::Unavailable`] column, and this module refuses it by name. A
//! command is never silently missing a match arm, because there is only one
//! match and the compiler walks every variant through it.
//!
//! A plugin-registered command goes through the same [`dispatch`] and the same
//! gate — it declares the surfaces it runs on
//! ([`PluginCommand::only`](crate::commands::PluginCommand::only)) and the gate
//! reads that instead of a table column. What it does *not* share is the match:
//! its body is its own handler, so it leaves the dispatcher one line after the
//! gate. That is the whole difference, and it is why the gate is asked of the
//! command rather than of the table.
//!
//! Adding another surface (the Telegram gateway was the third) is therefore: a
//! [`Surface`] variant, a column on every row of the table, and one
//! `impl CommandSurface`. The compiler asks for all three, and
//! `dispatch_reaches_every_command_on_every_surface` fails if the table and the
//! dispatcher disagree about any of them.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{
    CommandSpec, Execution, FusionAction, ProviderAction, ServerAction, SlashCommand, UltraAction,
    commands_for,
};
use crate::agent::RewindCandidate;
use crate::config::{Config, Mode, ProviderKind, ReasoningEffort, StepBudget, UltraConfig};
use crate::import_claude::ImportSelection;
use crate::tools::tasks::Task;

/* ---------------------------------------------------------------------- */
/* Surfaces                                                               */
/* ---------------------------------------------------------------------- */

/// The surfaces a slash command can be typed at.
///
/// A new one adds a variant here and a column to every row of [`crate::commands::COMMANDS`];
/// the compiler walks it through the whole table rather than letting it
/// quietly miss half of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The terminal UI (`wizard`).
    Tui,
    /// The GUI (`wizard gui`): the iced window, and the task worker that holds
    /// the agent it draws.
    ///
    /// Named `Gui` and not `Native` on purpose. It was the browser GUI's
    /// column — a page and the HTTP server behind it — and when that surface
    /// was deleted the window inherited the column rather than growing one of
    /// its own, because every answer in a second column would have been a copy
    /// of this one: same agent, same commands, same three refusals
    /// (`/vim`, `/theme`, `/quit`). A duplicated column is a column that
    /// drifts. See `src/plugins/native/command.rs`, which returns this
    /// variant and says why there is no `Surface::Native`.
    Gui,
    /// The Telegram gateway (`wizard gateway`): an allow-listed chat, and the
    /// operator's machine running the turn.
    ///
    /// It is all agent and no window. There is no terminal, no panel, no
    /// picker and nobody at a keyboard, so its column is [`Execution::Agent`]
    /// wherever the answer is expressible as a message and
    /// [`Execution::Unavailable`] wherever it needs a screen or a human
    /// choosing from a list. Nothing is [`Execution::Ui`]: unlike the window,
    /// the gateway has no second half that could run a command *instead of*
    /// the process holding the agent — the chat only carries text.
    Gateway,
}

impl Surface {
    /// Every surface, for the tests that hold each one to the whole table.
    pub const ALL: &'static [Surface] = &[Surface::Tui, Surface::Gui, Surface::Gateway];
}

/// One of a surface's interactive choosers: what a command opens when it is
/// typed with no argument.
///
/// A surface with none leaves [`CommandSurface::open`] alone, and [`dispatch`]
/// answers with the argument that would have been chosen at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chooser {
    /// `/model`: the installed models.
    Model,
    /// `/mode`: genie or sovereign.
    Mode,
    /// `/effort`: the reasoning levels.
    Effort,
    /// `/rewind`: the turns there is something to go back to.
    Rewind,
    /// `/resume`: the past sessions.
    Resume,
    /// `/resume-claude`: the conversations Claude Code recorded here.
    ResumeClaude,
    /// `/agents`: the subagent roster.
    Agents,
    /// `/provider`: switch providers, or add one.
    Provider,
    /// `/settings`: the in-app settings menu.
    Settings,
    /// `/fusion config`: which providers form the panel.
    FusionPanel,
    /// `/ultra config`: which lenses form the roster.
    UltraRoster,
}

impl Chooser {
    /// What to say on a surface with no such chooser: the argument the user
    /// would have named at it.
    ///
    /// `None` for a chooser that *is* the command (a session list, a settings
    /// menu, the roster of installed subagents), where naming an argument
    /// would answer a question nobody asked.
    fn usage(self) -> Option<&'static str> {
        match self {
            Chooser::Model => Some("usage: /model <tag> — or pick one from the model menu"),
            Chooser::Mode => Some("usage: /mode <genie|sovereign>"),
            Chooser::Effort => Some("usage: /effort <low|medium|high|default>"),
            Chooser::FusionPanel => Some(
                "`/fusion config` is an interactive editor; set the panel under [fusion] in \
                 ~/.wizard/config.toml, then /fusion to turn it on",
            ),
            Chooser::UltraRoster => Some(
                "`/ultra config` is an interactive editor; set the roster under [ultra] in \
                 ~/.wizard/config.toml, then /ultra to turn it on",
            ),
            Chooser::Rewind
            | Chooser::Resume
            | Chooser::ResumeClaude
            | Chooser::Agents
            | Chooser::Provider
            | Chooser::Settings => None,
        }
    }
}

/// One of a surface's own panels, toggled on and off beside the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// `/diff`: the git diff sidebar.
    Diff,
    /// `/todos`: the todo band above the composer.
    Todos,
    /// `/dashboard`: every live Wizard session on this machine.
    Dashboard,
}

impl Panel {
    /// The command word that toggles it.
    fn name(self) -> &'static str {
        match self {
            Panel::Diff => "diff",
            Panel::Todos => "todos",
            Panel::Dashboard => "dashboard",
        }
    }
}

/* ---------------------------------------------------------------------- */
/* What a command reads                                                   */
/* ---------------------------------------------------------------------- */

/// Plan mode and its chef's-choice flavor, which travel together: omakase *is*
/// plan mode with the review gate removed, so it can never be on without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanState {
    /// Read-only investigation until a plan is approved.
    pub plan: bool,
    /// The agent decides the approach and approves its own plan.
    pub omakase: bool,
}

impl PlanState {
    /// The state after `/plan`. Leaving plan mode leaves omakase with it.
    fn toggled_plan(self) -> Self {
        let plan = !self.plan;
        Self {
            plan,
            omakase: self.omakase && plan,
        }
    }

    /// The state after `/omakase`. Turning it on turns plan mode on; turning
    /// it off drops back to plain plan mode rather than out of it.
    fn toggled_omakase(self) -> Self {
        let omakase = !self.omakase;
        Self {
            plan: self.plan || omakase,
            omakase,
        }
    }

    /// How `/status` says it.
    fn describe(self) -> &'static str {
        match (self.plan, self.omakase) {
            (_, true) => "on (omakase — chef's choice)",
            (true, false) => "on",
            (false, false) => "off",
        }
    }
}

/// What `/status` and `/cost` report.
///
/// The surface fills in what it can see (the terminal's agent is out of its
/// slot while a turn runs, so some of it is `None` there), and the rendering
/// happens here, once, so both surfaces answer the same question the same way.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub model: String,
    pub provider_name: String,
    pub provider_kind: ProviderKind,
    pub provider_base_url: String,
    pub mode: Mode,
    pub effort: Option<ReasoningEffort>,
    /// The step budget a turn runs under, when the surface tracks one.
    pub max_steps: Option<StepBudget>,
    /// The session id, or `None` while a turn holds the agent.
    pub session: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// `(cache_read, cache_write)` prompt tokens, both subsets of
    /// `prompt_tokens`, when the surface can see them.
    ///
    /// `None` where a surface genuinely cannot know — the terminal mid-turn,
    /// where the agent is out of its slot and only the status bar's mirror of
    /// the flat totals survives. `/cost` prices `None` as all-fresh input,
    /// which overstates; it is the answer that was given unconditionally
    /// before this field existed.
    pub cache_tokens: Option<(u64, u64)>,
    /// Tokens the next turn would send, when the surface knows.
    pub context_tokens: Option<u64>,
    /// Background `execute` tasks still running, when the surface knows.
    pub background_tasks: Option<usize>,
    /// Todos done, todos total.
    pub todos: (usize, usize),
    pub plan: PlanState,
    /// The active `/ultra` roster's label, when there is one.
    pub ultra: Option<String>,
    pub usd_per_mtok_in: Option<f64>,
    pub usd_per_mtok_out: Option<f64>,
}

/* ---------------------------------------------------------------------- */
/* The interface                                                          */
/* ---------------------------------------------------------------------- */

/// Everything a built-in slash command needs of the surface it was typed at.
///
/// The methods are verbs, not commands: they say what to change and what can be
/// seen, never what to print about it. The prose belongs to [`dispatch`],
/// because two surfaces printing their own version of "plan mode on" is how
/// they drift.
///
/// The window-owning verbs carry defaults, so a surface that has no window to
/// change — the task worker in `src/plugins/gui/command.rs`, which holds the
/// agent while the window holds the panels — implements only what it runs and
/// answers the rest honestly. Everything else is required: a surface that
/// cannot set the model does not compile.
#[async_trait]
pub trait CommandSurface {
    // --- what the surface is ---

    /// Which surface this is, for the table's per-surface columns.
    fn surface(&self) -> Surface;

    /// The workspace root `/goal`, `/memory` and `/doctor` answer about.
    fn project_root(&self) -> PathBuf;

    /// The key bindings appended to `/help`, for a surface that has some.
    fn help_keys(&self) -> Option<&'static str> {
        None
    }

    // --- what it says ---

    /// Report success or information to the user.
    fn notice(&mut self, text: String);

    /// Report a refusal or a failure. A surface with one channel sends both
    /// here.
    fn error(&mut self, message: String);

    // --- what it can see ---

    /// The session as `/status` and `/cost` describe it.
    fn snapshot(&self) -> SessionSnapshot;

    /// Whether plan mode (and omakase) are on right now.
    fn plan(&self) -> PlanState;

    /// The background `execute` tasks this session has spawned, or why they
    /// cannot be listed at the moment.
    fn background_tasks(&self) -> Result<Vec<Task>, String>;

    /// The turns `/rewind` could go back to, newest first.
    fn rewind_candidates(&self) -> Vec<RewindCandidate>;

    // --- what it does to the agent ---

    /// `/model <tag>`. Reporting is the surface's: one rebuilds in the
    /// background and answers when it lands, the other swaps in place.
    async fn set_model(&mut self, tag: String);

    /// `/mode <genie|sovereign>`. `false` when the surface refused and has
    /// already said why (a turn is running, the agent is rebuilding).
    async fn set_mode(&mut self, mode: Mode) -> bool;

    /// `/effort <level>`; `None` clears back to the provider default.
    async fn set_effort(&mut self, effort: Option<ReasoningEffort>) -> bool;

    /// `/plan` and `/omakase`, which are one state: see [`PlanState`].
    async fn set_plan(&mut self, plan: PlanState) -> bool;

    /// `/compact`: summarize older history into a progress note now.
    async fn compact(&mut self);

    /// `/reload`: skills, scripted tools, and MCP servers.
    async fn reload(&mut self);

    /// `/rewind <turn>`: restore the file checkpoints and truncate history.
    async fn rewind(&mut self, turn: u64);

    /// `/btw <question>`: a side question that never enters history.
    async fn btw(&mut self, question: String);

    /// `/fork <task>`: a background side quest with the full conversation.
    async fn fork(&mut self, task: String);

    /// Start working toward a goal `/goal <text>` has just saved. `false` when
    /// the surface has nothing to start: the user sends the next message, and
    /// [`dispatch`] says so.
    async fn start_goal(&mut self, goal: String) -> bool;

    /// `/evolve`: add a skill, a scripted tool, or an MCP server.
    async fn evolve(&mut self, deep: bool, description: String);

    /// `/publish [branch]`: fork Wizard and hand back a one-line installer.
    async fn publish(&mut self, branch: Option<String>);

    /// `/fusion`: a council of providers, on or off.
    async fn toggle_fusion(&mut self);

    /// `/ultra`: a council of lenses, on or off.
    async fn toggle_ultra(&mut self);

    /// `/server [status|start|stop]`: a local model server's lifecycle.
    async fn server(&mut self, action: ServerAction);

    // --- what it does to its own window ---

    /// Open one of the surface's interactive choosers, returning whether it
    /// handled the command. Defaulted to "no chooser here", which is the
    /// honest answer from a server that holds an agent and draws nothing.
    async fn open(&mut self, chooser: Chooser) -> bool {
        let _ = chooser;
        false
    }

    /// `/clear`: drop the conversation and start a fresh session.
    async fn clear(&mut self) {
        let message = elsewhere("clear", self.surface());
        self.error(message);
    }

    /// `/resume <id>`: reopen a past session and continue it.
    async fn resume(&mut self, id: String) {
        let _ = id;
        let message = elsewhere("resume", self.surface());
        self.error(message);
    }

    /// `/resume-claude <id>`: import a Claude Code conversation and continue
    /// it here. `id` is a Claude Code session id, or a unique prefix of one.
    ///
    /// Not a variant of [`Surface::resume`]: this one reads
    /// `~/.claude/projects/` (never writing there), converts the conversation
    /// with [`crate::claude_resume::import`], and resumes the **new** Wizard
    /// session that produces.
    async fn resume_claude(&mut self, id: String) {
        let _ = id;
        let message = elsewhere("resume-claude", self.surface());
        self.error(message);
    }

    /// `/diff`, `/todos`, `/dashboard`: the panels beside the conversation.
    async fn toggle_panel(&mut self, panel: Panel) {
        let message = elsewhere(panel.name(), self.surface());
        self.error(message);
    }

    /// `/vim`: modal editing of the composer.
    async fn toggle_vim(&mut self) {
        let message = elsewhere("vim", self.surface());
        self.error(message);
    }

    /// `/ui [name]`: list the interfaces, or wear one.
    async fn set_ui(&mut self, name: Option<String>) {
        let _ = name;
        let message = elsewhere("ui", self.surface());
        self.error(message);
    }

    /// `/quit`: end the session.
    async fn quit(&mut self) {
        let message = elsewhere("quit", self.surface());
        self.error(message);
    }

    /// Save the roster chosen at the `/ultra config` editor.
    async fn apply_ultra(&mut self, config: UltraConfig) {
        let _ = config;
        let message = Chooser::UltraRoster
            .usage()
            .expect("the roster editor names the file to edit instead")
            .to_string();
        self.error(message);
    }

    /// `/provider list|use|add|remove`.
    async fn provider(&mut self, action: ProviderAction) {
        let _ = action;
        let message = elsewhere("provider", self.surface());
        self.error(message);
    }

    /// Finalize an interactive provider setup: store the key, add, switch.
    async fn provider_setup(
        &mut self,
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key: Option<String>,
    ) {
        let _ = (name, kind, base_url, model, api_key);
        let message = elsewhere("provider", self.surface());
        self.error(message);
    }

    /// `/login <provider> [force]`: an OAuth sign-in.
    async fn login(&mut self, provider: String, force: bool) {
        let _ = (provider, force);
        let message = elsewhere("login", self.surface());
        self.error(message);
    }

    /// Import the selected artifacts from Claude Code.
    async fn import_claude(&mut self, selection: ImportSelection) {
        let _ = selection;
        let message = elsewhere("settings", self.surface());
        self.error(message);
    }
}

/* ---------------------------------------------------------------------- */
/* The dispatcher                                                         */
/* ---------------------------------------------------------------------- */

/// Run one slash command against `surface`, built-in or plugin-registered.
///
/// The gate comes first, and it is asked of the *command* rather than of the
/// table, so it covers both kinds: whatever the surface declares
/// [`Execution::Unavailable`] is refused by name, with what it would have done
/// somewhere else. A plugin command then runs its own handler
/// ([`run_plugin`]); a built-in routes to a [`CommandSurface`] verb, and what
/// the user is told about it is written here rather than on the surface.
pub async fn dispatch<S: CommandSurface + Send + ?Sized>(command: SlashCommand, surface: &mut S) {
    let at = surface.surface();
    // The gate is asked of the command and not of the table, so a plugin
    // command's "TUI only" is enforced by this line rather than by a second
    // one somewhere that would have to remember to exist.
    if command.execution(at) == Execution::Unavailable {
        let message = unavailable(command.name(), at);
        return surface.error(message);
    }
    if let SlashCommand::Plugin { name, args } = command {
        return run_plugin(name, args, surface).await;
    }
    // Total for every remaining variant: `every_command_has_a_table_row`.
    // `Plugin` is the one that has none and it has already left.
    let Some(spec) = command.spec() else {
        return surface.error(format!("'/{}' has no command table row", command.name()));
    };

    match command {
        SlashCommand::Help => {
            let mut text = help_text(at);
            if let Some(keys) = surface.help_keys() {
                text.push_str("\n\n");
                text.push_str(keys);
            }
            surface.notice(text);
        }
        SlashCommand::Clear => surface.clear().await,

        SlashCommand::Model(None) => choose(surface, Chooser::Model, spec).await,
        SlashCommand::Model(Some(tag)) => surface.set_model(tag).await,

        SlashCommand::Mode(None) => choose(surface, Chooser::Mode, spec).await,
        SlashCommand::Mode(Some(mode)) => {
            if surface.set_mode(mode).await {
                surface.notice(format!("switched to {mode} mode"));
            }
        }

        SlashCommand::Effort(None) => choose(surface, Chooser::Effort, spec).await,
        SlashCommand::Effort(Some(effort)) => {
            if surface.set_effort(effort).await {
                surface.notice(match effort {
                    Some(effort) => format!("reasoning effort: {effort}"),
                    None => "reasoning effort: provider default".to_string(),
                });
            }
        }

        SlashCommand::Plan => {
            let next = surface.plan().toggled_plan();
            if surface.set_plan(next).await {
                surface.notice(plan_notice(next.plan).to_string());
            }
        }
        SlashCommand::Omakase => {
            let next = surface.plan().toggled_omakase();
            if surface.set_plan(next).await {
                surface.notice(omakase_notice(next.omakase).to_string());
            }
        }

        SlashCommand::Rewind(None) => {
            if !surface.open(Chooser::Rewind).await {
                let text = rewind_report(&surface.rewind_candidates());
                surface.notice(text);
            }
        }
        SlashCommand::Rewind(Some(turn)) => surface.rewind(turn).await,

        SlashCommand::Resume(None) => choose(surface, Chooser::Resume, spec).await,
        SlashCommand::Resume(Some(id)) => surface.resume(id).await,
        SlashCommand::ResumeClaude(None) => choose(surface, Chooser::ResumeClaude, spec).await,
        SlashCommand::ResumeClaude(Some(id)) => surface.resume_claude(id).await,

        SlashCommand::Compact => surface.compact().await,
        SlashCommand::Reload => surface.reload().await,

        SlashCommand::Agents => {
            if !surface.open(Chooser::Agents).await {
                surface.notice(agents_report());
            }
        }

        SlashCommand::Diff => surface.toggle_panel(Panel::Diff).await,
        SlashCommand::Todos => surface.toggle_panel(Panel::Todos).await,
        SlashCommand::Dashboard => surface.toggle_panel(Panel::Dashboard).await,

        SlashCommand::Cost => {
            let text = cost_report(&surface.snapshot());
            surface.notice(text);
        }
        SlashCommand::Status => {
            let text = status_report(&surface.snapshot());
            surface.notice(text);
        }
        SlashCommand::Bashes => {
            let text = match surface.background_tasks() {
                Ok(tasks) => bashes_report(&tasks),
                Err(why) => why,
            };
            surface.notice(text);
        }
        SlashCommand::Memory(action) => {
            let text = crate::memory::report(&surface.project_root(), &action);
            surface.notice(text);
        }
        SlashCommand::Doctor => {
            let checks = crate::doctor::run_checks(&surface.project_root()).await;
            surface.notice(format!("doctor:\n{}", crate::doctor::render(&checks)));
        }

        SlashCommand::Btw(question) => surface.btw(question).await,
        SlashCommand::Fork(task) => surface.fork(task).await,

        SlashCommand::Goal(None) => {
            let text = goal_report(&surface.project_root());
            surface.notice(text);
        }
        SlashCommand::Goal(Some(text)) => match save_goal(&surface.project_root(), &text) {
            Err(message) => surface.error(message),
            Ok(goal) => {
                let started = surface.start_goal(goal.clone()).await;
                let mut notice = format!("standing goal set:\n{goal}");
                if !started {
                    notice.push_str("\nsend a message to start working toward it");
                }
                surface.notice(notice);
            }
        },

        SlashCommand::Evolve { deep, description } => {
            surface.notice(format!(
                "evolving ({}): {description}",
                if deep { "deep" } else { "runtime" }
            ));
            surface.evolve(deep, description).await;
        }
        SlashCommand::Publish { branch } => {
            surface.notice(format!(
                "publishing Wizard{}…",
                branch
                    .as_deref()
                    .map(|branch| format!(" (branch: {branch})"))
                    .unwrap_or_default()
            ));
            surface.publish(branch).await;
        }

        SlashCommand::Fusion(FusionAction::Toggle) => surface.toggle_fusion().await,
        SlashCommand::Fusion(FusionAction::Config) => {
            choose(surface, Chooser::FusionPanel, spec).await
        }
        SlashCommand::Ultra(UltraAction::Toggle) => surface.toggle_ultra().await,
        SlashCommand::Ultra(UltraAction::Config) => {
            choose(surface, Chooser::UltraRoster, spec).await
        }
        SlashCommand::Ultra(UltraAction::Apply(config)) => surface.apply_ultra(config).await,

        SlashCommand::Provider(ProviderAction::Menu) => {
            choose(surface, Chooser::Provider, spec).await
        }
        SlashCommand::Provider(action) => surface.provider(action).await,
        SlashCommand::ProviderSetup {
            name,
            kind,
            base_url,
            model,
            api_key,
        } => {
            surface
                .provider_setup(name, kind, base_url, model, api_key)
                .await
        }

        SlashCommand::Server(action) => surface.server(action).await,

        // The one supported sign-in, checked here so both surfaces refuse an
        // unknown one in the same words.
        SlashCommand::Login { provider, .. } if provider != "xai" => {
            surface.error(format!(
                "unknown login provider '{provider}' (supported: xai)"
            ));
        }
        SlashCommand::Login { provider, force } => surface.login(provider, force).await,

        SlashCommand::Settings => choose(surface, Chooser::Settings, spec).await,
        SlashCommand::ImportClaude(selection) => surface.import_claude(selection).await,
        SlashCommand::Vim => surface.toggle_vim().await,
        SlashCommand::Ui(name) => surface.set_ui(name).await,
        SlashCommand::Quit => surface.quit().await,

        // Answered above, before `spec` was taken. The early return is what
        // lets every arm here hold a plain `&'static CommandSpec` instead of an
        // `Option` unwrapped thirty times.
        SlashCommand::Plugin { .. } => unreachable!("a plugin command returns above"),
    }
}

/// Open `chooser`, or say what a surface without one needs instead.
async fn choose<S: CommandSurface + Send + ?Sized>(
    surface: &mut S,
    chooser: Chooser,
    spec: &'static CommandSpec,
) {
    if surface.open(chooser).await {
        return;
    }
    let message = match chooser.usage() {
        Some(usage) => usage.to_string(),
        None => elsewhere(spec.name, surface.surface()),
    };
    surface.error(message);
}

/// Run a plugin-registered command and report what it answered.
///
/// Looked up now rather than carried in the variant, which is what makes an
/// unload exact from the user's side too: a `/name` typed while the plugin was
/// loaded and dispatched after it went away is refused here instead of running
/// a handler nothing owns any more.
async fn run_plugin<S: CommandSurface + Send + ?Sized>(
    name: String,
    args: String,
    surface: &mut S,
) {
    let Some(command) = crate::commands::plugin::get(&name) else {
        return surface.error(format!(
            "'/{name}' is not registered any more — the plugin that owned it was unloaded"
        ));
    };
    match command.run(args).await {
        // An empty answer is "nothing to say", the same as a built-in that
        // only toggles something. A blank notice would be a blank line in the
        // transcript and a blank message in a chat.
        Ok(text) if text.trim().is_empty() => {}
        Ok(text) => surface.notice(text),
        Err(err) => surface.error(format!("/{name}: {err:#}")),
    }
}

/* ---------------------------------------------------------------------- */
/* What a refusal says                                                    */
/* ---------------------------------------------------------------------- */

/// The answer to a command this part of the surface does not run itself.
fn elsewhere(name: &str, surface: Surface) -> String {
    match surface {
        Surface::Gui => format!(
            "'/{name}' is part of the window, not the agent — the window runs it, not the chat's \
             worker"
        ),
        Surface::Tui => format!("'/{name}' is not available in this session"),
        // The gateway has no second half to point at: there is one process,
        // and it is the one that just refused.
        Surface::Gateway => format!("'/{name}' is not available in this chat"),
    }
}

/// The answer to a command with genuinely nowhere to land on `surface`. It says
/// what the command *is*, rather than pretending to have done something or
/// going quietly missing.
fn unavailable(name: &str, surface: Surface) -> String {
    match (surface, name) {
        (Surface::Gui, "vim") => "'/vim' toggles modal editing of the terminal composer; this one \
                                  is a window's text field, and it edits the way the rest of your \
                                  desktop does"
            .to_string(),
        (Surface::Gui, "ui") => "'/ui' swaps the terminal's chrome for Codex's or Grok Build's. \
                                 This window draws its own widgets, so there is no terminal \
                                 chrome in it to reshape"
            .to_string(),
        (Surface::Gui, "quit") => "'/quit' exits the terminal app. This chat is a window: close \
                                   it, and the chats it was holding stop with it"
            .to_string(),
        (Surface::Gateway, "vim" | "ui") => format!(
            "'/{name}' changes how a composer, a palette or the chrome looks; this session is a \
             chat, and Telegram draws it"
        ),
        (Surface::Gateway, "quit") => "'/quit' exits the terminal app. The gateway is a service \
                                       every allow-listed chat shares: use /clear to start a \
                                       fresh conversation, or stop it on the machine running it"
            .to_string(),
        (_, other) => format!("'/{other}' does not run on this surface"),
    }
}

/// `/plan`, either way.
fn plan_notice(on: bool) -> &'static str {
    match on {
        true => {
            "plan mode on — the agent investigates read-only and presents a plan for approval \
                 (/plan to leave)"
        }
        false => "plan mode off",
    }
}

/// `/omakase`, either way.
fn omakase_notice(on: bool) -> &'static str {
    match on {
        true => {
            "omakase on — chef's choice: the agent explores read-only, decides the approach \
                 itself, and executes its own plan (/omakase to leave)"
        }
        false => "omakase off — back to plan mode (you review the plan)",
    }
}

/* ---------------------------------------------------------------------- */
/* The reports                                                            */
/* ---------------------------------------------------------------------- */

/// `/help` for `surface`: everything it runs, straight off the table, plus an
/// honest line about what it does not.
///
/// Derived on every surface, never hand-kept. A hand-written list is how the
/// terminal's `/help` came to omit `/exit`.
pub fn help_text(surface: Surface) -> String {
    let mut text = String::from("commands:");
    for row in crate::commands::available(surface) {
        match row.args.is_empty() {
            true => text.push_str(&format!("\n  /{} — {}", row.name, row.description)),
            false => text.push_str(&format!(
                "\n  /{} {} — {}",
                row.name, row.args, row.description
            )),
        }
    }
    // Built-ins only. A plugin command that is unavailable here is unavailable
    // because the plugin said so, and listing somebody else's `/name` under
    // "terminal only" would read as a promise this build cannot keep.
    let missing: Vec<String> = commands_for(surface, Execution::Unavailable)
        .map(|spec| format!("/{}", spec.name))
        .collect();
    if !missing.is_empty() {
        // What the gap *is* differs by surface: the window's is genuinely the
        // terminal's alone, while the gateway also misses commands the window
        // runs (`/diff`, `/settings`). What those have in common is a screen,
        // not a terminal, so saying "terminal only" there would be wrong.
        let label = match surface {
            Surface::Tui | Surface::Gui => "terminal only",
            Surface::Gateway => "not available over chat",
        };
        text.push_str(&format!("\n\n{label}: {}", missing.join(", ")));
    }
    text.push_str("\n\nplus any custom command in .wizard/commands/*.md, and @path to");
    text.push_str(" reference a file.");
    text
}

/// `/status`: what this session is, in one notice. Fields the surface could
/// not see are left out rather than invented.
fn status_report(session: &SessionSnapshot) -> String {
    let effort = session
        .effort
        .map(|effort| effort.to_string())
        .unwrap_or_else(|| "default".to_string());
    let mut text = format!(
        "model: {}\nprovider: {} ({:?} @ {})\nmode: {}\neffort: {effort}",
        session.model,
        session.provider_name,
        session.provider_kind,
        session.provider_base_url,
        session.mode,
    );
    if let Some(steps) = session.max_steps {
        text.push_str(&format!("\nsteps: {steps}"));
    }
    match &session.session {
        Some(id) => text.push_str(&format!("\nsession: {id}")),
        None => text.push_str("\nsession: (turn running)"),
    }
    text.push_str(&format!(
        "\nusage: {} prompt + {} completion tokens",
        session.prompt_tokens, session.completion_tokens
    ));
    if let Some(context) = session.context_tokens {
        text.push_str(&format!("\ncontext: {context} tokens"));
    }
    if let Some(running) = session.background_tasks {
        text.push_str(&format!("\nbackground tasks: {running} running"));
    }
    let (done, total) = session.todos;
    match total {
        0 => text.push_str("\ntodos: none"),
        _ => text.push_str(&format!("\ntodos: {done}/{total} done")),
    }
    text.push_str(&format!("\nplan mode: {}", session.plan.describe()));
    if let Some(ultra) = &session.ultra {
        text.push_str(&format!("\nultra: {ultra}"));
    }
    text
}

/// `/cost`: session token totals, with an estimate when the active provider
/// carries rates and the line that explains how to configure them when it does
/// not.
fn cost_report(session: &SessionSnapshot) -> String {
    let mut text = format!(
        "session usage: {} prompt + {} completion tokens",
        session.prompt_tokens, session.completion_tokens
    );
    let (cache_read, cache_write) = session.cache_tokens.unwrap_or((0, 0));
    match crate::usage::cost_usd(
        crate::usage::TurnTokens {
            prompt: session.prompt_tokens,
            completion: session.completion_tokens,
            cache_read,
            cache_write,
        },
        session.usd_per_mtok_in,
        session.usd_per_mtok_out,
    ) {
        Some(cost) => text.push_str(&format!(" · est. ${cost:.4}")),
        None => text.push_str(&format!(
            "\nset usd_per_mtok_in / usd_per_mtok_out on provider '{}' in \
             ~/.wizard/config.toml for cost estimates",
            session.provider_name
        )),
    }
    text
}

/// `/bashes`: the background `execute` tasks this session has spawned, running
/// and finished, newest last.
fn bashes_report(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "background tasks: none".to_string();
    }
    let mut text = String::from("background tasks:\n");
    for task in tasks {
        text.push_str(&format!(
            "  #{} [{}] {}\n",
            task.id,
            task.status.describe(),
            task.command
        ));
    }
    text.trim_end().to_string()
}

/// `/agents`: the subagent roster. A surface with a picker opens one over
/// exactly this list; here it is the list, and how to use it.
fn agents_report() -> String {
    let dir = match Config::subagents_dir() {
        Ok(dir) => dir,
        Err(err) => return format!("could not resolve the subagents directory: {err:#}"),
    };
    let configs = crate::agent::subagent::available_configs(&dir);
    if configs.is_empty() {
        return format!("no subagents available ({})", dir.display());
    }
    let mut text = String::from("subagents:\n");
    for config in &configs {
        let scope = match &config.tool_scope {
            None => "all tools".to_string(),
            Some(names) => names.join(", "),
        };
        text.push_str(&format!(
            "  {} — {} · {scope} · {}\n",
            config.name, config.description, config.max_steps
        ));
    }
    text.push_str("\nask for one by name to delegate to it.");
    text
}

/// Bare `/rewind`: the turns there is something to go back to, newest first. A
/// surface with a picker opens one over exactly this list.
fn rewind_report(candidates: &[RewindCandidate]) -> String {
    if candidates.is_empty() {
        return "nothing to rewind yet".to_string();
    }
    let mut text = String::from("rewind to before turn:\n");
    for candidate in candidates {
        text.push_str(&format!(
            "  {} — {}\n",
            candidate.turn,
            rewind_detail(candidate)
        ));
    }
    text.push_str("\n/rewind <turn> restores the files and truncates the conversation.");
    text
}

/// One rewind candidate's second line: the prompt that started the turn, the
/// files it snapshotted, or both.
fn rewind_detail(candidate: &RewindCandidate) -> String {
    let files = candidate
        .files
        .iter()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        })
        .collect::<Vec<_>>()
        .join(", ");
    match (candidate.prompt.is_empty(), files.is_empty()) {
        (false, false) => format!("{} · {files}", candidate.prompt),
        (false, true) => candidate.prompt.clone(),
        (true, false) => files,
        (true, true) => String::new(),
    }
}

/// `/goal`: the standing mission for this workspace
/// (`<project_root>/.wizard/mission.toml`), which drives sovereign runs.
fn goal_report(project_root: &Path) -> String {
    match crate::agent::mission::Mission::load(project_root) {
        Err(err) => format!("could not read mission: {err:#}"),
        Ok(None) => "no standing goal set — use `/goal <text>` to set one \
                     (drives sovereign/continuous mode)"
            .to_string(),
        Ok(Some(mission)) => {
            let mut text = format!(
                "goal: {}\ncycles: {}  ·  updated {}",
                mission.goal,
                mission.cycles,
                mission.updated.format("%Y-%m-%d %H:%M UTC"),
            );
            if !mission.notes.is_empty() {
                text.push_str("\nrecent:");
                let skip = mission.notes.len().saturating_sub(5);
                for note in &mission.notes[skip..] {
                    text.push_str(&format!("\n  - {note}"));
                }
            }
            text
        }
    }
}

/// `/goal <text>`: set the standing mission, noting the change on an existing
/// one rather than dropping its history. Returns the goal as saved.
fn save_goal(project_root: &Path, text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("usage: /goal <text>".to_string());
    }
    let mission = match crate::agent::mission::Mission::load(project_root) {
        Err(err) => return Err(format!("could not read mission: {err:#}")),
        Ok(Some(mut mission)) => {
            mission.goal = text.to_string();
            mission.note(format!("goal changed to: {text}"));
            mission
        }
        Ok(None) => crate::agent::mission::Mission::new(text),
    };
    mission
        .save(project_root)
        .map_err(|err| format!("could not save mission: {err:#}"))?;
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commands::{COMMANDS, spec};

    /// What a command did, as the recording surface saw it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Saw {
        Verb(&'static str),
        Notice,
        Error(String),
    }

    /// A surface that implements the whole interface by writing down which verb
    /// it was asked for. Everything is overridden, defaults included, so
    /// a command that reaches a default here is a command the dispatcher failed
    /// to route.
    #[derive(Default)]
    struct Recorder {
        at: Option<Surface>,
        saw: Vec<Saw>,
    }

    impl Recorder {
        fn new(at: Surface) -> Self {
            Self {
                at: Some(at),
                saw: Vec::new(),
            }
        }

        fn verb(&mut self, name: &'static str) {
            self.saw.push(Saw::Verb(name));
        }
    }

    #[async_trait]
    impl CommandSurface for Recorder {
        fn surface(&self) -> Surface {
            self.at.expect("a surface")
        }
        fn project_root(&self) -> PathBuf {
            std::env::temp_dir().join("wizard-command-dispatch-test")
        }
        fn notice(&mut self, _text: String) {
            self.saw.push(Saw::Notice);
        }
        fn error(&mut self, message: String) {
            self.saw.push(Saw::Error(message));
        }
        fn snapshot(&self) -> SessionSnapshot {
            SessionSnapshot {
                model: "m".to_string(),
                provider_name: "p".to_string(),
                provider_kind: ProviderKind::OLLAMA,
                provider_base_url: "u".to_string(),
                mode: Mode::Genie,
                effort: None,
                max_steps: None,
                session: None,
                prompt_tokens: 0,
                cache_tokens: None,
                completion_tokens: 0,
                context_tokens: None,
                background_tasks: None,
                todos: (0, 0),
                plan: PlanState::default(),
                ultra: None,
                usd_per_mtok_in: None,
                usd_per_mtok_out: None,
            }
        }
        fn plan(&self) -> PlanState {
            PlanState::default()
        }
        fn background_tasks(&self) -> Result<Vec<Task>, String> {
            Ok(Vec::new())
        }
        fn rewind_candidates(&self) -> Vec<RewindCandidate> {
            Vec::new()
        }

        async fn set_model(&mut self, _tag: String) {
            self.verb("set_model");
        }
        async fn set_mode(&mut self, _mode: Mode) -> bool {
            self.verb("set_mode");
            true
        }
        async fn set_effort(&mut self, _effort: Option<ReasoningEffort>) -> bool {
            self.verb("set_effort");
            true
        }
        async fn set_plan(&mut self, _plan: PlanState) -> bool {
            self.verb("set_plan");
            true
        }
        async fn compact(&mut self) {
            self.verb("compact");
        }
        async fn reload(&mut self) {
            self.verb("reload");
        }
        async fn rewind(&mut self, _turn: u64) {
            self.verb("rewind");
        }
        async fn btw(&mut self, _question: String) {
            self.verb("btw");
        }
        async fn fork(&mut self, _task: String) {
            self.verb("fork");
        }
        async fn start_goal(&mut self, _goal: String) -> bool {
            self.verb("start_goal");
            true
        }
        async fn evolve(&mut self, _deep: bool, _description: String) {
            self.verb("evolve");
        }
        async fn publish(&mut self, _branch: Option<String>) {
            self.verb("publish");
        }
        async fn toggle_fusion(&mut self) {
            self.verb("toggle_fusion");
        }
        async fn toggle_ultra(&mut self) {
            self.verb("toggle_ultra");
        }
        async fn server(&mut self, _action: ServerAction) {
            self.verb("server");
        }

        async fn open(&mut self, _chooser: Chooser) -> bool {
            self.verb("open");
            true
        }
        async fn clear(&mut self) {
            self.verb("clear");
        }
        async fn resume(&mut self, _id: String) {
            self.verb("resume");
        }
        async fn toggle_panel(&mut self, _panel: Panel) {
            self.verb("toggle_panel");
        }
        async fn toggle_vim(&mut self) {
            self.verb("toggle_vim");
        }
        async fn set_ui(&mut self, _name: Option<String>) {
            self.verb("set_ui");
        }
        async fn quit(&mut self) {
            self.verb("quit");
        }
        async fn apply_ultra(&mut self, _config: UltraConfig) {
            self.verb("apply_ultra");
        }
        async fn provider(&mut self, _action: ProviderAction) {
            self.verb("provider");
        }
        async fn provider_setup(
            &mut self,
            _name: String,
            _kind: ProviderKind,
            _base_url: String,
            _model: String,
            _api_key: Option<String>,
        ) {
            self.verb("provider_setup");
        }
        async fn login(&mut self, _provider: String, _force: bool) {
            self.verb("login");
        }
        async fn import_claude(&mut self, _selection: ImportSelection) {
            self.verb("import_claude");
        }
    }

    /// A line that exercises each table row, with the argument the row's form
    /// needs (`/evolve` and friends parse only with the thing they carry).
    fn invocation(spec: &CommandSpec) -> String {
        match spec.name {
            "evolve" => "/evolve add a linter".to_string(),
            "btw" => "/btw why".to_string(),
            "fork" => "/fork read the docs".to_string(),
            "login" => "/login xai".to_string(),
            name => format!("/{name}"),
        }
    }

    /// **The anti-drift test.** Every row of the table, on every surface,
    /// reaches the dispatcher and comes back with an answer: a verb the surface
    /// supplied, or a refusal the table asked for. A row nothing routes would
    /// come back silent, which is exactly what `/goal` did on the second
    /// surface for as long as nobody typed it.
    #[tokio::test]
    async fn dispatch_reaches_every_command_on_every_surface() {
        for &at in Surface::ALL {
            for spec in COMMANDS {
                let line = invocation(spec);
                let command = SlashCommand::parse(&line)
                    .unwrap_or_else(|| panic!("{line} is a slash command"))
                    .unwrap_or_else(|err| panic!("{line} parses: {err}"));
                let mut recorder = Recorder::new(at);
                dispatch(command, &mut recorder).await;
                assert!(
                    !recorder.saw.is_empty(),
                    "{line} on {at:?} was silently dropped"
                );
                if spec.execution(at) == Execution::Unavailable {
                    let refused = matches!(recorder.saw.first(), Some(Saw::Error(_)));
                    assert!(
                        refused,
                        "{line} is unavailable on {at:?}, so it is refused by name, not run: {:?}",
                        recorder.saw
                    );
                }
            }
        }
    }

    /// The refusal for an unavailable command names what the command *is*.
    /// "unknown command" would be a lie and a missing arm would be silence.
    #[tokio::test]
    async fn an_unavailable_command_is_refused_by_name() {
        for (name, expected) in [
            ("vim", "modal editing"),
            ("ui", "draws its own widgets"),
            ("quit", "close it"),
            ("exit", "close it"),
        ] {
            let mut recorder = Recorder::new(Surface::Gui);
            let command = SlashCommand::parse(&format!("/{name}")).unwrap().unwrap();
            dispatch(command, &mut recorder).await;
            match recorder.saw.first() {
                Some(Saw::Error(message)) => assert!(
                    message.contains(expected),
                    "/{name} should say what it is: {message}"
                ),
                other => panic!("/{name} should be refused, got {other:?}"),
            }
        }
    }

    /// Whether `line` is a `match` arm over [`SlashCommand`], the shape a
    /// surface's own copy of a command takes. Both the single-pattern arm and
    /// the `|`-continued one, because the second is what the hand-rolled
    /// executor this replaced ended with.
    fn is_a_command_arm(line: &str) -> bool {
        let line = line.trim_start();
        (line.starts_with("SlashCommand::") || line.starts_with("| SlashCommand::"))
            && line.contains("=>")
    }

    /// **The other anti-drift test.** No surface may keep its own copy of a
    /// command. [`dispatch`] owns every [`SlashCommand`] arm; a surface that
    /// grows a second `match` over the enum has forked the semantics, which is
    /// how `/goal`, `/model` and `/help` came to mean different things in
    /// different windows.
    ///
    /// This is also what keeps `/help` derived. A hand-written command list can
    /// only reach the user through an arm that shadows the registry's, and
    /// there is nowhere left to put one.
    #[test]
    fn no_surface_hand_rolls_a_handler_that_shadows_the_registry() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let commands = src.join("commands");
        let mut offenders: Vec<String> = Vec::new();
        walk(&src, &mut |path: &Path, body: &str| {
            // The table, the parser and this dispatcher are the one place.
            if path.starts_with(&commands) {
                return;
            }
            for (number, line) in body.lines().enumerate() {
                if is_a_command_arm(line) {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        });
        assert!(
            offenders.is_empty(),
            "these dispatch slash commands outside src/commands/, where the one \
             dispatcher lives:\n{}",
            offenders.join("\n")
        );
    }

    /// The scan above has to recognize the shapes a second executor is written
    /// in, or it passes because it cannot see one rather than because there is
    /// none.
    #[test]
    fn the_scan_knows_what_a_second_executor_looks_like() {
        assert!(is_a_command_arm(
            "            SlashCommand::Help => self.app.notice(HELP_TEXT),"
        ));
        assert!(is_a_command_arm("        SlashCommand::Compact => {"));
        assert!(is_a_command_arm(
            "        | SlashCommand::ImportClaude(_) => error(shared, elsewhere(&name)),"
        ));
        // Constructing a command is not executing one: the pickers emit them,
        // and they emit them *into* the dispatcher.
        assert!(!is_a_command_arm(
            "    \"model\" => return Some(AppAction::Command(SlashCommand::Model(None))),"
        ));
        assert!(!is_a_command_arm("        SlashCommand::Clear"));
    }

    /// `/help` is derived from the table wherever it is asked for. A
    /// hand-written list is how the terminal's came to omit `/exit`.
    #[test]
    fn help_is_derived_from_the_table_on_every_surface() {
        for &at in Surface::ALL {
            let text = help_text(at);
            for spec in COMMANDS {
                let listed = text.contains(&format!("\n  /{} ", spec.name))
                    || text.contains(&format!("\n  /{} —", spec.name));
                match spec.execution(at) {
                    Execution::Unavailable => {
                        assert!(
                            !listed,
                            "/{} does not run on {at:?}, so it is not offered there",
                            spec.name
                        );
                        assert!(
                            text.contains(&format!("/{}", spec.name)),
                            "/{} is named as unavailable on {at:?}, not silently absent",
                            spec.name
                        );
                    }
                    _ => assert!(listed, "/{} is missing from {at:?}'s help", spec.name),
                }
            }
        }
        // The one the hand-written list dropped.
        assert!(help_text(Surface::Tui).contains("/exit"));
    }

    /// `/help` says which commands are terminal-only rather than leaving them
    /// out, so the window's answer is honest about the gap.
    #[test]
    fn help_names_the_commands_the_window_does_not_run() {
        let text = help_text(Surface::Gui);
        assert!(
            text.contains("/goal [text] — show the standing goal, or set one and start working")
        );
        assert!(text.contains("/diff"));
        assert!(
            !text.contains("\n  /vim"),
            "not offered as a command: {text}"
        );
        assert!(
            text.contains("terminal only: /vim, /ui, /quit, /exit"),
            "but named, not silently absent: {text}"
        );
        assert!(text.contains("plus any custom command"));
    }

    #[test]
    fn plan_and_omakase_are_one_state() {
        let off = PlanState::default();
        assert_eq!(
            off.toggled_plan(),
            PlanState {
                plan: true,
                omakase: false
            }
        );
        // Omakase is plan mode with the review gate removed, so it turns plan
        // mode on with it and leaves it on when it goes.
        let omakase = off.toggled_omakase();
        assert_eq!(
            omakase,
            PlanState {
                plan: true,
                omakase: true
            }
        );
        assert_eq!(
            omakase.toggled_omakase(),
            PlanState {
                plan: true,
                omakase: false
            }
        );
        // Leaving plan mode leaves omakase with it.
        assert_eq!(omakase.toggled_plan(), PlanState::default());
        assert_eq!(omakase.describe(), "on (omakase — chef's choice)");
        assert_eq!(off.describe(), "off");
    }

    /// The bare form of a picker command answers with the argument the picker
    /// would have supplied, on a surface that has no picker.
    #[tokio::test]
    async fn a_surface_without_a_picker_names_the_argument_instead() {
        struct NoPickers(Recorder);

        #[async_trait]
        impl CommandSurface for NoPickers {
            fn surface(&self) -> Surface {
                self.0.surface()
            }
            fn project_root(&self) -> PathBuf {
                self.0.project_root()
            }
            fn notice(&mut self, text: String) {
                self.0.notice(text);
            }
            fn error(&mut self, message: String) {
                self.0.error(message);
            }
            fn snapshot(&self) -> SessionSnapshot {
                self.0.snapshot()
            }
            fn plan(&self) -> PlanState {
                self.0.plan()
            }
            fn background_tasks(&self) -> Result<Vec<Task>, String> {
                self.0.background_tasks()
            }
            fn rewind_candidates(&self) -> Vec<RewindCandidate> {
                self.0.rewind_candidates()
            }
            async fn set_model(&mut self, tag: String) {
                self.0.set_model(tag).await;
            }
            async fn set_mode(&mut self, mode: Mode) -> bool {
                self.0.set_mode(mode).await
            }
            async fn set_effort(&mut self, effort: Option<ReasoningEffort>) -> bool {
                self.0.set_effort(effort).await
            }
            async fn set_plan(&mut self, plan: PlanState) -> bool {
                self.0.set_plan(plan).await
            }
            async fn compact(&mut self) {
                self.0.compact().await;
            }
            async fn reload(&mut self) {
                self.0.reload().await;
            }
            async fn rewind(&mut self, turn: u64) {
                self.0.rewind(turn).await;
            }
            async fn btw(&mut self, question: String) {
                self.0.btw(question).await;
            }
            async fn fork(&mut self, task: String) {
                self.0.fork(task).await;
            }
            async fn start_goal(&mut self, goal: String) -> bool {
                self.0.start_goal(goal).await
            }
            async fn evolve(&mut self, deep: bool, description: String) {
                self.0.evolve(deep, description).await;
            }
            async fn publish(&mut self, branch: Option<String>) {
                self.0.publish(branch).await;
            }
            async fn toggle_fusion(&mut self) {
                self.0.toggle_fusion().await;
            }
            async fn toggle_ultra(&mut self) {
                self.0.toggle_ultra().await;
            }
            async fn server(&mut self, action: ServerAction) {
                self.0.server(action).await;
            }
        }

        let mut surface = NoPickers(Recorder::new(Surface::Gui));
        dispatch(SlashCommand::Model(None), &mut surface).await;
        match surface.0.saw.first() {
            Some(Saw::Error(message)) => assert!(message.contains("usage: /model <tag>")),
            other => panic!("expected the usage line, got {other:?}"),
        }

        // A chooser that *is* the command says so instead of inventing an
        // argument: the window has a settings sheet, the agent does not.
        let mut surface = NoPickers(Recorder::new(Surface::Gui));
        dispatch(SlashCommand::Settings, &mut surface).await;
        match surface.0.saw.first() {
            Some(Saw::Error(message)) => assert!(message.contains("part of the window")),
            other => panic!("expected the window's own, got {other:?}"),
        }

        // And a bare `/rewind` falls back to the list the picker would show.
        let mut surface = NoPickers(Recorder::new(Surface::Gui));
        dispatch(SlashCommand::Rewind(None), &mut surface).await;
        assert_eq!(surface.0.saw, vec![Saw::Notice]);
    }

    #[test]
    fn the_reports_say_what_is_there_and_what_is_not() {
        assert_eq!(bashes_report(&[]), "background tasks: none");
        assert_eq!(rewind_report(&[]), "nothing to rewind yet");

        let snapshot = Recorder::default().snapshot();
        let status = status_report(&snapshot);
        assert!(status.contains("model: m"));
        assert!(status.contains("effort: default"));
        assert!(status.contains("session: (turn running)"));
        assert!(status.contains("todos: none"));
        assert!(status.contains("plan mode: off"));
        assert!(!status.contains("steps:"), "not invented: {status}");
        assert!(!status.contains("ultra:"), "not invented: {status}");

        let cost = cost_report(&snapshot);
        assert!(cost.contains("0 prompt + 0 completion tokens"));
        assert!(cost.contains("usd_per_mtok_in"), "how to price it: {cost}");
    }

    #[test]
    fn a_goal_needs_text_and_keeps_the_missions_history() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            save_goal(tmp.path(), "  "),
            Err("usage: /goal <text>".to_string())
        );
        assert_eq!(
            save_goal(tmp.path(), " ship the release "),
            Ok("ship the release".to_string())
        );
        save_goal(tmp.path(), "cut a patch release").unwrap();
        let text = goal_report(tmp.path());
        assert!(text.contains("goal: cut a patch release"), "got: {text}");
        assert!(
            text.contains("goal changed to: cut a patch release"),
            "the change is noted, not silently overwritten: {text}"
        );
    }

    /// Read every `.rs` file under `dir`, handing each to `visit`.
    fn walk(dir: &Path, visit: &mut impl FnMut(&Path, &str)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, visit);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
                && let Ok(body) = std::fs::read_to_string(&path)
            {
                visit(&path, &body);
            }
        }
    }

    #[test]
    fn the_table_and_the_dispatcher_agree_on_who_runs_what() {
        assert_eq!(spec("goal").map(|spec| spec.gui), Some(Execution::Agent));
        assert_eq!(spec("diff").map(|spec| spec.gui), Some(Execution::Ui));
        assert_eq!(spec("diff").map(|spec| spec.tui), Some(Execution::Ui));
        assert_eq!(
            spec("ui").map(|spec| spec.gui),
            Some(Execution::Unavailable)
        );
    }

    /// The gateway's column is the shape the surface actually is: everything a
    /// message can carry runs against the agent, everything that needs a screen
    /// or a human at a picker is declared missing, and nothing is the chat's
    /// own — a Telegram chat has no client half to run a command in.
    #[test]
    fn the_gateway_column_is_all_agent_and_no_window() {
        assert_eq!(
            spec("status").map(|spec| spec.gateway),
            Some(Execution::Agent)
        );
        assert_eq!(
            spec("model").map(|spec| spec.gateway),
            Some(Execution::Agent),
            "/model <tag> is typeable in a chat, picker or no picker"
        );
        for screen_only in ["diff", "todos", "dashboard", "vim", "ui", "settings"] {
            assert_eq!(
                spec(screen_only).map(|spec| spec.gateway),
                Some(Execution::Unavailable),
                "/{screen_only} needs a screen the gateway does not have"
            );
        }
        assert!(COMMANDS.iter().all(|spec| spec.gateway != Execution::Ui));
    }

    /* ------------------------------------------------------------------ */
    /* Plugin-registered commands                                         */
    /* ------------------------------------------------------------------ */

    /// A registration that withdraws itself. The registry is process-wide, so a
    /// probe left behind joins another test's palette assertion.
    struct Held(String);

    impl Drop for Held {
        fn drop(&mut self) {
            crate::commands::plugin::uninstall(&self.0);
        }
    }

    fn hold(command: crate::commands::PluginCommand) -> Held {
        let name = command.name.clone();
        crate::commands::plugin::install("dispatch-test", command).expect("free");
        Held(name)
    }

    /// A plugin command goes through the one dispatcher on every surface, and
    /// its handler is what answers. Nothing about it is a second path: the same
    /// `dispatch` call the built-ins take.
    #[tokio::test]
    async fn a_plugin_command_runs_through_the_one_dispatcher() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = Arc::clone(&seen);
        let _held = hold(crate::commands::PluginCommand::new(
            "zzdispatch",
            "a probe",
            Arc::new(move |args: String| {
                let recorded = Arc::clone(&recorded);
                async move {
                    recorded.lock().expect("lock").push(args.clone());
                    Ok(format!("answered {args}"))
                }
            }),
        ));

        for &at in Surface::ALL {
            let mut recorder = Recorder::new(at);
            let command = SlashCommand::parse("/zzdispatch a  b")
                .expect("known")
                .expect("ok");
            dispatch(command, &mut recorder).await;
            assert_eq!(recorder.saw, vec![Saw::Notice], "on {at:?}");
        }
        assert_eq!(
            *seen.lock().expect("lock"),
            vec!["a  b".to_string(); Surface::ALL.len()],
            "the handler got the raw tail on every surface"
        );
    }

    /// The per-surface gate is the same line of code for both kinds of command:
    /// a plugin that says "TUI only" is refused elsewhere by name, exactly as
    /// `/vim` is in the window.
    #[tokio::test]
    async fn a_plugin_command_can_be_tui_only_and_is_refused_elsewhere() {
        let _held = hold(
            crate::commands::PluginCommand::new(
                "zzterminal",
                "a probe",
                Arc::new(|_: String| async move { Ok("ran".to_string()) }),
            )
            .only(&[Surface::Tui]),
        );

        let parse = || {
            SlashCommand::parse("/zzterminal")
                .expect("known")
                .expect("ok")
        };

        let mut tui = Recorder::new(Surface::Tui);
        dispatch(parse(), &mut tui).await;
        assert_eq!(tui.saw, vec![Saw::Notice]);

        for at in [Surface::Gui, Surface::Gateway] {
            let mut recorder = Recorder::new(at);
            dispatch(parse(), &mut recorder).await;
            match recorder.saw.as_slice() {
                [Saw::Error(message)] => assert!(
                    message.contains("zzterminal"),
                    "the refusal names the command: {message}"
                ),
                other => panic!("expected a refusal on {at:?}, saw {other:?}"),
            }
        }
    }

    /// The command is looked up at dispatch, not carried in the variant, so a
    /// `/name` typed while the plugin was loaded and dispatched after it went
    /// away is refused rather than run.
    #[tokio::test]
    async fn a_plugin_command_whose_plugin_unloaded_is_refused() {
        let command = SlashCommand::Plugin {
            name: "zzgone".to_string(),
            args: String::new(),
        };
        let mut recorder = Recorder::new(Surface::Tui);
        dispatch(command, &mut recorder).await;
        match recorder.saw.as_slice() {
            [Saw::Error(message)] => assert!(message.contains("zzgone"), "{message}"),
            other => panic!("expected a refusal, saw {other:?}"),
        }
    }

    /// A handler that fails is reported as an error, and a handler with nothing
    /// to say prints nothing rather than a blank line.
    #[tokio::test]
    async fn a_plugin_commands_failure_and_its_silence_both_land_honestly() {
        let _failing = hold(crate::commands::PluginCommand::new(
            "zzfails",
            "a probe",
            Arc::new(|_: String| async move { Err(anyhow::anyhow!("the disk is on fire")) }),
        ));
        let _silent = hold(crate::commands::PluginCommand::new(
            "zzquiet",
            "a probe",
            Arc::new(|_: String| async move { Ok(String::new()) }),
        ));

        let mut recorder = Recorder::new(Surface::Tui);
        dispatch(
            SlashCommand::parse("/zzfails").expect("known").expect("ok"),
            &mut recorder,
        )
        .await;
        match recorder.saw.as_slice() {
            [Saw::Error(message)] => assert!(message.contains("the disk is on fire"), "{message}"),
            other => panic!("expected the handler's error, saw {other:?}"),
        }

        let mut recorder = Recorder::new(Surface::Tui);
        dispatch(
            SlashCommand::parse("/zzquiet").expect("known").expect("ok"),
            &mut recorder,
        )
        .await;
        assert!(recorder.saw.is_empty(), "an empty answer says nothing");
    }

    /// `/help` is derived from the merged list, so a plugin's command is in it
    /// wherever that plugin runs — and out of it where it does not.
    #[test]
    fn help_lists_a_plugin_command_where_it_runs() {
        let _held = hold(
            crate::commands::PluginCommand::new("zzhelp", "what the probe does", {
                Arc::new(|_: String| async move { Ok(String::new()) })
            })
            .args("[thing]")
            .only(&[Surface::Tui]),
        );
        let tui = help_text(Surface::Tui);
        assert!(
            tui.contains("/zzhelp [thing] — what the probe does"),
            "{tui}"
        );
        assert!(!help_text(Surface::Gui).contains("/zzhelp"));
        // And it is not filed under "terminal only", which is the table's own
        // gap and not a promise this build can make for somebody else's plugin.
        assert!(!help_text(Surface::Gui).contains("zzhelp"));
    }
}
