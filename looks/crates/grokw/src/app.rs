// OWNER: shared (state, update, event loop; touch with care, other builders depend on it)
//! App state, the message type, `update`, and the event loop.
//!
//! Everything the screen shows lives in [`App`]. Terminal input and backend events both become
//! calls on it (`on_key`, `update`); the loop draws only when something marked it dirty or an
//! animation deadline passed, so an idle app wakes for nothing (`next_wake`).

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_core::transcript::{Part, Role, Transcript};
use agent_core::{
    Backend, BackendHandle, Config, Event, NoticeLevel, Request, SessionInfo, SlashCommand,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use futures_util::FutureExt;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tuikit::editor::Editor;
use tuikit::term::{Event as TermEvent, EventPump};

use crate::term::{TermGuard, Terminal};

use crate::theme::Theme;
use crate::ui::anim::{Clock, Wake};
use crate::ui::dialogs::Modal;
use crate::ui::transcript::View;
use crate::ui::{self};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// What the welcome screen and `--version` print as the Grok Build version this clones.
pub const GROK_VERSION: &str = "1.0.24";
const MIN_FRAME: Duration = Duration::from_millis(16);

/// Messages that reach the app from tasks and the backend.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Backend(Event),
    Branch(Option<String>),
    Files(Vec<String>),
    /// Text for a system block from a background thread (`/doctor`'s wizard checks).
    Note(String),
    /// A `!cmd` finished: its block's id, the command, what it printed and whether it exited 0.
    Shell {
        id: String,
        cmd: String,
        output: String,
        ok: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Home,
    Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Prompt,
    Scrollback,
    /// The todo pane (Ctrl+T) has the keyboard.
    Todo,
}

/// Shift+Tab cycles Normal and Plan; auto and always-approve are not wizard concepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Plan,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    /// Tick at which it goes away; a frozen clock never reaches it.
    pub until: u64,
}

/// A destructive key armed by a first press; the same chord inside the window fires it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub action: PendingAction,
    pub chord: (KeyCode, KeyModifiers),
    pub until: Duration,
    /// Text of the bar while armed (`Ctrl+q:press again to quit`).
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingAction {
    Quit,
    NewSession,
}

