//! TUI state machine: application state, slash commands, and the genie-mode
//! main loop. Rendering lives in [`crate::ui`]; raw events in
//! [`crate::event`].

mod command;
mod paste;
mod picker;
mod prompts;
mod recover;
mod runtime;
mod session;
mod tee;
mod term;
#[cfg(test)]
mod tests;
mod transcript;

pub use picker::{Picker, PickerItem, PickerKind, Selection, StatusLine, Suggestion};
pub use prompts::{Console, Interview, PlanReview, ProviderPrompt};
pub use runtime::run_tui;
pub use tee::MeshTee;
pub use term::restore_terminal_best_effort;
pub use transcript::{
    LOCAL_MARKER, PaneStatus, PeerOrigin, PeerStream, SubagentPane, TranscriptOrigin,
    TranscriptView,
};

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use serde_json::Value;

use crate::agent::{Agent, AgentEvent, DoneReason, PlanVerdict, ultra};
// The built-in command table and its parser live in [`crate::commands`].
use crate::commands::CustomCommand;
use crate::commands::{COMMANDS, ProviderAction, SlashCommand, UltraAction};
use crate::config::{Config, Mode, ProviderKind, ReasoningEffort, UltraConfig};
use crate::event::Event;
use crate::image_view::ImageCache;
use crate::import_claude::{self, ImportSelection};
use crate::session_registry::{self, SessionRecord, SessionState};
use crate::theme;
use crate::tools::todo::TodoItem;
use crate::vim::{self, Pending, VimMode, VimOp, VimState};

use crate::transcript::{ToolItemOutput, TranscriptItem};
use paste::{
    clipboard_image_bytes, looks_like_image_path_token, parse_data_image_url,
    resolve_pasted_image_path, save_image_bytes, save_pasted_image_bytes, sniff_image_ext,
};
use picker::is_builtin_command;
use prompts::{
    PROVIDER_ADD_ROW, PROVIDER_TYPES, PromptField, ULTRA_JUDGE_ROW, WEB_BACKENDS, prompt_question,
    web_backend_label, web_backend_needs_key, xai_oauth_session_present,
};
use transcript::PANE_LINGER;

/// How many user prompts may sit behind a running turn. Beyond this the next
/// Enter is refused with a notice rather than growing without bound.
const MESSAGE_QUEUE_CAP: usize = 32;

/// How long Ctrl-C waits for the turn to stop on its own before the task is
/// aborted instead.
///
/// The cooperative stop is worth waiting for: it keeps the agent (an abort
/// loses it and forces a rebuild off the session), it keeps the partial answer,
/// and it lets every subagent in flight — every `/ultra` candidate — close its
/// own pane out instead of being dropped mid-poll. Where the flag *is* checked
/// (each stream chunk, each tool boundary, each poll of the ultra fan-out) it
/// lands in milliseconds; this budget only bounds the case where it cannot be
/// checked, i.e. a tool call already running, which no flag can shorten.
const INTERRUPT_GRACE: Duration = Duration::from_millis(1_500);

/// Outcome of a background agent rebuild (model switch, crash recovery),
/// delivered to the main loop via [`Event::AgentRebuilt`].
pub struct AgentRebuild {
    /// Agent to restore into the main loop's slot. `None` when the rebuild
    /// failed outright and no previous agent could be preserved.
    pub agent: Option<Agent>,
    /// On a successful model switch, the tag to record in config/status.
    pub model: Option<String>,
    /// Notice appended to the transcript.
    pub notice: String,
}

impl std::fmt::Debug for AgentRebuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRebuild")
            .field("agent", &self.agent.is_some())
            .field("model", &self.model)
            .field("notice", &self.notice)
            .finish()
    }
}

/// The git diff sidebar's contents while it is open (`/diff`).
#[derive(Debug, Default)]
pub struct DiffPane {
    /// The rendered diff, cached so the sidebar does not shell out per frame.
    pub text: String,
    /// First visible line, clamped to the content height by the renderer.
    pub scroll: u16,
}

/// Where ↑/↓ recall has got to in [`App::history`], and what it displaced.
#[derive(Debug)]
struct HistoryBrowse {
    /// Index into `history` of the entry currently in the composer.
    position: usize,
    /// The in-progress input saved when browsing started, restored by ↓ past
    /// the newest entry.
    draft: String,
}

/// What the input line is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Composing a chat message.
    #[default]
    Chat,
    /// Composing a `/slash` command.
    Command,
    /// Answering an inline prompt (the interactive provider-setup flow): the
    /// composer collects one field at a time instead of submitting a message.
    Prompt,
}

/// Full TUI state. [`crate::ui::draw`] renders it; [`App::handle_event`]
/// mutates it.
#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub input: String,
    /// Cursor position in `input`, in characters.
    pub cursor: usize,
    pub input_mode: InputMode,
    /// Modal (vim-style) editing state for the composer. Inert (always
    /// insert-like) unless `vim.enabled`.
    pub vim: VimState,
    /// The conversation, its uncommitted streaming tail, this screen's fold
    /// flags and its scroll position. One [`crate::transcript::TranscriptModel`]
    /// and no copy of it: see [`transcript`].
    pub transcript: TranscriptView,
    pub status: StatusLine,
    /// Latched once the user submits anything — a slash command dispatches
    /// without adding transcript entries, so `has_conversation` alone would
    /// leave the welcome screen up after it.
    pub welcome_dismissed: bool,
    /// The git diff sidebar while it is open: its cached contents and the
    /// scroll offset (in lines, from the top) that lets PgUp/PgDn page a diff
    /// taller than the pane. `None` is the sidebar being closed — there is no
    /// separate visibility flag to disagree with the contents.
    pub diff: Option<DiffPane>,
    /// Compact todo band above the composer (toggled by `/todos`;
    /// auto-shown on the first todo update of the session). Reserves layout
    /// rows so it never covers transcript text.
    pub show_todos: bool,
    /// The agent's current todo list, mirrored from
    /// [`AgentEvent::TodoUpdated`].
    pub todos: Vec<TodoItem>,
    /// Whether a todo update has arrived yet (drives the one-time
    /// auto-show).
    todos_seen: bool,
    /// Full-screen agent dashboard visibility (toggled by `/dashboard`).
    pub show_dashboard: bool,
    /// Every subagent run this session, oldest first — the rail below the
    /// composer. Fed by the `AgentEvent::SubagentRun*` events.
    pub panes: Vec<SubagentPane>,
    /// Background-subagent registry, so the rail can kill a detached run even
    /// while a turn holds the agent. `None` until the agent is built.
    pub subagents: Option<Arc<crate::tools::subagent_tasks::SubagentTaskRegistry>>,
    /// Background-shell-task registry, for the same reason and by the same
    /// route: `/bashes` has to answer while a turn holds the agent, which is
    /// the only time somebody asks it. `None` until the agent is built.
    pub tasks: Option<Arc<crate::tools::tasks::TaskRegistry>>,
    /// Selected rail row while the rail has keyboard focus (↓ from the
    /// composer). `None` means the composer has focus and the rail is just
    /// on display. Indexes [`App::panes`].
    pub rail_focus: Option<usize>,
    /// The pane the user is *inside*: its transcript replaces the main chat
    /// until Esc. Indexes [`App::panes`].
    pub attached: Option<usize>,
    /// This session's id (heartbeat filename + dashboard identity).
    pub session_id: String,
    /// This session's display name (from the first prompt, or the id).
    pub session_name: String,
    /// Unix start time, stamped once at registration.
    pub session_started_unix: u64,
    /// Live sessions on the machine, refreshed from the registry while the
    /// dashboard is open.
    pub sessions: Vec<SessionRecord>,
    /// Armed by a first Ctrl-C; a second one exits. Disarmed by any other key.
    pub ctrl_c_armed: bool,
    /// Selected row in the dashboard list.
    pub dashboard_selected: usize,
    /// Dispatch input at the bottom of the dashboard (the prompt for a new
    /// background session).
    pub dashboard_input: String,
    /// Recent transcript of the selected session (role, text), shown in the
    /// dashboard's peek panel; refreshed as the selection moves.
    pub peek_lines: Vec<(String, String)>,
    /// Active or just-completed mouse text selection, if any. Drives the
    /// highlight overlay and clipboard copy.
    pub selection: Option<Selection>,
    /// Screen rows of tool-card header lines visible in the last-drawn frame,
    /// as `(row, transcript index)` — the click-to-toggle hit map. Rebuilt by
    /// [`crate::ui::draw`] every frame (hence the interior mutability: draw
    /// takes `&App`) and emptied while an overlay covers the transcript.
    pub card_hits: std::cell::RefCell<Vec<(u16, usize)>>,
    /// What this terminal can draw an image with, and every image it has drawn
    /// recently. Starts at the half-block floor so a frame can be rendered
    /// before anything has asked the terminal; `run_tui` replaces it with
    /// [`ImageCache::detect`] before it takes the screen. Interior mutability
    /// for the same reason as `card_hits` — draw takes `&App`, and decoding a
    /// PNG once per image is exactly what a cache is for.
    pub images: std::cell::RefCell<ImageCache>,
    pub should_quit: bool,
    /// Tick counter driving the busy spinner.
    pub tick: u64,
    /// Matching commands (builtin [`COMMANDS`] plus custom commands) for the
    /// current `/input`, shown as the suggestion popup.
    pub suggestions: Vec<Suggestion>,
    /// Highlighted row in `suggestions`.
    pub suggestion_index: usize,
    /// Whether any key has reached [`App::handle_key`] this session. Read by
    /// [`App::notice`] to tell a reply to the user from a startup message.
    key_pressed: bool,
    /// The composer text the popup was dismissed at, if Escape dismissed it.
    ///
    /// Dismissal has to be state rather than an empty list, because
    /// [`Self::sync_input_mode`] runs after *every* key and rebuilds the
    /// suggestions from the input — so clearing the list on Escape was undone
    /// before the next frame, and Escape appeared to do nothing at all.
    /// Holding the text it was dismissed at means the popup stays shut until
    /// the draft actually changes, which is what dismissing a completion menu
    /// means everywhere else.
    dismissed_suggestions_for: Option<String>,
    /// Custom commands loaded from `~/.wizard/commands/` and
    /// `<project>/.wizard/commands/` (set by `run_tui`, refreshed by
    /// `/reload`).
    pub custom_commands: Vec<CustomCommand>,
    /// Project root `@file` references resolve against.
    pub project_root: PathBuf,
    /// Open selection popup (model / mode / rewind / subagent picker), if any.
    pub picker: Option<Picker>,
    /// In-progress interactive provider setup, when the composer is collecting
    /// fields ([`InputMode::Prompt`]).
    pub prompt: Option<ProviderPrompt>,
    /// When the composer is collecting a pasted API key for a keyed
    /// `web_search` backend (the backend name); set from the `/settings` web
    /// search picker, consumed by [`App::submit_web_key`].
    pub web_key_backend: Option<String>,
    /// Image files staged for the next submit (from paste of image paths or
    /// `data:image/...;base64,...` blobs). Merged with `@file` image refs on
    /// submit, then cleared.
    pub pending_images: Vec<PathBuf>,
    /// Whether plan mode is active (mirrors the agent's flag for the status
    /// bar; toggled by `/plan` and Shift+Tab).
    pub plan_mode: bool,
    /// Whether omakase (chef's-choice) mode is active (mirrors the agent's
    /// flag; toggled by `/omakase`). Implies `plan_mode`.
    pub omakase: bool,
    /// Whether the active client is a [`FusionProvider`](crate::llm::fusion)
    /// (`/fusion` toggled on). Drives the loud status-bar indicator and lets
    /// `/fusion` toggle back to the underlying single provider.
    pub fusion_active: bool,
    /// The mixture-of-agents roster `/ultra` is running, or `None` when ultra is
    /// off. Holds the *built* engine, not the [`UltraConfig`] behind it, for two
    /// reasons: the `ULTRA ×N` badge then counts the lenses the agent will
    /// actually fan out over rather than a config that may no longer resolve,
    /// and [`restore_ultra`](session::restore_ultra) can re-arm a rebuilt agent by cloning the handle
    /// instead of rebuilding a roster that could fail at exactly the moment
    /// there is no good way to report it. The engine binds no client — the agent
    /// supplies the live one — so the same instance survives a `/model` switch
    /// and the candidates follow the new model.
    pub ultra: Option<Arc<ultra::UltraEngine>>,
    /// Open plan-review modal (the turn is paused inside `exit_plan` until
    /// it resolves), if any.
    pub plan_review: Option<PlanReview>,
    /// Open interview modal (the turn is paused inside the `interview` tool
    /// until the user answers or dismisses), if any.
    pub interview: Option<Interview>,
    /// The running foreground command this composer is typing into, if any.
    ///
    /// Unlike `plan_review` and `interview` this is not a modal: the turn is
    /// not paused, the transcript keeps moving, and the user can walk away from
    /// it with Esc. What it changes is where Enter goes — see [`Console`] and
    /// [`App::submit_console`].
    pub console: Option<Console>,
    /// A console whose writer we hold but whose command has not asked anything
    /// yet, if any.
    ///
    /// Every foreground command opens a console, and almost none of them ever
    /// prompt. Claiming has to be eager — the gate is claimed once, and the
    /// writer has to be ours before a question can appear — but taking the
    /// composer over for the whole of every `ls` is the bug this field exists
    /// to avoid. It is promoted into `console` on `ConsoleWaiting` and dropped
    /// on `ConsoleClosed`, so a command that never asks anything never touches
    /// the composer at all.
    pub console_pending: Option<Console>,
    /// Previously submitted inputs, oldest first (↑/↓ recall).
    pub history: Vec<String>,
    /// Where in `history` the composer is, and the draft it displaced, while
    /// the user is browsing it. `None` when composing fresh input — one field,
    /// because a saved draft with no position (or the reverse) is not a state
    /// the recall can be in.
    history_browse: Option<HistoryBrowse>,
    /// When the in-flight turn started (drives the elapsed-time display).
    pub turn_started: Option<Instant>,
    /// Label of an in-progress background agent rebuild (model switch,
    /// crash recovery); rendered as a spinner in the status bar. Input that
    /// needs the agent is rejected with a notice while this is `Some`.
    pub rebuilding: Option<String>,
    /// Verb shown next to the busy spinner ("Conjuring…"); re-rolled at the
    /// start of each busy period by [`App::roll_spinner_verb`].
    pub spinner_verb: String,
    /// Number of verb rolls so far, mixed into the roll seed so back-to-back
    /// turns starting on the same tick still draw fresh verbs.
    verb_rolls: u64,
    /// Set by the `/settings` "Open config file" row; the main loop (which owns
    /// the terminal) suspends the TUI, opens `$EDITOR` on the config file, then
    /// reloads config. Cleared once handled.
    pub pending_edit_config: bool,
    /// Set by Ctrl-G; the main loop suspends the TUI, opens the composer draft
    /// in `$EDITOR`, and reads the result back. Cleared once handled.
    pub pending_edit_prompt: bool,
    /// Set by `/compact`; the main loop takes the agent and runs compaction in
    /// the background. Cleared once the task is spawned.
    pub pending_compact: bool,
    /// True while a background `/compact` is running: the status bar shows an
    /// animated progress bar instead of its usual contents.
    pub compacting: bool,
    /// Set by `/btw <question>`; the main loop answers it off the event loop
    /// against a snapshot of the conversation (so it works mid-turn too).
    /// Cleared once the task is spawned.
    pub pending_btw: Option<String>,
    /// True while a background `/btw` is in flight, so a second one is refused
    /// rather than stacked.
    pub btw_inflight: bool,
    /// Set by `/fork <task>`; the main loop detaches a side quest that inherits
    /// the full conversation (so it works mid-turn too). Cleared once spawned.
    pub pending_fork: Option<String>,
    /// Set when the background MCP connect finishes while a turn is running
    /// (so the agent is out of its slot and can't take the rebuilt registry
    /// yet). The main loop merges the MCP tools once the turn returns the
    /// agent. Cleared then.
    pub mcp_merge_pending: bool,
    /// Slash commands the agent asked to run via the `run_command` tool during
    /// the current turn (raw command lines, e.g. `/effort high`). A turn in
    /// flight cannot be reconfigured, so the main loop drains and dispatches
    /// these once the turn ends and the agent is back in its slot.
    pub pending_agent_commands: Vec<String>,
    /// User prompts submitted while a turn is already running. FIFO; the main
    /// loop pops one after each turn finishes (and after any post-turn slash
    /// commands the agent queued) so the next turn starts without the user
    /// having to retype. Each entry is already preprocessed and has already
    /// been written to the transcript + history on enqueue. Capped at
    /// [`MESSAGE_QUEUE_CAP`].
    pub message_queue: VecDeque<crate::commands::Preprocessed>,
    /// True while the background MCP connect is in flight (servers spawning,
    /// `initialize` round-trips). Drives a transient status-bar indicator so a
    /// message sent before the tools arrive isn't a silent surprise. Cleared on
    /// the no-servers early-return and once the connect finishes or fails.
    pub mcp_connecting: bool,
    /// Set by the deferred cloud-provider health probe when it fails, so the
    /// breakage is visible at launch (welcome screen + status bar) instead of
    /// only on the first message. Cleared once a turn completes successfully —
    /// the provider has proven itself, so a transient blip self-heals.
    pub provider_health_error: Option<String>,
    /// This session's tee onto the mesh, when this node is listening.
    ///
    /// `None` for a default install, and `None` is the shipped default: a peer
    /// watches this node by dialling it, so with `[mesh] listen` off nobody can
    /// subscribe and a tee would be a socket bound for a stream nobody can
    /// open. See [`MeshTee`], which is also where the one call to
    /// [`crate::mesh::Mesh::publish_turn`] lives.
    pub mesh: Option<MeshTee>,
}

