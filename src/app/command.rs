//! The terminal UI's half of the one slash-command dispatcher.
//!
//! [`CommandContext`] borrows the main loop's stack for the duration of one
//! command and implements [`CommandSurface`]: the verbs a command needs, and
//! nothing about what a command *means*. That lives in
//! [`crate::commands::surface`], which every surface runs the same command
//! through, so the terminal and the window cannot answer `/goal` differently
//! again.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};

use crate::agent::{Agent, RewindCandidate};
use crate::commands::surface::{
    Chooser, CommandSurface, Panel, PlanState, SessionSnapshot, Surface, dispatch,
};
use crate::commands::{ProviderAction, ServerAction, SlashCommand};
use crate::config::{
    Config, Mode, ProviderConfig, ProviderKind, ReasoningEffort, StepBudget, UltraConfig,
};
use crate::event::{Event, EventLoop};
use crate::evolve::{EvolveRequest, EvolveTier, Evolver, PublishRequest, publish};
use crate::import_claude::{self, ImportSelection};
use crate::llm::provider::LlmProvider;
use crate::mcp::{McpConfig, McpManager};
use crate::server;
use crate::skills::Skill;
use crate::skin;
use crate::theme;
use crate::tools::tasks::Task;

use super::session::{
    SessionTarget, build_agent, build_registry, load_skill_roots, restore_ultra, switch_model_task,
};
use crate::agent::{subagent, ultra};

use super::{App, Picker, PickerItem, PickerKind};

/// Run `git <args>` in `root` and return stdout.
async fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .context("running git")?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compose the `/diff` sidebar contents: unstaged, then staged, then
/// untracked changes. Untracked (new) files are invisible to plain `git
/// diff`, so without the third section a tree whose only changes are new
/// files reads as "clean" — the diff sidebar looks broken.
pub(super) async fn git_diff_text(root: &Path) -> Result<String> {
    let unstaged = git_output(root, &["diff"]).await?;
    let staged = git_output(root, &["diff", "--staged"]).await?;
    let untracked = git_output(root, &["ls-files", "--others", "--exclude-standard"]).await?;
    let mut text = String::new();
    if !unstaged.trim().is_empty() {
        text.push_str(&unstaged);
    }
    if !staged.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("# --- staged ---\n");
        text.push_str(&staged);
    }
    let mut untracked_text = String::new();
    for file in untracked.lines().filter(|l| !l.trim().is_empty()) {
        // Skip Wizard's own session state (.wizard/checkpoints, snapshots,
        // etc.) — it's an implementation detail, not the user's work, and
        // dumping it here makes the diff sidebar look broken.
        if is_wizard_state_path(file) {
            continue;
        }
        untracked_text.push_str(&git_diff_untracked(root, file).await);
    }
    if !untracked_text.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("# --- untracked ---\n");
        text.push_str(&untracked_text);
    }
    if text.is_empty() {
        text = "(working tree clean)".to_string();
    }
    Ok(text)
}

/// Is this repo-relative path inside Wizard's own state dir (`.wizard/`)?
/// Such files (checkpoints, snapshots) are Wizard internals, not the user's
/// changes, so `/diff` omits them. Matches the dir at the repo root or in
/// any subdir, tolerating either path separator.
pub(super) fn is_wizard_state_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path == ".wizard" || path.starts_with(".wizard/") || path.contains("/.wizard/")
}

/// Render a single untracked file as a full addition by diffing it against
/// `/dev/null`. `git diff --no-index` exits 1 when the inputs differ (the
/// normal case here) and reads nothing from the index, so it stays
/// read-only; we take its stdout regardless of exit status and drop the
/// file silently if git can't read it.
async fn git_diff_untracked(root: &Path, file: &str) -> String {
    match tokio::process::Command::new("git")
        .args(["diff", "--no-index", "--no-color", "--", "/dev/null", file])
        .current_dir(root)
        .output()
        .await
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// `/ui [name]`: wear another agent's terminal chrome, or list what is
/// available.
///
/// Unlike [`theme_command`] this one *persists*: `[ui] skin` exists, so a
/// switch that only lasted until the next launch would be a worse answer than
/// writing the file. A failed write is reported and the switch still stands
/// for this session — the user asked to see it, and they can see it.
///
/// Switching brings the new skin's palette with it, unconditionally. It used
/// to do so only for a user who had never set `[ui] theme` or `WIZARD_THEME`,
/// which meant `/ui codex` drew Codex's frame in the old colors for exactly
/// the people most likely to notice. Those settings are gone and a skin now
/// owns its colors outright, so there is nothing left to defer to.
pub(super) fn ui_command(app: &mut App, name: Option<&str>) -> String {
    let Some(name) = name else {
        let active = skin::active();
        let mut text = format!("ui: {} — {}\n", active.label(), active.description());
        for candidate in skin::Skin::ALL {
            let marker = if candidate == active { "●" } else { "·" };
            text.push_str(&format!(
                "  {marker} {}  {}\n",
                candidate.key(),
                candidate.description()
            ));
        }
        text.push_str("switch with /ui <name> — the commands and keys stay Wizard's");
        return text;
    };

    let switched = match skin::set_active_by_name(name) {
        Ok(switched) => switched,
        Err(err) => return format!("error: {err:#}"),
    };

    let mut text = format!("ui: {} — {}", switched.label(), switched.description());
    if let Ok(theme) = theme::set_active_by_name(switched.companion_theme()) {
        text.push_str(&format!("\npalette: {} (came with the skin)", theme.name));
    }

    app.config.ui.skin = Some(switched.key().to_string());
    if let Err(err) = app.config.save() {
        text.push_str(&format!("\ncould not save config: {err:#}"));
    }
    text
}

/// The key bindings appended to `/help` here. The *commands* half of that
/// answer is derived from [`crate::commands::COMMANDS`] on every surface:
/// there used to be a hand-written list in this file, and it had already lost
/// `/exit`. Keys are the terminal's own, and have no table to come from.
const HELP_KEYS: &str = "keys:\n  \
Tab / \u{2192}                     accept command completion\n  \
Shift+Tab                   toggle plan mode\n  \
\u{2191} / \u{2193}                       select suggestion \u{b7} browse input history\n  \
PgUp/PgDn \u{b7} wheel           scroll the transcript (stays put while streaming)\n  \
Esc \u{b7} Ctrl-End              jump back to the live tail\n  \
drag                        select text \u{2014} copied to the clipboard on release\n  \
Ctrl-Y                      copy the last reply (works over SSH and in tmux)\n  \
click a tool card           expand / collapse its output\n  \
Ctrl-P                      model picker  \u{b7}  Ctrl-T toggle last tool card\n  \
Ctrl-A/E Home/End \u{2190}/\u{2192}       move cursor   \u{b7} Ctrl-W/U/K kill word/to start/to end\n  \
Ctrl-G                      edit the prompt in $EDITOR\n  \
Ctrl-C                      interrupt \u{b7} press twice to quit";

/// Everything a slash command may touch, borrowed from the main loop for
/// the duration of one dispatch.
pub(super) struct CommandContext<'a> {
    pub(super) app: &'a mut App,
    pub(super) client: &'a mut Arc<dyn LlmProvider>,
    pub(super) agent_slot: &'a mut Option<Agent>,
    pub(super) manager: &'a Arc<Mutex<McpManager>>,
    pub(super) skills: &'a mut Vec<Skill>,
    pub(super) project_root: &'a Path,
    pub(super) mcp_path: &'a Path,
    pub(super) genie_max_steps: StepBudget,
    pub(super) events: &'a EventLoop,
}