/// The one-line banner above the composer (mode switches, tips).
#[derive(Clone, Debug)]
pub struct Banner {
    /// Runs of text; `true` is the bold accent (`/compact-mode` in a tip).
    pub runs: Vec<(String, bool)>,
    pub until: u64,
    /// Fades over the last nine ticks.
    pub fade: bool,
    /// `  Switched to mode: Plan` form: indented, one colour.
    pub mode: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HomeState {
    /// Highlighted menu row after Esc unfocuses the composer.
    pub menu_sel: Option<usize>,
    pub hover: Option<usize>,
    /// The composer is focused until Esc.
    pub unfocused: bool,
    pub tip: String,
    /// Replaces grokw's own announcement (title, message); tests pin it.
    pub announcement: Option<(String, String)>,
    /// Resume picker over the card (Ctrl+R).
    pub picker: Option<crate::ui::dialogs::ResumePicker>,
    /// What the startup probes found; the banner shows one (spec 2.12).
    pub warnings: Vec<crate::ui::welcome::StartupWarning>,
    /// The New Worktree dialog (`Ctrl+W`).
    pub worktree: Option<crate::ui::worktree::Dialog>,
}

#[derive(Clone, Debug, Default)]
pub struct TodoPane {
    pub open: bool,
    pub sel: usize,
    pub top: usize,
    pub hide_done: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Turn {
    /// Clock time the prompt was sent.
    pub started: Option<Duration>,
    /// Identifies the phase; the phase timer restarts when it changes.
    pub phase: String,
    pub phase_started: Duration,
    /// Set by an Esc during the turn: the toast shows; nothing is cancelled.
    pub cancelling: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AppOpts {
    pub cwd: PathBuf,
    pub continue_latest: bool,
    pub pick_session: bool,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub plan: bool,
    pub prompts: Vec<String>,
    pub mock: bool,
    /// Where prompt history and the tip cursor live; `None` keeps them in memory (tests).
    pub state_dir: Option<PathBuf>,
    /// Fullscreen, `--no-alt-screen` or `--minimal`.
    pub screen: crate::term::ScreenMode,
}

pub struct App {
    pub opts: AppOpts,
    pub tx: UnboundedSender<Request>,
    pub msg_tx: UnboundedSender<Msg>,
    pub theme: Theme,
    pub clock: Clock,
    pub size: (u16, u16),
    pub screen: Screen,
    pub focus: Focus,
    pub home: HomeState,

    pub tr: Transcript,
    pub view: View,
    pub config: Config,
    pub commands: Vec<SlashCommand>,
    pub sessions: Vec<SessionInfo>,
    pub files: Vec<String>,
    pub branch: Option<String>,
    pub titles: HashMap<String, String>,

    pub ed: Editor,
    pub shell_mode: bool,
    pub multiline: bool,
    pub mode: Mode,
    pub stash: Option<String>,
    /// Composer state beyond the text: chips, the history panel, stash flags, command recency.
    pub inp: crate::ui::composer::input::Input,
    /// Settings the modal remembers for rows grokw does not act on.
    pub prefs: crate::ui::dialogs::settings::Prefs,
    pub popup_sel: usize,
    pub popup_for: String,
    pub popup_closed: bool,
    pub queue: Vec<String>,
    /// Prompts typed this run, newest last, for the history panel.
    pub history: Vec<String>,

    pub turn: Turn,
    pub todo: TodoPane,
    /// A stream retry in progress: attempt number and the reply size when it was announced.
    pub retry: Option<(u32, usize)>,
    pub toast: Option<Toast>,
    pub banner: Option<Banner>,
    pub pending: Option<Pending>,
    pub esc_armed: Option<Duration>,
    pub modal: Option<Modal>,
    pub compact_mode: bool,
    pub timestamps: bool,
    pub vim_mode: bool,
    pub show_thinking: bool,
    /// Wall clock `(hour, minute)` for message stamps; tests pin it.
    pub fixed_hm: Option<(u8, u8)>,
    /// Wall clock for session ages; tests pin it.
    pub wall: Option<i64>,
    /// Message stamps by `(message, part)`; `None` for replayed turns, which have none.
    pub stamps: HashMap<(usize, usize), Option<(u8, u8)>>,
    /// Numbers `!cmd` blocks.
    pub shell_seq: usize,
    /// When each tool call was first seen, for how long it took.
    pub tool_t0: HashMap<String, Instant>,
    /// Transcript rows minimal mode has already written into the scrollback.
    pub min_committed: usize,
    pub cursor: Option<(u16, u16)>,
    /// Something in the last frame is animating (a running rail in view).
    pub anim_in_view: bool,

    pub dirty: bool,
    /// Wipe the screen and write the next frame in full.
    pub repaint: bool,
    pub quit: bool,
    /// Quitting for a `/ui` switch: the look taking over is where the session goes on, so
    /// no resume hint is left behind.
    pub switched: bool,
    pub ready: bool,
    pub focused: bool,
    /// Set when a first prompt from the command line has been sent.
    pub boot_prompt_sent: bool,
    /// The model and effort the user picked; wizard resets both on every session load.
    pub chosen_model: Option<String>,
    pub chosen_effort: Option<String>,
}

impl App {
    pub fn new(opts: AppOpts, tx: UnboundedSender<Request>, msg_tx: UnboundedSender<Msg>) -> App {
        let mut ed = Editor::new();
        ed.set_punct_wrap(false);
        let hist = load_history(opts.state_dir.as_deref());
        ed.set_history(hist.clone());
        let tip = crate::ui::welcome::next_tip(opts.state_dir.as_deref());
        let inp = crate::ui::composer::input::Input::new(opts.state_dir.as_deref());
        let prefs = crate::ui::dialogs::settings::Prefs::load(opts.state_dir.as_deref());
        let screen = if opts.prompts.is_empty()
            && !opts.continue_latest
            && opts.resume.is_none()
            && !opts.pick_session
        {
            Screen::Home
        } else {
            Screen::Session
        };
        let mut app = App {
            theme: Theme::from_env(opts.state_dir.as_deref()),
            clock: Clock::new(),
            size: (120, 36),
            screen,
            focus: Focus::Prompt,
            home: HomeState {
                tip,
                ..Default::default()
            },
            tr: Transcript::new(),
            view: View::default(),
            config: Config::default(),
            commands: Vec::new(),
            sessions: Vec::new(),
            files: Vec::new(),
            branch: None,
            titles: HashMap::new(),
            ed,
            shell_mode: false,
            multiline: false,
            mode: Mode::Normal,
            stash: None,
            inp,
            prefs,
            popup_sel: 0,
            popup_for: String::new(),
            popup_closed: false,
            queue: Vec::new(),
            history: hist,
            turn: Turn::default(),
            todo: TodoPane::default(),
            retry: None,
            toast: None,
            banner: None,
            pending: None,
            esc_armed: None,
            modal: None,
            compact_mode: false,
            timestamps: true,
            vim_mode: false,
            show_thinking: true,
            fixed_hm: None,
            wall: None,
            stamps: HashMap::new(),
            shell_seq: 0,
            tool_t0: HashMap::new(),
            min_committed: 0,
            cursor: None,
            anim_in_view: false,
            dirty: true,
            repaint: false,
            quit: false,
            switched: false,
            ready: false,
            focused: true,
            boot_prompt_sent: false,
            chosen_model: None,
            chosen_effort: None,
            opts,
            tx,
            msg_tx,
        };
        if app.opts.pick_session {
            app.screen = Screen::Home;
        }
        app
    }

    // ---- small queries ------------------------------------------------------------------

    pub fn busy(&self) -> bool {
        self.tr.busy
    }

    pub fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    pub fn tick(&self) -> u64 {
        self.clock.tick()
    }

    /// `~`-collapsed working directory.
    pub fn cwd_label(&self) -> String {
        let cwd = if self.config.cwd.is_empty() {
            self.opts.cwd.display().to_string()
        } else {
            self.config.cwd.clone()
        };
        match std::env::var("HOME") {
            Ok(h) if !h.is_empty() && cwd == h => "~".into(),
            Ok(h) if !h.is_empty() && cwd.starts_with(&format!("{h}/")) => {
                format!("~{}", &cwd[h.len()..])
            }
            _ => cwd,
        }
    }

    /// Name of the active model as the composer label shows it: `Grok 4.7 (high)`.
    pub fn model_label(&self) -> String {
        let name = self
            .config
            .models
            .iter()
            .find(|m| m.id == self.config.model)
            .map(|m| m.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                if self.config.model.is_empty() {
                    "unknown".to_string()
                } else {
                    self.config
                        .model
                        .rsplit_once('/')
                        .map_or(self.config.model.clone(), |(_, n)| n.to_string())
                }
            });
        match self.config.effort.as_str() {
            "" | "default" => name,
            e => format!("{name} ({e})"),
        }
    }

    /// Context tokens and window for the header counter, when both are known.
    pub fn context(&self) -> Option<(u64, u64)> {
        let u = &self.tr.usage;
        let window = if u.context_window > 0 {
            u.context_window
        } else {
            window_for(&self.config.model)?
        };
        Some((u.context_tokens, window))
    }

    /// How much of the open reply has arrived (text bytes and part count), to tell when a
    /// retry has produced something new.
    pub fn reply_size(&self) -> usize {
        self.tr
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && m.took.is_none())
            .map_or(0, |m| {
                m.parts
                    .iter()
                    .map(|p| match p {
                        Part::Text(t) => t.len() + 1,
                        _ => 1,
                    })
                    .sum()
            })
    }

