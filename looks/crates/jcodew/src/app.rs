//! App state, the message type, and the event loop.
//!
//! Everything the screen shows lives in [`App`]. Terminal input and backend events become calls
//! on it; the loop draws when something marked it dirty or an animation deadline passed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_core::transcript::{Part, Role, Transcript};
use agent_core::{
    Backend, BackendHandle, Config, Event, NoticeLevel, Request, SessionInfo, SlashCommand,
    StopReason, ToolStatus, Usage,
};
use anyhow::Result;
use crossterm::event::{KeyEvent, MouseEvent};
use futures_util::FutureExt;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tuikit::editor::Editor;
use tuikit::term::{Event as TermEvent, EventPump, TermGuard, Terminal};

use crate::ui;
use crate::ui::tools::DiffMode;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The name this look answers to in `[ui] skin`.
pub const LOOK: &str = "jcode";
const MIN_FRAME: Duration = Duration::from_millis(16);
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
const NOTICE_TIME: Duration = Duration::from_secs(3);

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Backend(Event),
    Git(GitInfo),
}

/// What the status line and the widgets show about the repository.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitInfo {
    pub branch: Option<String>,
    pub modified: usize,
    pub staged: usize,
    pub untracked: usize,
    pub ahead: usize,
    pub behind: usize,
    /// `(status letter, path, added, removed)` of dirty files.
    pub files: Vec<(char, String, usize, usize)>,
}

/// A clock tests can stop.
#[derive(Clone, Debug)]
pub struct Clock {
    start: Instant,
    frozen: Option<Duration>,
}

impl Default for Clock {
    fn default() -> Self {
        Clock {
            start: Instant::now(),
            frozen: None,
        }
    }
}

impl Clock {
    pub fn elapsed(&self) -> Duration {
        self.frozen.unwrap_or_else(|| self.start.elapsed())
    }

    pub fn freeze(&mut self, at: Duration) {
        self.frozen = Some(at);
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }
}

#[derive(Clone, Debug, Default)]
pub struct AppOpts {
    pub cwd: PathBuf,
    pub continue_latest: bool,
    pub pick_session: bool,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompts: Vec<String>,
    pub mock: bool,
    /// Where prompt history is kept; `None` keeps it in memory (tests).
    pub state_dir: Option<PathBuf>,
}

/// The turn in flight.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    /// Clock time the prompt was sent.
    pub started: Option<Duration>,
    /// The backend has opened the turn.
    pub open: bool,
    /// Session totals when it began, to report what this turn used.
    pub usage_before: Usage,
}

/// The dim line under a finished turn: how long it took and what it used.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Footer {
    pub took: Duration,
    pub tokens: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    Help { scroll: usize },
    Sessions { sel: usize, loading: bool },
}

/// What the last frame laid out, for keys that scroll by it.
#[derive(Clone, Debug, Default)]
pub struct ViewMetrics {
    pub total: usize,
    pub height: usize,
    /// First row of every user prompt.
    pub prompts: Vec<usize>,
}

pub struct App {
    pub opts: AppOpts,
    pub tx: UnboundedSender<Request>,
    pub msg_tx: UnboundedSender<Msg>,
    pub clock: Clock,
    pub size: (u16, u16),

    pub tr: Transcript,
    pub config: Config,
    pub commands: Vec<SlashCommand>,
    pub sessions: Vec<SessionInfo>,
    pub git: GitInfo,

    pub ed: Editor,
    pub stash: Option<String>,
    pub queue: Vec<String>,
    pub history: Vec<String>,
    pub sugg_sel: usize,
    /// The text Tab started cycling from, and where it is in the list.
    pub tab_base: Option<(String, usize)>,
    pub picker_sel: usize,
    /// Up and Down are walking the prompt history.
    pub hist_walk: bool,

    pub turn: Turn,
    pub footers: HashMap<usize, Footer>,
    /// The colour distance each prompt was first drawn with. jcode keeps the colour a prompt
    /// got when it was sent rather than fading it as later prompts arrive.
    pub prompt_distance: HashMap<usize, usize>,
    /// Message index of the turn whose token counts have not arrived yet.
    pub awaiting_usage: Option<usize>,
    /// Row to keep at the top while not following the tail.
    pub scroll: Option<usize>,
    pub bookmark: Option<usize>,
    pub view: ViewMetrics,
    /// `Ctrl+L`: how many messages are hidden behind a screen of blank rows.
    pub cleared_at: Option<usize>,
    pub notice: Option<(String, Duration)>,
    /// What a rarely used chord just did, or that a chord does nothing.
    pub hotkey_note: Option<(String, Duration)>,
    pub hotkey_uses: HashMap<String, u32>,
    pub unknown_notes: HashMap<String, u32>,
    pub last_unknown_note: Option<Duration>,
    pub quit_armed: Option<Duration>,
    pub overlay: Option<Overlay>,
    pub diff_mode: DiffMode,
    pub todos_pinned: bool,
    pub cursor: Option<(u16, u16)>,

