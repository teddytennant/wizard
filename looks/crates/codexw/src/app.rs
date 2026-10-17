// OWNER: app (event loop, state, viewport layout, session switching, exit)
//! The application state and its loop.
//!
//! Everything above the viewport is scrollback, written once; `cells` is the source of truth and
//! a resize replays it (spec A.5). The loop is event driven: with nothing animating and nothing
//! to draw there is no timer, so an idle codexw uses no CPU.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_core::{
    BackendHandle, Config, Event, HistoryItem, Request, SessionInfo, SlashCommand, StopReason,
    Usage,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use tokio::sync::mpsc::UnboundedSender;
use tuikit::term::{Event as TermEvent, EventPump};

use crate::commands::Outcome;
use crate::style::{Palette, palette};
use crate::term::InlineTerminal;
use crate::ui::bottom_pane::{BottomPane, PaneEvent};
use crate::ui::composer::PLACEHOLDERS;
use crate::ui::history_cells::{ChatLog, ErrorCell, InfoCell, UserCell};
use crate::ui::session_header::{DEFAULT_TIP, SessionHeaderCell, SessionInfoCell, TooltipCell};
use crate::ui::{AppAction, BoxedCell, HistoryCell, Overlay, OverlayResult};
use crate::wrap::{WrapOpts, word_wrap_line};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CODEX_VERSION: &str = "0.147.0";
/// 120 frames per second at most.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_nanos(8_333_334);
pub const ANIMATION_INTERVAL: Duration = Duration::from_millis(32);
pub const RESIZE_DEBOUNCE: Duration = Duration::from_millis(75);
/// Rows kept when a resize replays history (spec A.5.1, unknown terminals).
pub const REFLOW_MAX_ROWS: usize = 1000;

#[derive(Clone, Debug, Default)]
pub struct AppOpts {
    pub cwd: PathBuf,
    /// Session id to load at start (`codexw resume <id>`).
    pub resume: Option<String>,
    /// Continue the newest session of this directory.
    pub resume_last: bool,
    /// Open the resume picker at start.
    pub resume_picker: bool,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub mock: bool,
    pub no_alt_screen: bool,
    /// Pick the composer placeholder (tests); `None` picks one at random.
    pub placeholder: Option<usize>,
    /// Home directory for `~` display; `None` reads `$HOME`.
    pub home: Option<String>,
    /// `-c tui.status_line=[]`: the footer shows Codex's hint row instead of the status line.
    pub status_line_off: bool,
}

// Backend events dwarf the other message, and they are the hot path: no boxing.
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Backend(Event),
    /// A `!cmd` finished running on its thread.
    Shell(crate::ui::shell_cell::ShellOutput),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ExitInfo {
    pub usage: Usage,
    pub session_id: String,
}

/// What a `/new` or `/clear` is waiting for from the backend's next `Ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSwitch {
    pub clear: bool,
    /// A saved session is being opened: its replay comes before the old session's summary.
    pub load: bool,
}

/// What a confirmed backtrack is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindFlow {
    /// The prompt that goes back into the composer.
    pub text: String,
    /// What wizard answered to `/rewind <turn>`.
    pub answer: Option<String>,
}

pub struct App<W: Write> {
    pub opts: AppOpts,
    pub tx: UnboundedSender<Request>,
    pub term: InlineTerminal<W>,
    pub screen: Size,
    /// Committed history, source of truth for replays.
    pub cells: Vec<BoxedCell>,
    /// Cells before this index are not replayed (Ctrl+L and `/clear` start the screen over).
    pub replay_from: usize,
    has_emitted_history: bool,
    pub log: ChatLog,
    pub pane: BottomPane,
    pub overlay: Option<Box<dyn Overlay>>,
    /// The `loading` header shown until the session is configured.
    pub placeholder_header: Option<SessionHeaderCell>,
    /// The tip under the session header.
    pub tip: &'static str,
    pub config: Config,
    pub commands: Vec<SlashCommand>,
    pub sessions: Vec<SessionInfo>,
    /// `/resume` asked for the list; open the picker when it arrives.
    pub want_sessions: Option<String>,
    pub session_id: String,
    pub usage: Usage,
    pub running: bool,
    pub turn_started: Option<Instant>,
    pub ready: bool,
    pub switch: Option<SessionSwitch>,
    /// `codexw resume` and `--last` pick a session before the chat starts.
    pub start_pick: Option<crate::ui::pickers::StartPick>,
    start_pick_asked: bool,
    /// A replayed session waiting for its header.
    replay_cells: Vec<BoxedCell>,
    pub esc_primed: bool,
    /// Permission requests waiting for the bottom pane (and for the composer to go idle).
    pub pending_perms: crate::ui::approval::PendingPerms,
    /// Last key press, so an approval does not land on a half-typed draft.
    pub last_input: Instant,
    /// Where a `!cmd` thread reports; `None` runs the command inline (headless tests).
    pub msg_tx: Option<UnboundedSender<Msg>>,
    pub should_exit: bool,
    /// Quitting for a `/ui` switch: the look taking over is where the session goes on, so
    /// no resume hint is left behind.
    pub switched: bool,
    /// `/plan` or `/omakase` to send on the first session: the look `/ui` left was planning.
    handed_plan: Option<&'static str>,
    /// Ctrl+G was pressed; `run` opens the editor once the hint frame is on screen.
    pub editor_requested: bool,
    /// `/title`: the items of the terminal title, in order.
    pub title_items: Vec<String>,
    /// Latest todo list progress, for the `task-progress` item.
    pub task_progress: Option<(usize, usize)>,
    /// Git root name and branch, read now and then rather than every frame.
    git: (Option<String>, Option<String>),
    git_read: Option<Instant>,
    /// The saved theme did not resolve; said once under the header.
    pub theme_warning: Option<String>,
    /// A `/rewind <turn>` is in flight: its answer is read, not shown, and the session is
    /// loaded again once the turn ends.
    pub rewind: Option<RewindFlow>,
    /// Text for the composer once the session replay is on screen (a rewound prompt).
    pub prefill_after_load: Option<String>,
    /// `/raw`: history is written as plain source text the terminal wraps itself.
    pub raw_output: bool,
    /// Messages that carry images: the text as shown, and the text wizard is sent (the chips
    /// become `@path`). Wizard takes text only over ACP.
    image_sends: Vec<(String, String)>,
    image_note_shown: bool,
    /// Parks the key reader while an external program owns the terminal.
    pub input_ctl: Option<tuikit::input::InputControl>,
    pub exit: ExitInfo,
    pub home: Option<String>,
    // scheduling
    needs_draw: bool,
    last_draw: Option<Instant>,
    reflow_at: Option<Instant>,
    reflow_pending_overlay: bool,
    last_anim: Instant,
    title: String,
    started: Instant,
    pub last_assistant_text: String,
}