impl App {
    pub fn new(config: Config) -> Self {
        // Install the active skin and theme before anything can draw. Both
        // resolve config > environment > default, and `[ui] skin` / `[ui]
        // theme` are the config half of each; passing them here is what makes
        // the documented precedence real.
        //
        // Skin first, because it is the theme's last resort: a skin names a
        // companion palette (`claude` chrome wants Claude Code's terracotta),
        // and that name is only consulted when the user has chosen no theme at
        // all. Resolving the theme first would leave every skin looking like
        // `minimal` on a fresh install.
        let skin_warning = crate::skin::init(config.ui.skin.as_deref());
        let theme_warning = theme::init(crate::skin::active().companion_theme());
        let mode = config.mode;
        // Omakase implies plan mode (the read-only exploration phase).
        let omakase = config.omakase;
        let plan_mode = config.plan_first || omakase;
        let spinner_verb = config.ui.spinner_verb(0).to_string();
        // Vim mode starts in Insert so typing works immediately; `Esc` drops
        // to Normal.
        let vim = VimState {
            enabled: config.ui.vim,
            ..VimState::default()
        };
        let status = StatusLine {
            model: config.active().model,
            mode,
            step: 0,
            max_steps: config.max_steps,
            busy: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            context_tokens: 0,
            background_tasks: 0,
            background_subagents: 0,
        };
        let mut app = Self {
            config,
            input: String::new(),
            cursor: 0,
            input_mode: InputMode::default(),
            vim,
            transcript: TranscriptView::new(),
            status,
            welcome_dismissed: false,
            diff: None,
            show_todos: false,
            todos: Vec::new(),
            todos_seen: false,
            show_dashboard: false,
            panes: Vec::new(),
            subagents: None,
            tasks: None,
            rail_focus: None,
            attached: None,
            session_id: String::new(),
            session_name: String::new(),
            session_started_unix: 0,
            sessions: Vec::new(),
            ctrl_c_armed: false,
            dashboard_selected: 0,
            dashboard_input: String::new(),
            peek_lines: Vec::new(),
            selection: None,
            card_hits: std::cell::RefCell::new(Vec::new()),
            images: std::cell::RefCell::new(ImageCache::fallback()),
            should_quit: false,
            tick: 0,
            suggestions: Vec::new(),
            suggestion_index: 0,
            key_pressed: false,
            dismissed_suggestions_for: None,
            custom_commands: Vec::new(),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            picker: None,
            prompt: None,
            web_key_backend: None,
            pending_images: Vec::new(),
            plan_mode,
            omakase,
            fusion_active: false,
            ultra: None,
            plan_review: None,
            interview: None,
            console: None,
            console_pending: None,
            history: Vec::new(),
            history_browse: None,
            turn_started: None,
            rebuilding: None,
            spinner_verb,
            verb_rolls: 0,
            pending_edit_config: false,
            pending_edit_prompt: false,
            pending_compact: false,
            compacting: false,
            pending_btw: None,
            btw_inflight: false,
            pending_fork: None,
            mcp_merge_pending: false,
            pending_agent_commands: Vec::new(),
            message_queue: VecDeque::new(),
            mcp_connecting: false,
            provider_health_error: None,
            // Joined by `run_tui` once the session has an id to stamp events
            // with, and only when `[mesh] listen` says a peer could watch.
            mesh: None,
        };
        // A skin or theme that would not load is worth saying out loud (the
        // user asked for it); the defaults are already installed, so the
        // session continues either way.
        for warning in [skin_warning, theme_warning].into_iter().flatten() {
            app.notice(warning);
        }
        app
    }

    /// True while the home screen should remain up: the conversation hasn't
    /// begun. Early system notices (e.g. a provider-health warning) land in the
    /// transcript before the user sends anything; those alone shouldn't dismiss
    /// the opening screen, so only non-`Notice` entries count as conversation.
    pub fn has_conversation(&self) -> bool {
        self.transcript
            .iter()
            .any(|item| !matches!(item, TranscriptItem::Notice(_)))
    }

    /// The agent's autonomy setting. Derived, not stored: `/mode` writes it
    /// through to the config, so a second copy on `App` could only ever be a
    /// chance to disagree with the file the next reload reads.
    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    /// True while the welcome screen should render: the conversation hasn't
    /// begun, nothing was ever submitted (a slash command counts even though
    /// it adds no transcript entries), and no turn is in flight.
    pub fn welcome_visible(&self) -> bool {
        !self.has_conversation()
            && !self.welcome_dismissed
            && self.transcript.streaming() == ("", "")
            && !self.status.busy
    }

    /// Pick a fresh spinner verb for a new busy period. The verb stays fixed
    /// until the next roll, so one turn reads as one activity.
    pub fn roll_spinner_verb(&mut self) {
        self.verb_rolls = self.verb_rolls.wrapping_add(1);
        let seed = self.tick.wrapping_add(self.verb_rolls);
        self.spinner_verb = self.config.ui.spinner_verb(seed).to_string();
    }

    /// Append a system notice to the transcript.
    pub fn notice(&mut self, message: impl Into<String>) {
        // A notice raised after the user has touched the keyboard is an answer
        // to something they did, so it has to be visible — which means getting
        // out from behind the welcome screen.
        //
        // `has_conversation` filters notices out on purpose, so that a startup
        // notice (a hook that appended context, an MCP server that did not
        // connect) does not replace the splash on a session nobody has used
        // yet. That is right for those, and wrong for these: Ctrl-V on a fresh
        // session printed "no image on the clipboard to attach" into a
        // transcript nobody could see, and — worse — the first Ctrl-C's
        // "press Ctrl-C again to exit" was invisible too, so a fresh session
        // gave no warning at all before the second press quit it.
        //
        // Keyed on whether a key has been pressed, which is exactly the
        // difference between the two cases.
        if self.key_pressed {
            self.welcome_dismissed = true;
        }
        self.transcript.notice(message.into());
    }

    /// The settings menu rows, in display order: `(action id, label, current
    /// value)`. [`open_settings_picker`](Self::open_settings_picker) renders
    /// the label/value and [`apply_setting`](Self::apply_setting) dispatches by
    /// the row index, so both share this single ordered source of truth.
    ///
    /// Numeric/list fields (`max_steps`, retry/compaction knobs, spinner verbs,
    /// gateway, …) are intentionally absent — the overlay has no text input, so
    /// they live behind the "Open config file" row.
    fn settings_rows(&self) -> Vec<(&'static str, String, String)> {
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        let providers = self.config.providers.len();
        let import_detail = if import_claude::claude_home().is_some() {
            "MCP servers, commands, spinner verbs".to_string()
        } else {
            "no ~/.claude found".to_string()
        };
        let config_path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.wizard/config.toml".to_string());
        vec![
            ("model", "Model".to_string(), self.config.active().model),
            ("mode", "Mode".to_string(), self.mode().to_string()),
            (
                "plan_first",
                "Plan mode at startup".to_string(),
                on(self.config.plan_first),
            ),
            (
                "continuous",
                "Continuous (sovereign)".to_string(),
                on(self.config.continuous),
            ),
            (
                "plan_each_cycle",
                "Plan each cycle".to_string(),
                on(self.config.plan_each_cycle),
            ),
            (
                "rollback",
                "Rollback failed cycles".to_string(),
                on(self.config.rollback_failed_cycles),
            ),
            (
                "vim",
                "Vim mode (modal input)".to_string(),
                on(self.config.ui.vim),
            ),
            (
                "skin",
                "Interface".to_string(),
                crate::skin::active().label().to_string(),
            ),
            (
                "web_backend",
                "Web search backend".to_string(),
                self.config.web.search_backend.clone(),
            ),
            (
                "web_allow_local",
                "Web: allow localhost".to_string(),
                on(self.config.web.allow_local),
            ),
            (
                "fleet_synthesize",
                "Fleet: synthesis turn".to_string(),
                on(self.config.fleet.synthesize),
            ),
            (
                "import",
                "Import from Claude Code".to_string(),
                import_detail,
            ),
            (
                "provider",
                "Manage providers…".to_string(),
                format!("{providers} configured"),
            ),
            ("config_file", "Open config file…".to_string(), config_path),
        ]
    }