    pub dirty: bool,
    pub quit: bool,
    /// Quitting for a `/ui` switch: the look taking over is where the session goes on, so
    /// no resume hint is left behind.
    pub switched: bool,
    /// `/plan` or `/omakase` to send on the first session: the look `/ui` left was planning.
    handed_plan: Option<&'static str>,
    pub ready: bool,
    pub boot_prompt_sent: bool,
    pub chosen_model: Option<String>,
    pub chosen_effort: Option<String>,
}

impl App {
    pub fn new(opts: AppOpts, tx: UnboundedSender<Request>, msg_tx: UnboundedSender<Msg>) -> App {
        let mut ed = Editor::new();
        let hist = load_history(opts.state_dir.as_deref());
        ed.set_history(hist.clone());
        App {
            clock: Clock::default(),
            size: (120, 40),
            tr: Transcript::new(),
            config: Config::default(),
            commands: Vec::new(),
            sessions: Vec::new(),
            git: GitInfo::default(),
            ed,
            stash: None,
            queue: Vec::new(),
            history: hist,
            sugg_sel: 0,
            tab_base: None,
            picker_sel: 0,
            hist_walk: false,
            turn: Turn::default(),
            footers: HashMap::new(),
            prompt_distance: HashMap::new(),
            awaiting_usage: None,
            scroll: None,
            bookmark: None,
            view: ViewMetrics::default(),
            cleared_at: None,
            notice: None,
            hotkey_note: None,
            hotkey_uses: crate::hotkeys::load(opts.state_dir.as_deref()),
            unknown_notes: HashMap::new(),
            last_unknown_note: None,
            quit_armed: None,
            overlay: None,
            diff_mode: DiffMode::default(),
            todos_pinned: true,
            cursor: None,
            dirty: true,
            quit: false,
            switched: false,
            handed_plan: agent_core::handed_plan(),
            ready: false,
            boot_prompt_sent: false,
            chosen_model: None,
            chosen_effort: None,
            opts,
            tx,
            msg_tx,
        }
    }

    pub fn busy(&self) -> bool {
        self.tr.busy
    }

    pub fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    pub fn cwd(&self) -> String {
        if self.config.cwd.is_empty() {
            self.opts.cwd.display().to_string()
        } else {
            self.config.cwd.clone()
        }
    }

    /// The active model's option, when the backend listed it.
    pub fn model_option(&self) -> Option<&agent_core::ModelOption> {
        self.config
            .models
            .iter()
            .find(|m| m.id == self.config.model)
    }

    pub fn provider(&self) -> String {
        self.model_option()
            .map(|m| m.provider.clone())
            .filter(|p| !p.is_empty())
            .or_else(|| {
                self.config
                    .model
                    .split_once('/')
                    .map(|(p, _)| p.to_string())
            })
            .unwrap_or_default()
    }

    /// The model's bare name: `grok-4.7`.
    pub fn model_name(&self) -> String {
        let name = self
            .model_option()
            .map(|m| m.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.config.model.clone());
        match name.rsplit_once('/') {
            Some((_, n)) if self.model_option().is_none() => n.to_string(),
            _ => name,
        }
    }

    pub fn user_count(&self) -> usize {
        self.tr
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count()
    }