impl<W: Write> App<W> {
    pub fn new(
        opts: AppOpts,
        tx: UnboundedSender<Request>,
        out: W,
        screen: Size,
        cursor: Position,
    ) -> Self {
        let pick = opts.placeholder.unwrap_or_else(random_index) % PLACEHOLDERS.len();
        let home = opts.home.clone().or_else(|| std::env::var("HOME").ok());
        let dir = display_path(&opts.cwd, home.as_deref());
        // The saved syntax theme, before anything is drawn in it.
        let theme_warning = home.as_deref().and_then(|h| {
            let dir = PathBuf::from(h).join(".config/codexw");
            let name = crate::config::get(&dir.join("config.toml"), "theme")?;
            crate::highlight::set_configured_theme(Some(name), Some(&dir))
        });
        let mut pane = BottomPane::new(PLACEHOLDERS[pick]);
        pane.composer
            .set_skills(crate::skills::load(home.as_deref(), &opts.cwd));
        pane.footer.cwd = dir.clone();
        let cfg = home.as_deref().map(crate::config::path);
        let status = cfg
            .as_deref()
            .map(crate::statusline::load_status)
            .unwrap_or_default();
        let title_items = cfg
            .as_deref()
            .map(crate::statusline::load_title)
            .unwrap_or_else(|| {
                crate::statusline::DEFAULT_TITLE_ITEMS
                    .map(String::from)
                    .to_vec()
            });
        pane.footer.status_items = status.items;
        pane.footer.status_colors = status.colors;
        pane.footer.status_line_enabled =
            !opts.status_line_off && !pane.footer.status_items.is_empty();
        pane.footer.model = opts.model.clone().unwrap_or_else(|| "loading".into());
        let header = SessionHeaderCell::new(None, "", &dir);
        Self {
            term: InlineTerminal::new(out, screen, cursor),
            screen,
            tx,
            cells: Vec::new(),
            replay_from: 0,
            has_emitted_history: false,
            log: ChatLog::new(),
            pane,
            overlay: None,
            placeholder_header: Some(header),
            tip: DEFAULT_TIP,
            config: Config::default(),
            commands: Vec::new(),
            sessions: Vec::new(),
            want_sessions: None,
            session_id: String::new(),
            usage: Usage::default(),
            running: false,
            turn_started: None,
            ready: false,
            switch: None,
            start_pick: if opts.resume_picker {
                Some(crate::ui::pickers::StartPick::Picker)
            } else if opts.resume_last {
                Some(crate::ui::pickers::StartPick::Last)
            } else {
                None
            },
            start_pick_asked: false,
            replay_cells: Vec::new(),
            esc_primed: false,
            pending_perms: Default::default(),
            last_input: Instant::now(),
            msg_tx: None,
            should_exit: false,
            switched: false,
            handed_plan: agent_core::handed_plan(),
            editor_requested: false,
            title_items,
            task_progress: None,
            git: (None, None),
            git_read: None,
            theme_warning,
            rewind: None,
            prefill_after_load: None,
            raw_output: false,
            image_sends: Vec::new(),
            image_note_shown: false,
            input_ctl: None,
            exit: ExitInfo::default(),
            home,
            needs_draw: true,
            last_draw: None,
            reflow_at: None,
            reflow_pending_overlay: false,
            last_anim: Instant::now(),
            title: String::new(),
            started: Instant::now(),
            last_assistant_text: String::new(),
            opts,
        }
    }

    pub fn request_draw(&mut self) {
        self.needs_draw = true;
    }

    pub fn cwd_display(&self) -> String {
        display_path(&self.opts.cwd, self.home.as_deref())
    }

    /// Short model name for the header and footer: `grok-4.6` from `xai-oauth/grok-4.6`.
    pub fn model_name(&self) -> String {
        let id = &self.config.model;
        self.config
            .models
            .iter()
            .find(|m| &m.id == id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| id.clone())
    }

    // ---- history ------------------------------------------------------------------------------

    /// Append a finished cell: remember it and queue its lines for the scrollback.
    pub fn push_cell(&mut self, cell: BoxedCell) {
        self.queue_cell_lines(cell.as_ref());
        self.cells.push(cell);
        self.request_draw();
    }

    fn queue_cell_lines(&mut self, cell: &dyn HistoryCell) {
        let lines = self.separated_lines(cell);
        if !lines.is_empty() {
            self.term.queue_history_with(lines, self.wrap_policy());
        }
    }

    fn wrap_policy(&self) -> crate::term::inline::WrapPolicy {
        if self.raw_output {
            crate::term::inline::WrapPolicy::Terminal
        } else {
            crate::term::inline::WrapPolicy::PreWrap
        }
    }

    /// `/raw` and Alt+R: switch history between the rich cells and their source text, and
    /// redraw everything above the composer in the new form.
    pub fn set_raw_output(&mut self, enabled: bool, notify: bool) {
        self.raw_output = enabled;
        if notify {
            self.push_cell(Box::new(InfoCell {
                text: if enabled {
                    "Raw output mode on: transcript text is shown for clean terminal selection."
                } else {
                    "Raw output mode off: rich transcript rendering restored."
                }
                .into(),
                hint: None,
            }));
        }
        self.reflow_now();
    }

