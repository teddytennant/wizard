//! Slash commands over ACP: what `wizard acp` advertises in
//! `available_commands_update`, and the [`CommandSurface`] a `/command` prompt
//! is dispatched through.
//!
//! The same [`dispatch`] the terminal and the gateway run, so `/effort high`
//! means one thing everywhere. This file supplies the verbs for one ACP
//! session: its agent, its [`Selection`], and a string to answer with. What
//! the client cannot use is [`Execution::Unavailable`] in the `acp` column of
//! [`crate::commands::COMMANDS`] and refused by name before it reaches a verb.
//!
//! A model switch is recorded here rather than applied: the session's agent is
//! rebuilt over its file (see `run_slash_command` in the parent), which needs the
//! sessions map this surface does not hold.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, UnstructuredCommandInput,
};
use anyhow::Result;
use async_trait::async_trait;

use super::models::{self, Selection};
use crate::agent::{Agent, RewindCandidate, ultra};
use crate::commands::surface::{
    Chooser, CommandSurface, Panel, PlanState, SessionSnapshot, Surface,
};
use crate::commands::{ProviderAction, ServerAction};
use crate::config::{Config, Mode, ReasoningEffort};
use crate::llm::provider::{LlmProvider, probe_native_tools};
use crate::mcp::McpManager;
use crate::tools::tasks::Task;

/// Cap on a command's answer, the same as a tool result's: `/diff` on a big
/// tree should not flood the thread.
const ANSWER_CAP: usize = super::TOOL_OUTPUT_CAP * 4;

/// The commands an ACP session advertises: every row of the table and every
/// plugin command this surface runs, with the argument hint as the input hint.
pub(super) fn available_commands() -> Vec<AvailableCommand> {
    crate::commands::available(Surface::Acp)
        .into_iter()
        .map(|row| {
            let command = AvailableCommand::new(row.name, row.description);
            match row.args.is_empty() {
                true => command,
                false => command.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(row.args),
                )),
            }
        })
        .collect()
}

/// The command line in a prompt, when the prompt is one: a `/` followed by a
/// word this build knows as a command. Anything else (`/etc/hosts`, a typo, a
/// custom command) goes to the model as it did before commands ran here.
pub(super) fn command_line(text: &str) -> Option<&str> {
    let line = text.trim();
    let rest = line.strip_prefix('/')?;
    let name = rest.split_whitespace().next()?;
    crate::commands::is_known(name).then_some(line)
}

/// One session's half of the dispatcher.
pub(super) struct AcpSurface<'a> {
    pub agent: &'a mut Agent,
    /// The user's config, for the provider list and `/model`'s provider names.
    pub base: &'a Config,
    /// The config with this session's selection applied: what its agent was
    /// built from.
    pub config: Config,
    pub selection: Selection,
    pub mcp: &'a McpManager,
    pub cwd: PathBuf,
    /// Whether turns run through the fusion panel.
    pub fusion: bool,
    /// A `/model` switch waiting on a rebuild.
    pub model_change: Option<(String, String)>,
    /// Everything the command said, in order. This is the answer.
    pub out: String,
}

impl AcpSurface<'_> {
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

    /// The seats an `/ultra` roster is dealt across: the fusion panel's when
    /// fusion is on, none otherwise. The same rule as the other surfaces.
    fn ultra_seats(&self) -> Result<Vec<ultra::Seat>> {
        if !self.fusion {
            return Ok(Vec::new());
        }
        let Some(fusion) = self.config.effective_fusion() else {
            return Ok(Vec::new());
        };
        crate::llm::fusion::panel_seats(&fusion, &self.config.providers)
    }

    fn build_ultra(&self) -> Result<ultra::UltraEngine> {
        Ok(self.config.build_ultra()?.with_seats(self.ultra_seats()?))
    }

    fn provider_list(&mut self) {
        let active = &self.selection.provider;
        if self.base.providers.is_empty() {
            let synth = self.base.active();
            return self.say(format!(
                "no providers configured, using the default: {} ({}) {} @ {}",
                synth.name, synth.kind, synth.model, synth.base_url
            ));
        }
        let mut lines = String::from("configured providers:");
        for provider in &self.base.providers {
            let marker = if &provider.name == active { "* " } else { "  " };
            lines.push_str(&format!(
                "\n{marker}{} ({}) {} @ {}",
                provider.name, provider.kind, provider.model, provider.base_url
            ));
        }
        lines.push_str("\n(* = this session)");
        self.say(lines);
    }
}