    /// Context tokens and window, once the backend has reported a size.
    pub fn context(&self) -> Option<(u64, u64)> {
        let u = &self.tr.usage;
        if u.context_tokens == 0 {
            return None;
        }
        let window = if u.context_window > 0 {
            u.context_window
        } else {
            window_for(&self.config.model)
        };
        Some((u.context_tokens, window))
    }

    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), self.now() + NOTICE_TIME));
        self.dirty = true;
    }

    pub fn system(&mut self, text: impl Into<String>) {
        self.tr.apply(&Event::Notice {
            level: NoticeLevel::Info,
            text: text.into(),
        });
        self.dirty = true;
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.tr.apply(&Event::Notice {
            level: NoticeLevel::Error,
            text: text.into(),
        });
        self.dirty = true;
    }

    pub fn send(&mut self, r: Request) {
        let _ = self.tx.send(r);
    }

    pub fn follow_bottom(&mut self) {
        self.scroll = None;
        self.dirty = true;
    }

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Backend(ev) => self.on_event(ev),
            Msg::Git(g) => {
                self.git = g;
                self.dirty = true;
            }
        }
    }

    pub fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        match &ev {
            Event::Ready { config, .. } => {
                let first = !self.ready;
                self.ready = true;
                self.config = config.clone();
                self.tr.apply(&ev);
                if let Some(cmd) = self.handed_plan.take() {
                    self.send(Request::Prompt(cmd.into()));
                }
                if first {
                    self.boot();
                } else {
                    self.reapply_choices();
                }
                return;
            }
            Event::ConfigChanged(c) => {
                // A switch made on wizard's side (`/provider use`, `/model x`) is the choice now.
                if self.chosen_model.is_some() {
                    self.chosen_model = Some(c.model.clone());
                }
                if self.chosen_effort.is_some() {
                    self.chosen_effort = Some(c.effort.clone());
                }
                self.config = c.clone();
            }
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
                self.footers.clear();
                self.prompt_distance.clear();
                self.cleared_at = None;
                self.scroll = None;
                return;
            }
            Event::TurnStart => {
                if self.turn.started.is_none() {
                    self.turn.started = Some(self.now());
                    self.turn.usage_before = self.tr.usage.clone();
                }
                self.turn.open = true;
            }
            Event::Usage(u) => {
                if let Some(idx) = self.awaiting_usage.take() {
                    let before = &self.turn.usage_before;
                    let tokens = (
                        u.input_tokens.saturating_sub(before.input_tokens),
                        u.output_tokens.saturating_sub(before.output_tokens),
                    );
                    if let Some(f) = self.footers.get_mut(&idx) {
                        if tokens != (0, 0) {
                            f.tokens = Some(tokens);
                        }
                    }
                }
            }
            Event::TurnEnd(reason) => {
                let reason = *reason;
                self.tr.apply(&ev);
                self.finish_turn(reason);
                if reason == StopReason::Cancelled {
                    self.system("Interrupted");
                }
                self.flush_queue();
                self.refresh_git();
                return;
            }
            Event::Fatal(_) => {
                self.tr.apply(&ev);
                self.turn = Turn::default();
                return;
            }
            Event::Notice { text, .. }
                if agent_core::switched_look(text).is_some_and(|l| l != LOOK) =>
            {
                // `/ui <look>`: wizard saved it; leaving hands the terminal to that look
                self.quit = true;
                self.switched = true;
                return;
            }
            _ => {}
        }
        self.tr.apply(&ev);
    }

    /// Record the footer of the turn that just closed.
    fn finish_turn(&mut self, reason: StopReason) {
        let turn = std::mem::take(&mut self.turn);
        let Some(started) = turn.started else { return };
        let idx = self
            .tr
            .messages
            .iter()
            .rposition(|m| m.role == Role::Assistant);
        let has_reply = idx.is_some_and(|i| !self.tr.messages[i].parts.is_empty());
        if let (Some(idx), true, StopReason::EndTurn) = (idx, has_reply, reason) {
            let u = &self.tr.usage;
            let tokens = (
                u.input_tokens
                    .saturating_sub(turn.usage_before.input_tokens),
                u.output_tokens
                    .saturating_sub(turn.usage_before.output_tokens),
            );
            self.footers.insert(
                idx,
                Footer {
                    took: self.now().saturating_sub(started),
                    tokens: (tokens != (0, 0)).then_some(tokens),
                },
            );
            if tokens == (0, 0) {
                // wizard reports the totals a moment after the turn closes
                self.awaiting_usage = Some(idx);
                self.turn.usage_before = turn.usage_before;
            }
        }
    }

    pub fn reapply_choices(&mut self) {
        if let Some(m) = self.chosen_model.clone() {
            self.send(Request::SetModel(m));
        }
        if let Some(e) = self.chosen_effort.clone() {
            self.send(Request::SetEffort(e));
        }
    }

    fn boot(&mut self) {
        if let Some(m) = self.opts.model.clone() {
            self.chosen_model = Some(m.clone());
            self.send(Request::SetModel(m));
        }
        if let Some(e) = self.opts.effort.clone() {
            self.chosen_effort = Some(e.clone());
            self.send(Request::SetEffort(e));
        }
        if self.opts.continue_latest {
            self.send(Request::ListSessions);
        }
        if self.opts.pick_session {
            self.open_sessions();
        }
        if !self.boot_prompt_sent && !self.opts.prompts.is_empty() {
            self.boot_prompt_sent = true;
            let text = self.opts.prompts.join(" ");
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
        if let Some(Overlay::Sessions { loading, sel }) = self.overlay.as_mut() {
            *loading = false;
            *sel = (*sel).min(self.sessions.len().saturating_sub(1));
        }
    }

    pub fn open_sessions(&mut self) {
        self.send(Request::ListSessions);
        self.overlay = Some(Overlay::Sessions {
            sel: 0,
            loading: true,
        });
        self.dirty = true;
    }

    pub fn send_prompt(&mut self, text: String) {
        self.tr.push_user(&text);
        let n = self.user_count();
        self.prompt_distance.insert(n, 1 + self.queue.len());
        self.turn = Turn {
            started: Some(self.now()),
            open: false,
            usage_before: self.tr.usage.clone(),
        };
        self.tr.busy = true;
        self.scroll = None;
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
        self.history.push(text.clone());
        self.sugg_sel = 0;
        self.tab_base = None;
        self.dispatch(text);
    }

    pub fn dispatch(&mut self, text: String) {
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if crate::commands::run_local(self, name, args.trim()) {
                return;
            }
            // wizard answers its own commands with a notice; they are not prompts
            self.send(Request::Prompt(text));
            self.scroll = None;
            self.dirty = true;
            return;
        }
        if self.busy() {
            self.queue.push(text);
            self.dirty = true;
            return;
        }
        self.send_prompt(text);
    }

    pub fn flush_queue(&mut self) {
        if self.busy() || self.queue.is_empty() {
            return;
        }
        let next = self.queue.remove(0);
        self.send_prompt(next);
    }

    /// `/clear`: a fresh session, an empty screen.
    pub fn new_session(&mut self) {
        if self.busy() {
            self.send(Request::Cancel);
        }
        self.send(Request::NewSession);
        self.tr = Transcript::new();
        self.footers.clear();
        self.prompt_distance.clear();
        self.queue.clear();
        self.cleared_at = None;
        self.scroll = None;
        self.turn = Turn::default();
        self.set_notice("Session cleared");
    }

    pub fn interrupt(&mut self) {
        self.send(Request::Cancel);
        self.set_notice("Interrupting...");
    }

    /// Ctrl+C or Ctrl+D while idle: the second press inside two seconds quits.
    pub fn quit_request(&mut self) {
        let now = self.now();
        if self.quit_armed.is_some_and(|t| now < t) {
            self.quit = true;
        } else {
            self.quit_armed = Some(now + QUIT_TIMEOUT);
            self.set_notice("Press Ctrl+C again to quit");
        }
    }

    pub fn refresh_git(&self) {
        let cwd = self.opts.cwd.clone();
        let tx = self.msg_tx.clone();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(Msg::Git(git_info(&cwd)));
            });
        }
    }

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

    pub fn on_tick(&mut self) {
        let now = self.now();
        if self.notice.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.notice = None;
            self.dirty = true;
        }
        if self
            .hotkey_note
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            self.hotkey_note = None;
            self.dirty = true;
        }
        if self.quit_armed.is_some_and(|t| now >= t) {
            self.quit_armed = None;
        }
        if self.busy() && !self.clock.is_frozen() {
            self.dirty = true;
        }
    }

    /// The running call the status line names, if any.
    pub fn running_tool(&self) -> Option<&agent_core::ToolCall> {
        let m = self
            .tr
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && m.took.is_none())?;
        m.parts.iter().rev().find_map(|p| match p {
            Part::Tool(c) if matches!(c.status, ToolStatus::Running | ToolStatus::Pending) => {
                Some(c)
            }
            _ => None,
        })
    }

    /// The open reply has visible text or a finished call.
    pub fn streaming(&self) -> bool {
        self.tr
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && m.took.is_none())
            .is_some_and(|m| !m.parts.is_empty())
    }

    pub fn next_wake(&self) -> Option<Instant> {
        if self.clock.is_frozen() {
            return None;
        }
        let mut next: Option<Duration> = None;
        let mut at = |d: Duration| next = Some(next.map_or(d, |n| n.min(d)));
        if self.busy() {
            at(Duration::from_millis(80));
        }
        let now = self.now();
        for (_, until) in self.notice.iter().chain(self.hotkey_note.iter()) {
            at(until.saturating_sub(now) + Duration::from_millis(1));
        }
        if let Some(t) = self.quit_armed {
            at(t.saturating_sub(now) + Duration::from_millis(1));
        }
        if !self.busy() && self.overlay.is_none() {
            // the idle tip comes and goes on the status line
            at(Duration::from_secs(1));
        }
        next.map(|d| Instant::now() + d)
    }

    /// What is printed after the alternate screen is left.
    pub fn epilogue(&self) -> String {
        let id = &self.tr.session_id;
        if id.is_empty() || self.user_count() == 0 || self.switched {
            return String::new();
        }
        format!(
            "\n\x1b[33mSession \x1b[1m{id}\x1b[0m\x1b[33m - to resume:\x1b[0m\n  wizard-ui-jcode --resume {id}\n\n"
        )
    }
}