    /// Open the `/settings` menu as a [`Picker`]. Re-callable: toggles re-open
    /// it so the new value is visible.
    pub fn open_settings_picker(&mut self) {
        let items: Vec<PickerItem> = self
            .settings_rows()
            .into_iter()
            .map(|(_, label, detail)| PickerItem {
                value: label,
                detail,
                current: false,
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Settings,
            title: " settings · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Dispatch the settings row at `selected`. Routing rows return an
    /// [`AppAction`] to run a command; inline toggle/cycle rows mutate config,
    /// persist, and re-open the menu (keeping the cursor on the same row).
    fn apply_setting(&mut self, selected: usize) -> Option<AppAction> {
        let rows = self.settings_rows();
        let (id, _, _) = rows.get(selected)?;
        match *id {
            "model" => return Some(AppAction::Command(SlashCommand::Model(None))),
            "mode" => return Some(AppAction::Command(SlashCommand::Mode(None))),
            "provider" => {
                return Some(AppAction::Command(SlashCommand::Provider(
                    ProviderAction::List,
                )));
            }
            "import" => {
                self.open_claude_import_picker();
                return None;
            }
            "config_file" => {
                // Handled by the main loop, which owns the terminal.
                self.pending_edit_config = true;
                return None;
            }
            "vim" => {
                let on = !self.config.ui.vim;
                self.config.ui.vim = on;
                // Keep the live composer state in step with the persisted flag.
                self.vim = VimState {
                    enabled: on,
                    mode: VimMode::Insert,
                    ..VimState::default()
                };
            }
            "plan_first" => self.config.plan_first = !self.config.plan_first,
            "continuous" => self.config.continuous = !self.config.continuous,
            "plan_each_cycle" => self.config.plan_each_cycle = !self.config.plan_each_cycle,
            "rollback" => {
                self.config.rollback_failed_cycles = !self.config.rollback_failed_cycles;
            }
            "web_backend" => {
                self.open_web_backend_picker();
                return None;
            }
            // Cycles rather than opening a picker of four: the change is
            // visible the instant it lands, and the menu is still on screen to
            // show it, so cycling *is* the preview.
            "skin" => {
                let active = crate::skin::active();
                let next = crate::skin::Skin::ALL[(crate::skin::Skin::ALL
                    .iter()
                    .position(|s| *s == active)
                    .unwrap_or(0)
                    + 1)
                    % crate::skin::Skin::ALL.len()];
                let notice = command::ui_command(self, Some(next.key()));
                // `ui_command` has already persisted and re-resolved the
                // palette; anything it has to say is worth saying.
                if notice.starts_with("error:") || notice.contains("could not save") {
                    self.notice(notice);
                }
                self.open_settings_picker();
                if let Some(picker) = self.picker.as_mut() {
                    picker.selected = selected.min(picker.items.len().saturating_sub(1));
                }
                return None;
            }
            "web_allow_local" => self.config.web.allow_local = !self.config.web.allow_local,
            "fleet_synthesize" => self.config.fleet.synthesize = !self.config.fleet.synthesize,
            _ => return None,
        }
        // Inline change: persist and re-open, restoring the cursor so repeated
        // toggles stay on the same row. (These flags take effect at the next
        // cycle / startup, not mid-session.)
        if let Err(err) = self.config.save() {
            self.notice(format!("could not save config: {err:#}"));
        }
        self.open_settings_picker();
        if let Some(picker) = self.picker.as_mut() {
            picker.selected = selected.min(picker.items.len().saturating_sub(1));
        }
        None
    }

    /// Open the "import from Claude Code" multi-select. Each row is a toggleable
    /// artifact (Space toggles, Enter runs); order is mcp / commands / verbs to
    /// match the [`ImportSelection`] built in the Enter handler.
    fn open_claude_import_picker(&mut self) {
        if import_claude::claude_home().is_none() {
            self.notice("no Claude Code install found (~/.claude)");
            return;
        }
        let (mcp, commands, verbs) = import_claude::counts();
        let items = vec![
            PickerItem {
                value: format!("MCP servers ({mcp})"),
                detail: "merge into ~/.wizard/mcp.toml".to_string(),
                current: false,
            },
            PickerItem {
                value: format!("Custom commands ({commands})"),
                detail: "copy into ~/.wizard/commands/".to_string(),
                current: false,
            },
            PickerItem {
                value: format!("Spinner verbs ({verbs})"),
                detail: "adopt Claude Code's spinner verbs".to_string(),
                current: false,
            },
        ];
        self.picker = Some(Picker {
            kind: PickerKind::ClaudeImport,
            title: " import from claude code · space toggles · enter runs ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the `/fusion config` multi-select: one row per configured provider,
    /// pre-toggled to the current/effective panel. Space toggles membership;
    /// Enter saves `[fusion]` (first toggled row = synthesizer).
    pub fn open_fusion_picker(&mut self) {
        if self.config.providers.is_empty() {
            self.notice(
                "fusion needs configured providers — add at least two with /provider first",
            );
            return;
        }
        let in_panel: std::collections::HashSet<String> = self
            .config
            .effective_fusion()
            .map(|fusion| fusion.panel.into_iter().collect())
            .unwrap_or_default();
        let items = self
            .config
            .providers
            .iter()
            .map(|provider| PickerItem {
                value: provider.name.clone(),
                detail: format!("{} · {}", provider.kind, provider.model),
                current: in_panel.contains(&provider.name),
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::FusionPanel,
            title: " fusion panel · space toggles · enter saves ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the `/ultra config` multi-select: one row per lens in the catalog
    /// (ultra's built-ins plus every subagent in `~/.wizard/subagents/`),
    /// pre-toggled to the configured roster, and a final [`ULTRA_JUDGE_ROW`] for
    /// the compare phase. Space toggles; Enter saves `[ultra]`. There is no
    /// separate "candidate count" row because there is no separate number: one
    /// toggled lens is one candidate.
    pub fn open_ultra_picker(&mut self) {
        let ultra = self.config.effective_ultra();
        let roster: std::collections::HashSet<&str> =
            ultra.lenses.iter().map(String::as_str).collect();
        let catalog = ultra::lens_catalog(&Config::subagents_dir().unwrap_or_default());
        let mut items: Vec<PickerItem> = catalog
            .iter()
            .map(|lens| PickerItem {
                value: lens.name.clone(),
                detail: lens.description.clone(),
                current: roster.contains(lens.name.as_str()),
            })
            .collect();
        items.push(PickerItem {
            value: ULTRA_JUDGE_ROW.to_string(),
            detail: match ultra.judges {
                0 => "off — the drafts go to the agent uncompared".to_string(),
                1 => "compares the drafts head-to-head before the agent executes".to_string(),
                n => format!("{n} judges compare the drafts head-to-head"),
            },
            current: ultra.judges > 0,
        });
        self.picker = Some(Picker {
            kind: PickerKind::UltraLenses,
            title: " ultra roster · space toggles · enter saves ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the `/resume` picker: every past session on disk, newest first,
    /// each row labeled with its first prompt. The current session is marked
    /// and selecting it is a no-op.
    pub fn open_resume_picker(&mut self) {
        let dir = match crate::config::Config::sessions_dir() {
            Ok(dir) => dir,
            Err(err) => {
                self.notice(format!("cannot locate sessions: {err:#}"));
                return;
            }
        };
        let summaries = crate::agent::session::summaries(&dir);
        if summaries.is_empty() {
            self.notice("no past sessions to resume");
            return;
        }
        let items: Vec<PickerItem> = summaries
            .into_iter()
            .map(|session| {
                let plural = if session.messages == 1 { "" } else { "s" };
                PickerItem {
                    detail: format!("{} · {} msg{plural}", session.summary, session.messages),
                    current: session.id == self.session_id,
                    value: session.id,
                }
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Resume,
            title: " resume session · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// `/resume-claude`: the conversations Claude Code recorded for this
    /// directory, as the same picker `/resume` opens.
    ///
    /// Rows come from [`session_registry::claude_chats`], which is what the
    /// window's sidebar reads, so the two surfaces cannot list different
    /// sessions. Claude Code files its history under a slug of the working
    /// directory, so an empty list here usually means Claude Code was run
    /// somewhere else rather than never run — the notice says so, because
    /// "no sessions" on its own sends people looking for a bug.
    ///
    /// Nothing is imported yet: selecting a row is what does that, in
    /// [`AppCommand::resume_claude`](crate::app::command::AppCommand). The
    /// branch-point count is on the row because it is why a conversation can
    /// come back shorter than the file it came from.
    pub fn open_resume_claude_picker(&mut self) {
        let cwd = self.project_root.display().to_string();
        let rows = session_registry::claude_chats(&cwd);
        if rows.is_empty() {
            self.notice(format!(
                "Claude Code has no sessions recorded for {cwd}. It files them under a slug of \
                 the working directory, so this is also what you get when it was run elsewhere."
            ));
            return;
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let items: Vec<PickerItem> = rows
            .into_iter()
            .map(|row| {
                let ago = session_registry::relative_age(row.updated_unix, now);
                let branches = match &row.origin {
                    session_registry::Origin::Claude { branch_points, .. } => *branch_points,
                    // Not reachable: `claude_chats` only produces Claude rows.
                    // Reported as unbranched rather than panicking on a row
                    // this picker did not build.
                    session_registry::Origin::Wizard => 0,
                };
                let detail = match branches {
                    0 => format!("{} · {ago}", row.title),
                    1 => format!("{} · {ago} · 1 branch point", row.title),
                    n => format!("{} · {ago} · {n} branch points", row.title),
                };
                PickerItem {
                    detail,
                    current: false,
                    value: row.id,
                }
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::ResumeClaude,
            // Says what Enter *does*, because it is not what the other picker's
            // Enter does: this one copies a conversation out of another
            // program rather than reopening one of Wizard's own.
            title: " continue from claude code · enter imports a copy · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Rebuild the transcript view from a session's persisted messages, so a
    /// resumed conversation reads back the way it was left.
    ///
    /// What the messages *mean* is [`crate::transcript`]'s reading, the same
    /// one the GUI's replay and a live turn use; all this does is project it
    /// into the rows the TUI draws ([`transcript::replayed_entries`]). The
    /// three-way drift that used to live here is the reason that module
    /// exists: this surface dropped system notes the GUI showed, and reported
    /// every failed tool call as a success because the session file does not
    /// record the failure flag.
    fn load_transcript(&mut self, entries: &[crate::agent::session::SessionEntry]) {
        self.transcript.replay(entries);
    }

    /// Open the provider picker (level 1): the configured providers (Enter
    /// switches) plus a final "＋ Add provider…" row that opens the type
    /// picker. With no providers configured, only the add row shows.
    pub fn open_provider_picker(&mut self) {
        let active = self.config.active().name;
        let mut items: Vec<PickerItem> = self
            .config
            .providers
            .iter()
            .map(|provider| PickerItem {
                value: provider.name.clone(),
                detail: format!(
                    "{} · {} @ {}",
                    provider.kind, provider.model, provider.base_url
                ),
                current: provider.name == active,
            })
            .collect();
        items.push(PickerItem {
            value: PROVIDER_ADD_ROW.to_string(),
            detail: "configure a new provider".to_string(),
            current: false,
        });
        self.picker = Some(Picker {
            kind: PickerKind::Provider,
            title: " providers · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the provider-type picker (level 2): the menu of provider kinds to
    /// add. Rows are dispatched by index against the fixed order in
    /// [`PROVIDER_TYPES`], followed by the OpenAI-compatible presets from
    /// [`crate::llm::compat::PRESETS`], so the labels stay human-readable.
    pub fn open_provider_type_picker(&mut self) {
        let items: Vec<PickerItem> = PROVIDER_TYPES
            .iter()
            .map(|(label, detail)| PickerItem {
                value: (*label).to_string(),
                detail: (*detail).to_string(),
                current: false,
            })
            .chain(crate::llm::compat::PRESETS.iter().map(|preset| PickerItem {
                value: format!("{} — API key", preset.label),
                detail: preset.detail.to_string(),
                current: false,
            }))
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::ProviderType,
            title: " add provider · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the `web_search` backend picker (from `/settings`). Marks the
    /// current backend so the user sees what is active.
    pub fn open_web_backend_picker(&mut self) {
        let active = self.config.web.search_backend.trim().to_ascii_lowercase();
        let items: Vec<PickerItem> = WEB_BACKENDS
            .iter()
            .map(|(value, label, detail)| PickerItem {
                value: (*value).to_string(),
                detail: format!("{label} — {detail}"),
                current: *value == active || (*value == "xai" && active == "grok"),
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::WebBackend,
            title: " web search · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Apply a `web_search` backend selection that needs no key entry
    /// (DuckDuckGo, or xAI once a session/key exists): persist and report.
    fn set_web_backend(&mut self, id: &str, note: &str) {
        self.config.web.search_backend = id.to_string();
        if let Err(err) = self.config.save() {
            self.notice(format!("could not save config: {err:#}"));
            return;
        }
        self.notice(note.to_string());
    }

    /// Handle a row from the `web_search` backend picker: DuckDuckGo applies at
    /// once; keyed backends start an inline key prompt; xAI reuses the OAuth
    /// session when present (no re-login) and otherwise points the user at
    /// `/login xai`.
    fn select_web_backend(&mut self, id: &str) {
        match id {
            "xai" | "grok" => {
                if xai_oauth_session_present() {
                    self.set_web_backend(
                        "xai",
                        "web search: using your xAI sign-in (no new login needed)",
                    );
                } else if crate::credentials::get("xai").is_some() {
                    self.set_web_backend("xai", "web search: using xAI (stored API key)");
                } else {
                    self.set_web_backend(
                        "xai",
                        "web search set to xAI — run /login xai to sign in, or set XAI_API_KEY",
                    );
                }
            }
            keyed if web_backend_needs_key(keyed) => self.begin_web_key_prompt(keyed),
            other => {
                let label = web_backend_label(other).to_string();
                self.set_web_backend(other, &format!("web search: using {label}"));
            }
        }
    }

    /// Start the inline prompt that collects (and stores) a pasted API key for
    /// a keyed `web_search` backend.
    fn begin_web_key_prompt(&mut self, id: &str) {
        self.web_key_backend = Some(id.to_string());
        self.input_mode = InputMode::Prompt;
        self.clear_input();
        self.suggestions.clear();
        self.suggestion_index = 0;
        self.notice(format!(
            "paste your {} API key, then Enter (Esc to cancel):",
            web_backend_label(id)
        ));
    }

    /// Consume the composer input as the pasted API key: store it under the
    /// backend name in `~/.wizard/credentials.toml`, switch to that backend,
    /// and return to normal input. An empty entry cancels.
    fn submit_web_key(&mut self) -> Option<AppAction> {
        let id = self.web_key_backend.take()?;
        let key = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.input_mode = InputMode::Chat;
        self.sync_input_mode();
        if key.is_empty() {
            self.notice("cancelled (no key entered)");
            return None;
        }
        if let Err(err) = crate::credentials::store(&id, &key) {
            self.notice(format!("could not save the {id} API key: {err:#}"));
            return None;
        }
        let label = web_backend_label(&id).to_string();
        self.set_web_backend(
            &id,
            &format!("web search: using {label} (key saved to ~/.wizard/credentials.toml)"),
        );
        None
    }

    /// True when the composer is collecting a masked field (an API key) in an
    /// inline prompt — provider setup or web-search key entry. Drives the
    /// bullet masking in [`crate::ui`].
    pub fn prompt_is_masked(&self) -> bool {
        if self.web_key_backend.is_some() {
            return true;
        }
        self.input_mode == InputMode::Prompt
            && self
                .prompt
                .as_ref()
                .and_then(|prompt| prompt.queue.front())
                .copied()
                == Some(PromptField::ApiKey)
    }

    /// Start the inline provider-setup prompt: switch the composer into
    /// [`InputMode::Prompt`] and ask the first queued field.
    pub fn begin_provider_prompt(&mut self, prompt: ProviderPrompt) {
        self.prompt = Some(prompt);
        self.input_mode = InputMode::Prompt;
        self.clear_input();
        self.suggestions.clear();
        self.suggestion_index = 0;
        if let Some(prompt) = self.prompt.as_ref()
            && let Some(field) = prompt.queue.front().copied()
        {
            let question = prompt_question(field, prompt);
            self.notice(question);
        }
    }

    /// Cancel an in-progress provider-setup prompt and return to normal input.
    fn cancel_prompt(&mut self) {
        self.prompt = None;
        self.web_key_backend = None;
        self.input.clear();
        self.cursor = 0;
        self.input_mode = InputMode::Chat;
        self.sync_input_mode();
        self.notice("cancelled");
    }

    /// Consume the current input as the answer to the front prompt field. When
    /// more fields remain, ask the next and stay in prompt mode; when the queue
    /// drains, emit a [`SlashCommand::ProviderSetup`].
    fn submit_prompt_field(&mut self) -> Option<AppAction> {
        let value = self.input.trim().to_string();
        let prompt = self.prompt.as_mut()?;
        let field = prompt.queue.pop_front()?;
        match field {
            PromptField::Name => prompt.name = value,
            PromptField::AccountId => {
                if value.is_empty() {
                    // No account id is treated as "never mind".
                    self.cancel_prompt();
                    return None;
                }
                // Substitute the account id into the base-URL template
                // (e.g. `.../accounts/{account_id}/ai/v1`).
                prompt.base_url = prompt
                    .base_url
                    .replace(crate::llm::cloudflare::ACCOUNT_ID_PLACEHOLDER, &value);
            }
            PromptField::BaseUrl => prompt.base_url = value,
            PromptField::Model => prompt.model = value,
            PromptField::ApiKey => {
                if value.is_empty() {
                    // An empty key is treated as "never mind".
                    self.cancel_prompt();
                    return None;
                }
                prompt.api_key = Some(value);
            }
        }
        self.input.clear();
        self.cursor = 0;
        if let Some(next) = prompt.queue.front().copied() {
            let question = prompt_question(next, prompt);
            self.notice(question);
            return None;
        }
        // Queue drained: build the setup command and return to normal input.
        let prompt = self.prompt.take().expect("prompt is set");
        self.input_mode = InputMode::Chat;
        self.sync_input_mode();
        Some(AppAction::Command(SlashCommand::ProviderSetup {
            name: prompt.name,
            kind: prompt.kind,
            base_url: prompt.base_url,
            model: prompt.model,
            api_key: prompt.api_key,
        }))
    }

    /// Current state for this session's heartbeat: needs-input when paused on a
    /// plan review, working while a turn streams, otherwise idle.
    fn session_state(&self) -> SessionState {
        if self.plan_review.is_some() || self.interview.is_some() {
            SessionState::NeedsInput
        } else if self.status.busy {
            SessionState::Working
        } else {
            SessionState::Idle
        }
    }

    /// One-line summary of what this session is doing, for the dashboard row.
    fn session_activity(&self) -> String {
        if self.plan_review.is_some() {
            return "waiting for plan approval".to_string();
        }
        if self.interview.is_some() {
            return "waiting for interview answers".to_string();
        }
        if !self.status.busy {
            return "idle".to_string();
        }
        // The newest in-flight tool call reads best; fall back to the verb.
        for item in self.transcript.iter().rev() {
            if let TranscriptItem::Tool(tool) = item
                && tool.output.is_none()
            {
                return tool.name.clone();
            }
        }
        format!("{}…", self.spinner_verb)
    }

    // ---- Subagent rail -------------------------------------------------
    //
    // The rail is the row of dots under the composer, one per subagent run.
    // ↓ from the composer focuses it, ↑/↓ move between dots, Enter opens the
    // selected one as a full chat view, Esc backs out.

    /// Index of the pane for `run`, if it is still on the rail.
    fn pane_index(&self, run: u64) -> Option<usize> {
        self.panes.iter().position(|pane| pane.run == run)
    }

    /// Append to a pane's transcript and bump its unread badge — unless the
    /// user is currently watching that pane, in which case they have already
    /// seen it. Scroll position is left alone: a following pane stays pinned
    /// by its follow flag, a scrolled-up pane keeps its top-anchored offset.
    fn pane_write(&mut self, run: u64, unread: bool, write: impl FnOnce(&mut TranscriptView)) {
        let Some(index) = self.pane_index(run) else {
            return;
        };
        let attached = self.attached == Some(index);
        let pane = &mut self.panes[index];
        write(&mut pane.transcript);
        if unread && !attached {
            pane.unread += 1;
        }
    }

    /// Fold a subagent run's event into that run's own transcript, as the
    /// plain conversation event it is.
    ///
    /// The `SubagentRun*` events are the same four things a main turn emits
    /// (a message, a tool call, its result, images) with a run id bolted on so
    /// concurrent runs do not interleave. Translating them back rather than
    /// folding them by hand is what keeps a pane's rendering identical to the
    /// main chat's: there is one reducer, not two.
    fn pane_event(&mut self, run: u64, event: AgentEvent) {
        self.pane_write(run, true, |view| view.apply(&event));
    }

    /// The pane the user is inside, if any.
    pub fn attached_pane(&self) -> Option<&SubagentPane> {
        self.attached.and_then(|index| self.panes.get(index))
    }

    /// Number of runs still going.
    ///
    /// Not "the count shown on the rail header", which is what this claimed:
    /// there is no rail header — `draw_rail` paints rows and nothing else, and
    /// the status bar's running count comes from `status.background_subagents`,
    /// which the event handler keeps by hand. Nothing calls this.
    pub fn running_panes(&self) -> usize {
        self.panes
            .iter()
            .filter(|pane| pane.status == PaneStatus::Running)
            .count()
    }

    /// Move the rail selection by `delta`, clamped at both ends. Moving up off
    /// the top row returns focus to the composer, which is what makes ↑/↓ feel
    /// continuous between the two.
    fn rail_select(&mut self, delta: isize) {
        let Some(current) = self.rail_focus else {
            return;
        };
        let next = current as isize + delta;
        if next < 0 {
            self.rail_focus = None;
            return;
        }
        self.rail_focus = Some((next as usize).min(self.panes.len().saturating_sub(1)));
    }

    /// Give the rail keyboard focus, selecting the first running pane if there
    /// is one (that is the one you almost always want) and the last pane
    /// otherwise. No-op when nothing has been delegated yet.
    pub fn focus_rail(&mut self) -> bool {
        if self.panes.is_empty() {
            return false;
        }
        let target = self
            .panes
            .iter()
            .position(|pane| pane.status == PaneStatus::Running)
            .unwrap_or(self.panes.len() - 1);
        self.rail_focus = Some(target);
        true
    }

    /// Open a pane as the main chat view: its transcript takes over the
    /// screen until Esc. Clears the unread badge — you are looking at it now.
    /// Starts following the live tail so opening a running agent shows the
    /// newest work rather than whatever offset it last held.
    pub fn attach_pane(&mut self, index: usize) {
        let Some(pane) = self.panes.get_mut(index) else {
            return;
        };
        pane.unread = 0;
        pane.transcript.scroll_to_bottom();
        self.attached = Some(index);
        self.rail_focus = Some(index);
    }

    /// Attach the pane `delta` rows away from `index`, wrapping around the
    /// rail so ↓ always lands on another run and the browse never dead-ends at
    /// the last one.
    ///
    /// With a single run there is nowhere to step, so ↑/↓ fall back to their
    /// other job and scroll the pane you are reading.
    fn step_pane(&mut self, index: usize, delta: isize) {
        let len = self.panes.len();
        if len < 2 {
            self.scroll_pane(index, if delta < 0 { 1 } else { -1 });
            return;
        }
        let next = (index as isize + delta).rem_euclid(len as isize) as usize;
        self.attach_pane(next);
    }

    /// Leave the attached pane and go all the way back to the main chat, with
    /// focus in the composer — one Esc, and you are typing again. (Leaving
    /// focus parked on the rail meant a second Esc to actually get out, which
    /// is one too many for the way back.)
    pub fn detach_pane(&mut self) {
        if let Some(index) = self.attached.take()
            && let Some(pane) = self.panes.get_mut(index)
        {
            pane.unread = 0;
        }
        self.rail_focus = None;
        // A run that finished while you were watching it has been sitting on
        // the rail with its linger clock stopped; let it retire now.
        self.retire_finished_panes();
    }

    /// Scroll the pane at `index` by `delta` lines, per [`scroll_step`].
    fn scroll_pane(&mut self, index: usize, delta: i16) {
        if let Some(pane) = self.panes.get_mut(index) {
            pane.transcript.scroll_by(delta);
        }
    }

    /// Jump a pane (or the main transcript when no pane is attached) to the
    /// live tail and re-enable stick-to-bottom.
    fn scroll_to_bottom(&mut self) {
        match self.attached.and_then(|index| self.panes.get_mut(index)) {
            Some(pane) => pane.transcript.scroll_to_bottom(),
            None => self.transcript.scroll_to_bottom(),
        }
    }

    /// Scroll the main transcript by `delta` lines, per [`scroll_step`].
    fn scroll_transcript(&mut self, delta: i16) {
        self.transcript.scroll_by(delta);
    }

    /// Close out every pane still marked running, because the turn that owned
    /// them was killed outright rather than asked to stop.
    ///
    /// A run's pane is closed by the `SubagentRunDone` its own loop emits. Abort
    /// the turn's task and that loop is dropped mid-poll, so the event never
    /// comes: the pane keeps `finished: None`, [`App::retire_finished_panes`]
    /// retains it forever (`None => true`), and the rail grows a permanent
    /// pulsing row — one per in-flight run, every time a turn is aborted.
    ///
    /// The cooperative path ([`CancelHandle`](crate::agent::CancelHandle)) does not need this: every loop
    /// closes its own pane on the way out. This is for the fallback that does
    /// not give them the chance.
    pub fn fail_running_panes(&mut self, why: &str) {
        let now = Instant::now();
        for pane in &mut self.panes {
            if pane.status != PaneStatus::Running {
                continue;
            }
            pane.status = PaneStatus::Failed;
            pane.finished = Some(now);
            pane.transcript.notice(format!("failed: {why}"));
        }
    }

    /// Put the UI back to an idle, usable state after a turn ended without
    /// ever saying so.
    ///
    /// The ordinary end of a turn is [`AgentEvent::Done`], and everything the
    /// spinner, the rail and the composer need to know is folded out of it.
    /// Two paths never produce one: a turn task aborted after the cooperative
    /// interrupt ran out of patience, and a turn task that died where nothing
    /// was watching. Each of those leaves a different piece of the surface
    /// lying: the status bar spins over a turn that has ended, the rail keeps
    /// a pulsing row for every subagent that was in flight (`/ultra` leaves
    /// several), and the composer keeps typing into the stdin of a command
    /// whose console is gone — which is what makes Enter appear to do nothing
    /// at all.
    ///
    /// `why` is what the abandoned subagent panes say about themselves, so it
    /// should name the event from the user's side ("interrupted"), not the
    /// mechanism.
    ///
    /// Deliberately *not* touching [`App::message_queue`]: whether prompts
    /// typed behind the turn should survive depends on why the turn ended, and
    /// only the caller knows that.
    pub(super) fn end_turn_abruptly(&mut self, why: &str) {
        self.transcript.commit();
        self.fail_running_panes(why);
        self.close_stale_console();
        self.status.busy = false;
        self.status.step = 0;
        self.turn_started = None;
    }

    /// Drop finished runs off the rail once they have been resting long enough
    /// to notice, so the rail shows live work instead of accumulating every
    /// subagent the session ever ran.
    ///
    /// Nothing is lost: a foreground run's report is the output of its
    /// `spawn_subagent` card in the main chat, a background run's report is
    /// written back into that same card when it lands (see
    /// [`App::record_subagent_report`]), and an `/ultra` candidate's draft is in
    /// the collapsed guidance card that phase pushes
    /// ([`AgentEvent::UltraGuidance`]).
    ///
    /// The pane you are *inside* never retires under you — its clock starts
    /// when you leave it.
    pub fn retire_finished_panes(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        // Selections are indices, and retiring shifts them — remember what they
        // point *at*, then re-find it afterwards.
        let attached_run = self.attached.and_then(|i| self.panes.get(i)).map(|p| p.run);
        let focus_run = self
            .rail_focus
            .and_then(|i| self.panes.get(i))
            .map(|p| p.run);

        let now = Instant::now();
        let before = self.panes.len();
        self.panes.retain(|pane| match pane.finished {
            _ if Some(pane.run) == attached_run => true,
            Some(at) => now.duration_since(at) < PANE_LINGER,
            None => true,
        });
        if self.panes.len() == before {
            return;
        }

        self.attached = attached_run.and_then(|run| self.pane_index(run));
        // If the run the rail was sitting on just retired, focus falls back to
        // the composer rather than silently jumping to some other subagent.
        self.rail_focus = focus_run.and_then(|run| self.pane_index(run));
    }

    /// Write a finished background run's report into the `spawn_subagent` card
    /// that launched it, replacing the "delegated, running in the background"
    /// placeholder. The card is the durable record of the run once its pane
    /// retires off the rail.
    fn record_subagent_report(&mut self, name: &str, task: &str, report: &str, is_error: bool) {
        self.transcript.amend_tool(
            |tool| {
                tool.name == "spawn_subagent"
                    && tool.args.get("subagent").and_then(Value::as_str) == Some(name)
                    && tool.args.get("task").and_then(Value::as_str) == Some(task)
            },
            ToolItemOutput {
                content: report.to_string(),
                is_error,
            },
        );
    }

    /// Kill the selected run. Only background runs can be killed — a
    /// foreground run has the parent turn blocked on it, so the way to stop it
    /// is to interrupt the turn (Ctrl-C).
    fn kill_pane(&mut self, index: usize) {
        let Some(pane) = self.panes.get(index) else {
            return;
        };
        let (name, bg) = (pane.name.clone(), pane.bg);
        let Some(bg) = bg else {
            self.notice(format!(
                "subagent '{name}' is running in the foreground — Ctrl-C interrupts the turn it \
                 is blocking"
            ));
            return;
        };
        let Some(registry) = self.subagents.clone() else {
            return;
        };
        if registry.kill(bg) {
            // Aborting the driver task means the run emits no closing event of
            // its own, so retire the pane here.
            if let Some(pane) = self.panes.get_mut(index) {
                pane.status = PaneStatus::Failed;
                pane.finished = Some(Instant::now());
                pane.transcript.notice("killed on request".to_string());
            }
            self.notice(format!("killed subagent '{name}' (#{bg})"));
        } else {
            self.notice(format!("subagent '{name}' (#{bg}) already finished"));
        }
    }

    /// Build this session's heartbeat record from current state.
    pub fn session_record(&self) -> SessionRecord {
        SessionRecord {
            id: self.session_id.clone(),
            name: self.session_name.clone(),
            cwd: self.project_root.display().to_string(),
            model: self.status.model.clone(),
            mode: self.mode().to_string(),
            state: self.session_state(),
            activity: self.session_activity(),
            pid: std::process::id(),
            started_unix: self.session_started_unix,
            updated_unix: 0, // stamped by session_registry::write
        }
    }

    /// Reload the live-session list from the registry, keeping the selection
    /// in range. Cheap (a few small files); safe to poll. The peek panel is
    /// refreshed separately on a slower cadence — see [`App::refresh_peek`].
    pub fn refresh_sessions(&mut self) {
        self.sessions = session_registry::list();
        if self.dashboard_selected >= self.sessions.len() {
            self.dashboard_selected = self.sessions.len().saturating_sub(1);
        }
    }

    /// Reload the peek panel with the selected session's recent transcript.
    /// Reads only the tail of the session file, so it is cheap enough to call
    /// on selection changes and a ~1s poll, but not every frame.
    pub fn refresh_peek(&mut self) {
        self.peek_lines = match self.sessions.get(self.dashboard_selected) {
            Some(session) => crate::agent::session::peek(&session.id, 50),
            None => Vec::new(),
        };
    }

    /// Spawn a detached background session for `prompt`: a headless sovereign
    /// `wizard --bg` run that registers in the session registry, so it shows up
    /// in every dashboard on the machine and survives this session exiting.
    fn dispatch_session(&mut self, prompt: String) {
        use std::os::unix::process::CommandExt;
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.notice(format!("could not locate the wizard binary: {err}"));
                return;
            }
        };
        let spawned = std::process::Command::new(exe)
            .arg("--bg")
            .arg("--mode")
            .arg("sovereign")
            .arg("-p")
            .arg(&prompt)
            .arg("--cwd")
            .arg(&self.project_root)
            .current_dir(&self.project_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Own process group: detached from the TUI's job control, and
            // killable as a group when stopped.
            .process_group(0)
            .spawn();
        match spawned {
            Ok(_) => {
                self.notice(format!("dispatched background session: {prompt}"));
                self.refresh_sessions();
            }
            Err(err) => self.notice(format!("dispatch failed: {err}")),
        }
    }

    /// Stop the selected background session (Ctrl-X): SIGTERM its process group
    /// and drop its registry row. Refuses to stop the session you're in.
    fn stop_selected_session(&mut self) {
        let Some(session) = self.sessions.get(self.dashboard_selected) else {
            return;
        };
        if session.id == self.session_id {
            self.notice("that's this session — use /quit to leave it");
            return;
        }
        let (id, name, pid) = (session.id.clone(), session.name.clone(), session.pid as i32);
        // Signal the whole group (dispatched sessions are group leaders, so
        // their tool subprocesses die too); fall back to the bare pid.
        unsafe {
            if libc::kill(-pid, libc::SIGTERM) != 0 {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        session_registry::remove(&id);
        self.notice(format!("stopped session: {name}"));
        self.refresh_sessions();
    }

    /// Move the dashboard selection up/down, clamped to the session list.
    fn dashboard_select(&mut self, delta: isize) {
        let len = self.sessions.len();
        if len == 0 {
            self.dashboard_selected = 0;
            return;
        }
        let last = len - 1;
        self.dashboard_selected = match delta {
            d if d < 0 => self.dashboard_selected.checked_sub(1).unwrap_or(last),
            _ if self.dashboard_selected >= last => 0,
            _ => self.dashboard_selected + 1,
        };
        self.refresh_peek();
    }

    /// Recompute [`InputMode`] from the input text, then refresh the command
    /// suggestions.
    fn sync_input_mode(&mut self) {
        // While answering an inline prompt the composer stays in Prompt mode no
        // matter what is typed (a key never flips it to Command/Chat), and the
        // suggestion popup is suppressed.
        if self.prompt.is_some() || self.web_key_backend.is_some() {
            return;
        }
        // A console owns the composer the same way: what is typed goes to a
        // child process verbatim, so `/usr/local` must not open a command
        // popup that Enter would then be read as completing.
        if self.console.is_some() {
            self.input_mode = InputMode::Chat;
            self.suggestions.clear();
            self.suggestion_index = 0;
            return;
        }
        self.input_mode = if self.input.trim_start().starts_with('/') {
            InputMode::Command
        } else {
            InputMode::Chat
        };
        // Dismissed, and the draft has not moved on: stay shut. Any edit clears
        // the dismissal, so typing another character reopens the popup.
        if self.dismissed_suggestions_for.as_deref() == Some(self.input.as_str()) {
            self.suggestions.clear();
            self.suggestion_index = 0;
            return;
        }
        self.dismissed_suggestions_for = None;
        self.refresh_suggestions();
    }

    /// Rebuild the suggestion list from the typed `/command` prefix.
    /// Prefix matches rank above substring matches; suggestions disappear
    /// once arguments are being typed.
    fn refresh_suggestions(&mut self) {
        // Remember an actively moved highlight (off the top row) so it does
        // not jump identity when the list is rebuilt; the default highlight
        // must keep tracking the best match.
        let previous = if self.suggestion_index > 0 {
            self.suggestions
                .get(self.suggestion_index)
                .map(|spec| spec.name.clone())
        } else {
            None
        };
        self.suggestions.clear();
        if self.input_mode != InputMode::Command || self.picker.is_some() {
            self.suggestion_index = 0;
            return;
        }
        let Some(token) = self.input.trim_start().strip_prefix('/') else {
            self.suggestion_index = 0;
            return;
        };
        if token.contains(char::is_whitespace) {
            self.suggestion_index = 0;
            return;
        }
        // Builtins in display order, then custom commands (already sorted).
        let candidates: Vec<Suggestion> = COMMANDS
            .iter()
            .map(Suggestion::from)
            .chain(self.custom_commands.iter().map(Suggestion::from))
            .collect();
        // Rank: exact match, then prefix matches, then substring matches.
        self.suggestions
            .extend(candidates.iter().filter(|spec| spec.name == token).cloned());
        self.suggestions.extend(
            candidates
                .iter()
                .filter(|spec| spec.name != token && spec.name.starts_with(token))
                .cloned(),
        );
        self.suggestions.extend(
            candidates
                .iter()
                .filter(|spec| !spec.name.starts_with(token) && spec.name.contains(token))
                .cloned(),
        );
        self.suggestion_index = previous
            .and_then(|name| self.suggestions.iter().position(|spec| spec.name == name))
            .unwrap_or(0);
    }

    /// Replace the input with the highlighted suggestion. Returns the
    /// completed suggestion, or `None` when nothing is highlighted.
    fn accept_suggestion(&mut self) -> Option<Suggestion> {
        let spec = self.suggestions.get(self.suggestion_index)?.clone();
        let mut text = format!("/{}", spec.name);
        if spec.takes_args {
            text.push(' ');
        }
        self.set_input(text);
        Some(spec)
    }

    // --- input editing (cursor is a character index into `input`) ---

    /// Byte offset of the cursor in `input`.
    fn byte_index(&self) -> usize {
        self.char_byte(self.cursor)
    }

    /// Character indices of the start and end of the line the caret is on.
    ///
    /// The composer is a real multi-line editor — Alt+Enter inserts a hard
    /// break — so "the line" and "the buffer" are different things, and the
    /// readline chords below mean the line. They used to mean the buffer, which
    /// on a three-line draft made Ctrl-K delete two lines the caret was not on
    /// and Ctrl-U delete every line above it. Nothing restores that: vim's undo
    /// does not cover readline chords, and vim is off by default.
    ///
    /// The end is the index of the newline (or the buffer end), so it is where
    /// `$`-style "end of line" should put the caret.
    fn line_bounds(&self) -> (usize, usize) {
        let chars: Vec<char> = self.input.chars().collect();
        let start = chars[..self.cursor.min(chars.len())]
            .iter()
            .rposition(|c| *c == '\n')
            .map(|at| at + 1)
            .unwrap_or(0);
        let end = chars[self.cursor.min(chars.len())..]
            .iter()
            .position(|c| *c == '\n')
            .map(|at| self.cursor.min(chars.len()) + at)
            .unwrap_or(chars.len());
        (start, end)
    }

    /// Byte offset of character index `n` in `input` (its end when out of
    /// range).
    fn char_byte(&self, n: usize) -> usize {
        self.input
            .char_indices()
            .nth(n)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    fn set_input(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.input = text;
        // History recall goes through here, and in Normal mode a caret one past
        // the last character is not a place vim ever leaves you: the block sat
        // in the empty cell after the text and the first `x` deleted nothing,
        // moving the block instead. Vim lands *on* a character after a recall,
        // and the first `x` deletes it.
        self.clamp_normal_cursor();
        self.sync_input_mode();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_browse = None;
        // Staged attachments belong to the composer's contents; emptying it
        // drops them too, so a cancelled draft never carries a ghost image
        // into the next submit.
        self.pending_images.clear();
        self.sync_input_mode();
    }

    /// Replace the composer with text edited externally (Ctrl-G). Editors
    /// append a trailing newline, so at most one line ending is trimmed; the
    /// cursor lands at the end.
    fn set_input_from_editor(&mut self, mut text: String) {
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        self.history_browse = None;
        self.set_input(text);
    }

    fn insert_char(&mut self, c: char) {
        let index = self.byte_index();
        self.input.insert(index, c);
        self.cursor += 1;
    }

    /// Insert a hard line break at the cursor (Shift/Alt+Enter). The composer
    /// grows to a multi-line box; Enter alone still submits.
    fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    fn insert_str(&mut self, text: &str) {
        let index = self.byte_index();
        self.input.insert_str(index, text);
        self.cursor += text.chars().count();
    }

    fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // A pasted image is one unit in the composer (`[Image #N]`). Backspace
        // on/inside that token removes the whole attachment, not one glyph.
        if self.try_delete_image_token_back() {
            return;
        }
        self.cursor -= 1;
        let index = self.byte_index();
        self.input.remove(index);
    }

    fn delete_forward(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        if self.try_delete_image_token_forward() {
            return;
        }
        let index = self.byte_index();
        self.input.remove(index);
    }

    /// Delete the word before the cursor (Ctrl-W).
    fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        while self.cursor > 0 && chars[self.cursor - 1].is_whitespace() {
            self.delete_back();
        }
        // Image tokens are atomic words: Ctrl-W on one unstages the image.
        if self.try_delete_image_token_back() {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut end = self.cursor;
        while end > 0 && !chars[end - 1].is_whitespace() {
            end -= 1;
        }
        while self.cursor > end {
            self.delete_back();
        }
    }

    /// If the character before the cursor sits inside an `[Image #N]` token,
    /// remove that whole token and unstage the matching attachment.
    fn try_delete_image_token_back(&mut self) -> bool {
        let Some((start, end, n)) = image_token_at(&self.input, self.cursor.saturating_sub(1))
        else {
            return false;
        };
        self.remove_image_token_range(start, end, n);
        true
    }

    /// If the character under the cursor sits inside an `[Image #N]` token,
    /// remove that whole token and unstage the matching attachment.
    fn try_delete_image_token_forward(&mut self) -> bool {
        let Some((start, end, n)) = image_token_at(&self.input, self.cursor) else {
            return false;
        };
        self.remove_image_token_range(start, end, n);
        true
    }

    /// Drop the `[Image #N]` span `[start, end)` (char indices), unstage image
    /// `N`, and renumber any higher tokens so the composer stays consistent.
    fn remove_image_token_range(&mut self, start: usize, end: usize, n: usize) {
        let byte_start = self.char_byte(start);
        let byte_end = self.char_byte(end);
        self.input.replace_range(byte_start..byte_end, "");
        self.cursor = start;

        if n >= 1 && n <= self.pending_images.len() {
            self.pending_images.remove(n - 1);
        }
        // Keep remaining tokens contiguous: `[Image #k]` for k > n becomes
        // `[Image #(k-1)]`, matching the new `pending_images` indices.
        if n >= 1 {
            renumber_image_tokens_after(&mut self.input, n);
            // Cursor was left at `start`; renumbering only rewrites tokens to
            // the right (or same position after a shorter/equal replacement),
            // so the char index stays valid for digits shrinking by one.
            let len = self.input.chars().count();
            if self.cursor > len {
                self.cursor = len;
            }
        }
    }

    // --- vim modal editing ---

    /// `/vim`: toggle modal editing, persist the choice to `[ui] vim`, and
    /// reset to Insert so typing works immediately when enabling.
    pub fn toggle_vim(&mut self) {
        let on = !self.vim.enabled;
        self.vim = VimState {
            enabled: on,
            mode: VimMode::Insert,
            ..VimState::default()
        };
        self.config.ui.vim = on;
        if let Err(err) = self.config.save() {
            self.notice(format!("could not save config: {err:#}"));
        }
        self.notice(if on {
            "vim mode on — Esc for NORMAL (hjkl/w/b/e move · i/a/I/A insert · \
             x/dd/dw/cw edit · u undo), i to type. /vim to leave"
        } else {
            "vim mode off"
        });
    }

    /// Enter Insert mode (text entry resumes).
    fn enter_insert(&mut self) {
        self.vim.mode = VimMode::Insert;
        self.vim.clear_pending();
    }

    /// Leave Insert for Normal mode. Vim nudges the cursor one cell left so it
    /// sits on the last typed character rather than past it.
    fn enter_normal_mode(&mut self) {
        self.vim.mode = VimMode::Normal;
        self.vim.clear_pending();
        self.cursor = self.cursor.saturating_sub(1);
        self.clamp_normal_cursor();
    }

    /// In Normal mode the cursor sits *on* a character, never past the last
    /// one (an empty line keeps it at 0).
    fn clamp_normal_cursor(&mut self) {
        if self.vim.mode != VimMode::Normal {
            return;
        }
        let len = self.input.chars().count();
        self.cursor = if len == 0 {
            0
        } else {
            self.cursor.min(len - 1)
        };
    }

    /// Snapshot the line for `u` before a Normal-mode edit.
    fn vim_snapshot(&mut self) {
        let cursor = self.cursor;
        self.vim.push_undo(&self.input, cursor);
    }

    /// Drop the most recent undo snapshot back into the line (`u`).
    fn vim_undo(&mut self) {
        if let Some((input, cursor)) = self.vim.undo.pop() {
            self.input = input;
            self.cursor = cursor;
            self.clamp_normal_cursor();
        }
    }

    /// Remove characters `[start, end)` and return them; leaves the cursor at
    /// `start`. Used by `x`/`D` and the `d`/`c` operators.
    fn vim_delete_range(&mut self, start: usize, end: usize) -> String {
        let len = self.input.chars().count();
        let start = start.min(len);
        let end = end.min(len);
        if start >= end {
            return String::new();
        }
        let bstart = self.char_byte(start);
        let bend = self.char_byte(end);
        let removed = self.input[bstart..bend].to_string();
        self.input.replace_range(bstart..bend, "");
        self.cursor = start;
        removed
    }

    /// Replace the character under the cursor with `c` (`r`).
    fn vim_replace_char(&mut self, c: char) {
        let len = self.input.chars().count();
        if self.cursor >= len {
            return;
        }
        self.vim_snapshot();
        let idx = self.byte_index();
        self.input.remove(idx);
        self.input.insert(idx, c);
    }

    /// Apply an operator over the character range `[start, end)` (`dw`, `c$`,
    /// `ye`, …). Delete/Change stash the text in the register; Change then
    /// enters Insert. Yank only copies.
    fn vim_apply_op(&mut self, op: VimOp, start: usize, end: usize) {
        let (start, end) = (start.min(end), start.max(end));
        if start >= end {
            return;
        }
        match op {
            VimOp::Yank => {
                let bstart = self.char_byte(start);
                let bend = self.char_byte(end);
                self.vim.register = self.input[bstart..bend].to_string();
            }
            VimOp::Delete => {
                self.vim_snapshot();
                self.vim.register = self.vim_delete_range(start, end);
                self.clamp_normal_cursor();
            }
            VimOp::Change => {
                self.vim_snapshot();
                self.vim.register = self.vim_delete_range(start, end);
                self.enter_insert();
            }
        }
    }

    /// Linewise operator (`dd`/`cc`/`yy`): the whole single-line buffer.
    fn vim_apply_linewise(&mut self, op: VimOp) {
        match op {
            VimOp::Yank => self.vim.register = self.input.clone(),
            VimOp::Delete => {
                self.vim_snapshot();
                self.vim.register = std::mem::take(&mut self.input);
                self.cursor = 0;
            }
            VimOp::Change => {
                self.vim_snapshot();
                self.vim.register = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.enter_insert();
            }
        }
    }

    /// Paste the register `n` times, after the cursor (`p`) or before it
    /// (`P`); the cursor lands on the last pasted character.
    fn vim_paste(&mut self, after: bool, n: usize) {
        if self.vim.register.is_empty() {
            return;
        }
        self.vim_snapshot();
        let text = self.vim.register.repeat(n.max(1));
        let len = self.input.chars().count();
        let at = if after && len > 0 {
            (self.cursor + 1).min(len)
        } else {
            self.cursor
        };
        let byte = self.char_byte(at);
        self.input.insert_str(byte, &text);
        self.cursor = at + text.chars().count().saturating_sub(1);
        self.clamp_normal_cursor();
    }

    /// Handle one key in Normal mode. Returns an [`AppAction`] only for keys
    /// that submit (Enter); everything else mutates composer state in place.
    fn handle_vim_normal(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        let mut action = None;
        let printable = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        'dispatch: {
            // `r` armed: the next printable key replaces the char under cursor.
            if self.vim.pending == Some(Pending::Replace) {
                self.vim.pending = None;
                if let (KeyCode::Char(c), true) = (key.code, printable) {
                    self.vim_replace_char(c);
                }
                break 'dispatch;
            }

            // Count prefix: digits accumulate (a leading `0` is the motion, not
            // a count).
            if let KeyCode::Char(c @ '0'..='9') = key.code
                && printable
                && !(c == '0' && self.vim.count.is_none())
            {
                let digit = c as usize - '0' as usize;
                let next = self.vim.count.unwrap_or(0).saturating_mul(10) + digit;
                self.vim.count = Some(next.min(100_000));
                break 'dispatch;
            }

            // An operator is pending: read this key as its motion, or as the
            // linewise form when the operator key repeats (`dd`/`cc`/`yy`).
            if let Some(Pending::Operator(op)) = self.vim.pending {
                self.vim.pending = None;
                // Vim multiplies the operator's count by the motion's.
                let n = self.vim.operator_count.take().unwrap_or(1)
                    * self.vim.count.take().unwrap_or(1);
                let repeated = matches!(
                    (op, key.code),
                    (VimOp::Delete, KeyCode::Char('d'))
                        | (VimOp::Change, KeyCode::Char('c'))
                        | (VimOp::Yank, KeyCode::Char('y'))
                );
                if repeated {
                    self.vim_apply_linewise(op);
                } else if let KeyCode::Char(motion) = key.code {
                    let chars: Vec<char> = self.input.chars().collect();
                    if let Some(m) = vim::resolve_motion(motion, n, &chars, self.cursor) {
                        // `cw` is `ce`, not `dw`. Vim's own special case: a
                        // change stops at the end of the word and leaves the
                        // whitespace after it, because you are about to type a
                        // replacement word and would otherwise have to retype
                        // the space. `dw` takes the space and is already right.
                        //
                        // Both were handed the identical `w` range, so `cw` on
                        // "foo bar baz" gave "bar baz" where vim gives
                        // " bar baz". `/vim`'s own notice advertises `cw`.
                        let end = match (op, motion) {
                            (VimOp::Change, 'w' | 'W') => {
                                let mut end = m.end;
                                while end > m.start
                                    && chars.get(end - 1).is_some_and(|c| c.is_whitespace())
                                {
                                    end -= 1;
                                }
                                end
                            }
                            _ => m.end,
                        };
                        self.vim_apply_op(op, m.start, end);
                    }
                }
                break 'dispatch;
            }

            let len = self.input.chars().count();
            match key.code {
                // --- motions ---
                KeyCode::Char('h') | KeyCode::Left => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.cursor = self.cursor.saturating_sub(n);
                }
                KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.cursor = (self.cursor + n).min(len);
                    self.clamp_normal_cursor();
                }
                // The bare motions duplicate what `vim::resolve_motion` does
                // for the operator forms, so they have to agree with it: these
                // three are line-scoped there and were buffer-scoped here,
                // which is how `0` on line two reached the start of line one.
                KeyCode::Char('0') => {
                    self.vim.count = None;
                    self.cursor = self.line_bounds().0;
                }
                KeyCode::Char('^') => {
                    self.vim.count = None;
                    let chars: Vec<char> = self.input.chars().collect();
                    let (start, end) = self.line_bounds();
                    self.cursor = start + vim::first_non_blank(&chars[start..end]);
                }
                KeyCode::Char('$') => {
                    self.vim.count = None;
                    // *On* the last character of the line, which is where vim
                    // leaves you — not on the newline after it.
                    // `clamp_normal_cursor` only knows about the buffer's end,
                    // so it cannot do this for an inner line.
                    let (start, end) = self.line_bounds();
                    self.cursor = if end > start { end - 1 } else { start };
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('w') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let chars: Vec<char> = self.input.chars().collect();
                    for _ in 0..n {
                        self.cursor = vim::word_forward(&chars, self.cursor);
                    }
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('b') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let chars: Vec<char> = self.input.chars().collect();
                    for _ in 0..n {
                        self.cursor = vim::word_back(&chars, self.cursor);
                    }
                }
                KeyCode::Char('e') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let chars: Vec<char> = self.input.chars().collect();
                    for _ in 0..n {
                        self.cursor = vim::word_end(&chars, self.cursor);
                    }
                    self.clamp_normal_cursor();
                }
                // Single-line analog of j/k: walk the input history.
                KeyCode::Char('k') | KeyCode::Up => {
                    self.vim.count = None;
                    self.history_prev();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.vim.count = None;
                    if self.history_browse.is_some() {
                        self.history_next();
                    } else {
                        // Not browsing history: like plain ↓, drop into the
                        // subagent rail when there is one.
                        self.focus_rail();
                    }
                }

                // --- insert transitions ---
                KeyCode::Char('i') => self.enter_insert(),
                KeyCode::Char('I') => {
                    let chars: Vec<char> = self.input.chars().collect();
                    self.cursor = vim::first_non_blank(&chars);
                    self.enter_insert();
                }
                KeyCode::Char('a') => {
                    self.cursor = (self.cursor + 1).min(len);
                    self.enter_insert();
                }
                KeyCode::Char('A') => {
                    self.cursor = len;
                    self.enter_insert();
                }
                // Single-line: `o`/`O` have no new line to open, so they map to
                // append-at-end / insert-at-start.
                KeyCode::Char('o') => {
                    self.cursor = len;
                    self.enter_insert();
                }
                KeyCode::Char('O') => {
                    self.cursor = 0;
                    self.enter_insert();
                }

                // --- edits ---
                KeyCode::Char('x') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(self.cursor, self.cursor + n);
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('X') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let start = self.cursor.saturating_sub(n);
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(start, self.cursor);
                }
                // To the end of the *line*, not the end of the draft — both of
                // them deleted every line below the caret on a multi-line
                // composer. See the module header in `vim.rs` for which
                // commands are line-aware and which are not.
                KeyCode::Char('D') => {
                    self.vim.count = None;
                    self.vim_snapshot();
                    let end = self.line_bounds().1;
                    self.vim.register = self.vim_delete_range(self.cursor, end);
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('C') => {
                    self.vim.count = None;
                    self.vim_snapshot();
                    let end = self.line_bounds().1;
                    self.vim.register = self.vim_delete_range(self.cursor, end);
                    self.enter_insert();
                }
                KeyCode::Char('s') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(self.cursor, self.cursor + n);
                    self.enter_insert();
                }
                KeyCode::Char('S') => {
                    self.vim.count = None;
                    self.vim_apply_linewise(VimOp::Change);
                }
                KeyCode::Char('r') => self.vim.pending = Some(Pending::Replace),

                // --- operators (await a motion) ---
                //
                // The count typed *before* the operator is set aside here, so
                // the digits of the motion's own count start from nothing.
                // Sharing one field made them concatenate: `2d3w` read as 23
                // words rather than 2 x 3.
                KeyCode::Char('d') => {
                    self.vim.operator_count = self.vim.count.take();
                    self.vim.pending = Some(Pending::Operator(VimOp::Delete));
                }
                KeyCode::Char('c') => {
                    self.vim.operator_count = self.vim.count.take();
                    self.vim.pending = Some(Pending::Operator(VimOp::Change));
                }
                KeyCode::Char('y') => {
                    self.vim.operator_count = self.vim.count.take();
                    self.vim.pending = Some(Pending::Operator(VimOp::Yank));
                }

                // --- paste / undo ---
                KeyCode::Char('p') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_paste(true, n);
                }
                KeyCode::Char('P') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_paste(false, n);
                }
                KeyCode::Char('u') => {
                    self.vim.count = None;
                    self.vim_undo();
                }

                // --- still-useful editing keys in Normal mode ---
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    self.vim.count = None;
                    self.insert_newline();
                }
                KeyCode::Enter => {
                    self.vim.count = None;
                    action = self.submit();
                }
                KeyCode::Backspace => {
                    self.vim.count = None;
                    self.cursor = self.cursor.saturating_sub(1);
                }
                // Esc keeps wizard's escape hatches (close diff, dismiss
                // todos, reset scroll) since Normal-mode Esc would otherwise
                // be a no-op.
                KeyCode::Esc => {
                    self.vim.clear_pending();
                    if self.diff.take().is_some() {
                    } else if !self.suggestions.is_empty() {
                        // The same arm Insert mode has. Without it the popup
                        // was unreachable from Normal mode: Escape did not
                        // close it, Tab fell through to the catch-all below,
                        // and Up/Down are bound to history here — so the only
                        // way out was to edit the text, while the status bar
                        // read "↑↓ select · Tab complete · Enter run".
                        self.dismissed_suggestions_for = Some(self.input.clone());
                        self.suggestions.clear();
                        self.suggestion_index = 0;
                    } else if self.show_todos {
                        // Then the todo band (it auto-opens on the first
                        // todo update, so it needs a way out that isn't
                        // `/todos`).
                        self.show_todos = false;
                    } else if !self.transcript.follow {
                        self.scroll_to_bottom();
                    }
                }
                // Completing is not a motion, and Normal mode is still where
                // somebody who typed `/st` and hit Escape is standing. Without
                // this Tab fell into the catch-all and cleared the count.
                KeyCode::Tab if !self.suggestions.is_empty() => {
                    self.accept_suggestion();
                    self.enter_insert();
                }
                KeyCode::PageUp => self.scroll_transcript(10),
                KeyCode::PageDown => self.scroll_transcript(-10),
                _ => self.vim.count = None,
            }
        }
        self.sync_input_mode();
        Ok(action)
    }

    // --- input history (↑/↓ recall, shell-style) ---

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let position = match &self.history_browse {
            None => {
                self.history_browse = Some(HistoryBrowse {
                    position: self.history.len() - 1,
                    draft: self.input.clone(),
                });
                self.history.len() - 1
            }
            Some(browse) if browse.position == 0 => return,
            Some(browse) => browse.position - 1,
        };
        self.set_input(self.history[position].clone());
        if let Some(browse) = self.history_browse.as_mut() {
            browse.position = position;
        }
    }

    fn history_next(&mut self) {
        let Some(browse) = self.history_browse.as_mut() else {
            return;
        };
        if browse.position + 1 < self.history.len() {
            browse.position += 1;
            let entry = self.history[browse.position].clone();
            self.set_input(entry);
            return;
        }
        // Past the newest entry: back to whatever was being composed.
        let draft = std::mem::take(&mut browse.draft);
        self.history_browse = None;
        self.set_input(draft);
    }

    /// Toggle the expansion of the most recent finished tool card (Ctrl-T).
    fn toggle_last_tool_card(&mut self) {
        self.transcript.toggle_last_tool();
    }

    /// Toggle the tool card whose header line was drawn on screen row `row`
    /// in the last frame (a plain click on it). No-op off-card, on a
    /// still-running card, or while an overlay covers the transcript (the
    /// hit map is empty then — see `card_hits`).
    fn toggle_card_at(&mut self, row: u16) {
        let hit = self
            .card_hits
            .borrow()
            .iter()
            .find(|(y, _)| *y == row)
            .map(|(_, index)| *index);
        // A still-running card has nothing folded away, so clicking it is a
        // no-op rather than a fold with no content behind it.
        if let Some(index) = hit
            && let Some(TranscriptItem::Tool(tool)) = self.transcript.get(index)
            && tool.output.is_some()
        {
            self.transcript.toggle(index);
        }
    }

    /// Dispatch one event from the merged stream. Returns the user action
    /// the main loop must perform (start a turn, run a slash command, ...).
    pub fn handle_event(&mut self, event: Event) -> Result<Option<AppAction>> {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => {
                let cell = (mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scroll_transcript(3);
                        // The content under each cell just moved, so the old
                        // selection no longer maps to it.
                        self.selection = None;
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll_transcript(-3);
                        self.selection = None;
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.selection = Some(Selection {
                            anchor: cell,
                            head: cell,
                            dragging: true,
                        });
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.head = cell;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.head = cell;
                            sel.dragging = false;
                            if sel.is_empty() {
                                // A plain click (no drag) clears any previous
                                // selection, and on a tool-card header line
                                // toggles that card's output.
                                self.selection = None;
                                self.toggle_card_at(cell.1);
                            } else {
                                // Hand off to the main loop: it owns the
                                // terminal, so it reads the rendered cells and
                                // copies them.
                                return Ok(Some(AppAction::CopySelection));
                            }
                        }
                    }
                    _ => {}
                }
                Ok(None)
            }
            Event::Paste(text) => {
                self.handle_paste(&text);
                Ok(None)
            }
            Event::Resize(_, _) => Ok(None),
            Event::InputClosed(why) => {
                // Nothing can reach this session again, so staying up only
                // produces a window that repaints forever and answers nothing.
                // The notice is recorded before quitting so the reason lands in
                // the transcript on disk, where it can still be read afterwards.
                self.notice(format!("{why} — ending the session"));
                self.should_quit = true;
                Ok(None)
            }
            Event::Tick => {
                self.tick = self.tick.wrapping_add(1);
                // Keep the dashboard's session list current while it's open.
                if self.show_dashboard {
                    // List is cheap (small files); peek reads a transcript tail
                    // so poll it less often.
                    if self.tick.is_multiple_of(4) {
                        self.refresh_sessions();
                    }
                    if self.tick.is_multiple_of(10) {
                        self.refresh_peek();
                    }
                }
                // Age finished runs off the rail (the tick is ~100ms, so this
                // is a cheap retain over a handful of panes).
                self.retire_finished_panes();
                Ok(None)
            }
            Event::Agent(agent_event) => {
                self.handle_agent_event(agent_event);
                Ok(None)
            }
            Event::Notice(message) => {
                self.notice(message);
                Ok(None)
            }
            // Owned by the main loop (it holds the agent slot / config); never
            // reach here.
            Event::AgentRebuilt(_)
            | Event::ProviderActivated(_)
            | Event::McpConnected { .. }
            | Event::ProviderHealthFailed(_)
            | Event::BtwFinished => Ok(None),
        }
    }

    /// Keyboard handling for the current [`InputMode`]. Priority: global
    /// chords, open picker, then line editing.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        if key.kind == KeyEventKind::Release {
            return Ok(None);
        }
        // From here on, a notice is a reply to the user rather than something
        // the session did on its own. See `App::notice`.
        self.key_pressed = true;

        // Any keystroke dismisses a lingering text selection (it was copied on
        // release; the highlight is just a leftover once the user moves on).
        self.selection = None;

        // Any key other than Ctrl-C disarms the "press again to exit" latch.
        let is_ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if !is_ctrl_c {
            self.ctrl_c_armed = false;
        }

        // Global chords, regardless of input mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    // Once interrupts a running turn; pressed again (while
                    // armed) it exits. Idle: first press arms, second exits.
                    if self.ctrl_c_armed {
                        self.should_quit = true;
                        return Ok(None);
                    }
                    self.ctrl_c_armed = true;
                    if self.status.busy {
                        // The cancel handle reaches a parked `execute` from
                        // inside the tool, which kills the command's whole
                        // process group — the same kill the timeout does. Say
                        // what is being stopped, because with a console open
                        // "interrupting" is ambiguous between the two.
                        self.notice(match &self.console {
                            Some(console) => {
                                format!("stopping {}… (Ctrl-C again to exit)", console.command)
                            }
                            None => "interrupting… (Ctrl-C again to exit)".to_string(),
                        });
                        return Ok(Some(AppAction::Interrupt));
                    }
                    self.notice("press Ctrl-C again to exit");
                    return Ok(None);
                }
                KeyCode::Char('d') => {
                    // A running command's console owns Ctrl-D, exactly as a
                    // terminal does: it closes the child's stdin so a program
                    // reading a list of lines learns there are no more. Quitting
                    // Wizard out from under a command the user is mid-answer
                    // with would be the wrong reading of the same key.
                    if let Some(console) = self.console.as_ref() {
                        console.writer.eof();
                        self.notice("stdin closed (Ctrl-D) — the command sees end of input");
                        return Ok(None);
                    }
                    self.should_quit = true;
                    return Ok(None);
                }
                // Ctrl-End jumps the transcript (or attached pane) to the live
                // tail and re-enables stick-to-bottom after reading history
                // during a long stream.
                KeyCode::End => {
                    self.scroll_to_bottom();
                    return Ok(None);
                }
                KeyCode::Char('u') => {
                    // Readline-style: kill from the line start to the cursor.
                    // The *line*, which is what this comment always claimed and
                    // what the code did not do — it drained from the buffer
                    // start, taking every line above the caret with it.
                    let (line_start, _) = self.line_bounds();
                    let from = self.char_byte(line_start);
                    let to = self.byte_index();
                    self.input.drain(from..to);
                    self.cursor = line_start;
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('w') => {
                    self.delete_word_back();
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('a') => {
                    self.cursor = self.line_bounds().0;
                    return Ok(None);
                }
                KeyCode::Char('e') => {
                    self.cursor = self.line_bounds().1;
                    return Ok(None);
                }
                KeyCode::Char('k') => {
                    // To the end of the line, not the end of the draft.
                    let (_, line_end) = self.line_bounds();
                    let from = self.byte_index();
                    let to = self.char_byte(line_end);
                    self.input.drain(from..to);
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('t') => {
                    self.toggle_last_tool_card();
                    return Ok(None);
                }
                // Attach an image from the clipboard — the explicit companion to
                // the empty-paste path, for terminals (or a tmux passthrough)
                // that don't forward an image paste at all. Not while collecting
                // a masked field, where the clipboard would hold a secret.
                KeyCode::Char('v') if !self.prompt_is_masked() => {
                    if !self.attach_clipboard_image() {
                        self.notice("no image on the clipboard to attach");
                    }
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('p') => {
                    // Shortcut for the interactive model picker; ignored
                    // while a turn runs.
                    if self.status.busy {
                        return Ok(None);
                    }
                    return Ok(Some(AppAction::Command(SlashCommand::Model(None))));
                }
                _ => {}
            }
        }

        // The dashboard is modal: ↑/↓ move the selection, typing fills the
        // dispatch input, Enter dispatches a background session, Ctrl-X stops
        // the selected one, Esc clears the input or closes. (Enter will also
        // attach to the selected session once the supervisor lands.)
        if self.show_dashboard {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Up | KeyCode::BackTab => self.dashboard_select(-1),
                KeyCode::Down | KeyCode::Tab => self.dashboard_select(1),
                KeyCode::Char('x') if ctrl => self.stop_selected_session(),
                KeyCode::Enter => {
                    let prompt = self.dashboard_input.trim().to_string();
                    if !prompt.is_empty() {
                        self.dashboard_input.clear();
                        self.dispatch_session(prompt);
                    }
                }
                KeyCode::Backspace => {
                    self.dashboard_input.pop();
                }
                KeyCode::Esc => {
                    if self.dashboard_input.is_empty() {
                        self.show_dashboard = false;
                    } else {
                        self.dashboard_input.clear();
                    }
                }
                KeyCode::Char(c) if !ctrl => self.dashboard_input.push(c),
                _ => {}
            }
            return Ok(None);
        }

        // Inside a subagent's pane. Its transcript has taken over the screen,
        // so navigation keys scroll *it* — but the composer stays live
        // underneath, so anything else falls through to normal typing and you
        // can keep driving the main conversation while you watch.
        if let Some(index) = self.attached {
            // Every navigation key is captured here. Letting an arrow fall
            // through to the composer underneath would scroll the *main*
            // chat's history while the user is plainly looking at a pane —
            // the keys have to belong to what is on screen.
            match key.code {
                KeyCode::Esc => {
                    self.detach_pane();
                    return Ok(None);
                }
                // Plain ↑/↓ keep doing what they did on the rail: walk the
                // subagents. Opening one is not supposed to end the browse —
                // you keep arrowing and each run takes over the screen in
                // turn, wrapping around, so there is never a reason to back
                // out to the rail just to see the next one. j/k join in under
                // vim Normal mode, where letters are motions rather than text.
                KeyCode::Up if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.step_pane(index, -1);
                    return Ok(None);
                }
                KeyCode::Down if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.step_pane(index, 1);
                    return Ok(None);
                }
                KeyCode::Char('k')
                    if self.vim.is_normal()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.step_pane(index, -1);
                    return Ok(None);
                }
                KeyCode::Char('j')
                    if self.vim.is_normal()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.step_pane(index, 1);
                    return Ok(None);
                }
                // Scrolling the run you are reading moves to Shift+↑/↓ (and
                // PageUp/PageDown below).
                KeyCode::Up => {
                    self.scroll_pane(index, 1);
                    return Ok(None);
                }
                KeyCode::Down => {
                    self.scroll_pane(index, -1);
                    return Ok(None);
                }
                KeyCode::PageUp => {
                    self.scroll_pane(index, 10);
                    return Ok(None);
                }
                KeyCode::PageDown => {
                    self.scroll_pane(index, -10);
                    return Ok(None);
                }
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.kill_pane(index);
                    return Ok(None);
                }
                _ => {}
            }
        }

        // The rail has keyboard focus: ↑/↓ move between subagent dots, Enter
        // opens the selected one, Esc drops back to the composer.
        if let Some(index) = self.rail_focus
            && self.attached.is_none()
        {
            let plain = !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            match key.code {
                // Arrows only — no j/k. The rail is a focus you land in from a
                // live text composer, so every letter has to fall through and
                // be typed; binding letters here would eat the first character
                // of "just do X". Vim Normal mode is the exception: letters
                // are motions there, not text, so j/k mirror ↑/↓.
                KeyCode::Up => {
                    self.rail_select(-1);
                    return Ok(None);
                }
                KeyCode::Down => {
                    self.rail_select(1);
                    return Ok(None);
                }
                KeyCode::Char('k') if plain && self.vim.is_normal() => {
                    self.rail_select(-1);
                    return Ok(None);
                }
                KeyCode::Char('j') if plain && self.vim.is_normal() => {
                    self.rail_select(1);
                    return Ok(None);
                }
                KeyCode::Enter => {
                    self.attach_pane(index);
                    return Ok(None);
                }
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.kill_pane(index);
                    return Ok(None);
                }
                KeyCode::Esc => {
                    self.rail_focus = None;
                    return Ok(None);
                }
                // Anything else means the user is done browsing and wants to
                // type: hand focus back to the composer and let the key land
                // there, so you never lose a keystroke to the rail.
                _ => self.rail_focus = None,
            }
        }

        // An open plan review captures all keys: the turn is paused inside
        // exit_plan until a verdict is sent.
        if self.plan_review.is_some() {
            self.handle_plan_review_key(key);
            return Ok(None);
        }

        // An open interview captures all keys: the turn is paused inside the
        // interview tool until the user answers or dismisses it.
        if self.interview.is_some() {
            self.handle_interview_key(key);
            return Ok(None);
        }

        // An open picker captures navigation keys.
        if let Some(picker) = self.picker.as_mut() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                    picker.selected = if picker.selected == 0 {
                        picker.items.len().saturating_sub(1)
                    } else {
                        picker.selected - 1
                    };
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                    picker.selected = if picker.selected + 1 >= picker.items.len() {
                        0
                    } else {
                        picker.selected + 1
                    };
                }
                // Space toggles a checkbox row in a multi-select picker.
                KeyCode::Char(' ')
                    if matches!(
                        picker.kind,
                        PickerKind::ClaudeImport
                            | PickerKind::FusionPanel
                            | PickerKind::UltraLenses
                    ) =>
                {
                    if let Some(item) = picker.items.get_mut(picker.selected) {
                        item.current = !item.current;
                    }
                }
                KeyCode::Esc => {
                    self.picker = None;
                }
                KeyCode::Enter => {
                    let picker = self.picker.take().expect("picker is open");
                    let Some(item) = picker.items.get(picker.selected) else {
                        return Ok(None);
                    };
                    let action = match picker.kind {
                        PickerKind::Model => {
                            AppAction::Command(SlashCommand::Model(Some(item.value.clone())))
                        }
                        PickerKind::Mode => {
                            let mode = if item.value == "sovereign" {
                                Mode::Sovereign
                            } else {
                                Mode::Genie
                            };
                            AppAction::Command(SlashCommand::Mode(Some(mode)))
                        }
                        PickerKind::Effort => {
                            let effort = match item.value.as_str() {
                                "low" => Some(ReasoningEffort::Low),
                                "medium" => Some(ReasoningEffort::Medium),
                                "high" => Some(ReasoningEffort::High),
                                _ => None,
                            };
                            AppAction::Command(SlashCommand::Effort(Some(effort)))
                        }
                        PickerKind::Rewind => {
                            // Item values are always turn ids we formatted.
                            let Ok(turn) = item.value.parse::<u64>() else {
                                return Ok(None);
                            };
                            AppAction::Command(SlashCommand::Rewind(Some(turn)))
                        }
                        PickerKind::Resume => {
                            // Item values are session ids.
                            AppAction::Command(SlashCommand::Resume(Some(item.value.clone())))
                        }
                        PickerKind::ResumeClaude => {
                            // Item values are Claude Code session ids, which
                            // the command handler resolves back to a
                            // transcript before importing it.
                            AppAction::Command(SlashCommand::ResumeClaude(Some(item.value.clone())))
                        }
                        PickerKind::Subagent => {
                            // Subagents are spawned by the model, not run as a
                            // command. Pre-fill a delegation request so the user
                            // just types the task and submits.
                            self.set_input(format!("Use the {} subagent to ", item.value));
                            return Ok(None);
                        }
                        PickerKind::Settings => {
                            let selected = picker.selected;
                            return Ok(self.apply_setting(selected));
                        }
                        PickerKind::ClaudeImport => {
                            // Build the selection from the toggled rows (order
                            // matches `open_claude_import_picker`: mcp, commands,
                            // verbs) and hand off to the command handler, which
                            // has the live MCP manager.
                            let flags: Vec<bool> = picker.items.iter().map(|i| i.current).collect();
                            let selection = ImportSelection {
                                mcp: flags.first().copied().unwrap_or(false),
                                commands: flags.get(1).copied().unwrap_or(false),
                                verbs: flags.get(2).copied().unwrap_or(false),
                            };
                            if selection.is_empty() {
                                self.notice("nothing selected to import");
                                return Ok(None);
                            }
                            AppAction::Command(SlashCommand::ImportClaude(selection))
                        }
                        PickerKind::FusionPanel => {
                            // Panel = toggled rows; the first becomes the
                            // synthesizer (the sole tool-caller). Persist
                            // [fusion]; the new config takes effect next time
                            // /fusion turns on.
                            let panel: Vec<String> = picker
                                .items
                                .iter()
                                .filter(|i| i.current)
                                .map(|i| i.value.clone())
                                .collect();
                            if panel.is_empty() {
                                self.notice(
                                    "select at least one provider for the panel (Space toggles)",
                                );
                                return Ok(None);
                            }
                            let synthesizer = panel[0].clone();
                            let rounds = self.config.fusion.as_ref().map(|f| f.rounds).unwrap_or(1);
                            self.config.fusion = Some(crate::config::FusionConfig {
                                panel: panel.clone(),
                                synthesizer: synthesizer.clone(),
                                rounds,
                            });
                            if let Err(err) = self.config.save() {
                                self.notice(format!("could not save fusion config: {err:#}"));
                                return Ok(None);
                            }
                            let tail = if self.fusion_active {
                                " — /fusion off then on to apply"
                            } else {
                                " — /fusion to turn on"
                            };
                            self.notice(format!(
                                "fusion: {} · synthesizer {synthesizer} · {rounds} round(s){tail}",
                                panel.join("+")
                            ));
                            return Ok(None);
                        }
                        PickerKind::UltraLenses => {
                            // The judge row is the tail (open_ultra_picker put it
                            // there); everything above it is a lens. Saving is
                            // left to the command handler, which is the only
                            // place that can both persist [ultra] and re-arm a
                            // running engine in one step.
                            let Some((judge, lenses)) = picker.items.split_last() else {
                                return Ok(None);
                            };
                            let lenses: Vec<String> = lenses
                                .iter()
                                .filter(|item| item.current)
                                .map(|item| item.value.clone())
                                .collect();
                            if lenses.is_empty() {
                                self.notice(
                                    "select at least one lens (Space toggles) — ultra has nothing \
                                     to fan out over without one",
                                );
                                return Ok(None);
                            }
                            // A checkbox can only say none-or-one, so a count
                            // above one (which only `config.toml` can set) is
                            // preserved when the row stays on, exactly as the
                            // fusion picker preserves `rounds`.
                            let base = self.config.effective_ultra();
                            let judges = if judge.current { base.judges.max(1) } else { 0 };
                            AppAction::Command(SlashCommand::Ultra(UltraAction::Apply(
                                UltraConfig {
                                    lenses,
                                    judges,
                                    ..base
                                },
                            )))
                        }
                        PickerKind::Provider => {
                            // The final row opens the add-provider type menu;
                            // every other row switches to that provider.
                            if picker.selected + 1 == picker.items.len()
                                || item.value == PROVIDER_ADD_ROW
                            {
                                self.open_provider_type_picker();
                                return Ok(None);
                            }
                            AppAction::Command(SlashCommand::Provider(ProviderAction::Use(
                                item.value.clone(),
                            )))
                        }
                        PickerKind::ProviderType => {
                            use crate::llm::{cloudflare, openrouter, xai_oauth};
                            use std::collections::VecDeque;
                            match picker.selected {
                                // xAI sign-in: run the OAuth flow; login()
                                // auto-adds the provider on success.
                                0 => {
                                    return Ok(Some(AppAction::Command(SlashCommand::Login(
                                        "xai".to_string(),
                                    ))));
                                }
                                // xAI API key.
                                1 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Xai,
                                        name: "xai".to_string(),
                                        base_url: xai_oauth::DEFAULT_BASE_URL.to_string(),
                                        model: xai_oauth::DEFAULT_MODEL.to_string(),
                                        api_key: None,
                                        queue: VecDeque::from([PromptField::ApiKey]),
                                    });
                                }
                                // OpenRouter — model is unknown, so prompt for
                                // it alongside the key.
                                2 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::OpenRouter,
                                        name: "openrouter".to_string(),
                                        base_url: openrouter::DEFAULT_BASE_URL.to_string(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // Cloudflare Workers AI — account id (folded
                                // into the base URL) + token; model defaults to
                                // GLM 5.2 and can be changed later via /model.
                                3 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Cloudflare,
                                        name: "cloudflare".to_string(),
                                        base_url: cloudflare::BASE_URL_TEMPLATE.to_string(),
                                        model: cloudflare::DEFAULT_MODEL.to_string(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::AccountId,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // OpenAI — model + key.
                                4 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Openai,
                                        name: "openai".to_string(),
                                        base_url: "https://api.openai.com/v1".to_string(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // Anthropic — model + key.
                                5 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Anthropic,
                                        name: "claude".to_string(),
                                        base_url: "https://api.anthropic.com".to_string(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // OpenAI-compatible custom — everything is
                                // prompted, starting with the name.
                                6 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Openai,
                                        name: String::new(),
                                        base_url: String::new(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Name,
                                            PromptField::BaseUrl,
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // OpenAI-compatible presets (Gemini, DeepSeek,
                                // Groq, …) appended after the fixed rows — the
                                // default model is preset, so only the key is
                                // asked for.
                                index => {
                                    if let Some(preset) = crate::llm::compat::PRESETS
                                        .get(index - PROVIDER_TYPES.len())
                                    {
                                        self.begin_provider_prompt(ProviderPrompt {
                                            kind: ProviderKind::Openai,
                                            name: preset.name.to_string(),
                                            base_url: preset.base_url.to_string(),
                                            model: preset.default_model().to_string(),
                                            api_key: None,
                                            queue: VecDeque::from([PromptField::ApiKey]),
                                        });
                                    }
                                }
                            }
                            return Ok(None);
                        }
                        PickerKind::WebBackend => {
                            let id = item.value.clone();
                            self.select_web_backend(&id);
                            return Ok(None);
                        }
                    };
                    return Ok(Some(action));
                }
                _ => {}
            }
            return Ok(None);
        }

        // In the inline provider-setup prompt, Esc cancels; every other key
        // falls through to normal line editing and the Enter→submit path
        // (which `submit` routes to `submit_prompt_field`).
        if (self.prompt.is_some() || self.web_key_backend.is_some()) && key.code == KeyCode::Esc {
            self.cancel_prompt();
            return Ok(None);
        }

        // Modal (vim) editing. In Normal mode keys are motions/operators, not
        // text; in Insert mode the only extra binding is Esc → Normal, and
        // everything else falls through to ordinary line editing below.
        if self.vim.enabled {
            match self.vim.mode {
                VimMode::Normal => return self.handle_vim_normal(key),
                VimMode::Insert => {
                    if key.code == KeyCode::Esc
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.enter_normal_mode();
                        return Ok(None);
                    }
                }
            }
        }

        let suggesting = !self.suggestions.is_empty();
        let action = match key.code {
            // Shift+Enter (terminals with keyboard enhancement) or Alt+Enter
            // (the fallback elsewhere) inserts a newline instead of submitting.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_newline();
                None
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                self.delete_back();
                None
            }
            KeyCode::Delete => {
                self.delete_forward();
                None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                if self.cursor < len {
                    self.cursor += 1;
                } else if suggesting {
                    // Cursor at the end: → accepts the ghost-text prediction.
                    self.accept_suggestion();
                }
                None
            }
            // Line-scoped, like Ctrl-A/Ctrl-E and like every other editor. On
            // a single line — wrapped or not — this is identical to what it was.
            KeyCode::Home => {
                self.cursor = self.line_bounds().0;
                None
            }
            KeyCode::End => {
                self.cursor = self.line_bounds().1;
                None
            }
            KeyCode::Tab => {
                if suggesting {
                    self.accept_suggestion();
                } else {
                    self.complete_at_path();
                }
                None
            }
            // Shift+Tab toggles plan mode (same as /plan, welcome screen
            // included).
            KeyCode::BackTab => {
                self.welcome_dismissed = true;
                Some(AppAction::Command(SlashCommand::Plan))
            }
            KeyCode::Esc => {
                if self.console.is_some() {
                    // Topmost thing Esc can close: the composer means something
                    // different while a console is open, so giving it back
                    // comes before closing a sidebar the user can still read.
                    self.detach_console();
                } else if self.diff.is_some() {
                    // Esc closes the diff sidebar before touching the input.
                    self.diff = None;
                } else if self.show_todos {
                    // Then the todo band (it auto-opens on the first todo
                    // update, so it needs a way out that isn't `/todos`).
                    self.show_todos = false;
                } else if !self.suggestions.is_empty() {
                    // The command popup, before anything that touches the
                    // draft. It was not in this chain at all, so Escape fell
                    // through to `clear_input` and threw the typed text away
                    // along with the menu: `/quit` then Esc left an empty
                    // composer. Every other overlay here closes without
                    // destroying what is behind it, and the status bar says
                    // "Esc cancel" while the popup is up — cancelling the
                    // popup, not the sentence.
                    self.dismissed_suggestions_for = Some(self.input.clone());
                    self.suggestions.clear();
                    self.suggestion_index = 0;
                } else if !self.transcript.follow {
                    self.scroll_to_bottom();
                } else {
                    self.clear_input();
                }
                None
            }
            // While the diff sidebar is open it owns paging: read a long diff
            // top-to-bottom (offset from the top). Otherwise PgUp/PgDn scroll
            // the transcript; leaving the bottom freezes the viewport while
            // output streams, returning to it re-enables stick-to-bottom.
            KeyCode::PageUp if self.diff.is_some() => {
                if let Some(diff) = self.diff.as_mut() {
                    diff.scroll = diff.scroll.saturating_sub(10);
                }
                None
            }
            KeyCode::PageDown if self.diff.is_some() => {
                if let Some(diff) = self.diff.as_mut() {
                    diff.scroll = diff.scroll.saturating_add(10);
                }
                None
            }
            KeyCode::PageUp => {
                self.scroll_transcript(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_transcript(-10);
                None
            }
            KeyCode::Up => {
                // While browsing history, ↑/↓ keep navigating history even
                // when a recalled slash command repopulates suggestions.
                if suggesting && self.history_browse.is_none() {
                    self.suggestion_index = if self.suggestion_index == 0 {
                        self.suggestions.len() - 1
                    } else {
                        self.suggestion_index - 1
                    };
                } else {
                    self.history_prev();
                }
                None
            }
            KeyCode::Down => {
                if suggesting && self.history_browse.is_none() {
                    self.suggestion_index = if self.suggestion_index + 1 >= self.suggestions.len() {
                        0
                    } else {
                        self.suggestion_index + 1
                    };
                } else if self.history_browse.is_some() {
                    // Mid-history: ↓ keeps walking forward through it.
                    self.history_next();
                } else if !self.focus_rail() {
                    // Past the end of history with no subagents to drop into:
                    // ↓ is a no-op, which is what history_next already does.
                    self.history_next();
                }
                None
            }
            // Ctrl-G drafts the prompt in an external editor (handled by the
            // main loop, which owns the terminal). Masked key entry stays
            // inline — a secret must not land in a temp file.
            KeyCode::Char('g')
                if key.modifiers.contains(KeyModifiers::CONTROL) && !self.prompt_is_masked() =>
            {
                self.pending_edit_prompt = true;
                None
            }
            KeyCode::Char(c) => {
                // Unbound Ctrl/Alt chords must not insert their literal char.
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    self.insert_char(c);
                }
                None
            }
            _ => None,
        };
        self.sync_input_mode();
        Ok(action)
    }

    /// Enter pressed: complete the highlighted suggestion if the command is
    /// still partial, then parse the input line into an action.
    fn submit(&mut self) -> Option<AppAction> {
        // The inline prompts intercept Enter: each submission is an answer to a
        // field (provider setup) or a pasted web-search key, not a message.
        if self.web_key_backend.is_some() {
            return self.submit_web_key();
        }
        if self.prompt.is_some() {
            return self.submit_prompt_field();
        }
        // A running command with an open console owns Enter. Checked after the
        // provider-setup prompts above, which are a half-finished piece of
        // configuration the user started themselves and should get to finish.
        if self.console.is_some() {
            return self.submit_console();
        }
        if self.input_mode == InputMode::Command && !self.suggestions.is_empty() {
            let typed = self
                .input
                .trim_start()
                .strip_prefix('/')
                .unwrap_or_default()
                .to_string();
            let spec =
                self.suggestions[self.suggestion_index.min(self.suggestions.len() - 1)].clone();
            // An exactly-typed command always runs as typed; otherwise Enter
            // completes the highlighted suggestion first.
            let exact = COMMANDS.iter().any(|command| command.name == typed)
                || self.custom_commands.iter().any(|c| c.name == typed);
            if !exact && typed != spec.name {
                let takes_args = spec.takes_args;
                self.accept_suggestion();
                if takes_args {
                    // Completed to "/evolve " — wait for the arguments.
                    return None;
                }
            }
        }

        let input = self.input.trim().to_string();
        if input.is_empty() {
            return None;
        }
        match SlashCommand::parse(&input) {
            Some(Ok(command)) => {
                // A dispatched command counts as activity even though it adds
                // no transcript entries; drop the welcome screen.
                self.welcome_dismissed = true;
                self.push_history(&input);
                self.clear_input();
                Some(AppAction::Command(command))
            }
            Some(Err(message)) => {
                let word = input
                    .trim_start()
                    .strip_prefix('/')
                    .unwrap_or_default()
                    .split_whitespace()
                    .next()
                    .unwrap_or_default();
                // A known builtin with bad arguments keeps its usage notice;
                // custom commands and unknown `/words` go to the model (the
                // custom expansion happens in `submit_prompt`).
                if is_builtin_command(word) {
                    self.welcome_dismissed = true;
                    self.push_history(&input);
                    self.clear_input();
                    self.notice(message);
                    None
                } else {
                    self.submit_prompt(input)
                }
            }
            None => self.submit_prompt(input),
        }
    }

    /// Enter while a command's console is open: send the line to the child.
    ///
    /// The line goes **as typed**, with a newline appended and nothing else
    /// done to it. No trimming, no `/command` parsing, no `@file` expansion:
    /// an installer asking for an install prefix wants the characters the user
    /// typed, `/usr/local` is not a slash command, and a password is not a
    /// place for a helpful rewrite.
    ///
    /// An *empty* line is sent too, and that is the whole point of the bug
    /// report: pressing Enter at `Do you want to continue? [Y/n]` is how a
    /// person accepts the default, and swallowing it as "nothing to submit"
    /// is precisely the behaviour that made the prompt unanswerable.
    fn submit_console(&mut self) -> Option<AppAction> {
        let line = std::mem::take(&mut self.input);
        self.clear_input();
        let console = self.console.as_ref()?;
        if !console.writer.line(line.clone()) {
            // Either the command ended between the keystroke and here, or the
            // queue is full because it is not reading. Say so and hand the
            // composer back rather than dropping the line in silence.
            self.console = None;
            self.notice("that command is no longer reading input — Enter goes to the agent again");
            return None;
        }
        // Echo it into the running command's card. Nothing else will: a pipe
        // does not echo the way a terminal does, so without this the answer the
        // user gave leaves no trace in the conversation at all.
        self.transcript.console_echo(&line);
        self.transcript.scroll_to_bottom();
        None
    }

    /// Drop a console whose command is gone without having said so.
    ///
    /// The tool announces `ConsoleClosed` on every path it controls; this is
    /// for the paths it does not — a turn that died on a hard error, or a turn
    /// task aborted after the cooperative interrupt ran out of patience. Silent
    /// on the common case, because on the common case there is nothing open.
    pub(super) fn close_stale_console(&mut self) {
        // A console still only held has no composer to give back, so it goes
        // without a notice — but it still has to go, or its writer outlives the
        // turn and keeps a dead command's clock stopped.
        self.console_pending = None;
        if self.console.take().is_some() {
            self.notice("the command ended — Enter goes to the agent again");
        }
    }

    /// Let go of a running command's console (Esc).
    ///
    /// The command keeps running — this is not a kill, which is Ctrl-C. What it
    /// gives back is the composer, so the user can talk to the agent about
    /// something else while a build grinds on. The command's timeout clock,
    /// stopped while somebody was there to answer it, starts again when the
    /// writer drops: nobody is attending it any more, so "unattended" is once
    /// again the truth.
    fn detach_console(&mut self) {
        let Some(console) = self.console.take() else {
            return;
        };
        self.notice(format!(
            "detached from {} — Enter goes to the agent again (Ctrl-C stops the command)",
            console.command
        ));
    }

    /// Submit `input` as a user prompt: record it verbatim in history and
    /// the transcript, hand the preprocessed form (custom-command and `@file`
    /// expansion, plus any staged image attachments) to the agent.
    ///
    /// When a turn is already running the prompt is queued instead of rejected:
    /// it still lands in the transcript (so the user sees their words) and the
    /// main loop starts it once the current turn finishes. Rebuilds still
    /// refuse — the agent slot is empty then, and a queued turn would only
    /// bounce again.
    fn submit_prompt(&mut self, input: String) -> Option<AppAction> {
        if self.rebuilding.is_some() {
            self.notice("the agent is rebuilding — try again in a moment");
            return None;
        }
        let mut prepared =
            crate::commands::preprocess(&input, &self.custom_commands, &self.project_root);
        // Merge staged paste attachments; prefer absolute unique paths.
        for path in self.pending_images.drain(..) {
            if !prepared.images.iter().any(|p| p == &path) {
                prepared.images.push(path);
            }
        }
        if self.status.busy {
            if self.message_queue.len() >= MESSAGE_QUEUE_CAP {
                self.notice(format!(
                    "message queue is full ({MESSAGE_QUEUE_CAP}) — wait for a turn to finish"
                ));
                // Put the staged images back so the user doesn't lose them.
                self.pending_images.append(&mut prepared.images);
                return None;
            }
            self.push_history(&input);
            self.clear_input();
            self.record_prompt(input);
            let position = self.message_queue.len() + 1;
            self.message_queue.push_back(prepared);
            self.notice(format!("queued — will send after this turn (#{position})"));
            return None;
        }
        self.push_history(&input);
        self.clear_input();
        self.record_prompt(input);
        Some(AppAction::Submit(prepared))
    }

    /// Record a prompt in the conversation and jump to it.
    ///
    /// Attachments are deliberately not passed along. A staged image is a
    /// `PathBuf` at this point, not the
    /// [`ImageRef`](crate::images::ImageRef) a rendered row needs, and the TUI
    /// has never drawn a thumbnail of what the *user* sent — only of what the
    /// turn produced. Handing the model half a reference so the renderer could
    /// ignore it would put a difference between a live turn and its replay
    /// that no screen shows.
    fn record_prompt(&mut self, text: String) {
        self.transcript.user(text, Vec::new());
        self.scroll_to_bottom();
    }

    /// Pop the next queued user prompt, if any. Used by the main loop once a
    /// turn returns the agent and any post-turn agent commands have run.
    pub fn pop_queued_message(&mut self) -> Option<crate::commands::Preprocessed> {
        self.message_queue.pop_front()
    }

    /// Queue the first working turn for a freshly set `/goal`. The prompt
    /// lands in the transcript and the message queue, so the main loop's
    /// post-command drain starts it immediately when the agent is idle, or
    /// right after the current turn otherwise.
    pub fn queue_goal_kickoff(&mut self, goal: &str) {
        if self.message_queue.len() >= MESSAGE_QUEUE_CAP {
            self.notice(format!(
                "goal saved, but the message queue is full ({MESSAGE_QUEUE_CAP}) — \
                 work will not auto-start; send a message once a turn finishes"
            ));
            return;
        }
        let kickoff = format!(
            "A standing goal was just set for this project:\n\n{goal}\n\n\
             Start working toward it now: break it into concrete steps and \
             begin executing them. Keep going until you reach a natural \
             checkpoint, then summarize the progress made and what remains."
        );
        self.record_prompt(kickoff.clone());
        self.message_queue
            .push_back(crate::commands::Preprocessed::text_only(kickoff));
    }

    /// Handle a bracketed paste: stage image file paths / data-URL images as
    /// attachments, otherwise insert the text into the composer.
    fn handle_paste(&mut self, text: &str) {
        // data:image/...;base64,... → write under ~/.wizard/attachments and attach.
        if let Some((mime, b64)) = parse_data_image_url(text.trim()) {
            match save_pasted_image_bytes(mime, b64) {
                Ok(path) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("image")
                        .to_string();
                    self.stage_image(path, &name);
                }
                Err(err) => self.notice(format!("could not save pasted image: {err}")),
            }
            self.sync_input_mode();
            return;
        }

        // One or more existing image paths (whitespace / newline separated).
        let tokens: Vec<&str> = text.split_whitespace().filter(|t| !t.is_empty()).collect();
        if !tokens.is_empty() && tokens.iter().all(|t| looks_like_image_path_token(t)) {
            let mut any = false;
            for token in tokens {
                if let Some(path) = resolve_pasted_image_path(token, &self.project_root) {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(token)
                        .to_string();
                    self.stage_image(path, &name);
                    any = true;
                }
            }
            if any {
                self.sync_input_mode();
                return;
            }
        }

        // An image paste the terminal can't deliver arrives as an empty paste:
        // bracketed paste only carries text, so the image's bytes are left on
        // the OS clipboard. Read them there and attach — the same affordance as
        // Claude Code's `[Image #N]`. A genuinely empty paste finds nothing and
        // stays silent.
        if text.trim().is_empty() {
            self.attach_clipboard_image();
            self.sync_input_mode();
            return;
        }

        self.insert_str(text);
        self.sync_input_mode();
    }

    /// Attach an image from the OS clipboard, if one is present, staging it for
    /// the next submit and showing an `[Image #N]` token. Returns whether an
    /// image was found — so an explicit Ctrl-V can report an empty clipboard
    /// while an empty paste can stay quiet.
    fn attach_clipboard_image(&mut self) -> bool {
        let Some(bytes) = clipboard_image_bytes() else {
            return false;
        };
        let ext = sniff_image_ext(&bytes).unwrap_or("png");
        match save_image_bytes(&bytes, ext) {
            Ok(path) => self.stage_image(path, "pasted image"),
            Err(err) => self.notice(format!("could not attach pasted image: {err}")),
        }
        true
    }

    /// Stage `path` for the next submit and insert a numbered `[Image #N]`
    /// token — the composer indicator Claude Code shows for a pasted image.
    /// `label` names the source only for the confirmation notice.
    fn stage_image(&mut self, path: PathBuf, label: &str) {
        if self.pending_images.iter().any(|p| p == &path) {
            self.notice(format!("{label} is already attached"));
            return;
        }
        self.pending_images.push(path);
        let n = self.pending_images.len();
        let token = format!("[Image #{n}]");
        if !self.input.is_empty() && !self.input.chars().last().is_some_and(|c| c.is_whitespace()) {
            self.insert_char(' ');
        }
        self.insert_str(&token);
        self.notice(format!("attached {label} as Image #{n}"));
    }

    /// Minimal Tab path-completion for `@path` tokens: complete the token
    /// under the cursor from its directory listing (longest common prefix;
    /// a unique directory match gains a trailing `/`).
    fn complete_at_path(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let token: String = chars[start..self.cursor.min(chars.len())].iter().collect();
        let Some(partial) = token.strip_prefix('@') else {
            return;
        };
        if partial.starts_with('@') {
            return;
        }
        // Split the partial path into the directory to list and the name
        // prefix to match.
        let (dir_part, prefix) = match partial.rfind('/') {
            Some(slash) => (&partial[..=slash], &partial[slash + 1..]),
            None => ("", partial),
        };
        let expanded = shellexpand::tilde(dir_part);
        let dir_path = Path::new(expanded.as_ref());
        let dir = if dir_path.is_absolute() {
            dir_path.to_path_buf()
        } else {
            self.project_root.join(dir_path)
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut matches: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                let is_dir = entry.file_type().ok()?.is_dir();
                name.starts_with(prefix).then_some((name, is_dir))
            })
            .collect();
        matches.sort();
        let completion = match matches.as_slice() {
            [] => return,
            [(name, is_dir)] => {
                let mut full = name.clone();
                if *is_dir {
                    full.push('/');
                }
                full
            }
            many => {
                let mut common = many[0].0.clone();
                for (name, _) in &many[1..] {
                    let shared = common
                        .char_indices()
                        .zip(name.chars())
                        .take_while(|((_, a), b)| a == b)
                        .count();
                    common = common.chars().take(shared).collect();
                }
                common
            }
        };
        if completion.len() <= prefix.len() {
            return;
        }
        self.insert_str(&completion[prefix.len()..]);
    }

    /// Keys while the plan-review modal is open. Review state: `y`/Enter
    /// approves, `n` opens a feedback line, ↑/↓/PgUp/PgDn scroll the plan.
    /// Feedback state: typing edits, Enter sends the rejection, Esc returns
    /// to the review.
    fn handle_plan_review_key(&mut self, key: KeyEvent) {
        let Some(review) = self.plan_review.as_mut() else {
            return;
        };
        if let Some(feedback) = review.feedback.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let feedback = review.feedback.take().unwrap_or_default();
                    self.finish_plan_review(PlanVerdict::reject(feedback));
                }
                KeyCode::Esc => review.feedback = None,
                KeyCode::Backspace => {
                    feedback.pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    feedback.push(c);
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.finish_plan_review(PlanVerdict::approve());
            }
            KeyCode::Char('n') => review.feedback = Some(String::new()),
            KeyCode::Up | KeyCode::Char('k') => review.scroll = review.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => review.scroll = review.scroll.saturating_add(1),
            KeyCode::PageUp => review.scroll = review.scroll.saturating_sub(10),
            KeyCode::PageDown => review.scroll = review.scroll.saturating_add(10),
            _ => {}
        }
    }