#[async_trait]
impl CommandSurface for AcpSurface<'_> {
    fn surface(&self) -> Surface {
        Surface::Acp
    }

    fn project_root(&self) -> PathBuf {
        self.cwd.clone()
    }

    fn notice(&mut self, text: String) {
        self.say(text);
    }

    fn error(&mut self, message: String) {
        self.say(message);
    }

    fn snapshot(&self) -> SessionSnapshot {
        let provider = self.config.active();
        let usage = self.agent.usage();
        let (prompt_tokens, completion_tokens) = usage.session_totals();
        SessionSnapshot {
            model: self.agent.model().to_string(),
            provider_name: provider.name.clone(),
            provider_kind: provider.kind,
            provider_base_url: provider.base_url.clone(),
            mode: self.agent.mode(),
            effort: self.selection.effort,
            max_steps: Some(self.config.max_steps),
            session: Some(self.agent.session().id.clone()),
            prompt_tokens,
            completion_tokens,
            cache_tokens: Some(usage.session_cache_totals()),
            reasoning_tokens: Some(usage.session_reasoning_tokens()),
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

    /// `<provider>/<model>` as the model menu names it, or a bare tag on this
    /// session's provider. A tag with a slash that names no provider
    /// (`anthropic/claude-sonnet-5` on OpenRouter) is the bare-tag case.
    async fn set_model(&mut self, tag: String) {
        let change = models::parse_model_id(&tag, self.base)
            .unwrap_or_else(|| (self.selection.provider.clone(), tag));
        self.model_change = Some(change);
    }

    async fn set_mode(&mut self, mode: Mode) -> bool {
        self.agent.set_mode(mode);
        self.selection.mode = mode;
        true
    }

    async fn set_effort(&mut self, effort: Option<ReasoningEffort>) -> bool {
        self.agent.set_reasoning_effort(effort);
        self.selection.effort = effort;
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

    /// Skills and scripted tools. MCP servers are connected once per `wizard
    /// acp` process and shared by every session, so they stay as they are.
    async fn reload(&mut self) {
        let hooks = Arc::clone(self.agent.hooks());
        let client = Arc::clone(self.agent.client());
        if let Some(computer) = Config::computer_on_disk() {
            self.config.computer = computer;
        }
        let built =
            crate::agent::build_tool_registry(&self.config, &client, &hooks, self.mcp).await;
        match built {
            Ok((registry, subagent_model)) => {
                let tools = registry.len();
                let skills = crate::agent::load_skills();
                let count = skills.len();
                self.agent.set_registry(registry);
                self.agent.bind_subagent_model(subagent_model);
                self.agent.set_skills(skills);
                self.say(format!(
                    "reloaded: {tools} tools, {count} skills (MCP servers reconnect when the \
                     client restarts wizard acp)"
                ));
            }
            Err(err) => self.say(format!("reload failed: {err:#}")),
        }
    }

    async fn rewind(&mut self, turn: u64) {
        match self.agent.rewind_to(turn) {
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
                    "rewound to before turn {turn}: {files}; conversation truncated"
                ));
            }
            Err(err) => self.say(format!("rewind failed: {err:#}")),
        }
    }

    async fn btw(&mut self, question: String) {
        match self.agent.answer_side_question(&question).await {
            Ok(answer) => self.say(answer),
            Err(err) => self.say(format!("/btw failed: {err:#}")),
        }
    }

    /// The fork's report lands in history at the top of the next turn.
    async fn fork(&mut self, task: String) {
        match self.agent.spawn_fork(&task, None).await {
            Ok(id) => self.say(format!("fork #{id} started: {task}")),
            Err(err) => self.say(format!("/fork failed: {err:#}")),
        }
    }

    async fn start_goal(&mut self, _goal: String) -> bool {
        false
    }

    async fn evolve(&mut self, deep: bool, description: String) {
        let tier = match deep {
            true => crate::evolve::EvolveTier::Deep,
            false => crate::evolve::EvolveTier::Runtime,
        };
        let request = crate::evolve::EvolveRequest { description, tier };
        let mut evolver = crate::evolve::Evolver::new(self.config.clone());
        match evolver.run(request).await {
            Ok(outcome) => self.say(crate::evolve::describe_outcome(&outcome)),
            Err(err) => self.say(format!("evolve failed: {err:#}")),
        }
    }

    async fn publish(&mut self, branch: Option<String>) {
        let args = serde_json::json!({ "branch": branch });
        match crate::plugins::run_tool("publish", "tool-publish", args).await {
            Ok(summary) | Err(summary) => self.say(summary),
        }
    }

    /// Swaps this session's client in place, so the conversation survives.
    /// A model switch rebuilds the agent on the new model and leaves fusion.
    async fn toggle_fusion(&mut self) {
        let (client, label) = match self.fusion {
            true => match self.config.active().build() {
                Ok(client) => (
                    client,
                    format!("fusion off, back to {}", self.selection.model),
                ),
                Err(err) => return self.say(format!("could not rebuild the provider: {err:#}")),
            },
            false => {
                let Some(fusion) = self.config.effective_fusion() else {
                    return self.say(
                        "fusion needs at least one configured provider; set [fusion] in \
                         ~/.wizard/config.toml, then /fusion",
                    );
                };
                match self.config.build_fusion_from(&fusion) {
                    Ok(provider) => {
                        let label = provider.label();
                        (
                            Arc::new(provider) as Arc<dyn LlmProvider>,
                            format!("{label}: every turn now fuses the panel; /fusion to turn off"),
                        )
                    }
                    Err(err) => return self.say(format!("could not start fusion: {err:#}")),
                }
            }
        };
        let model = self.agent.model().to_string();
        let native = probe_native_tools(client.as_ref(), &model).await;
        self.agent.set_client(Arc::clone(&client), native);
        // The spawn tool holds the old client until the registry is rebuilt.
        let hooks = Arc::clone(self.agent.hooks());
        match crate::agent::build_tool_registry(&self.config, &client, &hooks, self.mcp).await {
            Ok((registry, subagent_model)) => {
                self.agent.set_registry(registry);
                self.agent.bind_subagent_model(subagent_model);
            }
            Err(err) => tracing::warn!("acp: rebuilding the registry for /fusion: {err:#}"),
        }
        self.fusion = !self.fusion;
        if self.agent.ultra() {
            match self.build_ultra() {
                Ok(engine) => self.agent.set_ultra(Some(Arc::new(engine))),
                Err(err) => self.say(format!("ultra roster could not be re-seated: {err:#}")),
            }
        }
        self.say(label);
    }

    async fn toggle_ultra(&mut self) {
        if self.agent.ultra() {
            self.agent.set_ultra(None);
            return self.say("ultra off: one agent per turn again");
        }
        match self.build_ultra() {
            Ok(engine) => {
                let engine = Arc::new(engine);
                let label = engine.label();
                self.agent.set_ultra(Some(engine));
                self.say(format!(
                    "{label}: each turn now drafts, compares, then acts; /ultra to turn off"
                ));
            }
            Err(err) => self.say(format!("could not start ultra: {err:#}")),
        }
    }

    async fn server(&mut self, action: ServerAction) {
        let provider = self.config.active();
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
                let wait = Arc::new(crate::progress::ServerSpinner::start());
                let outcome = managed.start(provider, Box::new(Arc::clone(&wait))).await;
                wait.finish(outcome.is_ok());
                match outcome {
                    Ok(line) | Err(line) => line,
                }
            }
            ServerAction::Stop => managed.stop(),
        };
        self.say(text);
    }

    /// Bare `/provider` answers with the list: the client's model menu is
    /// where a session switches.
    async fn open(&mut self, chooser: Chooser) -> bool {
        match chooser {
            Chooser::Provider => {
                self.provider_list();
                true
            }
            _ => false,
        }
    }

    async fn provider(&mut self, action: ProviderAction) {
        match action {
            ProviderAction::List | ProviderAction::Menu => self.provider_list(),
            ProviderAction::Use(_) | ProviderAction::Add { .. } | ProviderAction::Remove(_) => self
                .say(
                    "over ACP /provider only lists. Switch this session with the model menu or \
                     /model <provider>/<model>; add or remove providers in the terminal",
                ),
        }
    }

    /// `/ui [name]`: the looks, or which one `wizard` starts in. A full look
    /// is this server's client, so a switch made from inside one lands when
    /// it quits and the `wizard` that started it reads `[ui] skin` again.
    async fn set_ui(&mut self, name: Option<String>) {
        let mut config = match Config::load() {
            Ok(config) => config,
            Err(err) => return self.say(format!("error: could not read config: {err:#}")),
        };
        let saved = config
            .ui
            .skin
            .as_deref()
            .and_then(crate::skin::Skin::from_key)
            .unwrap_or_default();
        let Some(name) = name else {
            return self.say(crate::skin::listing(saved));
        };
        let Some(skin) = crate::skin::Skin::from_key(&name) else {
            return self.say(format!("error: unknown skin '{name}'. /ui lists them."));
        };
        config.ui.skin = Some(skin.key().to_string());
        match config.save() {
            Ok(()) => self.say(format!(
                "saved [ui] skin = \"{}\". Quit this look to switch now; anywhere else it \
                 applies the next time wizard starts.",
                skin.key()
            )),
            Err(err) => self.say(format!("error: could not save config: {err:#}")),
        }
    }

    /// No panels here: the panel's contents are the answer.
    async fn toggle_panel(&mut self, panel: Panel) {
        match panel {
            Panel::Diff => {
                let text = match crate::app::git_diff_text(&self.cwd).await {
                    Ok(text) => super::truncate(&text, ANSWER_CAP),
                    Err(err) => format!("could not read git diff: {err:#}"),
                };
                self.say(format!("```diff\n{}\n```", text.trim_end()));
            }
            Panel::Todos => {
                let todos = self.agent.todos();
                match todos.is_empty() {
                    true => self.say("todo list is empty"),
                    false => self.say(crate::tools::todo::render(&todos)),
                }
            }
            // Refused by the table before it gets here.
            Panel::Dashboard => self.say("'/dashboard' is part of the terminal UI"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_command_word_is_a_command_and_anything_else_is_a_prompt() {
        assert_eq!(command_line("  /effort high \n"), Some("/effort high"));
        assert_eq!(command_line("/help"), Some("/help"));
        assert_eq!(command_line("/genie"), Some("/genie"));
        assert_eq!(command_line("/frobnicate the widget"), None);
        assert_eq!(command_line("/etc/hosts is wrong"), None);
        assert_eq!(command_line("what does /help do?"), None);
        assert_eq!(command_line("/"), None);
    }

    #[test]
    fn the_advertised_commands_are_what_acp_runs() {
        let names: Vec<String> = available_commands().into_iter().map(|c| c.name).collect();
        for name in [
            "model", "effort", "mode", "plan", "compact", "usage", "help", "diff", "ui",
        ] {
            assert!(
                names.iter().any(|n| n == name),
                "/{name} missing: {names:?}"
            );
        }
        for name in [
            "vim", "view", "quit", "clear", "resume", "settings", "login",
        ] {
            assert!(
                !names.iter().any(|n| n == name),
                "/{name} advertised: {names:?}"
            );
        }
        let effort = available_commands()
            .into_iter()
            .find(|c| c.name == "effort")
            .expect("effort");
        assert!(matches!(
            effort.input,
            Some(AvailableCommandInput::Unstructured(ref input)) if input.hint.contains("high")
        ));
    }
}