    /// The cell's lines with the blank separator the inserter adds before it (spec A.4.5).
    fn separated_lines(&mut self, cell: &dyn HistoryCell) -> Vec<Line<'static>> {
        let mut lines = if self.raw_output {
            cell.raw_lines()
        } else {
            cell.display_lines(self.screen.width)
        };
        if lines.is_empty() {
            return lines;
        }
        if !cell.is_stream_continuation() {
            if self.has_emitted_history {
                lines.insert(0, Line::default());
            } else {
                self.has_emitted_history = true;
            }
        }
        lines
    }

    // ---- backend events -----------------------------------------------------------------------

    pub fn on_backend(&mut self, ev: Event) {
        self.log.set_context(self.screen.width, &self.opts.cwd);
        if let (Event::Notice { text, .. }, Some(flow)) = (&ev, self.rewind.as_mut()) {
            // The answer to `/rewind <turn>` is read by `finish_rewind`, not printed.
            flow.answer = Some(text.clone());
            return;
        }
        if let Event::Notice { text, .. } = &ev {
            if agent_core::switched_look(text).is_some_and(|l| l != "codex") {
                // `/ui <look>`: wizard saved it; leaving hands the terminal to that look
                self.should_exit = true;
                self.switched = true;
                return;
            }
            // wizard says where plan mode is: after `/plan`, and when an approved plan leaves it
            if let Some(on) = agent_core::plan_mode(text) {
                self.pane.footer.plan_mode = on;
            }
        }
        match &ev {
            Event::Ready { session_id, config } => {
                self.session_id = session_id.clone();
                self.config = config.clone();
                self.on_ready();
            }
            Event::Commands(c) => {
                self.pane.composer.backend_commands = c
                    .iter()
                    .map(|c| (c.name.clone(), c.description.clone()))
                    .collect();
                self.commands = c.clone();
            }
            Event::ConfigChanged(c) => {
                self.config = c.clone();
                self.sync_footer();
            }
            Event::Sessions(s) => {
                self.sessions = s.clone();
                if let Some(q) = self.want_sessions.take() {
                    self.show_sessions(&q);
                }
            }
            Event::History { items, .. } => {
                self.replay_cells = replay_cells(items);
            }
            Event::TurnStart => {
                self.set_running(true);
            }
            Event::Usage(u) => {
                self.usage = u.clone();
                // Only shown when the status line is off; wizard may not report either number.
                let (win, used) = (u.context_window, u.context_tokens);
                self.pane.footer.context_percent =
                    (win > 0 && used > 0).then(|| 100 - ((used * 100) / win).min(100) as i64);
                self.pane.footer.context_tokens = (used > 0).then_some(used as i64);
            }
            Event::Todos(t) => {
                let done = t
                    .iter()
                    .filter(|x| x.status == agent_core::TodoStatus::Completed)
                    .count();
                self.task_progress = (!t.is_empty()).then_some((done, t.len()));
                for c in self.log.apply(&ev) {
                    self.push_cell(c);
                }
            }
            Event::Permission(req) => {
                crate::ui::approval::on_permission(self, req.clone());
            }
            Event::TurnEnd(reason) => {
                let cancelled = matches!(reason, StopReason::Cancelled);
                let steered = cancelled && !self.pane.steers.is_empty();
                if steered {
                    let steers = std::mem::take(&mut self.pane.steers);
                    for c in self.log.apply(&Event::TurnEnd(StopReason::EndTurn)) {
                        self.push_cell(c);
                    }
                    self.push_cell(Box::new(InfoCell {
                        text: "Model interrupted to submit steer instructions.".into(),
                        hint: None,
                    }));
                    for s in steers {
                        self.push_cell(Box::new(UserCell { text: s }));
                    }
                } else {
                    for c in self.log.apply(&ev) {
                        self.push_cell(c);
                    }
                    self.set_running(false);
                    if self.rewind.is_some() {
                        self.finish_rewind();
                    } else {
                        self.send_queued();
                    }
                }
            }
            Event::Fatal(_) => {
                for c in self.log.apply(&ev) {
                    self.push_cell(c);
                }
                self.set_running(false);
            }
            Event::TextDelta(d) => {
                self.last_assistant_text.push_str(d);
                for c in self.log.apply(&ev) {
                    self.push_cell(c);
                }
            }
            _ => {
                for c in self.log.apply(&ev) {
                    self.push_cell(c);
                }
            }
        }
        if self.log.take_reflow() {
            self.reflow_now();
        }
        self.request_draw();
    }

    fn on_ready(&mut self) {
        self.ready = true;
        // `-m` wins over the backend's default and is sent on so the session really uses it.
        if let Some(m) = self.opts.model.take() {
            if m != self.config.model {
                self.config.model = m.clone();
                let _ = self.tx.send(Request::SetModel(m));
            }
        }
        if let Some(cmd) = self.handed_plan.take() {
            self.pane.footer.plan_mode = true;
            let _ = self.tx.send(Request::Prompt(cmd.into()));
        }
        self.sync_footer();
        // The session list decides whether this session is the one the user gets.
        if self.start_pick.is_some() && !self.start_pick_asked {
            self.start_pick_asked = true;
            self.want_sessions = Some(String::new());
            let _ = self.tx.send(Request::ListSessions);
            return;
        }
        self.finish_ready();
    }

    /// Everything after the backend is up and any startup pick is settled: the header, the
    /// session switch summary, a replayed transcript and the first prompt.
    pub fn finish_ready(&mut self) {
        let mut header = SessionHeaderCell::new(
            Some(self.model_name()),
            self.effort_label_for_header(),
            &self.cwd_display(),
        );
        header.version = self.config.version().to_string();
        let info = SessionInfoCell {
            header,
            tip: Some(TooltipCell::new(self.tip)),
        };
        let mut tail: Option<BoxedCell> = None;
        match self.switch.take() {
            Some(sw) => {
                let summary = self.summary_cell();
                self.last_assistant_text.clear();
                if sw.load && sw.clear {
                    // The same session cut back by `/rewind`: a fresh screen, its replay, no
                    // summary of a session that has not ended.
                    self.clear_screen_for_new_chat();
                    self.push_cell(Box::new(info));
                } else if sw.load {
                    self.push_cell(Box::new(info));
                    tail = summary;
                } else if sw.clear {
                    self.clear_screen_for_new_chat();
                    self.push_cell(Box::new(info));
                    if let Some(s) = summary {
                        self.push_cell(s);
                    }
                } else {
                    if let Some(s) = summary {
                        self.push_cell(s);
                    }
                    self.push_cell(Box::new(info));
                }
                self.usage = Usage::default();
            }
            None => {
                self.placeholder_header = None;
                self.push_cell(Box::new(info));
            }
        }
        for c in std::mem::take(&mut self.replay_cells) {
            self.push_cell(c);
        }
        if let Some(t) = tail {
            self.push_cell(t);
        }
        if let Some(w) = self.theme_warning.take() {
            self.push_cell(Box::new(crate::ui::history_cells::WarningCell { text: w }));
        }
        if let Some(text) = self.prefill_after_load.take() {
            self.push_cell(Box::new(InfoCell {
                text: "You\u{2019}re continuing from this point. Wizard restored the files and cut the conversation back to before the selected prompt.".into(),
                hint: None,
            }));
            self.pane.composer.set_text(&text);
        }
        if let Some(p) = self.opts.prompt.take() {
            self.submit_prompt(p);
        }
    }

    fn effort_label_for_header(&self) -> &str {
        match self.config.effort.as_str() {
            "" | "default" => "",
            e => e,
        }
    }

    pub fn sync_footer(&mut self) {
        self.pane.footer.model = self.model_name();
        self.pane.footer.effort = self.config.effort.clone();
    }

    fn summary_cell(&self) -> Option<BoxedCell> {
        let lines = self.exit_lines(false);
        if lines.is_empty() {
            None
        } else {
            Some(Box::new(SummaryCell { lines }))
        }
    }

    // ---- external editor ----------------------------------------------------------------------

    /// Ctrl+G: say what is about to happen in the footer; `launch_editor` runs after that frame.
    pub fn request_editor(&mut self) {
        if self.editor_requested {
            return;
        }
        self.editor_requested = true;
        self.pane.footer.hint_override = Some(crate::editor::HINT.into());
        self.request_draw();
    }

    /// Run `$VISUAL` or `$EDITOR` on the draft with the terminal handed over, then take the text
    /// back. Called from the loop, never from a key handler, so the hint is drawn first.
    pub fn launch_editor(&mut self) -> Result<()> {
        // The hint frame must be on screen before the editor covers it.
        self.draw()?;
        self.editor_requested = false;
        let cmd = match crate::editor::resolve_from_env() {
            Ok(c) => c,
            Err(e) => {
                self.finish_editor(Err(e));
                return Ok(());
            }
        };
        let seed = self.pane.composer.text_for_editor();
        let _ = self.term.writer().flush();
        if let Some(c) = &self.input_ctl {
            c.pause();
        }
        crate::term::modes::restore_after_exit();
        let result = crate::editor::run_editor(&seed, &cmd);
        let _ = crate::term::modes::set_modes(self.term.writer());
        crate::term::modes::flush_input();
        if let Some(c) = &self.input_ctl {
            c.resume();
        }
        // What the editor left on screen is not ours: paint the viewport from scratch.
        self.term.invalidate_viewport();
        self.finish_editor(result);
        Ok(())
    }

    /// The editor returned: apply its text, or say why not. Always clears the footer hint.
    pub fn finish_editor(&mut self, result: Result<String, crate::editor::EditorError>) {
        use crate::editor::EditorError;
        self.editor_requested = false;
        self.pane.footer.hint_override = None;
        match result {
            Ok(text) => self.pane.composer.apply_external_edit(&text),
            Err(EditorError::Missing) => self.push_cell(Box::new(ErrorCell {
                text: "Cannot open external editor: set $VISUAL or $EDITOR before starting Wizard."
                    .into(),
            })),
            Err(e) => self.push_cell(Box::new(ErrorCell {
                text: format!("Failed to open editor: {e}"),
            })),
        }
        self.request_draw();
    }

    // ---- turns --------------------------------------------------------------------------------

    pub fn set_running(&mut self, running: bool) {
        if running == self.running {
            return;
        }
        self.running = running;
        self.pane.set_task_running(running);
        self.turn_started = running.then(Instant::now);
        self.last_anim = Instant::now();
    }

    /// Send text to the backend as a new turn and show it as a user cell.
    pub fn submit_prompt(&mut self, text: String) {
        self.esc_primed = false;
        self.pane.footer.esc_hint = false;
        self.push_cell(Box::new(UserCell { text: text.clone() }));
        let send = self.outgoing(&text);
        // The status row waits for the backend's `TurnStart`, as Codex waits for its server.
        let _ = self.tx.send(Request::Prompt(send));
    }

    // ---- images -------------------------------------------------------------------------------

    /// Ctrl+V: the clipboard's image as an `[Image #N]` chip, or Codex's error line.
    pub fn paste_clipboard_image(&mut self) {
        match crate::images::clipboard_image_to_temp() {
            Ok(path) => self.pane.composer.attach_image(path),
            Err(e) => self.push_cell(Box::new(ErrorCell {
                text: format!("Failed to paste image: {e}"),
            })),
        }
        self.request_draw();
    }

    /// Remember what wizard should be sent for a message whose draft carried images.
    fn collect_images(&mut self, shown: &str) {
        let imgs = self.pane.composer.take_submitted_images();
        if imgs.is_empty() {
            return;
        }
        let mut send = shown.to_string();
        for i in &imgs {
            let p = i.path.display().to_string();
            let at = if p.chars().any(char::is_whitespace) {
                format!("@\"{p}\"")
            } else {
                format!("@{p}")
            };
            send = send.replacen(&i.placeholder, &at, 1);
        }
        self.image_sends.retain(|(s, _)| s != shown);
        self.image_sends.push((shown.to_string(), send));
    }

    /// The text for the backend: a message with image chips goes out with each chip as `@path`.
    /// The first time, say what that means.
    fn outgoing(&mut self, shown: &str) -> String {
        let Some(i) = self.image_sends.iter().position(|(s, _)| s == shown) else {
            return shown.to_string();
        };
        let (_, send) = self.image_sends.remove(i);
        if !self.image_note_shown {
            self.image_note_shown = true;
            self.push_cell(Box::new(InfoCell {
                text: "Wizard takes text only over ACP, so each [Image #N] is sent as @path. The model gets the path, not the picture.".into(),
                hint: None,
            }));
        }
        send
    }

    fn send_queued(&mut self) {
        if self.pane.queued.is_empty() {
            return;
        }
        let next = self.pane.queued.remove(0);
        if let Some(cmd) = next.strip_prefix('!') {
            self.run_shell(cmd.trim().to_string());
        } else if crate::commands::parse(&next).is_some() {
            self.run_slash(&next);
        } else {
            self.submit_prompt(next);
        }
    }

    pub fn interrupt(&mut self) {
        let _ = self.tx.send(Request::Cancel);
    }

    // ---- input --------------------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.last_input = Instant::now();
        if let Some(ov) = &mut self.overlay {
            match ov.handle_key(key) {
                OverlayResult::Pending => {}
                OverlayResult::Close => self.close_overlay(),
                OverlayResult::CloseWith(a) => {
                    self.close_overlay();
                    self.apply_action(a);
                }
            }
            self.request_draw();
            return;
        }
        // Anything but Esc resets the primed backtrack.
        if !matches!(key.code, KeyCode::Esc) {
            self.esc_primed = false;
            self.pane.footer.esc_hint = false;
        }
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.pane.view.is_some() || self.pane.composer.captures_ctrl_c() {
                    self.route_to_pane(key);
                } else if self.running {
                    self.interrupt();
                } else {
                    self.should_exit = true;
                }
            }
            KeyCode::Char('d')
                if ctrl && self.pane.view.is_none() && self.pane.composer.is_empty() =>
            {
                self.should_exit = true;
            }
            KeyCode::Char('g')
                if ctrl && self.pane.view.is_none() && !self.pane.composer.popup_active() =>
            {
                self.request_editor();
            }
            KeyCode::Char('v')
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.pane.view.is_none() =>
            {
                self.paste_clipboard_image();
            }
            KeyCode::Char('r')
                if key.modifiers.contains(KeyModifiers::ALT) && self.pane.view.is_none() =>
            {
                self.set_raw_output(!self.raw_output, false);
            }
            KeyCode::Char('o') if ctrl && self.pane.view.is_none() => self.copy_last_message(),
            KeyCode::Char('l') if ctrl => self.clear_ui(),
            KeyCode::Char('t') if ctrl => self.open_pager(),
            KeyCode::Esc if self.pane.view.is_none() && self.pane.composer.captures_escape(key) => {
                self.route_to_pane(key);
            }
            KeyCode::BackTab => self.toggle_plan_mode(),
            KeyCode::Char(',') | KeyCode::Char('.')
                if key.modifiers.contains(KeyModifiers::ALT) && self.pane.view.is_none() =>
            {
                self.step_effort(key.code == KeyCode::Char('.'));
            }
            // shift-up and shift-down are the second binding of the same two actions
            KeyCode::Up | KeyCode::Down
                if key.modifiers == KeyModifiers::SHIFT
                    && self.pane.view.is_none()
                    && !self.pane.composer.popup_active() =>
            {
                self.step_effort(key.code == KeyCode::Up);
            }
            // ctrl-/ toggles a side conversation; wizard's version needs a question
            KeyCode::Char('/' | '7')
                if ctrl && self.pane.view.is_none() && !self.pane.composer.popup_active() =>
            {
                self.run_slash("/side");
            }
            KeyCode::Esc if self.pane.view.is_none() => {
                if self.running {
                    self.interrupt();
                } else if self.pane.composer.is_empty() {
                    self.pane.composer.on_esc_idle();
                    if self.esc_primed {
                        self.esc_primed = false;
                        self.pane.footer.esc_hint = false;
                        self.backtrack();
                    } else if self.cells.iter().any(|c| c.is_user_message()) {
                        self.esc_primed = true;
                        self.pane.footer.esc_hint = true;
                    } else {
                        self.esc_primed = true;
                    }
                }
            }
            _ => self.route_to_pane(key),
        }
        self.request_draw();
    }

    fn route_to_pane(&mut self, key: KeyEvent) {
        match self.pane.handle_key(key) {
            PaneEvent::None => {}
            PaneEvent::Submit(t) => {
                self.collect_images(&t);
                self.submit_plain(t)
            }
            PaneEvent::Queue(t) => {
                self.collect_images(&t);
                self.pane.queued.push(t);
            }
            PaneEvent::Command(t) => {
                if self.run_slash(&t) == Outcome::Quit {
                    self.should_exit = true;
                }
            }
            PaneEvent::Shell(c) => self.run_shell(c),
            PaneEvent::Info(t) => self.push_cell(Box::new(InfoCell {
                text: t,
                hint: None,
            })),
            PaneEvent::Error(t) => self.push_cell(Box::new(ErrorCell { text: t })),
            PaneEvent::Action(a) => self.apply_action(a),
        }
    }

    /// Text from the composer: a steer while a turn runs, else a new turn. Slash commands never
    /// get here; the composer dispatches them itself.
    fn submit_plain(&mut self, text: String) {
        if self.running {
            self.pane.steers.push(text.clone());
            let send = self.outgoing(&text);
            let _ = self.tx.send(Request::Steer(send));
        } else {
            self.submit_prompt(text);
        }
    }

    /// Run `!cmd` now, even while a turn runs, and print the result above the composer.
    pub fn run_shell(&mut self, cmd: String) {
        if cmd.is_empty() {
            return;
        }
        let cwd = self.opts.cwd.clone();
        match &self.msg_tx {
            Some(tx) => {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let out = crate::ui::shell_cell::run_shell(&cwd, &cmd);
                    let _ = tx.send(Msg::Shell(out));
                });
            }
            None => {
                let out = crate::ui::shell_cell::run_shell(&cwd, &cmd);
                self.on_shell_output(out);
            }
        }
    }

    pub fn on_shell_output(&mut self, out: crate::ui::shell_cell::ShellOutput) {
        self.push_cell(Box::new(crate::ui::shell_cell::UserShellCell {
            result: out,
        }));
        self.push_cell(Box::new(crate::ui::shell_cell::RuleCell));
        self.request_draw();
    }

    /// Shift+Tab: flip between the default and plan modes. Wizard toggles with `/plan`.
    pub fn toggle_plan_mode(&mut self) {
        if self.pane.view.is_some() || self.running {
            return;
        }
        self.pane.footer.plan_mode = !self.pane.footer.plan_mode;
        let text = if self.pane.footer.plan_mode {
            "Plan mode on."
        } else {
            "Plan mode off."
        };
        let _ = self.tx.send(Request::Prompt("/plan".into()));
        self.push_cell(Box::new(InfoCell {
            text: text.into(),
            hint: None,
        }));
    }

    /// Alt+. and Alt+,: one step up or down the reasoning efforts wizard offers.
    pub fn step_effort(&mut self, up: bool) {
        let efforts = &self.config.efforts;
        if efforts.is_empty() {
            return;
        }
        let cur = efforts.iter().position(|e| *e == self.config.effort);
        let next = match (cur, up) {
            (Some(i), true) => (i + 1).min(efforts.len() - 1),
            (Some(i), false) => i.saturating_sub(1),
            (None, _) => 0,
        };
        if Some(next) != cur {
            let e = efforts[next].clone();
            let _ = self.tx.send(Request::SetEffort(e));
        }
    }

    /// Enter on a draft: slash command, steer while a turn runs, or a new turn.
    pub fn submit_text(&mut self, text: String) {
        if crate::commands::parse(&text).is_some_and(|(n, _)| !n.is_empty())
            && text.trim_start().starts_with('/')
        {
            match self.run_slash(&text) {
                Outcome::Quit => self.should_exit = true,
                Outcome::Done => {}
            }
            return;
        }
        if self.running {
            self.pane.steers.push(text.clone());
            let _ = self.tx.send(Request::Steer(text));
        } else {
            self.submit_prompt(text);
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some(ov) = &mut self.overlay {
            ov.handle_paste(text);
        } else if let Some(v) = &mut self.pane.view {
            v.handle_paste(text);
        } else {
            self.pane.composer.handle_paste(text);
        }
        self.request_draw();
    }

    pub fn apply_action(&mut self, a: AppAction) {
        match a {
            AppAction::SetModel(m) => {
                let _ = self.tx.send(Request::SetModel(m));
            }
            AppAction::SetEffort(e) => {
                let _ = self.tx.send(Request::SetEffort(e));
            }
            AppAction::SetMode(m) => {
                let _ = self.tx.send(Request::SetMode(m));
            }
            AppAction::ChangeModel { model, effort } => self.change_model(model, effort),
            AppAction::StartFresh => self.start_fresh(),
            AppAction::RenameSession(n) => self.rename_session(n),
            AppAction::Decide {
                id,
                allow,
                always,
                note,
            } => {
                let scope = if always {
                    agent_core::DecideScope::Always
                } else {
                    agent_core::DecideScope::Once
                };
                let _ = self.tx.send(Request::Decide {
                    id,
                    allow,
                    scope,
                    note,
                });
            }
            AppAction::LoadSession(id) => self.load_session(id),
            AppAction::Submit(t) => self.submit_text(t),
            AppAction::Info(t) => self.push_cell(Box::new(InfoCell {
                text: t,
                hint: None,
            })),
            AppAction::Error(t) => self.push_cell(Box::new(ErrorCell { text: t })),
            AppAction::OpenPager => self.open_pager(),
            AppAction::Decision(d) => crate::ui::approval::apply_decision(self, d),
            AppAction::EditPrompt { nth, text } => self.edit_previous_prompt(nth, text),
            AppAction::Rewind { turn, text } => self.start_rewind(turn, text),
            AppAction::SyntaxThemeSelected(name) => self.set_syntax_theme_choice(name),
            AppAction::StatusLineSet { items, colors } => self.set_status_line(items, colors),
            AppAction::TitleSet(items) => self.set_title_items(items),
            AppAction::ShowStatic { title, lines } => self.open_static(&title, lines),
            AppAction::InsertText(t) => self.pane.composer.insert_text(&t),
            AppAction::Quit => self.should_exit = true,
        }
    }

    // ---- overlays -----------------------------------------------------------------------------

    pub fn open_overlay(&mut self, ov: Box<dyn Overlay>) {
        if self.overlay.is_some() {
            return;
        }
        if !self.opts.no_alt_screen {
            let _ = self.term.enter_alt_screen(self.screen);
        }
        self.overlay = Some(ov);
        self.request_draw();
    }

    pub fn close_overlay(&mut self) {
        if self.overlay.take().is_some() {
            let _ = self.term.leave_alt_screen();
            if self.reflow_pending_overlay {
                self.reflow_pending_overlay = false;
                self.reflow_at = Some(Instant::now());
            }
            self.request_draw();
        }
    }

    // ---- clearing and replay ------------------------------------------------------------------

    /// Ctrl+L: wipe screen and scrollback, show only the header and the composer.
    pub fn clear_ui(&mut self) {
        self.term.clear_pending_history();
        let _ = self.term.clear_scrollback_and_screen();
        self.has_emitted_history = false;
        self.replay_from = self.cells.len();
        let mut header = SessionHeaderCell::new(
            Some(self.model_name()),
            self.effort_label_for_header(),
            &self.cwd_display(),
        );
        header.version = self.config.version().to_string();
        self.push_cell(Box::new(header));
    }

    fn clear_screen_for_new_chat(&mut self) {
        self.term.clear_pending_history();
        let _ = self.term.clear_scrollback_and_screen();
        self.has_emitted_history = false;
        self.replay_from = self.cells.len();
    }

    /// `/clear`: start a new chat; the screen is wiped once the backend confirms the new session.
    pub fn clear_and_new(&mut self) {
        self.switch = Some(SessionSwitch {
            clear: true,
            load: false,
        });
        let _ = self.tx.send(Request::NewSession);
    }

    pub fn new_session_cmd(&mut self) {
        self.switch = Some(SessionSwitch {
            clear: false,
            load: false,
        });
        let _ = self.tx.send(Request::NewSession);
    }

    /// Wipe and re-emit every stored cell at the current width (spec A.5.2), newest rows only.
    pub fn reflow_now(&mut self) {
        self.reflow_at = None;
        self.term.clear_pending_history();
        let _ = self.term.clear_scrollback_and_screen();
        self.has_emitted_history = false;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let start = self.replay_from.min(self.cells.len());
        let mut cells = std::mem::take(&mut self.cells);
        for cell in &cells[start..] {
            lines.extend(self.separated_lines(cell.as_ref()));
        }
        std::mem::swap(&mut self.cells, &mut cells);
        let w = self.screen.width as usize;
        if lines.len() > REFLOW_MAX_ROWS {
            let notice = Line::from("Earlier messages are available \u{2014} press ctrl + t to view the full transcript").dim();
            let rows = word_wrap_line(&notice, &WrapOpts::new(w));
            let keep = REFLOW_MAX_ROWS.saturating_sub(rows.len());
            let cut = lines.len() - keep;
            lines.drain(..cut);
            let mut with_notice = rows;
            with_notice.extend(lines);
            lines = with_notice;
        }
        if !lines.is_empty() {
            self.term.queue_history_with(lines, self.wrap_policy());
        }
        self.request_draw();
    }

    pub fn on_resize(&mut self, w: u16, h: u16) {
        let new = Size::new(w, h);
        if new == self.screen {
            return;
        }
        let width_changed = new.width != self.screen.width;
        let height_changed = new.height != self.screen.height;
        self.screen = new;
        if width_changed || height_changed {
            self.term.clear_pending_history();
            if self.overlay.is_some() {
                self.reflow_pending_overlay = true;
            } else {
                self.reflow_at = Some(Instant::now() + RESIZE_DEBOUNCE);
            }
        }
        self.request_draw();
    }

    // ---- drawing ------------------------------------------------------------------------------

    fn active_lines(&self, width: u16) -> Vec<Line<'static>> {
        if let Some(h) = &self.placeholder_header {
            return h.display_lines(width);
        }
        self.log.active_lines(width)
    }

    pub fn desired_height(&self) -> u16 {
        let w = self.screen.width;
        let active = self.active_lines(w).len() as u16;
        let active_h = if active > 0 { active + 1 } else { 0 };
        (active_h + 1 + self.pane.desired_height(w)).min(self.screen.height)
    }

    pub fn draw(&mut self) -> std::io::Result<()> {
        crate::ui::approval::open_next_if_idle(self);
        self.pane.footer.values = self.status_values();
        self.needs_draw = false;
        self.last_draw = Some(Instant::now());
        self.update_title();
        let screen = self.screen;
        if let Some(ov) = &mut self.overlay {
            let live = self.log.active_lines(screen.width);
            ov.sync_cells(&self.cells, &live, screen.width);
            return self.term.draw_alt(screen, |f| {
                let area = f.area();
                ov.render(area, f.buffer_mut());
            });
        }
        if let Some(st) = &mut self.pane.status {
            st.header = self.log.status_header().unwrap_or_else(|| "Working".into());
        }
        self.pane.status_hidden = self.log.text_streaming();
        let height = self.desired_height();
        let active = self.active_lines(screen.width);
        let pane = &mut self.pane;
        self.term.draw(height, screen, |f| {
            let area = f.area();
            if area.is_empty() {
                return;
            }
            let pane_h = (pane.desired_height(area.width) + 1).min(area.height);
            let active_room = area.height - pane_h;
            // The in-flight cell gets what the bottom pane leaves; it shows its last rows.
            if !active.is_empty() && active_room > 1 {
                let rows = active_room as usize - 1;
                let skip = active.len().saturating_sub(rows);
                for (i, l) in active.iter().skip(skip).enumerate() {
                    tuikit::paint::put_line(f.buffer_mut(), area.x, area.y + 1 + i as u16, l, area);
                }
            }
            let pane_area = Rect::new(
                area.x,
                area.bottom() - pane_h + 1,
                area.width,
                pane_h.saturating_sub(1),
            );
            let cursor = pane.render(pane_area, f.buffer_mut());
            if let Some(c) = cursor {
                f.set_cursor_position(c);
            }
            if pane.view.is_none() && pane.composer.editor.uses_vim_insert_cursor() {
                f.set_cursor_style(crossterm::cursor::SetCursorStyle::SteadyBar);
            }
        })
    }

    /// What the status line items and the terminal title read from, as of now.
    pub fn status_values(&mut self) -> crate::statusline::Values {
        if self
            .git_read
            .is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            self.git = crate::statusline::git_info(&self.opts.cwd);
            self.git_read = Some(Instant::now());
        }
        let u = &self.usage;
        let chat = self.config.mode == "chat";
        crate::statusline::Values {
            model: self.model_name(),
            effort: self.config.effort.clone(),
            cwd: self.cwd_display(),
            project: self.git.0.clone(),
            branch: self.git.1.clone(),
            run_state: if !self.ready {
                "Starting"
            } else if self.running {
                "Working"
            } else {
                "Ready"
            }
            .into(),
            permissions: if chat { "Chat mode" } else { "Full Access" }.into(),
            approval: "never".into(),
            context_percent_left: self.pane.footer.context_percent,
            context_window: (u.context_window > 0).then_some(u.context_window),
            used_tokens: u.input_tokens.saturating_sub(u.cached_tokens) + u.output_tokens,
            input_tokens: u.input_tokens.saturating_sub(u.cached_tokens),
            output_tokens: u.output_tokens,
            version: self.config.version().to_string(),
            session_id: self.session_id.clone(),
            thread_title: self
                .home
                .as_deref()
                .and_then(|h| crate::ui::pickers::names::get(h, &self.session_id)),
            raw_output: self.raw_output,
            task_progress: self.task_progress,
        }
    }

    fn action_required(&self) -> bool {
        self.pane.view.as_ref().is_some_and(|v| v.needs_action())
    }

    /// The terminal title now: the configured items (spec A.9). The activity item is the braille
    /// spinner while busy; while a view waits for the user the title is the alternating
    /// `Action Required` prefix and the other items.
    fn title_now(&mut self) -> String {
        let elapsed = self.started.elapsed().as_millis();
        let mut v = self.status_values();
        // The title's project name is the working directory's unless a repository gives one.
        if v.project.is_none() {
            v.project = self
                .opts
                .cwd
                .file_name()
                .map(|s| s.to_string_lossy().to_string());
        }
        let uses_activity = self
            .title_items
            .iter()
            .any(|i| crate::statusline::canonical(i) == "activity");
        if self.action_required() && uses_activity {
            let mark = if (elapsed / 1000).is_multiple_of(2) {
                "[ ! ]"
            } else {
                "[ . ]"
            };
            return crate::statusline::action_required_title(
                &format!("{mark} Action Required"),
                &self.title_items,
                &["run-state"],
                &v,
            );
        }
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spinner = (!self.ready || self.running).then(|| FRAMES[(elapsed / 100) as usize % 10]);
        crate::statusline::title_text(&self.title_items, spinner, &v)
    }

    /// Write the title again on the next frame (the items changed).
    pub fn reset_title(&mut self) {
        self.title = String::new();
    }

    fn update_title(&mut self) {
        let title = self.title_now();
        if title != self.title {
            let _ = crate::term::modes::set_title(self.term.writer(), &title);
            self.title = title;
        }
    }

    // ---- scheduling ---------------------------------------------------------------------------

    fn animating(&self) -> bool {
        self.overlay
            .as_ref()
            .is_some_and(|o| o.animation_interval().is_some())
            || ((self.running || !self.ready) && self.overlay.is_none())
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut min = |t: Instant| d = Some(d.map_or(t, |x: Instant| x.min(t)));
        if self.needs_draw {
            let at = self
                .last_draw
                .map_or_else(Instant::now, |l| l + MIN_FRAME_INTERVAL);
            min(at);
        }
        if let Some(t) = self.pane.composer.paste_burst_deadline() {
            min(t);
        }
        if self.animating() {
            min(self.last_anim + ANIMATION_INTERVAL);
        }
        if self.action_required() {
            // the `[ ! ]` / `[ . ]` prefix flips on whole seconds
            let secs = self.started.elapsed().as_secs() + 1;
            min(self.started + Duration::from_secs(secs));
        }
        if !self.pending_perms.is_empty() && self.pane.view.is_none() {
            // an approval is waiting for the composer to go idle
            min(self.last_input + crate::ui::approval::TYPING_IDLE);
        }
        if let Some(r) = self.reflow_at {
            min(r);
        }
        d
    }

    pub fn on_timer(&mut self) -> Result<()> {
        let now = Instant::now();
        if self.pane.composer.flush_paste_burst_if_due() {
            self.needs_draw = true;
        }
        if self.reflow_at.is_some_and(|r| r <= now) && self.overlay.is_none() {
            self.reflow_now();
        }
        if self.action_required() && self.title_now() != self.title {
            self.needs_draw = true;
        }
        if !self.pending_perms.is_empty() && self.pane.view.is_none() {
            self.needs_draw = true;
        }
        if self.animating() && now >= self.last_anim + ANIMATION_INTERVAL {
            self.last_anim = now;
            self.needs_draw = true;
        }
        self.draw_if_due(now)?;
        Ok(())
    }

    pub fn draw_if_due(&mut self, now: Instant) -> Result<()> {
        if self.needs_draw && self.last_draw.is_none_or(|l| now >= l + MIN_FRAME_INTERVAL) {
            self.draw()?;
        }
        Ok(())
    }

    // ---- exit ---------------------------------------------------------------------------------

    /// The lines printed after the terminal is restored (spec A.13.3). `color` adds the cyan SGR
    /// around the resume command.
    pub fn exit_lines(&self, color: bool) -> Vec<String> {
        let mut out = Vec::new();
        let u = &self.usage;
        let total = u.input_tokens.saturating_sub(u.cached_tokens) + u.output_tokens;
        if total != 0 {
            let non_cached = u.input_tokens.saturating_sub(u.cached_tokens);
            let mut s = format!(
                "Token usage: total={} input={}",
                group(total),
                group(non_cached)
            );
            if u.cached_tokens > 0 {
                s.push_str(&format!(" (+ {} cached)", group(u.cached_tokens)));
            }
            s.push_str(&format!(" output={}", group(u.output_tokens)));
            out.push(s);
        }
        if let Some(cmd) = self.resume_command() {
            let cmd = if color {
                format!("\x1b[36m{cmd}\x1b[39m")
            } else {
                cmd
            };
            out.push(format!("To continue this session, run {cmd}"));
        }
        out
    }

    /// `codexw resume <id>` when wizard has a non-empty session file for the current session.
    pub fn resume_command(&self) -> Option<String> {
        if self.session_id.is_empty() || self.switched {
            return None;
        }
        let home = self.home.as_deref()?;
        let path = PathBuf::from(home)
            .join(".wizard/sessions")
            .join(format!("{}.jsonl", self.session_id));
        let meta = std::fs::metadata(path).ok()?;
        (meta.is_file() && meta.len() > 0).then(|| {
            let name = crate::ui::pickers::names::get(home, &self.session_id);
            crate::ui::pickers::names::resume_hint(name.as_deref(), &self.session_id)
        })
    }

    /// Clear the viewport rows so the composer disappears and the cursor rests at their top.
    pub fn finish_screen(&mut self) {
        if self.overlay.is_some() {
            self.close_overlay();
        }
        let _ = self.term.clear();
        let _ = self.term.writer().flush();
        let _ = crate::term::modes::set_title(self.term.writer(), "");
        let _ = self.term.writer().flush();
        self.exit = ExitInfo {
            usage: self.usage.clone(),
            session_id: self.session_id.clone(),
        };
    }
}