    /// Close the plan review and send `verdict` back into the paused
    /// `exit_plan` call. Approval mirrors the agent clearing its plan-mode
    /// flag; rejection stays in plan mode.
    fn finish_plan_review(&mut self, verdict: PlanVerdict) {
        let Some(mut review) = self.plan_review.take() else {
            return;
        };
        let approved = verdict.approved;
        if let Some(respond) = review.respond.take() {
            let _ = respond.send(verdict);
        }
        if approved {
            self.plan_mode = false;
            self.notice("plan approved — executing it");
        } else {
            self.notice("plan rejected — still in plan mode");
        }
    }

    /// Drive the interview modal: number keys pick a suggested option, typing
    /// composes a free-text answer, Enter commits the current question and
    /// advances (committing the last one sends every answer back), and Esc
    /// dismisses the whole interview (the model proceeds on its own judgment).
    fn handle_interview_key(&mut self, key: KeyEvent) {
        let Some(interview) = self.interview.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.finish_interview(None);
            }
            KeyCode::Enter => {
                // Commit the current answer (the typed text wins; empty means
                // "skip this one") and advance.
                let answer = interview.input.trim().to_string();
                interview.answers.push(answer);
                interview.input.clear();
                interview.current += 1;
                if interview.current >= interview.questions.len() {
                    let answers = std::mem::take(&mut interview.answers);
                    self.finish_interview(Some(answers));
                }
            }
            KeyCode::Backspace => {
                interview.input.pop();
            }
            // 1-9 fill the input with the matching suggested option, so the
            // user can accept it with Enter or edit it first.
            KeyCode::Char(c)
                if c.is_ascii_digit()
                    && c != '0'
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let idx = (c as u8 - b'1') as usize;
                if let Some(option) = interview
                    .current_question()
                    .and_then(|q| q.options.get(idx))
                {
                    interview.input = option.clone();
                } else {
                    interview.input.push(c);
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                interview.input.push(c);
            }
            _ => {}
        }
    }

    /// Close the interview and send `answers` back into the paused
    /// `interview` call: `Some(answers)` aligned with the questions, or `None`
    /// when the user dismissed it (the model then uses its best judgment).
    fn finish_interview(&mut self, answers: Option<Vec<String>>) {
        let Some(mut interview) = self.interview.take() else {
            return;
        };
        let answered = answers.is_some();
        if let Some(respond) = interview.respond.take() {
            let _ = respond.send(answers);
        }
        self.notice(if answered {
            "answers sent — the agent is finishing its plan"
        } else {
            "interview dismissed — the agent will use its best judgment"
        });
    }

    /// Record a submitted input for ↑/↓ recall (skipping immediate repeats).
    fn push_history(&mut self, input: &str) {
        if self.history.last().map(String::as_str) != Some(input) {
            self.history.push(input.to_string());
        }
    }

    /// Fold an agent event into the conversation, and into whatever else on
    /// this screen the same event drives.
    ///
    /// The conversation half is one line: the model reads the event, exactly
    /// as the GUI's does. Everything below is what is *not* conversation — the
    /// status bar's counters, the modal a gate opens, the subagent rail — and
    /// none of it touches the transcript, which is what keeps the two surfaces
    /// from drifting again.
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        self.transcript.apply(&event);
        // The tee: the same value, on its way to the peers watching this node.
        //
        // Here rather than in the turn task, because *here* is the one place
        // every agent event on this surface passes through — a turn's stream, a
        // session-start hook, a background task reporting in, a subagent run —
        // and a tee hung off the turn alone would show a watcher a session that
        // went silent between turns. What crosses is not decided here: see
        // [`MeshTee::publish`].
        if let Some(mesh) = &self.mesh {
            mesh.publish(&event);
        }
        match event {
            // Already folded above; there is nothing else on screen these
            // drive. (`ToolFinished`'s pairing, the orphan row it falls back
            // to, and the notice wording for a finished task or subagent all
            // live in the model now, so a second phrasing of any of them
            // cannot appear here.)
            AgentEvent::TextDelta(_)
            | AgentEvent::ThinkingDelta(_)
            | AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::Images { .. }
            | AgentEvent::Error(_)
            | AgentEvent::Notice(_)
            | AgentEvent::StreamRetrying
            | AgentEvent::HookFired { .. } => {}
            // The pre-phase's drafts and verdict, as a card that is always
            // folded: it is tens of KB, and the point of the turn is the
            // answer below it.
            AgentEvent::UltraGuidance { .. } => self.transcript.set_last_tool_folded(true),
            AgentEvent::CommandRequested(line) => {
                // A turn in flight can't be reconfigured, so queue the command;
                // the main loop drains it once the agent is back in its slot.
                // The notice the user sees was written by the model above.
                self.pending_agent_commands.push(line);
            }
            AgentEvent::StepCompleted { step } => {
                self.status.step = step;
            }
            AgentEvent::PlanReady { plan, gate } => {
                // Claim the verdict channel before anything else can: the modal
                // below is what answers this gate, and the turn stays parked
                // inside `exit_plan` until it does. A gate already claimed
                // (a duplicate event, or a second consumer on a teed stream) is
                // not ours to review.
                let Some(respond) = gate.claim() else {
                    return;
                };
                // A plan awaiting review implies plan mode is on, however
                // the turn was started (e.g. `--plan`).
                self.plan_mode = true;
                self.plan_review = Some(PlanReview {
                    plan,
                    respond: Some(respond),
                    feedback: None,
                    scroll: 0,
                });
            }
            AgentEvent::Interview { questions, gate } => {
                let Some(respond) = gate.claim() else {
                    return;
                };
                // Defensive: an empty set would leave the modal with nothing
                // to answer and the turn wedged — decline immediately.
                if questions.is_empty() {
                    let _ = respond.send(None);
                } else {
                    self.interview = Some(Interview {
                        questions,
                        answers: Vec::new(),
                        current: 0,
                        input: String::new(),
                        respond: Some(respond),
                    });
                }
            }
            // A foreground command opened its stdin to a human. Claim the
            // writer before anything else can — the same rule, and the same
            // reason, as `PlanReady` above: the composer below is what types
            // into this child, and a gate somebody else already claimed is not
            // ours to drive.
            AgentEvent::ConsoleOpened { command, gate } => {
                let Some(writer) = gate.claim() else {
                    return;
                };
                // Held, not driven. Claiming has to happen now — the writer
                // must be ours before any question can appear — but the
                // composer stays the agent's until the command actually asks
                // something, which is `ConsoleWaiting` below. Most commands
                // never get there.
                self.console_pending = Some(Console {
                    command,
                    gate,
                    writer,
                });
            }
            // The command asked something. Now the composer changes hands, and
            // says so: a composer that quietly meant something else would be a
            // worse bug than the one consoles fix.
            AgentEvent::ConsoleWaiting { gate } => {
                if self
                    .console_pending
                    .as_ref()
                    .is_some_and(|pending| pending.gate == gate)
                    && let Some(console) = self.console_pending.take()
                {
                    self.notice(format!(
                        "▶ {} — Enter now types into this command \
                         (Esc detaches · Ctrl-D ends input · Ctrl-C stops it)",
                        console.command
                    ));
                    self.console = Some(console);
                }
            }
            // Already folded into the running tool's card by the model above;
            // there is nothing else on screen it drives.
            AgentEvent::ConsoleOutput { .. } => {}
            AgentEvent::ConsoleClosed { gate } => {
                // A console that was only ever held goes quietly: the composer
                // never changed, so there is nothing to hand back and nothing
                // worth a line in the transcript.
                if self
                    .console_pending
                    .as_ref()
                    .is_some_and(|pending| pending.gate == gate)
                {
                    self.console_pending = None;
                }
                // Only if it is *this* console. A close for a command we never
                // attached to (or already detached from) must not silently
                // take the composer back from one we did.
                if self.console.as_ref().is_some_and(|open| open.gate == gate) {
                    self.console = None;
                    self.notice("command finished — Enter goes to the agent again");
                }
            }
            AgentEvent::OmakaseProceeding { .. } => {
                // Chef's choice: no review gate. Mirror the agent clearing its
                // flags, and open the card the model just laid down — it is
                // the only record of a decision the user never got to review,
                // so folding it by length would hide the whole point of it.
                self.plan_mode = false;
                self.omakase = false;
                self.transcript.set_last_tool_folded(false);
                self.notice("omakase — chef's choice: proceeding with the agent's own plan");
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                // Session lifetime totals (for /cost).
                self.status.prompt_tokens += prompt_tokens;
                self.status.completion_tokens += completion_tokens;
                // Context meter: the most recent prompt size *is* what the
                // next turn will load (history grows by completion tokens
                // too, but the next call's reported prompt will supersede
                // this; until then the last prompt is the best known figure).
                if prompt_tokens > 0 {
                    self.status.context_tokens = prompt_tokens;
                }
            }
            AgentEvent::ContextSize { tokens } => {
                // History just shrank (auto-compaction): replace the meter
                // with the post-compact estimate without touching /cost totals.
                self.status.context_tokens = tokens;
            }
            // TaskStarted is also mirrored to the gateway's JSON stream (see
            // output.rs); the TUI additionally bumps the live status-bar
            // counter (see draw_status_bar) so a running task stays visible
            // without waiting for the finish notice.
            AgentEvent::TaskStarted { .. } => {
                self.status.background_tasks += 1;
            }
            AgentEvent::TaskFinished { .. } => {
                self.status.background_tasks = self.status.background_tasks.saturating_sub(1);
            }
            // Same pattern as TaskStarted/TaskFinished above, for subagents
            // delegated with `background: true`.
            AgentEvent::SubagentStarted { .. } => {
                self.status.background_subagents += 1;
            }
            AgentEvent::SubagentFinished { .. } => {
                self.status.background_subagents =
                    self.status.background_subagents.saturating_sub(1);
            }
            // ---- The subagent rail --------------------------------------
            //
            // These carry a `run` id, so concurrent runs (even two of the same
            // subagent) each land in their own pane instead of interleaving
            // into the parent transcript. Their conversation content is
            // translated back into ordinary events and folded by the same
            // model — see [`App::pane_event`].
            AgentEvent::SubagentRunStarted {
                run,
                bg,
                name,
                task,
            } => {
                self.panes.push(SubagentPane::new(run, bg, name, task));
            }
            AgentEvent::SubagentRunText { run, text } => {
                self.pane_write(run, true, |view| view.assistant(text));
            }
            AgentEvent::SubagentRunToolStarted { run, name, args } => {
                self.pane_event(run, AgentEvent::ToolStarted { name, args });
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                // No unread bump: a result landing on a card the badge
                // already counted is not a second thing to go and look at.
                let event = AgentEvent::ToolFinished { name, output };
                self.pane_write(run, false, |view| view.apply(&event));
            }
            AgentEvent::SubagentRunImages {
                run,
                source,
                images,
            } => {
                self.pane_event(run, AgentEvent::Images { source, images });
            }
            AgentEvent::SubagentRunStep { run, step } => {
                if let Some(index) = self.pane_index(run) {
                    self.panes[index].steps = step;
                }
            }
            AgentEvent::SubagentRunDone {
                run,
                completed,
                output,
                error,
                ..
            } => self.finish_pane(run, completed, output, error),
            AgentEvent::TodoUpdated(items) => {
                self.todos = items;
                // Auto-show the overlay the first time the agent starts a
                // list; afterwards /todos controls visibility.
                if !self.todos_seen && !self.todos.is_empty() {
                    self.todos_seen = true;
                    self.show_todos = true;
                }
            }
            AgentEvent::Done { reason } => {
                self.status.busy = false;
                self.turn_started = None;
                // A console cannot outlive the turn that opened it. The tool
                // closes its own on the way out, but a turn that died on a hard
                // error never reached that line, and a composer left pointing
                // at a dead child is a composer whose Enter key does nothing —
                // which is the bug, not the fix.
                self.close_stale_console();
                match reason {
                    DoneReason::Completed => {}
                    DoneReason::MaxSteps => self.notice(format!(
                        "step budget reached ({}) — send another message to continue",
                        self.status.max_steps
                    )),
                    DoneReason::TimeLimit => self.notice("time limit reached"),
                    DoneReason::Stopped => self.notice("turn stopped"),
                    DoneReason::CircuitBreaker => {
                        self.notice("circuit breaker tripped: repeated identical failures");
                    }
                }
            }
        }
    }

    /// Close a subagent run's pane out: its report, its verdict, and the
    /// write-back into the `spawn_subagent` card that launched it.
    fn finish_pane(&mut self, run: u64, completed: bool, output: String, error: Option<String>) {
        let Some(index) = self.pane_index(run) else {
            return;
        };
        let attached = self.attached == Some(index);
        let pane = &mut self.panes[index];
        pane.status = if completed {
            PaneStatus::Done
        } else {
            PaneStatus::Failed
        };
        pane.finished = Some(Instant::now());
        // The subagent's final message is the step that made no tool call, so
        // the sub-loop ends on it without streaming it — it arrives here, as
        // the report. Without this the pane would show all of the work and
        // none of the conclusion.
        let already_last = matches!(
            pane.transcript.last(),
            Some(TranscriptItem::Text(text)) if text == &output
        );
        if !already_last {
            pane.transcript.assistant(output.clone());
        }
        match &error {
            Some(error) => pane.transcript.notice(format!("failed: {error}")),
            None if !completed => pane.transcript.notice("hit its step budget".to_string()),
            None => {}
        }
        if !attached {
            pane.unread += 1;
        }

        // The pane retires off the rail shortly; fold its report back into the
        // `spawn_subagent` card in the main chat so the run is still there to
        // read afterwards. A foreground run's card already carries the report
        // (it is the tool's own result), so only a detached one needs writing
        // back.
        let (name, task, steps, bg) = (pane.name.clone(), pane.task.clone(), pane.steps, pane.bg);
        if bg.is_none() {
            return;
        }
        let report = match &error {
            Some(error) => format!("failed: {error}"),
            None if !completed => {
                format!("hit its step budget after {steps} step(s).\n\n{output}")
            }
            None => output,
        };
        self.record_subagent_report(&name, &task, &report, !completed);
    }
}