    pub fn session_title(&self) -> Option<String> {
        self.titles.get(&self.tr.session_id).cloned()
    }

    pub fn unix_now(&self) -> i64 {
        self.wall.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        })
    }

    pub fn popup_open_state(&self) -> Option<crate::ui::composer::Popup> {
        crate::ui::composer::popup(self)
    }

    pub fn view_select_last(&mut self) {
        self.view.select_last();
    }

    pub fn view_select(&mut self, dir: i32) {
        self.view.select(dir);
    }

    pub fn view_fold(&mut self, expand: Option<bool>) {
        self.view.fold(expand);
    }

    pub fn resume_session(&mut self, id: String) {
        self.enter_session();
        self.send(Request::LoadSession(id));
        self.dirty = true;
    }

    // ---- `!cmd` ---------------------------------------------------------------------------

    /// Start a `Run (user)` block for `!cmd`: a closed assistant message holding one running
    /// execute call, so it never takes streamed text and ends no turn. Returns its id.
    pub fn begin_shell(&mut self, cmd: &str) -> String {
        self.shell_seq += 1;
        let id = format!("bang-{}", self.shell_seq);
        let call = agent_core::ToolCall {
            id: id.clone(),
            name: "execute".into(),
            kind: agent_core::ToolKind::Execute,
            title: cmd.to_string(),
            input: serde_json::json!({"command": cmd, "user": true}),
            status: agent_core::ToolStatus::Running,
            ..Default::default()
        };
        self.tr.messages.push(agent_core::transcript::Message {
            role: Role::Assistant,
            parts: vec![Part::Tool(call)],
            started: Instant::now(),
            took: Some(Duration::ZERO),
            stop: None,
            model: String::new(),
        });
        self.tr.rev += 1;
        self.view.to_bottom();
        self.dirty = true;
        id
    }

    fn finish_shell(&mut self, id: &str, cmd: &str, output: String, ok: bool) {
        let found = self.tr.messages.iter_mut().rev().find_map(|m| {
            m.parts.iter_mut().find_map(|p| match p {
                Part::Tool(c) if c.id == id => Some(c),
                _ => None,
            })
        });
        match found {
            Some(c) => {
                c.status = if ok {
                    agent_core::ToolStatus::Completed
                } else {
                    agent_core::ToolStatus::Failed
                };
                c.output = Some(output);
                self.tr.rev += 1;
            }
            // the session was replaced while it ran: say what it printed instead
            None => self.note(format!("$ {cmd}\n{}", output.trim_end())),
        }
        self.dirty = true;
    }

    // ---- toasts, banners, confirmation --------------------------------------------------

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast_for(text, 90);
    }

    pub fn toast_for(&mut self, text: impl Into<String>, ticks: u64) {
        self.toast = Some(Toast {
            text: text.into(),
            until: self.tick() + ticks,
        });
        self.dirty = true;
    }

    /// The mode-switch banner: two seconds, then a fade.
    pub fn mode_banner(&mut self, text: impl Into<String>) {
        self.banner = Some(Banner {
            runs: vec![(text.into(), false)],
            until: self.tick() + 69,
            fade: true,
            mode: true,
        });
        self.dirty = true;
    }

    /// A tip in the banner row for `ticks` ticks.
    pub fn tip(&mut self, runs: Vec<(String, bool)>, ticks: u64) {
        self.banner = Some(Banner {
            runs,
            until: self.tick() + ticks,
            fade: false,
            mode: false,
        });
        self.dirty = true;
    }

    /// A transcript line of the kind slash commands print.
    pub fn note(&mut self, text: impl Into<String>) {
        self.tr.apply(&Event::Notice {
            level: NoticeLevel::Info,
            text: text.into(),
        });
        self.dirty = true;
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.tr.apply(&Event::Notice {
            level: NoticeLevel::Warn,
            text: text.into(),
        });
        self.dirty = true;
    }

    /// Print what a command said and make sure the session screen is showing.
    pub fn enter_session(&mut self) {
        if self.screen == Screen::Home {
            self.screen = Screen::Session;
            self.dirty = true;
            // small terminals get the compact-mode tip for a few seconds
            if self.size.1 <= 24 {
                self.tip(
                    vec![
                        ("Tight on space? Try ".into(), false),
                        ("/compact-mode".into(), true),
                    ],
                    90,
                );
            }
        }
    }

    // ---- backend ------------------------------------------------------------------------

    pub fn send(&mut self, r: Request) {
        let _ = self.tx.send(r);
    }

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Backend(ev) => self.on_event(ev),
            Msg::Branch(b) => {
                self.branch = b;
                self.dirty = true;
            }
            Msg::Files(f) => self.files = f,
            Msg::Note(t) => self.note(t),
            Msg::Shell {
                id,
                cmd,
                output,
                ok,
            } => self.finish_shell(&id, &cmd, output, ok),
        }
    }

    pub fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        match &ev {
            Event::Ready { config, .. } => {
                let first = !self.ready;
                self.ready = true;
                self.config = config.clone();
                if first {
                    self.boot();
                } else {
                    // a session switch resets wizard's model and effort to its defaults
                    self.reapply_choices();
                }
            }
            Event::ConfigChanged(c) => self.config = c.clone(),
            Event::Commands(c) => {
                self.commands = c.clone();
                return;
            }
            Event::Sessions(s) => {
                self.sessions = s.clone();
                self.sessions_arrived();
                return;
            }
            Event::History { .. } => {
                self.tr.apply(&ev);
                self.view.reset();
                self.stamps.clear();
                // wizard does not time-stamp replayed messages
                for (mi, m) in self.tr.messages.iter().enumerate() {
                    self.stamps.insert((mi, 0), None);
                    for pi in 0..m.parts.len() {
                        self.stamps.insert((mi, pi), None);
                    }
                }
                self.enter_session();
                return;
            }
            Event::Tool(call) => {
                // wizard does not time its tools; the rows that say `completed in 6.4s` need it
                let now = Instant::now();
                let t0 = *self.tool_t0.entry(call.id.clone()).or_insert(now);
                let mut call = call.clone();
                if matches!(
                    call.status,
                    agent_core::ToolStatus::Completed | agent_core::ToolStatus::Failed
                ) {
                    let secs = now.duration_since(t0).as_secs_f64();
                    if call.input.is_null() {
                        call.input = serde_json::json!({});
                    }
                    if let Some(o) = call.input.as_object_mut() {
                        o.entry("grokw_secs").or_insert(serde_json::json!(secs));
                    }
                }
                self.tr.apply(&Event::Tool(call));
                return;
            }
            Event::TurnStart => {
                self.retry = None;
                if self.turn.started.is_none() {
                    self.turn.started = Some(self.now());
                }
                self.tr.apply(&ev);
                self.view.follow_if_flipped();
                return;
            }
            Event::TurnEnd(_) => {
                self.retry = None;
                self.tr.apply(&ev);
                self.turn = Turn::default();
                self.flush_queue();
                return;
            }
            Event::Fatal(_) => {
                self.tr.apply(&ev);
                self.turn = Turn::default();
                return;
            }
            Event::Notice { text, .. } if is_retry_notice(text) && self.busy() => {
                // wizard says its stream dropped and restarts: the turn row shows it, the
                // transcript keeps nothing
                let n = self.retry.map_or(1, |(n, _)| n + 1);
                self.retry = Some((n, self.reply_size()));
                return;
            }
            Event::Notice { text, .. }
                if agent_core::switched_look(text).is_some_and(|l| l != "grok") =>
            {
                // `/ui <look>`: wizard saved it; leaving hands the terminal to that look
                self.quit = true;
                self.switched = true;
                return;
            }
            Event::Notice { text, .. } => {
                // the plan toggle's own answer says which state it is in
                if let Some(plan) = agent_core::plan_mode(text) {
                    self.mode = if plan { Mode::Plan } else { Mode::Normal };
                }
            }
            _ => {}
        }
        self.tr.apply(&ev);
    }

    /// After a session load wizard is back on its default model and effort: put the user's back.
    pub fn reapply_choices(&mut self) {
        if let Some(m) = self.chosen_model.clone() {
            self.send(Request::SetModel(m));
        }
        if let Some(e) = self.chosen_effort.clone() {
            self.send(Request::SetEffort(e));
        }
    }

    /// First `Ready`: apply command-line choices and send the first prompt.
    fn boot(&mut self) {
        if let Some(m) = self.opts.model.clone() {
            self.chosen_model = Some(m.clone());
            self.send(Request::SetModel(m));
        }
        if let Some(e) = self.opts.effort.clone() {
            self.chosen_effort = Some(e.clone());
            self.send(Request::SetEffort(e));
        }
        // plan mode (or omakase) also comes over from the look `/ui` left
        let handed = agent_core::handed_plan();
        if self.opts.plan || handed.is_some() {
            self.send(Request::Prompt(handed.unwrap_or("/plan").into()));
            self.mode = Mode::Plan;
        }
        if self.opts.continue_latest {
            self.send(Request::ListSessions);
        }
        if self.opts.pick_session {
            self.open_resume();
        }
        if !self.boot_prompt_sent && !self.opts.prompts.is_empty() {
            self.boot_prompt_sent = true;
            let text = self.opts.prompts.join(" ");
            self.enter_session();
            self.send_prompt(text);
        }
    }

    fn sessions_arrived(&mut self) {
        if self.opts.continue_latest {
            self.opts.continue_latest = false;
            if let Some(s) = self.sessions.first() {
                let id = s.id.clone();
                self.send(Request::LoadSession(id));
            }
        }
        if let Some(Modal::Resume(p)) = self.modal.as_mut() {
            p.set_sessions(&self.sessions);
        }
        if let Some(p) = self.home.picker.as_mut() {
            p.set_sessions(&self.sessions);
        }
    }

    // ---- sending ------------------------------------------------------------------------

    /// Send a prompt now (the caller has checked the turn is idle, or wants it queued by the
    /// backend). Records the user message, history and the turn clock.
    pub fn send_prompt(&mut self, text: String) {
        self.enter_session();
        self.tr.push_user(&text);
        self.view.on_send(self.tr.messages.len());
        self.turn = Turn {
            started: Some(self.now()),
            phase: String::new(),
            phase_started: self.now(),
            cancelling: false,
        };
        self.tr.busy = true;
        self.history.push(text.clone());
        self.send(Request::Prompt(text));
        self.dirty = true;
    }

    /// Enter on a non-empty composer.
    pub fn submit(&mut self) {
        let text = self.ed.submit();
        let text = text.trim_end().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.shell_mode = false;
        self.popup_closed = false;
        self.dispatch(text);
    }

    pub fn dispatch(&mut self, text: String) {
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if crate::commands::is_known(self, name) {
                crate::commands::run(self, name, args.trim());
                return;
            }
        }
        if self.busy() {
            self.queue.push(text);
            self.dirty = true;
            return;
        }
        self.send_prompt(text);
    }

    /// The turn that just ended may have left prompts behind it: start the first.
    pub fn flush_queue(&mut self) {
        if self.busy() || self.queue.is_empty() {
            return;
        }
        let next = self.queue.remove(0);
        self.send_prompt(next);
    }

    pub fn open_resume(&mut self) {
        self.send(Request::ListSessions);
        let p = crate::ui::dialogs::ResumePicker {
            loading: true,
            ..Default::default()
        };
        if self.screen == Screen::Home {
            self.home.picker = Some(p);
        } else {
            self.modal = Some(Modal::Resume(p));
        }
        self.dirty = true;
    }

    pub fn new_session(&mut self) {
        if self.busy() {
            self.toast("Cancel the turn first");
            return;
        }
        self.send(Request::NewSession);
        self.tr = Transcript::new();
        self.tr.session_id.clear();
        self.view.reset();
        self.stamps.clear();
        self.queue.clear();
        self.ed.clear();
        self.mode = Mode::Normal;
        self.screen = Screen::Session;
        self.focus = Focus::Prompt;
        self.dirty = true;
    }

    pub fn go_home(&mut self) {
        self.screen = Screen::Home;
        self.home.picker = None;
        self.home.menu_sel = None;
        self.home.unfocused = false;
        self.focus = Focus::Prompt;
        self.dirty = true;
    }

    pub fn quit_now(&mut self) {
        self.quit = true;
    }

    // ---- loop hooks ---------------------------------------------------------------------

    pub fn on_resize(&mut self, w: u16, h: u16) {
        self.size = (w, h);
        self.dirty = true;
    }

    pub fn on_paste(&mut self, text: &str) {
        crate::keys::on_paste(self, text);
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        crate::keys::on_mouse(self, ev);
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        crate::keys::on_key(self, key);
    }

    /// Expire things whose time has come. Marks dirty when something visibly changed.
    pub fn on_tick(&mut self, _now: Instant) {
        let tick = self.tick();
        if self.toast.as_ref().is_some_and(|t| tick >= t.until) {
            self.toast = None;
            self.dirty = true;
        }
        if self.banner.as_ref().is_some_and(|b| tick >= b.until) {
            self.banner = None;
            self.dirty = true;
        }
        let now = self.now();
        if self.pending.as_ref().is_some_and(|p| now >= p.until) {
            self.pending = None;
            self.dirty = true;
        }
        if self.esc_armed.is_some_and(|t| now >= t) {
            self.esc_armed = None;
        }
        if self.animating() {
            self.dirty = true;
        }
    }

    /// Whether the next frame can differ from the last one for time alone.
    pub fn animating(&self) -> bool {
        if self.clock.is_frozen() {
            return false;
        }
        match self.screen {
            // the logo shimmers for as long as the welcome screen is up
            Screen::Home => true,
            Screen::Session => {
                self.busy() || self.anim_in_view || self.toast.is_some() || self.banner.is_some()
            }
        }
    }

    /// The next instant the loop must wake for, `None` when nothing is moving.
    pub fn next_wake(&self, _now: Instant) -> Option<Instant> {
        let mut w = Wake::none();
        if !self.clock.is_frozen() {
            match self.screen {
                Screen::Home => w.at(self.clock.next_frame(crate::ui::anim::SHIMMER_FPS)),
                Screen::Session => {
                    if self.busy() {
                        let every = if self.anim_in_view { 1 } else { 4 };
                        w.at(self.clock.next_tick_multiple(every));
                    }
                    if self.toast.is_some() || self.banner.is_some() {
                        w.at(self.clock.next_tick_multiple(1));
                    }
                }
            }
            if let Some(p) = &self.pending {
                w.at(self.clock.instant_at(p.until));
            }
            if let Some(t) = self.esc_armed {
                w.at(self.clock.instant_at(t));
            }
        }
        w.get()
    }

    // ---- exit ---------------------------------------------------------------------------

    /// What is printed after the alternate screen is left (spec 1.5.2).
    pub fn epilogue(&self, width: u16) -> String {
        let w = width as usize;
        let id = &self.tr.session_id;
        if id.is_empty() || self.switched {
            return String::new();
        }
        let user = self
            .tr
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(first_text);
        let reply = self
            .tr
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .and_then(first_text);
        let minimal = self.opts.screen == crate::term::ScreenMode::Minimal;
        let mut out = String::from("\n");
        // minimal leaves the conversation in the scrollback, so it prints no summary
        if !minimal && (user.is_some() || reply.is_some()) {
            let title = self
                .session_title()
                .or_else(|| {
                    user.as_ref().map(|u| {
                        let first = u.lines().next().unwrap_or("");
                        if first.chars().count() > 60 {
                            format!("{}...", first.chars().take(60).collect::<String>())
                        } else {
                            first.to_string()
                        }
                    })
                })
                .unwrap_or_else(|| format!("session {}", &id[..id.len().min(8)]));
            out.push_str(&tuikit::width::truncate(&title, w));
            out.push('\n');
            if let Some(u) = &user {
                let line = u.lines().next().unwrap_or("");
                out.push_str(&tuikit::width::truncate(&format!("> {line}"), w));
                out.push('\n');
            }
            if let Some(r) = &reply {
                let line = r.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                out.push_str(&tuikit::width::truncate(&format!("  {line}"), w));
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str("Resume this session with:\n");
        let flag = if minimal { "--minimal " } else { "" };
        out.push_str(&format!("  wizard-ui-grok {flag}--resume {id}\n"));
        out
    }
}

fn first_text(m: &agent_core::transcript::Message) -> Option<String> {
    m.parts.iter().find_map(|p| match p {
        Part::Text(t) if !t.trim().is_empty() => Some(t.clone()),
        _ => None,
    })
}

/// The line wizard injects when a provider stream drops and restarts.
fn is_retry_notice(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("stream dropped") || t.contains("retrying") || t.contains("restarts below")
}

/// Context window of a model when the backend does not say.
pub fn window_for(model: &str) -> Option<u64> {
    let m = model.to_lowercase();
    if m.contains("grok-4") || m.contains("grok 4") {
        Some(256_000)
    } else if m.contains("claude") || m.contains("sonnet") || m.contains("opus") {
        Some(200_000)
    } else if m.contains("gpt-5") {
        Some(400_000)
    } else {
        None
    }
}

fn load_history(dir: Option<&std::path::Path>) -> Vec<String> {
    dir.and_then(|d| std::fs::read_to_string(d.join("history.json")).ok())
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .unwrap_or_default()
}

pub fn save_history(dir: Option<&std::path::Path>, entries: &[String]) {
    let Some(dir) = dir else { return };
    let _ = std::fs::create_dir_all(dir);
    let keep: Vec<&String> = entries.iter().rev().take(200).collect::<Vec<_>>();
    let keep: Vec<&String> = keep.into_iter().rev().collect();
    if let Ok(t) = serde_json::to_string(&keep) {
        let _ = std::fs::write(dir.join("history.json"), t);
    }
}

/// Start the chosen backend and forward its events into `msg_tx`. Returns the request sender
/// and the forwarding task, which ends when the backend does.
pub fn spawn_backend(
    cwd: PathBuf,
    resume: Option<String>,
    mock: bool,
    msg_tx: UnboundedSender<Msg>,
) -> Result<(UnboundedSender<Request>, tokio::task::JoinHandle<()>)> {
    let BackendHandle { tx, mut rx } = if mock {
        agent_core::mock::MockBackend::spawn(cwd, resume)?
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

/// Current git branch, read from `.git/HEAD` walking up from `cwd`.
pub fn git_branch(cwd: &std::path::Path) -> Option<String> {
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        let head = if git.is_dir() {
            git.join("HEAD")
        } else if git.is_file() {
            // a linked worktree: `gitdir: <path>`
            let t = std::fs::read_to_string(&git).ok()?;
            let p = t.strip_prefix("gitdir:")?.trim();
            let p = if std::path::Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                dir.join(p)
            };
            p.join("HEAD")
        } else {
            continue;
        };
        let t = std::fs::read_to_string(head).ok()?;
        let t = t.trim();
        return Some(match t.strip_prefix("ref: refs/heads/") {
            Some(b) => b.to_string(),
            None => "detached".to_string(),
        });
    }
    None
}

/// Files and directories under `cwd` for the `@` picker (directories end in `/`).
pub fn list_files(cwd: &std::path::Path) -> Vec<String> {
    crate::ui::composer::at::index(cwd)
}

fn osc52_copy(text: &str) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = text.as_bytes();
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    let mut o = std::io::stdout();
    let _ = write!(o, "\x1b]52;c;{out}\x07");
    let _ = o.flush();
}