/// Context window of a model when the backend does not say: jcode's generic default.
pub fn window_for(model: &str) -> u64 {
    let m = model.to_lowercase();
    if m.contains("grok-4") {
        256_000
    } else if m.contains("gpt-5") {
        400_000
    } else {
        200_000
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
    let start = entries.len().saturating_sub(200);
    if let Ok(t) = serde_json::to_string(&entries[start..]) {
        let _ = std::fs::write(dir.join("history.json"), t);
    }
}

/// Branch, dirty counts and per-file line counts, read with `git`.
pub fn git_info(cwd: &std::path::Path) -> GitInfo {
    let run = |args: &[&str]| -> Option<String> {
        let out = tuikit::git::command(cwd).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let mut g = GitInfo::default();
    let Some(status) = run(&["status", "--porcelain=v2", "--branch"]) else {
        return g;
    };
    for line in status.lines() {
        if let Some(b) = line.strip_prefix("# branch.head ") {
            g.branch = Some(b.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    g.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    g.behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(rest) = line.strip_prefix("? ") {
            g.untracked += 1;
            g.files.push(('?', rest.to_string(), 0, 0));
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let f: Vec<&str> = line.split(' ').collect();
            let xy = f.get(1).copied().unwrap_or("..");
            let path = if line.starts_with("1 ") {
                f.get(8..).map(|p| p.join(" "))
            } else {
                f.get(9..)
                    .map(|p| p.join(" ").split('\t').next().unwrap_or("").to_string())
            }
            .unwrap_or_default();
            let (x, y) = (
                xy.chars().next().unwrap_or('.'),
                xy.chars().nth(1).unwrap_or('.'),
            );
            if y != '.' {
                g.modified += 1;
            } else if x != '.' {
                g.staged += 1;
            }
            g.files.push((if y != '.' { y } else { x }, path, 0, 0));
        }
    }
    if let Some(numstat) = run(&["diff", "HEAD", "--numstat"]) {
        for line in numstat.lines() {
            let mut p = line.split('\t');
            let (a, r, path) = (p.next(), p.next(), p.next());
            if let (Some(a), Some(r), Some(path)) = (a, r, path) {
                if let Some(f) = g.files.iter_mut().find(|f| f.1 == path) {
                    f.2 = a.parse().unwrap_or(0);
                    f.3 = r.parse().unwrap_or(0);
                }
            }
        }
    }
    for f in g.files.iter_mut().filter(|f| f.0 == '?') {
        f.2 = std::fs::read_to_string(cwd.join(&f.1)).map_or(0, |t| t.lines().count());
    }
    g
}

/// Start the chosen backend and forward its events into `msg_tx`.
pub fn spawn_backend(
    cwd: PathBuf,
    resume: Option<String>,
    mock: bool,
    msg_tx: UnboundedSender<Msg>,
) -> Result<(UnboundedSender<Request>, tokio::task::JoinHandle<()>)> {
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

type Last = Option<(Buffer, Option<(u16, u16)>)>;

fn draw_frame(terminal: &mut Terminal, app: &mut App, last: &mut Last) -> std::io::Result<()> {
    let area = terminal.get_frame().area();
    let mut buf = Buffer::empty(Rect::new(0, 0, area.width, area.height));
    app.size = (area.width, area.height);
    ui::draw(&mut buf, app);
    let cursor = app.cursor;
    if last
        .as_ref()
        .is_some_and(|(b, c)| *b == buf && *c == cursor)
    {
        return Ok(());
    }
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
    *last = Some((buf, cursor));
    Ok(())
}

/// Run until the app quits or the terminal closes.
pub async fn run(
    app: &mut App,
    terminal: &mut Terminal,
    _guard: &mut TermGuard,
    msg_rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    let mut pump = EventPump::<Msg>::spawn(Duration::ZERO, msg_rx);
    if let Ok((w, h)) = crossterm::terminal::size() {
        app.size = (w, h);
    }
    app.refresh_git();
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut last: Last = None;
    loop {
        let now = Instant::now();
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
            app.next_wake()
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
        while !app.quit {
            match pump.next().now_or_never() {
                Some(Some(e)) => handle(app, e),
                Some(None) => app.quit = true,
                None => break,
            }
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
        TermEvent::Tick => app.dirty = app.dirty || !app.clock.is_frozen(),
        TermEvent::Msg(m) => app.update(m),
        _ => {}
    }
    app.on_tick();
}