/// If `char_pos` lands inside an `[Image #N]` token in `input`, return
/// `(start, end, n)` as character indices into `input` (`end` exclusive).
fn image_token_at(input: &str, char_pos: usize) -> Option<(usize, usize, usize)> {
    let chars: Vec<char> = input.chars().collect();
    if chars.is_empty() || char_pos >= chars.len() {
        return None;
    }
    // Scan every `[Image #N]` occurrence; return the one covering char_pos.
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '[' {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < chars.len() && chars[end] != ']' {
            // Tokens are a single line; a newline means this `[` is not ours.
            if chars[end] == '\n' {
                break;
            }
            end += 1;
        }
        if end >= chars.len() || chars[end] != ']' {
            i += 1;
            continue;
        }
        end += 1; // exclusive end past `]`
        let token: String = chars[i..end].iter().collect();
        if let Some(n) = parse_image_token_number(&token) {
            if char_pos >= i && char_pos < end {
                return Some((i, end, n));
            }
            i = end;
            continue;
        }
        i += 1;
    }
    None
}

/// Parse `N` from a full `[Image #N]` token, or `None` if the shape differs.
fn parse_image_token_number(token: &str) -> Option<usize> {
    let rest = token.strip_prefix("[Image #")?.strip_suffix(']')?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// After removing image `n`, rewrite every `[Image #k]` with `k > n` down by
/// one so token numbers stay packed and match `pending_images` indices.
fn renumber_image_tokens_after(input: &mut String, removed_n: usize) {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut replacements: Vec<(usize, usize, usize)> = Vec::new(); // start, end, new_n
    while i < chars.len() {
        if chars[i] == '[' {
            let mut end = i + 1;
            while end < chars.len() && chars[end] != ']' {
                if chars[end] == '\n' {
                    break;
                }
                end += 1;
            }
            if end < chars.len() && chars[end] == ']' {
                end += 1;
                let token: String = chars[i..end].iter().collect();
                if let Some(k) = parse_image_token_number(&token) {
                    if k > removed_n {
                        replacements.push((i, end, k - 1));
                    }
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    // Apply from the right so earlier indices stay valid against `chars`.
    for (start, end, new_n) in replacements.into_iter().rev() {
        let byte_start: usize = chars[..start].iter().map(|c| c.len_utf8()).sum();
        let byte_end: usize = chars[..end].iter().map(|c| c.len_utf8()).sum();
        let new_token = format!("[Image #{new_n}]");
        input.replace_range(byte_start..byte_end, &new_token);
    }
}

/// Side effects the main loop performs on behalf of [`App`] (the app itself
/// stays synchronous and side-effect free).
#[derive(Debug)]
pub enum AppAction {
    /// Start an agent turn with this user message (text + optional image paths).
    Submit(crate::commands::Preprocessed),
    /// Execute a parsed slash command.
    Command(SlashCommand),
    /// Interrupt the running turn (Ctrl-C): ask it to stop cooperatively, and
    /// abort its task if it does not (see [`INTERRUPT_GRACE`]).
    Interrupt,
    /// Copy the current mouse selection to the clipboard. Handled in the main
    /// loop because it owns the terminal (and thus the rendered cell buffer).
    CopySelection,
}