/// Copy text to the terminal's clipboard (OSC 52).
pub fn copy_to_clipboard(text: &str) {
    osc52_copy(text);
}

/// The last frame written and where the cursor was, so an identical frame writes nothing.
type Last = Option<(Buffer, Option<(u16, u16)>)>;

fn draw_frame(terminal: &mut Terminal, app: &mut App, last: &mut Last) -> std::io::Result<()> {
    use crate::term::ScreenMode;
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    // fullscreen owns the screen; the other modes own an inline viewport that may not start at
    // the top, so the frame is built at the origin and copied in
    let area = terminal.get_frame().area();
    let mut buf = Buffer::empty(Rect::new(0, 0, area.width, area.height));
    let mut commit = None;
    if app.opts.screen == ScreenMode::Minimal {
        app.size = (area.width, area.height);
        app.cursor = None;
        commit = ui::minimal::draw(&mut buf, app);
    } else {
        ui::draw(&mut buf, app);
    }
    let cursor = app.cursor;
    if commit.is_none()
        && last
            .as_ref()
            .is_some_and(|(b, c)| *b == buf && *c == cursor)
    {
        return Ok(());
    }
    let mut out = std::io::stdout();
    crossterm::queue!(out, BeginSynchronizedUpdate)?;
    if let Some(rows) = commit {
        // finished rows go above the live area, into the terminal's own scrollback
        terminal.insert_before(rows.area.height, |b| {
            let (ox, oy) = (b.area.x, b.area.y);
            for y in 0..rows.area.height {
                for x in 0..rows.area.width.min(b.area.width) {
                    b[(ox + x, oy + y)] = rows[(x, y)].clone();
                }
            }
        })?;
        *last = None;
    }
    let area = terminal.get_frame().area();
    terminal.draw(|f| {
        let b = f.buffer_mut();
        for y in 0..buf.area.height.min(area.height) {
            for x in 0..buf.area.width.min(area.width) {
                b[(area.x + x, area.y + y)] = buf[(x, y)].clone();
            }
        }
        if let Some((x, y)) = cursor {
            f.set_cursor_position((area.x + x, area.y + y));
        }
    })?;
    crossterm::execute!(out, EndSynchronizedUpdate)?;
    if app.opts.screen != ScreenMode::Fullscreen {
        crate::term::note_viewport_bottom(area.y + area.height.saturating_sub(1));
    }
    *last = Some((buf, cursor));
    Ok(())
}