impl CommandContext<'_> {
    /// Execute one slash command against the running stack.
    ///
    /// Straight through the one dispatcher, so what a command means here is
    /// what it means everywhere. This surface supplies the verbs below.
    pub(super) async fn run(mut self, command: SlashCommand) {
        dispatch(command, &mut self).await;
    }

    /// True (with a notice) when the agent cannot be touched right now —
    /// a turn is running or a background rebuild is in flight.
    fn agent_unavailable(&mut self, action: &str) -> bool {
        if self.app.status.busy {
            self.app
                .notice(format!("cannot {action} while a turn is running"));
            true
        } else if self.app.rebuilding.is_some() {
            self.app
                .notice(format!("cannot {action} while the agent is rebuilding"));
            true
        } else {
            false
        }
    }

    async fn toggle_diff(&mut self) {
        if self.app.diff.take().is_some() {
            return;
        }
        self.app.diff = Some(crate::app::DiffPane {
            text: match git_diff_text(self.project_root).await {
                Ok(text) => text,
                Err(err) => format!("could not read git diff: {err:#}"),
            },
            scroll: 0,
        });
    }

    /// `/todos`: toggle the compact todo band above the composer.
    fn toggle_todos(&mut self) {
        self.app.show_todos = !self.app.show_todos;
        if self.app.show_todos && self.app.todos.is_empty() {
            self.app
                .notice("todo list is empty — the agent fills it via the `todo` tool");
        }
    }

    /// `/dashboard`: toggle the machine-wide session manager. On open, refresh
    /// the live-session list from the registry; the event loop keeps it current
    /// while it's up.
    fn toggle_dashboard(&mut self) {
        self.app.show_dashboard = !self.app.show_dashboard;
        if self.app.show_dashboard {
            self.app.refresh_sessions();
            self.app.refresh_peek();
        }
    }

    /// `/clear`: rotate the session and empty the transcript view.
    fn clear_conversation(&mut self) {
        if self.agent_unavailable("clear") {
            return;
        }
        if let Some(agent) = self.agent_slot.as_mut()
            && let Err(err) = agent.clear()
        {
            self.app
                .notice(format!("failed to rotate session: {err:#}"));
            return;
        }
        self.app.transcript.clear();
        // Drop any prompts queued behind a previous turn — a cleared
        // conversation shouldn't auto-fire messages the user typed mid-turn.
        self.app.message_queue.clear();
        self.app.scroll_to_bottom();
        // Mirror the agent's counter reset so the status bar drops the old
        // conversation's totals immediately (not after the next Usage event).
        self.app.status.prompt_tokens = 0;
        self.app.status.completion_tokens = 0;
        self.app.status.context_tokens = self
            .agent_slot
            .as_ref()
            .map(|agent| agent.context_tokens())
            .unwrap_or(0);
        self.app.notice("conversation cleared");
    }

    /// Open the interactive model picker with all installed models.
    async fn open_model_picker(&mut self) {
        if self.agent_unavailable("switch models") {
            return;
        }
        match self.client.list_models().await {
            Ok(models) if !models.is_empty() => {
                let current = self.app.status.model.clone();
                let items: Vec<PickerItem> = models
                    .into_iter()
                    .map(|model| PickerItem {
                        current: model == current
                            || model.split(':').next() == Some(current.as_str()),
                        detail: String::new(),
                        value: model,
                    })
                    .collect();
                let selected = items.iter().position(|item| item.current).unwrap_or(0);
                self.app.picker = Some(Picker {
                    kind: PickerKind::Model,
                    title: " select model ".to_string(),
                    items,
                    selected,
                });
            }
            Ok(_) => self
                .app
                .notice("no models installed — try `ollama pull <model>`"),
            Err(err) => self.app.notice(format!("could not list models: {err:#}")),
        }
    }

    /// Switch models off the event loop: the validation probe and any agent
    /// rebuild run in a background task and come back as
    /// [`Event::AgentRebuilt`], so the TUI never freezes.
    fn switch_model(&mut self, tag: String) {
        if self.agent_unavailable("switch models") {
            return;
        }
        let agent = self.agent_slot.take();
        self.app.rebuilding = Some(format!("switching to {tag}"));
        let client = self.client.clone();
        let config = self.app.config.clone();
        let skills = self.skills.clone();
        let project_root = self.project_root.to_path_buf();
        let manager = Arc::clone(self.manager);
        let notify = self.events.sender();
        // The agent has left its slot for the duration. A panic in the switch
        // (a provider's `list_models`, the native-tool probe) would strand it
        // there with `rebuilding` lit and the session unable to run another
        // turn; the fallback lets the main loop rebuild from the session file
        // instead. See [`crate::app::recover::spawn_answering`].
        crate::app::recover::spawn_answering(
            notify,
            Event::AgentRebuilt(Box::new(crate::app::AgentRebuild {
                agent: None,
                model: None,
                notice: "the model switch crashed — restarting the agent".to_string(),
            })),
            async move {
                // Bounded like every other task that holds the agent: a
                // `list_models` or tool-support probe against an endpoint that
                // accepts the connection and then says nothing would otherwise
                // end the session without ending the process.
                let rebuild = match crate::app::recover::within(
                    "switching model",
                    crate::app::recover::AGENT_REBUILD_DEADLINE,
                    switch_model_task(agent, tag, &client, config, skills, project_root, manager),
                )
                .await
                {
                    Ok(rebuild) => rebuild,
                    Err(timed_out) => crate::app::AgentRebuild {
                        agent: None,
                        model: None,
                        notice: timed_out,
                    },
                };
                Some(Event::AgentRebuilt(Box::new(rebuild)))
            },
        );
    }

    /// Open the interactive mode picker.
    fn open_mode_picker(&mut self) {
        if self.agent_unavailable("switch modes") {
            return;
        }
        let items = vec![
            PickerItem {
                value: "genie".to_string(),
                detail: "interactive — bypass permissions; acts without asking".to_string(),
                current: self.app.mode() == Mode::Genie,
            },
            PickerItem {
                value: "sovereign".to_string(),
                detail: "autonomous — works continuously; self-directing".to_string(),
                current: self.app.mode() == Mode::Sovereign,
            },
        ];
        let selected = items.iter().position(|item| item.current).unwrap_or(0);
        self.app.picker = Some(Picker {
            kind: PickerKind::Mode,
            title: " select mode ".to_string(),
            items,
            selected,
        });
    }

    /// `/agents`: open the subagent roster picker. Lists the built-in and
    /// user-defined subagents with their purpose, tool scope, and step budget.
    /// Selecting one pre-fills a delegation request (subagents are spawned by
    /// the model, so this isn't a direct command).
    fn open_agents_picker(&mut self) {
        let dir = Config::subagents_dir().unwrap_or_default();
        let configs = subagent::available_configs(&dir);
        if configs.is_empty() {
            self.app.notice("no subagents available");
            return;
        }
        let items: Vec<PickerItem> = configs
            .into_iter()
            .map(|config| {
                let scope = match &config.tool_scope {
                    None => "all tools".to_string(),
                    Some(names) => names.join(", "),
                };
                PickerItem {
                    detail: format!("{} · {scope} · {}", config.description, config.max_steps),
                    value: config.name,
                    current: false,
                }
            })
            .collect();
        self.app.picker = Some(Picker {
            kind: PickerKind::Subagent,
            title: " delegate to subagent ".to_string(),
            items,
            selected: 0,
        });
    }

    /// `/plan` (and Shift+Tab) and `/omakase`, which are one state: omakase is
    /// plan mode with the review gate removed, so the two flags move together
    /// and [`PlanState`] is what decides how. Mirrored onto the live agent and
    /// onto the badge, which must not disagree.
    fn apply_plan(&mut self, plan: PlanState) -> bool {
        if self.agent_unavailable("change plan mode") {
            return false;
        }
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_plan_mode(plan.plan);
            agent.set_omakase(plan.omakase);
        }
        self.app.plan_mode = plan.plan;
        self.app.omakase = plan.omakase;
        true
    }

    /// `/rewind`: open the turn picker (newest first). Each row shows the
    /// turn number, the files its edits snapshotted, and the first line of
    /// the prompt that started it. Esc cancels.
    fn open_rewind_picker(&mut self) {
        if self.agent_unavailable("rewind") {
            return;
        }
        let Some(agent) = self.agent_slot.as_ref() else {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        };
        let candidates = agent.rewind_candidates(20);
        if candidates.is_empty() {
            self.app.notice("nothing to rewind yet");
            return;
        }
        let items: Vec<PickerItem> = candidates
            .iter()
            .map(|candidate| {
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
                let detail = match (candidate.prompt.is_empty(), files.is_empty()) {
                    (false, false) => format!("{} · {files}", candidate.prompt),
                    (false, true) => candidate.prompt.clone(),
                    (true, false) => files,
                    (true, true) => String::new(),
                };
                PickerItem {
                    value: candidate.turn.to_string(),
                    detail,
                    current: false,
                }
            })
            .collect();
        self.app.picker = Some(Picker {
            kind: PickerKind::Rewind,
            title: " rewind to before turn ".to_string(),
            items,
            selected: 0,
        });
    }

    /// `/rewind <turn>` (or a picker selection): restore the files and drop
    /// the rewound turns from the session and the transcript.
    fn rewind_to_turn(&mut self, turn: u64) {
        if self.agent_unavailable("rewind") {
            return;
        }
        let Some(agent) = self.agent_slot.as_mut() else {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        };
        match agent.rewind_to(turn) {
            Ok(restored) => {
                // The rewound turns no longer exist: replay the truncated
                // conversation into the transcript view (same as `/resume`).
                let entries = agent.session().entries().unwrap_or_default();
                self.app.load_transcript(&entries);
                let files = if restored.is_empty() {
                    "no files needed restoring".to_string()
                } else {
                    format!(
                        "restored {}",
                        restored
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                self.app.notice(format!(
                    "rewound to before turn {turn} — {files}; conversation truncated"
                ));
            }
            Err(err) => self.app.notice(format!("rewind failed: {err:#}")),
        }
    }

    /// `/resume <id>` (or a picker selection): swap the live agent for one
    /// reopened on session `id` and replay its transcript. The agent must be
    /// idle (the slot is taken during a turn).
    async fn resume_session(&mut self, id: String) {
        if id == self.app.session_id {
            self.app.notice("already in this session");
            return;
        }
        if self.agent_unavailable("resume a session") {
            return;
        }
        if self.agent_slot.is_none() {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        }
        let manager = self.manager.lock().await;
        let agent = build_agent(
            self.client,
            &self.app.config,
            self.skills,
            self.project_root,
            &manager,
            SessionTarget::Id(id.clone()),
        )
        .await;
        drop(manager);
        let mut agent = match agent {
            Ok(agent) => agent,
            Err(err) => {
                self.app
                    .notice(format!("could not resume session: {err:#}"));
                return;
            }
        };
        if self.app.plan_mode {
            agent.set_plan_mode(true);
        }
        restore_ultra(self.app, &mut agent);
        // Replay the reopened conversation into the transcript view.
        let entries = agent.session().entries().unwrap_or_default();
        let resumed_id = agent.session().id.clone();
        self.app.load_transcript(&entries);
        // Named and counted off the replayed conversation rather than off the
        // raw messages: the model already knows which user-role records were
        // things a person said and which were an agent carrying a tool's
        // images back, and only the first kind is a prompt.
        let prompts: Vec<&str> = self
            .app
            .transcript
            .iter()
            .filter_map(|item| match item {
                crate::transcript::TranscriptItem::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let turns = prompts.len();
        let name = prompts
            .first()
            .and_then(|text| text.lines().next())
            .map(|line| line.trim().chars().take(48).collect::<String>())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| resumed_id.clone());
        *self.agent_slot = Some(agent);

        // Hand this session's identity over to the new one: drop the old
        // heartbeat, adopt the resumed id, and re-register.
        crate::session_registry::remove(&self.app.session_id);
        self.app.session_id = resumed_id.clone();
        self.app.session_name = name;
        crate::session_registry::write(&self.app.session_record());
        self.app
            .notice(format!("resumed session {resumed_id} · {turns} turns"));
    }

    /// `/resume-claude <id>`: copy a Claude Code conversation into a new
    /// Wizard session and continue it, where `id` is a Claude Code session id
    /// or a unique prefix of one.
    ///
    /// The import is the only new part; everything after it is
    /// [`Self::resume_session`] on the session it produced, so a conversation
    /// that came from Claude Code behaves from here on exactly like one that
    /// did not.
    ///
    /// `~/.claude` is read and never written — the guard that keeps that true
    /// is a source-level one in [`crate::claude_session`].
    pub(crate) async fn resume_claude(&mut self, id: String) {
        // Refused before the import rather than after, because importing
        // writes a session file: failing afterwards would leave one behind for
        // a resume that was never going to happen.
        if self.agent_unavailable("resume a Claude Code session") {
            return;
        }
        if self.agent_slot.is_none() {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        }
        let cwd = self.app.project_root.display().to_string();
        let matches: Vec<_> = crate::session_registry::claude_chats(&cwd)
            .into_iter()
            .filter(|row| row.id.starts_with(&id))
            .collect();
        let row = match matches.as_slice() {
            [only] => only.clone(),
            [] => {
                self.app.notice(format!(
                    "no Claude Code session here has an id starting with {id:?}"
                ));
                return;
            }
            many => {
                self.app.notice(format!(
                    "{id:?} matches {} Claude Code sessions; give more of the id",
                    many.len()
                ));
                return;
            }
        };
        let crate::session_registry::Origin::Claude { path, leaf, .. } = row.origin else {
            // `claude_chats` produces nothing else. Reported rather than
            // asserted: the wrong row is a reason to say so, not to abort a
            // session the user is working in.
            self.app.notice("that row did not come from Claude Code");
            return;
        };

        // Off the UI thread: this parses a whole transcript, and a large one
        // is thousands of lines with base64 images in it. On the reactor it
        // freezes the frame loop, which reads as the TUI having hung at
        // exactly the moment the user is waiting to see a conversation.
        let root = self.app.project_root.clone();
        let imported = tokio::task::spawn_blocking(move || {
            crate::claude_resume::import(&path, leaf.as_deref(), &root)
        })
        .await;
        let imported = match imported {
            Ok(Ok(imported)) => imported,
            Ok(Err(err)) => {
                self.app.notice(format!("could not import: {err:#}"));
                return;
            }
            Err(err) => {
                self.app.notice(format!("the import task failed: {err}"));
                return;
            }
        };

        // Said before the resume, so a truncated chain is explained *above*
        // the conversation it truncated rather than under it.
        self.app.notice(imported.summary());
        self.resume_session(imported.id).await;
    }

    /// `/compact`: ask the main loop to run compaction in the background (it
    /// owns the agent slot). Guarded so it can't stack on a busy/rebuilding
    /// agent or a compaction already in flight.
    fn request_compact(&mut self) {
        if self.agent_unavailable("compact") {
            return;
        }
        if self.app.compacting {
            self.app.notice("already compacting");
            return;
        }
        if self.agent_slot.is_none() {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        }
        self.app.pending_compact = true;
    }

    /// `/btw <question>`: one-shot side question. Unlike most commands this is
    /// allowed *while a turn is running* — that is the point — so it does not
    /// go through [`Self::agent_unavailable`]. The main loop owns the client
    /// and either the live agent or a mid-turn snapshot of its history.
    fn ask_aside(&mut self, question: String) {
        if self.app.rebuilding.is_some() {
            self.app
                .notice("cannot ask a side question while the agent is rebuilding");
            return;
        }
        if self.app.pending_btw.is_some() || self.app.btw_inflight {
            self.app
                .notice("already answering a /btw — wait for it to finish");
            return;
        }
        // A light "working on it" marker; the answer arrives as its own notice.
        self.app.notice("answering /btw…");
        self.app.pending_btw = Some(question);
    }

    /// `/fork <task>`: background side quest that inherits the full conversation.
    /// Allowed mid-turn (same as `/btw`); the main loop snapshots a
    /// [`crate::agent::ForkContext`] and detaches the run into the parent's
    /// background-subagent registry so the report lands in history when done.
    fn start_side_quest(&mut self, task: String) {
        if self.app.rebuilding.is_some() {
            self.app.notice("cannot fork while the agent is rebuilding");
            return;
        }
        if self.app.pending_fork.is_some() {
            self.app.notice("already starting a /fork — wait a moment");
            return;
        }
        self.app.notice(format!("forking: {task}"));
        self.app.pending_fork = Some(task);
    }

    fn switch_mode(&mut self, mode: Mode) -> bool {
        if self.agent_unavailable("switch modes") {
            return false;
        }
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_mode(mode);
        }
        self.app.config.mode = mode;
        self.app.status.mode = mode;
        match mode {
            Mode::Sovereign => {
                self.app.config.max_steps = self.app.config.max_steps.for_mode(Mode::Sovereign);
            }
            Mode::Genie => {
                self.app.config.max_steps = self.genie_max_steps;
            }
        }
        self.app.status.max_steps = self.app.config.max_steps;
        // Persist so the mode survives a restart (consistent with /provider).
        self.persist_config();
        true
    }

    /// Open the interactive reasoning-effort picker (`/effort`).
    fn open_effort_picker(&mut self) {
        if self.agent_unavailable("change effort") {
            return;
        }
        let current = self.app.config.reasoning_effort;
        let rows = [
            (
                "high",
                "most reasoning — slowest, best on hard tasks",
                Some(ReasoningEffort::High),
            ),
            (
                "medium",
                "balanced reasoning",
                Some(ReasoningEffort::Medium),
            ),
            (
                "low",
                "least reasoning — fastest, cheapest",
                Some(ReasoningEffort::Low),
            ),
            (
                "default",
                "leave the provider default (e.g. Grok 4.5 → high)",
                None,
            ),
        ];
        let items: Vec<PickerItem> = rows
            .iter()
            .map(|(value, detail, effort)| PickerItem {
                value: (*value).to_string(),
                detail: (*detail).to_string(),
                current: *effort == current,
            })
            .collect();
        let selected = items.iter().position(|item| item.current).unwrap_or(0);
        self.app.picker = Some(Picker {
            kind: PickerKind::Effort,
            title: " reasoning effort ".to_string(),
            items,
            selected,
        });
    }

    /// Set the reasoning effort (`/effort <level>`): applies to the live agent
    /// and persists so it survives a restart. Only reaches providers whose
    /// models accept a `reasoning_effort` field; others ignore it.
    fn apply_effort(&mut self, effort: Option<ReasoningEffort>) -> bool {
        if self.agent_unavailable("change effort") {
            return false;
        }
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_reasoning_effort(effort);
        }
        self.app.config.reasoning_effort = effort;
        self.persist_config();
        true
    }

    async fn reload_extensions(&mut self) {
        if self.agent_unavailable("reload") {
            return;
        }
        *self.skills = load_skill_roots();
        self.app.custom_commands = crate::commands::load(self.project_root);
        let mut manager = self.manager.lock().await;
        match McpConfig::load(self.mcp_path) {
            Ok(mcp_config) => {
                if let Err(err) = manager.reload(&mcp_config).await {
                    self.app.notice(format!("MCP reload warning: {err:#}"));
                }
            }
            Err(err) => self
                .app
                .notice(format!("could not reload MCP config: {err:#}")),
        }
        // The rebuilt registry's subagent spawner keeps the session's hooks.
        let Some(hooks) = self
            .agent_slot
            .as_ref()
            .map(|agent| Arc::clone(agent.hooks()))
        else {
            return;
        };
        match build_registry(&self.app.config, &manager, self.client, &hooks).await {
            Ok((registry, subagent_model)) => {
                let tool_count = registry.len();
                if let Some(agent) = self.agent_slot.as_mut() {
                    agent.set_registry(registry);
                    agent.bind_subagent_model(subagent_model);
                    agent.set_skills(self.skills.clone());
                }
                self.app.notice(format!(
                    "reloaded: {tool_count} tools, {} skills",
                    self.skills.len()
                ));
            }
            Err(err) => self.app.notice(format!("reload failed: {err:#}")),
        }
    }

    /// Merge the already-connected MCP servers' tools into the live agent's
    /// registry. Called after the startup background connect finishes — the
    /// slow part (spawning servers, `initialize`) is already done, so this just
    /// re-enumerates tools and swaps the registry. No-op if the agent is not in
    /// its slot (a turn is running); the main loop defers via `mcp_merge_pending`.
    pub(super) async fn merge_mcp_registry(&mut self) {
        let Some(hooks) = self
            .agent_slot
            .as_ref()
            .map(|agent| Arc::clone(agent.hooks()))
        else {
            return;
        };
        let manager = self.manager.lock().await;
        match build_registry(&self.app.config, &manager, self.client, &hooks).await {
            Ok((registry, subagent_model)) => {
                // Success is silent: tools simply start working and the
                // "connecting tools…" indicator disappears. A success notice
                // here is tool-flex narration and, emitted ~2s in, would float
                // above the user's first message as if it were a reply to it.
                if let Some(agent) = self.agent_slot.as_mut() {
                    agent.set_registry(registry);
                    agent.bind_subagent_model(subagent_model);
                }
            }
            Err(err) => self.app.notice(format!(
                "MCP connected but registry rebuild failed: {err:#}"
            )),
        }
    }

    /// Run a Claude Code import (dispatched from the `/settings` import
    /// picker), then reload custom commands + MCP servers live so the imported
    /// artifacts take effect without a restart.
    async fn run_claude_import(&mut self, selection: ImportSelection) {
        if self.agent_unavailable("import from Claude Code") {
            return;
        }
        let outcome = match import_claude::run_import(&selection) {
            Ok(outcome) => outcome,
            Err(err) => {
                self.app
                    .notice(format!("Claude Code import failed: {err:#}"));
                return;
            }
        };

        // Adopt the imported spinner verbs (replacing the active list).
        if !outcome.spinner_verbs.is_empty() {
            self.app.config.ui.spinner_verbs = outcome.spinner_verbs.clone();
            self.persist_config();
        }

        // Reload custom commands + MCP servers and rebuild the live tool
        // registry (mirrors `reload`) so imports are usable immediately.
        self.app.custom_commands = crate::commands::load(self.project_root);
        let mut manager = self.manager.lock().await;
        match McpConfig::load(self.mcp_path) {
            Ok(mcp_config) => {
                if let Err(err) = manager.reload(&mcp_config).await {
                    self.app.notice(format!("MCP reload warning: {err:#}"));
                }
            }
            Err(err) => self
                .app
                .notice(format!("could not reload MCP config: {err:#}")),
        }
        if let Some(hooks) = self
            .agent_slot
            .as_ref()
            .map(|agent| Arc::clone(agent.hooks()))
            && let Ok((registry, subagent_model)) =
                build_registry(&self.app.config, &manager, self.client, &hooks).await
            && let Some(agent) = self.agent_slot.as_mut()
        {
            agent.set_registry(registry);
            agent.bind_subagent_model(subagent_model);
            agent.set_skills(self.skills.clone());
        }
        drop(manager);

        let summary = outcome.summary();
        self.app.notice(if summary.is_empty() {
            "nothing to import from Claude Code".to_string()
        } else {
            format!("imported from Claude Code:\n{summary}")
        });
    }

    fn start_evolve(&mut self, deep: bool, description: String) {
        let tier = if deep {
            EvolveTier::Deep
        } else {
            EvolveTier::Runtime
        };
        // The explicit `/evolve` command is the user's consent; the outcome
        // notice reports exactly what was added.
        let request = EvolveRequest { description, tier };
        let mut evolver = Evolver::new(self.app.config.clone());
        let notify = self.events.sender();
        tokio::spawn(async move {
            let message = match evolver.run(request).await {
                Ok(outcome) => crate::evolve::describe_outcome(&outcome),
                Err(err) => format!("evolve failed: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
    }

    /// Fork Wizard to the user's GitHub and surface the one-liner install
    /// command. Runs in a background task so the TUI stays responsive.
    fn start_publish(&mut self, branch: Option<String>) {
        let config = self.app.config.clone();
        let notify = self.events.sender();
        tokio::spawn(async move {
            let req = PublishRequest { branch };
            let message = match publish(&config, req, false).await {
                Ok(outcome) => format!(
                    "publish: forked to {}  (branch: {})\n\nInstall one-liner:\n{}",
                    outcome.fork_url, outcome.branch, outcome.install_one_liner
                ),
                Err(err) => format!("publish failed: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
    }

    /// Persist `App.config` to disk, surfacing any error as a notice.
    fn persist_config(&mut self) {
        if let Err(err) = self.app.config.save() {
            self.app.notice(format!("could not save config: {err:#}"));
        }
    }

    /// Rebuild the live client + agent from the current active provider (after
    /// a `/provider use`/`add`). Runs synchronously; reports `summary` on
    /// success. Mirrors how the model picker probes the backend inline.
    async fn rebuild_active_provider(&mut self, summary: String) {
        let provider = self.app.config.active();
        let client = match provider.build() {
            Ok(client) => client,
            Err(err) => {
                self.app.notice(format!(
                    "could not build provider '{}': {err:#}",
                    provider.name
                ));
                return;
            }
        };
        *self.client = client;
        // A switch to llama.cpp may target a server that is not up yet:
        // kick off the auto-start in the background (the rebuild below
        // proceeds regardless; probes fall back until the model loads).
        if provider.kind == ProviderKind::LlamaCpp
            && server::probe(&provider.base_url).await == server::Health::Down
        {
            self.app.notice(format!(
                "llama-server at {} is not running — starting it…",
                provider.base_url
            ));
            self.start_server_task(provider.clone());
        }
        let model = self.app.config.active().model;
        self.rebuild_agent_with(model, summary, "switched provider")
            .await;
    }

    /// Rebuild the live agent against the current `client` (which the caller has
    /// already set), set the status-bar model label, and report `summary`.
    /// Shared by [`rebuild_active_provider`](Self::rebuild_active_provider) and
    /// the `/fusion` toggle. `context` names the action in the failure notice.
    async fn rebuild_agent_with(&mut self, model_label: String, summary: String, context: &str) {
        let manager = self.manager.lock().await;
        match build_agent(
            self.client,
            &self.app.config,
            self.skills,
            self.project_root,
            &manager,
            SessionTarget::Fresh,
        )
        .await
        {
            Ok(mut agent) => {
                // A rebuilt agent starts with plan mode off; restore the
                // session's setting.
                if self.app.plan_mode {
                    agent.set_plan_mode(true);
                }
                restore_ultra(self.app, &mut agent);
                *self.agent_slot = Some(agent);
                self.app.status.model = model_label;
                self.app.notice(summary);
            }
            Err(err) => {
                *self.agent_slot = None;
                self.app.notice(format!(
                    "{context} but could not start the agent: {err:#} — /quit and relaunch"
                ));
            }
        }
    }

    /// Toggle `/fusion`: swap the active client to a
    /// [`FusionProvider`](crate::llm::fusion) (panel debate → synthesizer) when
    /// off, or back to the underlying single provider when on. Like a provider
    /// switch, this resets the session.
    ///
    /// It no longer refuses to stack with `/ultra`. The two used to be mutually
    /// exclusive because each owned its own fan-out, so stacking them meant
    /// every ultra candidate re-running the whole panel: candidates × panel ×
    /// rounds before the first token. Now both are the one council, and turning
    /// fusion on *deals* the ultra roster across the panel's providers instead
    /// of nesting one fan-out inside the other. That is
    /// [`Self::reseat_ultra`]'s job, and it happens on both edges of this
    /// toggle.
    async fn switch_fusion(&mut self) {
        if self.agent_unavailable("toggle fusion") {
            return;
        }
        if self.app.fusion_active {
            self.app.fusion_active = false;
            // The seats went with the panel; a roster still holding them would
            // keep answering from providers that are no longer active.
            self.reseat_ultra();
            self.rebuild_active_provider("fusion off — back to the single model".to_string())
                .await;
            return;
        }

        let fusion = match self.app.config.effective_fusion() {
            Some(fusion) => fusion,
            None => {
                self.app.notice(
                    "fusion needs at least one configured provider — add one with /provider, \
                     then /fusion config",
                );
                return;
            }
        };
        let provider = match self.app.config.build_fusion_from(&fusion) {
            Ok(provider) => provider,
            Err(err) => {
                self.app.notice(format!("could not start fusion: {err:#}"));
                return;
            }
        };
        let label = provider.label();
        *self.client = Arc::new(provider);
        self.app.fusion_active = true;
        // Deal any live `/ultra` roster across the panel *before* the rebuild,
        // because the rebuild re-arms the new agent from `app.ultra` and an
        // unseated roster under a fused client runs the whole panel debate once
        // per candidate.
        self.reseat_ultra();
        let stacked = match self.app.ultra.as_ref() {
            Some(engine) => format!(" \u{00b7} {}", engine.label()),
            None => String::new(),
        };
        self.rebuild_agent_with(
            label.clone(),
            format!("{label}{stacked}. Every turn now fuses the panel; /fusion to turn off"),
            "started fusion",
        )
        .await;
    }

    /// The seats an `/ultra` roster is dealt across, given what is active now.
    ///
    /// Empty unless `/fusion` is on, which is the whole of what "the two modes
    /// compose" means here: `[ultra]` names no provider and never will, because
    /// which providers exist is a question about the session and the answer
    /// changes when `/fusion` is toggled without a line of `[ultra]` changing.
    fn ultra_seats(&self) -> Result<Vec<ultra::Seat>, String> {
        if !self.app.fusion_active {
            return Ok(Vec::new());
        }
        let Some(fusion) = self.app.config.effective_fusion() else {
            return Ok(Vec::new());
        };
        crate::llm::fusion::panel_seats(&fusion, &self.app.config.providers)
            .map_err(|err| format!("{err:#}"))
    }

    /// Build an engine from `cfg` and seat it for the current session.
    fn build_seated_ultra(&self, cfg: &UltraConfig) -> Result<Arc<ultra::UltraEngine>, String> {
        let engine = self
            .app
            .config
            .build_ultra_from(cfg)
            .map_err(|err| format!("{err:#}"))?;
        Ok(Arc::new(engine.with_seats(self.ultra_seats()?)))
    }

    /// Re-deal the live `/ultra` roster across the seats the session now
    /// offers, on both edges of the `/fusion` toggle. The roster does not
    /// change; where each candidate runs does, and an engine left holding the
    /// wrong seats is either a panel debate per candidate or a draft from a
    /// provider the user just switched away from.
    fn reseat_ultra(&mut self) {
        if self.app.ultra.is_none() {
            return;
        }
        let built = self.build_seated_ultra(&self.app.config.effective_ultra());
        match built {
            Ok(engine) => {
                if let Some(agent) = self.agent_slot.as_mut() {
                    agent.set_ultra(Some(engine.clone()));
                }
                self.app.ultra = Some(engine);
            }
            // The roster stays as it was rather than being silently dropped:
            // ultra is still on, and saying so is more useful than turning it
            // off behind the user's back.
            Err(err) => self
                .app
                .notice(format!("ultra roster could not be re-seated: {err}")),
        }
    }

    /// Toggle `/ultra`: a council of lens subagents. Where `/fusion` swaps the
    /// client and therefore has to rebuild the agent from scratch, ultra swaps
    /// nothing: the candidates fan out over the client and model that are
    /// already active, or over the fusion panel's providers when that is also
    /// on. So this is a plain flag on the live agent: no rebuild, no session
    /// reset, and the conversation in front of the user survives the toggle,
    /// which is what makes it usable mid-task ("that answer was thin — /ultra,
    /// try again").
    fn switch_ultra(&mut self) {
        if self.agent_unavailable("toggle ultra") {
            return;
        }
        if self.app.ultra.is_some() {
            self.app.ultra = None;
            if let Some(agent) = self.agent_slot.as_mut() {
                agent.set_ultra(None);
            }
            self.app
                .notice("ultra off — one agent per turn again, no pre-phase");
            return;
        }
        // `build_ultra_from` is the sole validation gate for `[ultra]`, so a
        // roster the user hand-edited into an unusable state surfaces here, at
        // the toggle, instead of at the top of their next turn. Seating is part
        // of the same step: with `/fusion` on, a roster that failed to seat
        // would bill the turn at candidates × panel × rounds, so it is refused
        // rather than run.
        let engine = match self.build_seated_ultra(&self.app.config.effective_ultra()) {
            Ok(engine) => engine,
            Err(err) => {
                self.app.notice(format!("could not start ultra: {err}"));
                return;
            }
        };
        let label = engine.label();
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_ultra(Some(engine.clone()));
        }
        self.app.ultra = Some(engine);
        self.app.notice(format!(
            "{label}: each turn now drafts, compares, then acts; /ultra to turn off"
        ));
    }

    /// Save the roster chosen at that editor. Building it first is not a
    /// formality: [`UltraEngine::build`](ultra::UltraEngine::build) is the only
    /// thing that rejects an unknown lens or an out-of-range count, so a roster
    /// that would not run is reported and never written. When ultra is already
    /// on, the live agent moves to the new roster in the same breath as the
    /// badge — the two must not disagree about how many candidates the next turn
    /// is about to spend.
    fn save_ultra_roster(&mut self, ultra: UltraConfig) {
        let engine = match self.build_seated_ultra(&ultra) {
            Ok(engine) => engine,
            Err(err) => {
                self.app.notice(format!("ultra roster rejected: {err}"));
                return;
            }
        };
        let label = engine.label();
        self.app.config.ultra = Some(ultra);
        if let Err(err) = self.app.config.save() {
            self.app
                .notice(format!("could not save ultra config: {err:#}"));
            return;
        }
        if self.app.ultra.is_none() {
            self.app.notice(format!("{label} — /ultra to turn on"));
            return;
        }
        match self.agent_slot.as_mut() {
            Some(agent) => {
                agent.set_ultra(Some(engine.clone()));
                self.app.ultra = Some(engine);
                self.app.notice(format!("{label} — applied"));
            }
            // Mid-turn the agent is inside the turn and holds the old engine.
            // Swapping only the badge would misreport a fan-out the user is
            // watching run, so leave both alone and say which one they have.
            None => self.app.notice(format!(
                "{label} — saved; the running turn keeps the old roster, /ultra off then on to \
                 pick this one up"
            )),
        }
    }

    /// Handle `/provider` subcommands: list, switch, add, or remove providers.
    async fn provider_command(&mut self, action: ProviderAction) {
        match action {
            // The bare `/provider` menu is a chooser, opened by the dispatcher.
            ProviderAction::Menu => self.app.open_provider_picker(),
            ProviderAction::List => self.provider_list(),
            ProviderAction::Use(name) => self.provider_use(name).await,
            ProviderAction::Add {
                name,
                kind,
                base_url,
                model,
                api_key_env,
            } => {
                self.provider_add(name, kind, base_url, model, api_key_env)
                    .await
            }
            ProviderAction::Remove(name) => self.provider_remove(name),
        }
    }

    fn provider_list(&mut self) {
        if self.app.config.providers.is_empty() {
            let synth = self.app.config.active();
            self.app.notice(format!(
                "no providers configured — using the default: {} ({}) {} @ {}\n\
                 add one with /provider (interactive)",
                synth.name, synth.kind, synth.model, synth.base_url
            ));
            return;
        }
        let active = self.app.config.active().name;
        let mut lines = String::from("configured providers:");
        for provider in &self.app.config.providers {
            let marker = if provider.name == active { "* " } else { "  " };
            let key = provider
                .api_key_env
                .as_deref()
                .map(|env| format!(" [key: ${env}]"))
                .unwrap_or_default();
            lines.push_str(&format!(
                "\n{marker}{} ({}) {} @ {}{key}",
                provider.name, provider.kind, provider.model, provider.base_url
            ));
        }
        lines.push_str("\n(* = active)");
        self.app.notice(lines);
    }

    async fn provider_use(&mut self, name: String) {
        if self.agent_unavailable("switch providers") {
            return;
        }
        if !self.app.config.providers.iter().any(|p| p.name == name) {
            self.app
                .notice(format!("no provider named '{name}' — try /provider list"));
            return;
        }
        self.app.config.active_provider = Some(name.clone());
        self.persist_config();
        self.rebuild_active_provider(format!("switched to provider '{name}'"))
            .await;
    }

    async fn provider_add(
        &mut self,
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key_env: Option<String>,
    ) {
        if self.agent_unavailable("add a provider") {
            return;
        }
        let provider = ProviderConfig {
            name: name.clone(),
            kind,
            base_url,
            model,
            api_key_env: api_key_env.clone(),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        let reminder = api_key_env
            .map(|env| format!(" — remember to `export {env}=<key>` for this provider"))
            .unwrap_or_default();
        self.add_provider_config(
            provider,
            format!("added and switched to provider '{name}'{reminder}"),
        )
        .await;
    }

    /// Add (or replace) `provider`, switch to it, persist config, and rebuild
    /// the live agent. Shared by the text `/provider add`, the interactive
    /// setup flow, and the xAI OAuth auto-add.
    pub(super) async fn add_provider_config(&mut self, provider: ProviderConfig, summary: String) {
        let name = provider.name.clone();
        // Dedup by name: replace an existing entry with the same name.
        self.app.config.providers.retain(|p| p.name != name);
        self.app.config.providers.push(provider);
        self.app.config.active_provider = Some(name);
        self.persist_config();
        self.rebuild_active_provider(summary).await;
    }

    /// Finalize an interactive provider setup ([`SlashCommand::ProviderSetup`]):
    /// store the API key in `~/.wizard/credentials.toml` when present, then add
    /// and switch to the provider.
    async fn finish_provider_setup(
        &mut self,
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key: Option<String>,
    ) {
        if self.agent_unavailable("add a provider") {
            return;
        }
        if let Some(key) = api_key.as_deref()
            && !key.is_empty()
            && let Err(err) = crate::credentials::store(&name, key)
        {
            self.app
                .notice(format!("could not save API key for '{name}': {err:#}"));
        }
        let provider = ProviderConfig {
            name: name.clone(),
            kind,
            base_url,
            model,
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        self.add_provider_config(provider, format!("added and switched to provider '{name}'"))
            .await;
    }

    fn provider_remove(&mut self, name: String) {
        if self.app.config.active().name == name {
            self.app.notice(format!(
                "'{name}' is the active provider — switch with /provider use <other> first"
            ));
            return;
        }
        let before = self.app.config.providers.len();
        self.app.config.providers.retain(|p| p.name != name);
        if self.app.config.providers.len() == before {
            self.app.notice(format!("no provider named '{name}'"));
            return;
        }
        self.persist_config();
        self.app.notice(format!("removed provider '{name}'"));
    }

    /// Handle `/server` subcommands: status, start, or stop the local
    /// llama-server.
    async fn server_command(&mut self, action: ServerAction) {
        match action {
            ServerAction::Status => self.server_status().await,
            ServerAction::Start => self.server_start().await,
            ServerAction::Stop => self.server_stop(),
        }
    }

    /// The active provider when it is llama.cpp; otherwise a notice that
    /// `/server` does not apply.
    fn llamacpp_provider(&mut self) -> Option<ProviderConfig> {
        let provider = self.app.config.active();
        if provider.kind == ProviderKind::LlamaCpp {
            Some(provider)
        } else {
            self.app.notice(format!(
                "the active provider '{}' is {} — /server only manages a local llama.cpp server",
                provider.name, provider.kind
            ));
            None
        }
    }

    async fn server_status(&mut self) {
        let Some(provider) = self.llamacpp_provider() else {
            return;
        };
        let spawned = server::spawned_pid()
            .map(|pid| format!(" (PID {pid}, started by wizard)"))
            .unwrap_or_default();
        let line = match server::probe(&provider.base_url).await {
            server::Health::Ready => {
                format!("llama-server at {}: ready{spawned}", provider.base_url)
            }
            server::Health::Loading => format!(
                "llama-server at {}: loading its model{spawned}",
                provider.base_url
            ),
            server::Health::Down => format!(
                "llama-server at {}: not running — start it with /server start",
                provider.base_url
            ),
        };
        self.app.notice(line);
    }

    async fn server_start(&mut self) {
        let Some(provider) = self.llamacpp_provider() else {
            return;
        };
        if server::probe(&provider.base_url).await == server::Health::Ready {
            self.app.notice(format!(
                "llama-server at {} is already running",
                provider.base_url
            ));
            return;
        }
        self.app
            .notice(format!("starting llama-server at {}…", provider.base_url));
        self.start_server_task(provider);
    }

    fn server_stop(&mut self) {
        let message = match server::stop() {
            Ok(server::StopOutcome::Stopped(pid)) => format!("stopped llama-server (PID {pid})"),
            Ok(server::StopOutcome::NotRecorded) => {
                "wizard has not started a llama-server — nothing to stop".to_string()
            }
            Ok(server::StopOutcome::NotRunning(pid)) => {
                format!("llama-server (PID {pid}) already exited")
            }
            Ok(server::StopOutcome::NotOurs { pid, name }) => {
                format!("refusing to stop PID {pid}: it is '{name}', not llama-server")
            }
            Err(err) => format!("could not stop llama-server: {err:#}"),
        };
        self.app.notice(message);
    }

    /// `/login <provider>`: run an OAuth sign-in in the background, streaming
    /// progress (including the URL to open) into the transcript as notices.
    fn start_login(&mut self, provider: String) {
        let _ = provider;
        let notify = self.events.sender();
        self.app
            .notice("starting the xAI sign-in; your browser should open shortly");
        tokio::spawn(async move {
            let progress = {
                let notify = notify.clone();
                move |line: &str| {
                    // The progress callback is sync; relay each line through
                    // its own send task.
                    let notify = notify.clone();
                    let line = line.to_string();
                    tokio::spawn(async move {
                        let _ = notify.send(Event::Notice(line)).await;
                    });
                }
            };
            // The TUI is reading stdin for keystrokes, so the paste fallback
            // has no channel here; `login` still reports how to forward the
            // callback port, which is the way through from a remote session.
            let paste = crate::llm::oauth_callback::PasteChannel::Disabled;
            match crate::llm::xai_oauth::login(progress, paste).await {
                Ok(()) => {
                    // Auto-add the OAuth provider and switch to it; the main
                    // loop owns the config + agent slot.
                    let provider = ProviderConfig {
                        name: "xai-oauth".to_string(),
                        kind: ProviderKind::XaiOauth,
                        base_url: crate::llm::xai_oauth::DEFAULT_BASE_URL.to_string(),
                        model: crate::llm::xai_oauth::DEFAULT_MODEL.to_string(),
                        api_key_env: None,
                        gguf_path: None,
                        usd_per_mtok_in: None,
                        usd_per_mtok_out: None,
                    };
                    let _ = notify
                        .send(Event::ProviderActivated(Box::new(provider)))
                        .await;
                }
                Err(err) => {
                    let _ = notify
                        .send(Event::Notice(format!("xAI sign-in failed: {err:#}")))
                        .await;
                }
            }
        });
    }

    /// Background half of `/server start` (and the post-switch auto-start):
    /// ensure a llama-server is running for `provider`, streaming progress
    /// into the transcript as notices.
    fn start_server_task(&self, provider: ProviderConfig) {
        let notify = self.events.sender();
        tokio::spawn(async move {
            let progress = NoticeProgress {
                notify: notify.clone(),
            };
            let message = match server::ensure_running(&provider, &progress).await {
                Ok(()) => format!("llama-server at {} is ready", provider.base_url),
                Err(err) => format!("llama-server: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
    }
}

/// The terminal's half of the one dispatcher.
///
/// Every method here is a verb: what to change, what can be seen. None of them
/// decides what a command *means* or what it says about itself. That is
/// [`crate::commands::surface::dispatch`]'s, once, for every surface.
#[async_trait]
impl CommandSurface for CommandContext<'_> {
    fn surface(&self) -> Surface {
        Surface::Tui
    }

    fn project_root(&self) -> PathBuf {
        self.project_root.to_path_buf()
    }

    fn help_keys(&self) -> Option<&'static str> {
        Some(HELP_KEYS)
    }

    /// One channel: a refusal reads like any other line of the transcript.
    fn notice(&mut self, text: String) {
        self.app.notice(text);
    }

    fn error(&mut self, message: String) {
        self.app.notice(message);
    }

    /// Mid-turn the agent is out of its slot, so the session id and the task
    /// count are unknown and the status bar's mirror is the best source for the
    /// token counts. Reported as `None` rather than as zero.
    fn snapshot(&self) -> SessionSnapshot {
        let provider = self.app.config.active();
        let agent = self.agent_slot.as_ref();
        let (prompt_tokens, completion_tokens) = match agent {
            Some(agent) => agent.usage().session_totals(),
            None => (
                self.app.status.prompt_tokens,
                self.app.status.completion_tokens,
            ),
        };
        SessionSnapshot {
            model: self.app.status.model.clone(),
            provider_name: provider.name.clone(),
            provider_kind: provider.kind,
            provider_base_url: provider.base_url.clone(),
            mode: self.app.mode(),
            effort: self.app.config.reasoning_effort,
            max_steps: None,
            session: agent.map(|agent| agent.session().id.clone()),
            prompt_tokens,
            completion_tokens,
            // Only when the agent is in its slot. The status bar mirrors the
            // two flat totals and nothing else, so mid-turn there is no cache
            // split to report and `/cost` says so by pricing as all-fresh.
            cache_tokens: agent.map(|agent| agent.usage().session_cache_totals()),
            context_tokens: None,
            background_tasks: agent.map(|agent| agent.running_tasks()),
            todos: crate::tools::todo::progress(&self.app.todos),
            plan: self.plan(),
            ultra: self.app.ultra.as_ref().map(|ultra| ultra.label()),
            usd_per_mtok_in: provider.usd_per_mtok_in,
            usd_per_mtok_out: provider.usd_per_mtok_out,
        }
    }

    fn plan(&self) -> PlanState {
        PlanState {
            plan: self.app.plan_mode,
            omakase: self.app.omakase,
        }
    }

    fn background_tasks(&self) -> Result<Vec<Task>, String> {
        // The registry, not the agent. Mid-turn the agent is out of its slot,
        // and this used to answer "unavailable while a turn is running" — the
        // one moment anybody types `/bashes`, since a background task is
        // something a *running* turn put there. `App::tasks` is a clone of the
        // same `Arc` the agent holds, so it stays reachable either way.
        match (self.agent_slot.as_ref(), self.app.tasks.as_ref()) {
            (Some(agent), _) => Ok(agent.tasks()),
            (None, Some(registry)) => Ok(registry.list()),
            (None, None) => Err("background tasks: no agent has been built yet".to_string()),
        }
    }

    fn rewind_candidates(&self) -> Vec<RewindCandidate> {
        self.agent_slot
            .as_ref()
            .map(|agent| agent.rewind_candidates(20))
            .unwrap_or_default()
    }

    async fn set_model(&mut self, tag: String) {
        self.switch_model(tag);
    }

    async fn set_mode(&mut self, mode: Mode) -> bool {
        self.switch_mode(mode)
    }

    async fn set_effort(&mut self, effort: Option<ReasoningEffort>) -> bool {
        self.apply_effort(effort)
    }

    async fn set_plan(&mut self, plan: PlanState) -> bool {
        self.apply_plan(plan)
    }

    async fn compact(&mut self) {
        self.request_compact();
    }

    async fn reload(&mut self) {
        self.reload_extensions().await;
    }

    async fn rewind(&mut self, turn: u64) {
        self.rewind_to_turn(turn);
    }

    async fn btw(&mut self, question: String) {
        self.ask_aside(question);
    }

    async fn fork(&mut self, task: String) {
        self.start_side_quest(task);
    }

    /// A goal set here starts a turn toward it immediately, queued behind any
    /// running one, so the dispatcher does not add the "send a message" line
    /// that the surfaces without a queue need.
    async fn start_goal(&mut self, goal: String) -> bool {
        self.app.queue_goal_kickoff(&goal);
        true
    }

    async fn evolve(&mut self, deep: bool, description: String) {
        self.start_evolve(deep, description);
    }

    async fn publish(&mut self, branch: Option<String>) {
        self.start_publish(branch);
    }

    async fn toggle_fusion(&mut self) {
        self.switch_fusion().await;
    }

    async fn toggle_ultra(&mut self) {
        self.switch_ultra();
    }

    async fn server(&mut self, action: ServerAction) {
        self.server_command(action).await;
    }

    /// Every chooser this surface has, which is all of them: the terminal is
    /// where the pickers live.
    async fn open(&mut self, chooser: Chooser) -> bool {
        match chooser {
            Chooser::Model => self.open_model_picker().await,
            Chooser::Mode => self.open_mode_picker(),
            Chooser::Effort => self.open_effort_picker(),
            Chooser::Rewind => self.open_rewind_picker(),
            Chooser::Resume => self.app.open_resume_picker(),
            Chooser::ResumeClaude => self.app.open_resume_claude_picker(),
            Chooser::Agents => self.open_agents_picker(),
            Chooser::Provider => self.app.open_provider_picker(),
            Chooser::Settings => self.app.open_settings_picker(),
            Chooser::FusionPanel => self.app.open_fusion_picker(),
            Chooser::UltraRoster => self.app.open_ultra_picker(),
        }
        true
    }

    async fn clear(&mut self) {
        self.clear_conversation();
    }

    async fn resume(&mut self, id: String) {
        self.resume_session(id).await;
    }

    async fn resume_claude(&mut self, id: String) {
        CommandContext::resume_claude(self, id).await;
    }

    async fn toggle_panel(&mut self, panel: Panel) {
        match panel {
            Panel::Diff => self.toggle_diff().await,
            Panel::Todos => self.toggle_todos(),
            Panel::Dashboard => self.toggle_dashboard(),
        }
    }

    async fn toggle_vim(&mut self) {
        self.app.toggle_vim();
    }

    async fn set_ui(&mut self, name: Option<String>) {
        let text = ui_command(self.app, name.as_deref());
        self.app.notice(text);
    }

    async fn quit(&mut self) {
        self.app.should_quit = true;
    }

    async fn apply_ultra(&mut self, config: UltraConfig) {
        self.save_ultra_roster(config);
    }

    async fn provider(&mut self, action: ProviderAction) {
        self.provider_command(action).await;
    }

    async fn provider_setup(
        &mut self,
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key: Option<String>,
    ) {
        self.finish_provider_setup(name, kind, base_url, model, api_key)
            .await;
    }

    async fn login(&mut self, provider: String) {
        self.start_login(provider);
    }

    async fn import_claude(&mut self, selection: ImportSelection) {
        self.run_claude_import(selection).await;
    }
}

/// [`crate::server::Progress`] adapter for the TUI's `/server start`: relays
/// status lines and download milestones into the transcript as notices (the
/// callback is sync, so each line is sent from its own task). Byte progress
/// is throttled to whole-percent steps, the way the plain-terminal download
/// bar fills, so a multi-GB pull does not flood the transcript.
struct NoticeProgress {
    notify: mpsc::Sender<Event>,
}

impl NoticeProgress {
    fn notice(notify: &mpsc::Sender<Event>, line: String) {
        let notify = notify.clone();
        tokio::spawn(async move {
            let _ = notify.send(Event::Notice(line)).await;
        });
    }
}

impl server::Progress for NoticeProgress {
    fn status(&self, line: &str) {
        Self::notice(&self.notify, line.to_string());
    }

    fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn server::ByteProgress> {
        Box::new(NoticeBytes {
            notify: self.notify.clone(),
            label: label.to_string(),
            total: total.filter(|total| *total > 0),
            written: std::sync::atomic::AtomicU64::new(0),
            last_percent: std::sync::atomic::AtomicU64::new(0),
        })
    }
}

/// Byte-progress guard for [`NoticeProgress`]: emits a transcript notice on
/// each whole-percent advance and a closing milestone on finish.
struct NoticeBytes {
    notify: mpsc::Sender<Event>,
    label: String,
    total: Option<u64>,
    written: std::sync::atomic::AtomicU64,
    last_percent: std::sync::atomic::AtomicU64,
}

impl server::ByteProgress for NoticeBytes {
    fn inc(&self, n: u64) {
        use std::sync::atomic::Ordering;
        let written = self.written.fetch_add(n, Ordering::Relaxed) + n;
        if let Some(total) = self.total {
            let percent = written * 100 / total;
            if percent > self.last_percent.swap(percent, Ordering::Relaxed) {
                NoticeProgress::notice(
                    &self.notify,
                    format!(
                        "{} — {percent}% of {:.1} GB",
                        self.label,
                        total as f64 / 1e9
                    ),
                );
            }
        }
    }

    fn finish(self: Box<Self>, msg: &str) {
        if !msg.is_empty() {
            NoticeProgress::notice(&self.notify, msg.to_string());
        }
    }
}