/// `1234567` as `1,234,567`.
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn random_index() -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as usize / 13)
}

/// Home prefix as `~`.
pub fn display_path(p: &std::path::Path, home: Option<&str>) -> String {
    let s = p.display().to_string();
    match home {
        Some(h) if !h.is_empty() && (s == h || s.starts_with(&format!("{h}/"))) => {
            format!("~{}", &s[h.len()..])
        }
        _ => s,
    }
}

/// Plain history lines (the old session's summary after `/new`).
#[derive(Debug)]
pub struct SummaryCell {
    pub lines: Vec<String>,
}

impl HistoryCell for SummaryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines
            .iter()
            .map(|l| match l.split_once("run ") {
                Some((a, b)) if l.starts_with("To continue") => Line::from(vec![
                    Span::from(format!("{a}run ")),
                    Span::styled(
                        b.to_string(),
                        ratatui::style::Style::default().fg(ratatui::style::Color::Cyan),
                    ),
                ]),
                _ => Line::from(l.clone()),
            })
            .collect()
    }
}

/// A replayed session as cells: user messages, answers, reasoning and finished tool calls.
pub fn replay_cells(items: &[HistoryItem]) -> Vec<BoxedCell> {
    let mut log = ChatLog::new_replay();
    let mut out: Vec<BoxedCell> = Vec::new();
    for it in items {
        match it {
            HistoryItem::User(t) => {
                out.extend(log.apply(&Event::TurnEnd(StopReason::EndTurn)));
                out.push(Box::new(UserCell { text: t.clone() }));
            }
            HistoryItem::Assistant(t) => out.extend(log.apply(&Event::TextDelta(t.clone()))),
            HistoryItem::Thought(t) => out.extend(log.apply(&Event::ThoughtDelta(t.clone()))),
            HistoryItem::Tool(c) => out.extend(log.apply(&Event::Tool(c.clone()))),
        }
    }
    out.extend(log.apply(&Event::TurnEnd(StopReason::EndTurn)));
    out
}