/// Run until the app quits or the terminal closes.
pub async fn run(
    app: &mut App,
    terminal: &mut Terminal,
    guard: &mut TermGuard,
    msg_rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    let input = tuikit::input::Input::spawn()?;
    let input_ctl = input.control();
    let mut pump = EventPump::<Msg>::spawn_raw(input, Duration::ZERO, msg_rx);
    if let Ok((w, h)) = crossterm::terminal::size() {
        app.size = (w, h);
    }
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut last: Last = None;
    loop {
        let now = Instant::now();
        if app.repaint {
            // focus returned under a multiplexer: wipe and write everything again
            app.repaint = false;
            terminal.clear()?;
            last = None;
            app.dirty = true;
        }
        if app.dirty && now.duration_since(last_draw) >= MIN_FRAME {
            app.dirty = false;
            draw_frame(terminal, app, &mut last)?;
            last_draw = Instant::now();
        }
        if app.quit {
            break;
        }
        let deadline = if app.dirty {
            Some(last_draw + MIN_FRAME)
        } else {
            app.next_wake(now)
        };
        let first = match deadline {
            Some(d) => tokio::select! {
                e = pump.next() => e,
                _ = tokio::time::sleep_until(d.into()) => Some(TermEvent::Tick),
            },
            None => pump.next().await,
        };
        let Some(first) = first else { break };
        handle(app, first);
        // Take everything already queued so a burst of deltas costs one frame.
        while !app.quit {
            match pump.next().now_or_never() {
                Some(Some(e)) => handle(app, e),
                Some(None) => app.quit = true,
                None => break,
            }
        }
        // `$EDITOR` needs the terminal to itself: park the reader, hand it over, take it back
        if let Some(text) = app.inp.edit_request.take() {
            input_ctl.pause();
            let kitty = guard.suspend();
            let res = crate::ui::composer::input::run_editor(&text, &app.opts.cwd);
            let _ = guard.resume(kitty);
            input_ctl.resume();
            app.repaint = true;
            app.dirty = true;
            crate::ui::composer::input::edit_done(app, res);
        }
    }
    save_history(app.opts.state_dir.as_deref(), &app.history);
    Ok(())
}

fn handle(app: &mut App, ev: TermEvent<Msg>) {
    match ev {
        TermEvent::Key(k) => app.on_key(k),
        TermEvent::Mouse(m) => app.on_mouse(m),
        TermEvent::Paste(s) => app.on_paste(&s),
        TermEvent::Resize(w, h) => app.on_resize(w, h),
        TermEvent::FocusGained => {
            app.focused = true;
            app.repaint = true;
            app.dirty = true;
        }
        TermEvent::FocusLost => app.focused = false,
        TermEvent::ColorScheme(_) => {}
        TermEvent::Tick => {}
        TermEvent::Msg(m) => app.update(m),
    }
    // A wake with nothing to do must not repaint.
    app.on_tick(Instant::now());
}