/// Start the chosen backend and forward its events as app messages.
pub fn spawn_backend(
    cwd: PathBuf,
    resume: Option<String>,
    mock: bool,
    msg_tx: UnboundedSender<Msg>,
) -> Result<(UnboundedSender<Request>, tokio::task::JoinHandle<()>)> {
    use agent_core::Backend;
    let BackendHandle { tx, mut rx } = if mock {
        crate::fake::FakeBackend::spawn(cwd, resume)?
    } else {
        backend_wizard::WizardBackend::spawn(cwd, resume)?
    };
    let fwd = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if msg_tx.send(Msg::Backend(ev)).is_err() {
                break;
            }
        }
    });
    Ok((tx, fwd))
}

/// The event loop. Returns when the user quits or the terminal closes.
pub async fn run<W: Write + Send>(app: &mut App<W>, mut pump: EventPump<Msg>) -> Result<()> {
    app.draw()?;
    loop {
        let deadline = app.next_deadline();
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            ev = pump.next() => match ev {
                None => break,
                Some(TermEvent::Key(k)) => app.on_key(k),
                Some(TermEvent::Paste(s)) => app.on_paste(&s),
                Some(TermEvent::Resize(w, h)) => app.on_resize(w, h),
                Some(TermEvent::Msg(Msg::Backend(e))) => app.on_backend(e),
                Some(TermEvent::Msg(Msg::Shell(o))) => app.on_shell_output(o),
                Some(_) => {}
            },
            _ = sleep => {}
        }
        if app.should_exit {
            break;
        }
        app.on_timer()?;
        if app.editor_requested {
            app.launch_editor()?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn _palette_used(p: Palette) -> Palette {
    let _ = palette();
    p
}
