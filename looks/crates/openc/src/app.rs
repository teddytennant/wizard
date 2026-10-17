// OWNER: app
//! Application state, the message type and the update function. The loop in `main.rs` feeds
//! terminal events and backend messages in and draws when `dirty` is set; nothing here blocks:
//! requests to the backend go through an unbounded channel.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_core::{
    Config, DecideScope, Event, Request, SessionInfo, SlashCommand, StopReason, Todo,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc::UnboundedSender;
use tuikit::editor::Editor;
use tuikit::term::Event as TermEvent;
use tuikit::theme::Theme;

use crate::clip;
use crate::commands::{self, Cmd, Local};
use crate::keys::{self, Action, Scope};
use crate::palette::{Depth, Glyphs, Kind, Palette};
use crate::store;
use crate::term::Caps;
use crate::ui::blocks::WelcomeInfo;
use crate::ui::composer::{Composer, Out};
use crate::ui::dialogs::{self, OvOut, Overlay, SelKind};
use crate::ui::permission::PermPanel;
use crate::ui::resume::ResumeOut;
use crate::ui::row::Cx;
use crate::ui::status::StatusInfo;
use crate::ui::tools::diffview::DiffView;
use crate::ui::tools::rewind::RewindPanel;
use crate::ui::transcript::{age_text, Kind as BKind, MouseUp, Transcript, View};

pub const FRAME: Duration = Duration::from_millis(16);
const SPIN: Duration = Duration::from_millis(100);
const FLASH: Duration = Duration::from_millis(1500);
/// A warning is a sentence the reader has to finish, so it stays longer.
const FLASH_WARN: Duration = Duration::from_millis(4500);

fn flash_len(warn: bool) -> Duration {
    if warn {
        FLASH_WARN
    } else {
        FLASH
    }
}
pub const SCROLLBAR_FADE: Duration = Duration::from_millis(1500);
const ESC_ARM: Duration = Duration::from_millis(1500);
const QUIT_ARM: Duration = Duration::from_secs(2);
const DRAFT_QUIET: Duration = Duration::from_millis(500);

#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Backend(Event),
    Git(GitInfo),
    /// A chunk of the `@` file listing; `first` starts a new listing.
    Files {
        chunk: Vec<String>,
        first: bool,
    },
    /// Rows for an `@` query the search worker finished.
    FileHits {
        query: String,
        items: Vec<crate::ui::popup::PItem>,
    },
    /// The helpers of a copy have finished; the report is built where frames are drawn.
    Copied(crate::clipboard::Copied),
    /// The clipboard image read that `ctrl+v` started.
    ClipImage(Result<clip::Image, String>),
    /// Session rows for the resume dialog: this directory, or every project.
    Resume {
        all: bool,
        rows: Vec<backend_claude::sessions::SessionDetail>,
        /// No more chunks follow.
        last: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: String,
    pub dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Nav,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RailMode {
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeChoice {
    Auto,
    Hearth,
    Parchment,
}

impl ThemeChoice {
    pub fn name(self) -> &'static str {
        match self {
            ThemeChoice::Auto => "auto",
            ThemeChoice::Hearth => "hearth",
            ThemeChoice::Parchment => "parchment",
        }
    }

    pub fn parse(s: &str) -> Option<ThemeChoice> {
        match s {
            "auto" => Some(ThemeChoice::Auto),
            "hearth" => Some(ThemeChoice::Hearth),
            "parchment" => Some(ThemeChoice::Parchment),
            _ => None,
        }
    }
}

/// A key that types a character: no ctrl or alt.
fn is_text_key(k: &KeyEvent) -> bool {
    matches!(k.code, KeyCode::Char(_))
        && !k
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

pub struct Opts {
    pub cwd: PathBuf,
    pub theme: ThemeChoice,
    pub prompt: Option<String>,
    pub mouse: bool,
    pub spinner: bool,
}

/// Work the main loop must do with the terminal released.
pub enum External {
    Editor(String),
    Pager(String),
}

pub struct App {
    pub opts: Opts,
    pub caps: Caps,
    pub depth: Depth,
    pub p: Palette,
    pub theme: Theme,
    pub g: Glyphs,
    pub tx: UnboundedSender<Request>,
    pub app_tx: UnboundedSender<Msg>,
    pub start: Instant,

    pub cfg: Config,
    pub commands: Vec<SlashCommand>,
    pub cmds: Vec<Cmd>,
    cmd_sources: std::collections::HashMap<String, &'static str>,
    pub sessions: Vec<SessionInfo>,
    pub session_id: String,
    pub want_sessions: bool,

    pub tr: Transcript,
    pub view: View,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    pub composer: Composer,
    pub search_ed: Option<Editor>,
    pub queue: VecDeque<String>,
    /// Permission requests that arrived while another was on screen (parallel tool calls).
    pub perm_queue: VecDeque<agent_core::PermissionRequest>,
    pub todos: Vec<Todo>,
    pub todos_open: Option<bool>,
    pub perm: Option<PermPanel>,
    /// The latest printable key that went to the composer. A permission panel that appears
    /// while you are typing locks instead of taking your letters.
    pub last_typed: Option<Instant>,
    /// The backend reported a fatal error and has not said hello since.
    pub backend_down: bool,
    /// A message sent while it was down. If the restart fails too, it goes back to the composer
    /// instead of being lost.
    unsent: Option<String>,
    /// Full-screen diff viewer, when open.
    pub diffview: Option<DiffView>,
    pub rewind: Option<RewindPanel>,
    pub detail: bool,
    pub rail: RailMode,
    pub git: GitInfo,
    /// The `@` index while it is small enough to score inline; a big one lives in `file_search`
    /// and this keeps only its first entries.
    pub files: Vec<String>,
    pub file_search: Option<crate::files::FileSearch>,
    /// A clipboard image read is running on a worker.
    paste_busy: bool,
    pub welcome: WelcomeInfo,

    /// A prompt was sent and `TurnStart` has not come back yet.
    pub pending: bool,
    pub flash: Option<(String, Instant, bool)>,
    pub esc_armed: Option<Instant>,
    pub quit_armed: Option<Instant>,
    /// Which key armed the quit, for the message (`ctrl+c` or `ctrl+d`).
    pub quit_key: &'static str,
    pub focused: bool,
    pub mouse_on: bool,
    pub wheel: VecDeque<Instant>,

    pub size: (u16, u16),
    pub geo: crate::ui::Geo,
    pub dirty: bool,
    pub last_draw: Instant,
    pub should_quit: bool,
    pub suspend: bool,
    pub repaint: bool,
    pub external: Option<External>,
    pub theme_before_preview: Option<ThemeChoice>,
    pub theme_choice: ThemeChoice,
    /// The last scheme the terminal reported (mode 2031); beats the startup colour probe.
    pub scheme_dark: Option<bool>,
    pub last_frame_bytes: usize,
    /// When the draft is written next: a pause after the last key, so the file holds what is
    /// on screen and a killed process loses at most the last half second.
    draft_due: Option<Instant>,
}

fn trim_version(v: &str) -> String {
    let mut it = v.split('.');
    match (it.next(), it.next()) {
        (Some(a), Some(b)) => format!("{a}.{b}"),
        _ => v.to_string(),
    }
}

/// `Opus 5.5` -> `opus-5.5`, `Sonnet 5.5 (1M)` -> `sonnet-5.5 1m`.
pub fn model_display(cfg: &Config, id: &str) -> String {
    let name = cfg
        .models
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.name.clone());
    match name {
        Some(n) if id != "default" => {
            let n = n.to_lowercase();
            let (base, suffix) = match n.split_once('(') {
                Some((b, s)) => (
                    b.trim().to_string(),
                    format!(" {}", s.trim_end_matches(')').to_lowercase()),
                ),
                None => (n, String::new()),
            };
            format!("{}{}", base.replace(' ', "-"), suffix)
        }
        _ => id.trim_start_matches("claude-").to_string(),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", crate::clip::b64(text.as_bytes()))
}

impl App {
    pub fn new(
        opts: Opts,
        tx: UnboundedSender<Request>,
        app_tx: UnboundedSender<Msg>,
        caps: Caps,
        depth: Depth,
        mut g: Glyphs,
        size: (u16, u16),
    ) -> App {
        let kind = match opts.theme {
            ThemeChoice::Hearth => Kind::Hearth,
            ThemeChoice::Parchment => Kind::Parchment,
            ThemeChoice::Auto => {
                if caps.light_background() == Some(true) {
                    Kind::Parchment
                } else {
                    Kind::Hearth
                }
            }
        };
        // The glyph table says whether `…` can be drawn; truncation everywhere follows it.
        tuikit::width::set_ascii_ellipsis(g.ellipsis != "…");
        if !opts.spinner {
            g.spinner = if g.sep == "-" { &["."] } else { &["…"] };
        }
        let p = Palette::new(kind, depth);
        let theme = p.theme();
        let mut composer = Composer::new();
        let cwd = opts.cwd.to_string_lossy().into_owned();
        composer.ed.set_history(store::load_history(&cwd));
        let mouse_on = opts.mouse;
        let theme_choice = opts.theme;
        let cwd_path = opts.cwd.clone();
        let mut app = App {
            opts,
            caps,
            depth,
            p,
            theme,
            g,
            tx,
            app_tx,
            start: Instant::now(),
            cfg: Config::default(),
            commands: Vec::new(),
            cmds: Vec::new(),
            cmd_sources: commands::scan_sources(&cwd_path),
            sessions: Vec::new(),
            session_id: String::new(),
            want_sessions: false,
            tr: Transcript::new(),
            view: View::default(),
            focus: Focus::Composer,
            overlay: None,
            composer,
            search_ed: None,
            queue: VecDeque::new(),
            todos: Vec::new(),
            todos_open: None,
            perm: None,
            last_typed: None,
            backend_down: false,
            unsent: None,
            diffview: None,
            perm_queue: VecDeque::new(),
            rewind: None,
            detail: false,
            rail: RailMode::Auto,
            git: GitInfo::default(),
            files: Vec::new(),
            file_search: None,
            paste_busy: false,
            welcome: WelcomeInfo {
                version: trim_version(env!("CARGO_PKG_VERSION")),
                ..WelcomeInfo::default()
            },
            pending: false,
            flash: None,
            esc_armed: None,
            quit_armed: None,
            quit_key: "ctrl+c",
            focused: true,
            mouse_on,
            wheel: VecDeque::new(),
            size,
            geo: crate::ui::Geo::default(),
            dirty: true,
            last_draw: Instant::now() - Duration::from_secs(1),
            should_quit: false,
            suspend: false,
            repaint: false,
            external: None,
            theme_before_preview: None,
            theme_choice,
            scheme_dark: None,
            last_frame_bytes: 0,
            draft_due: None,
        };
        app.cmds = app.registry();
        app.refresh_welcome();
        app
    }

    // ---- derived state -------------------------------------------------------------------

    pub fn busy(&self) -> bool {
        self.tr.busy || self.pending
    }

    pub fn spin_index(&self, now: Instant) -> usize {
        (now.saturating_duration_since(self.start).as_millis() / SPIN.as_millis()) as usize
    }

    pub fn cwd_str(&self) -> String {
        if self.cfg.cwd.is_empty() {
            self.opts.cwd.to_string_lossy().into_owned()
        } else {
            self.cfg.cwd.clone()
        }
    }

    pub fn session_title(&self) -> String {
        if let Some(s) = self.sessions.iter().find(|s| s.id == self.session_id) {
            if !s.title.trim().is_empty() {
                return s.title.clone();
            }
        }
        self.tr
            .blocks
            .iter()
            .find_map(|b| match &b.kind {
                BKind::User { text, .. } => Some(text.lines().next().unwrap_or("").to_string()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn status_info(&self, now: Instant) -> StatusInfo {
        let u = &self.tr.usage;
        let flash = self
            .flash
            .as_ref()
            .filter(|(_, t, warn)| now < *t + flash_len(*warn))
            .map(|(m, _, warn)| (m.clone(), *warn));
        let flash = flash.or_else(|| {
            self.esc_armed
                .filter(|t| now < *t + ESC_ARM)
                .map(|_| ("esc again to interrupt".to_string(), true))
                .or_else(|| {
                    self.quit_armed
                        .filter(|t| now < *t + QUIT_ARM)
                        .map(|_| (format!("{} again to quit", self.quit_key), true))
                })
        });
        let search = self.tr.search_counts();
        StatusInfo {
            mode: self.cfg.mode.clone(),
            model: model_display(&self.cfg, &self.cfg.model),
            effort: self.cfg.effort.clone(),
            cwd: self.cwd_str(),
            branch: self.git.branch.clone(),
            dirty: self.git.dirty,
            ctx_tokens: u.context_tokens,
            ctx_window: u.context_window,
            cost: u.cost_usd,
            nav: self.focus == Focus::Nav,
            down: self.backend_down,
            busy: self.busy() && self.focused,
            // The transcript's own spinner is the one to watch while a tool row shows one.
            spinner: if self.tr.spinner_visible {
                self.g.busy
            } else {
                self.g.spinner[self.spin_index(now) % self.g.spinner.len()]
            },
            detail: self.detail,
            search,
            flash,
        }
    }

    pub fn refresh_welcome(&mut self) {
        let last = self
            .sessions
            .iter()
            .find(|s| s.id != self.session_id && !s.title.trim().is_empty())
            .map(|s| (s.title.replace('\n', " "), age_text(now_secs() - s.updated)));
        let cwd = self.cwd_str();
        self.welcome = WelcomeInfo {
            version: trim_version(env!("CARGO_PKG_VERSION")),
            backend: self.cfg.backend.clone(),
            model: model_display(&self.cfg, &self.cfg.model),
            effort: self.cfg.effort.clone(),
            cwd: crate::ui::status::short_path(&cwd, 48),
            branch: if self.git.dirty {
                format!("{}*", self.git.branch)
            } else {
                self.git.branch.clone()
            },
            last: if self.session_id.is_empty() {
                None
            } else {
                last
            },
        };
        self.tr.set_welcome(self.welcome.clone());
    }

    pub fn set_flash(&mut self, msg: impl Into<String>, warn: bool, now: Instant) {
        self.flash = Some((msg.into(), now, warn));
        self.dirty = true;
    }

    // ---- event entry points --------------------------------------------------------------

    pub fn handle(&mut self, ev: TermEvent<Msg>, now: Instant) {
        self.dirty = true;
        match ev {
            TermEvent::Key(k) => self.on_key(k, now),
            TermEvent::Mouse(m) => self.on_mouse(m, now),
            TermEvent::Paste(s) => self.on_paste(&s),
            // ratatui notices the new size on the next draw and clears and redraws by itself.
            TermEvent::Resize(w, h) => self.size = (w, h),
            TermEvent::FocusGained => self.focused = true,
            TermEvent::FocusLost => self.focused = false,
            TermEvent::ColorScheme(dark) => self.on_scheme(dark),
            TermEvent::Tick => {}
            TermEvent::Msg(m) => self.on_msg(m, now),
        }
    }

    pub fn on_msg(&mut self, m: Msg, now: Instant) {
        match m {
            Msg::Backend(ev) => self.on_backend(ev, now),
            Msg::Git(g) => {
                self.git = g;
                self.refresh_welcome();
            }
            Msg::Files { chunk, first } => {
                if first {
                    self.files.clear();
                    if let Some(f) = &self.file_search {
                        f.reset();
                    }
                }
                if let Some(f) = &self.file_search {
                    f.add(chunk.clone());
                }
                if self.files.len() <= crate::files::SYNC_LIMIT {
                    self.files.extend(chunk);
                }
                let big = self.files.len() > crate::files::SYNC_LIMIT;
                self.composer.remote_files = big && self.file_search.is_some();
                // A token typed before the listing finished sees the rows that arrive.
                self.composer.update_popup(&self.cmds, &self.files);
                self.pump_file_query();
            }
            Msg::FileHits { query, items } => self.composer.apply_hits(&query, items),
            Msg::Copied(c) => {
                let r = crate::clipboard::finish(c);
                self.set_flash(r.msg, r.warn, now);
            }
            Msg::ClipImage(r) => {
                self.paste_busy = false;
                match r {
                    Ok(img) => {
                        self.focus = Focus::Composer;
                        self.composer.paste_image(img);
                        self.draft_due = Some(now + DRAFT_QUIET);
                    }
                    Err(e) => self.set_flash(e, true, now),
                }
            }
            Msg::Resume { all, rows, last } => {
                if let Some(Overlay::Resume(d)) = self.overlay.as_mut() {
                    d.add_rows(rows.into_iter().map(Into::into).collect(), all, last);
                }
            }
        }
    }

    /// Timer wake: animations, flashes, the throttled frame.
    pub fn on_wake(&mut self, now: Instant) {
        if self.draft_due.is_some_and(|t| now >= t) {
            self.draft_due = None;
            self.save_draft();
        }
        if self.tr.retry_wake(now).is_some() {
            self.tr.tick_retry(now);
            self.dirty = true;
        }
        if self.animating(now) || self.flash_active(now) || self.rewind.is_some() {
            self.dirty = true;
        }
        // A message that has just run out must be drawn away once, or it stays on screen
        // until the next key.
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, t, warn)| now >= *t + flash_len(*warn))
        {
            self.flash = None;
            self.dirty = true;
        }
        if self.esc_armed.is_some_and(|t| now >= t + ESC_ARM) {
            self.esc_armed = None;
            self.dirty = true;
        }
        if self.quit_armed.is_some_and(|t| now >= t + QUIT_ARM) {
            self.quit_armed = None;
            self.dirty = true;
        }
        if self
            .view
            .scrolled_at
            .is_some_and(|t| now.saturating_duration_since(t) >= SCROLLBAR_FADE)
        {
            self.view.scrolled_at = None;
            self.dirty = true;
        }
    }

    fn flash_active(&self, now: Instant) -> bool {
        self.flash
            .as_ref()
            .is_some_and(|(_, t, warn)| now < *t + flash_len(*warn))
            || self.esc_armed.is_some_and(|t| now < t + ESC_ARM)
            || self.quit_armed.is_some_and(|t| now < t + QUIT_ARM)
    }

    pub fn animating(&self, _now: Instant) -> bool {
        self.focused && self.opts.spinner && (self.busy() || self.tr.animating())
    }

    /// When the loop should wake next with no input, or `None` to sleep until an event.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        let mut w: Option<Instant> = None;
        let mut take = |t: Instant| w = Some(w.map_or(t, |o| o.min(t)));
        if self.dirty {
            take((self.last_draw + FRAME).max(now));
        }
        if self.animating(now) {
            let n = self.spin_index(now) as u32 + 1;
            take(self.start + SPIN * n);
        }
        if self.flash_active(now) {
            let end = [
                self.flash.as_ref().map(|(_, t, w)| *t + flash_len(*w)),
                self.esc_armed.map(|t| t + ESC_ARM),
                self.quit_armed.map(|t| t + QUIT_ARM),
            ]
            .into_iter()
            .flatten()
            .filter(|t| *t > now)
            .min();
            if let Some(e) = end {
                take(e);
            }
        }
        if let Some(t) = self.draft_due {
            take(t.max(now));
        }
        if let Some(t) = self.perm.as_ref().and_then(|p| p.lock_end(now)) {
            take(t);
        }
        if let Some(t) = self.tr.retry_wake(now) {
            take(t);
        }
        if let Some(t) = self.rewind.as_ref().and_then(|r| r.wake_at(now)) {
            take(t);
        }
        if let Some(t) = self.view.scrolled_at {
            let end = t + SCROLLBAR_FADE;
            if end > now {
                take(end);
            }
        }
        // A half-typed word of a streaming answer shows after 120 ms of quiet.
        if let Some(b) = self.tr.blocks.last() {
            if let BKind::Assistant(a) = &b.kind {
                if a.streaming {
                    let at = a.last_delta
                        + Duration::from_millis(crate::ui::transcript::PARTIAL_WORD_MS as u64);
                    if at > now {
                        take(at);
                    }
                }
            }
        }
        w
    }

    // ---- backend events ------------------------------------------------------------------

    fn registry(&self) -> Vec<Cmd> {
        commands::registry_with(
            &self.commands,
            &self.cmd_sources,
            &store::load_recent_commands(),
        )
    }

    fn set_cfg(&mut self, c: &Config) {
        self.cfg = c.clone();
        self.refresh_welcome();
    }

    pub fn on_backend(&mut self, ev: Event, now: Instant) {
        match &ev {
            Event::Ready { session_id, config } => {
                self.backend_down = false;
                self.unsent = None;
                self.session_id = session_id.clone();
                self.set_cfg(config);
                let _ = self.tx.send(Request::ListSessions);
                if let Some(d) = store::take_draft(session_id) {
                    self.composer.set_text(&d);
                }
                if let Some(p) = self.opts.prompt.take() {
                    self.dispatch(p.clone(), p);
                }
            }
            Event::Commands(c) => {
                self.commands = c.clone();
                self.cmds = self.registry();
            }
            Event::ConfigChanged(c) => self.set_cfg(c),
            Event::Sessions(list) => {
                self.sessions = list.clone();
                self.refresh_welcome();
                if self.want_sessions {
                    self.want_sessions = false;
                    let mut d = self.resume_dialog();
                    d.set_rows(self.sessions.iter().map(Into::into).collect(), false);
                    d.set_rows(self.sessions.iter().map(Into::into).collect(), true);
                    self.overlay = Some(Overlay::Resume(Box::new(d)));
                }
            }
            Event::History { session_id, .. } => {
                self.session_id = session_id.clone();
                self.tr.apply(&ev, now);
                self.todos.clear();
                self.view = View::default();
                self.focus = Focus::Composer;
                self.pending = false;
                if let Some(d) = store::take_draft(session_id) {
                    if self.composer.is_empty() {
                        self.composer.set_text(&d);
                    }
                }
                self.refresh_welcome();
            }
            Event::Todos(t) => self.todos = t.clone(),
            Event::Permission(r) => {
                self.mark_awaiting(&r.tool, &r.input);
                if self.perm.is_some() {
                    self.perm_queue.push_back(r.clone());
                } else {
                    self.perm = Some(
                        PermPanel::new(r.clone(), now)
                            .with_cwd(&self.cwd_str())
                            .lock_for_typing(self.last_typed, now),
                    );
                }
            }
            Event::RewindPreview(p) => self.on_rewind_preview(p.clone()),
            Event::TurnStart => {
                self.backend_down = false;
                self.unsent = None;
                self.pending = false;
                self.tr.apply(&ev, now);
            }
            Event::TurnEnd(reason) => {
                self.pending = false;
                self.perm = None;
                self.perm_queue.clear();
                self.flash = None;
                self.tr.apply(&ev, now);
                self.spawn_git();
                if *reason != StopReason::Cancelled {
                    if let Some(next) = self.queue.pop_front() {
                        self.dispatch_queued(next);
                    }
                }
            }
            Event::Fatal(_) => {
                self.backend_down = true;
                self.pending = false;
                if let Some(t) = self.unsent.take() {
                    // The restart this message asked for failed too. It was never answered, so
                    // it goes back where you can see and resend it.
                    if self.composer.is_empty() {
                        self.composer.set_text(&t);
                        self.composer.ed.set_cursor(t.len());
                    } else {
                        self.queue.push_front(t);
                    }
                    self.set_flash("claude did not start; your message is back", true, now);
                }
                self.perm = None;
                self.perm_queue.clear();
                self.tr.apply(&ev, now);
            }
            _ => self.tr.apply(&ev, now),
        }
    }

    /// The editor opened with `ctrl+g` has exited. `new` is the file's text, `None` when the
    /// editor failed. Unchanged text keeps the composer as it was, paste chips included.
    pub fn editor_returned(&mut self, original: &str, new: Option<String>) {
        self.focus = Focus::Composer;
        let Some(new) = new else { return };
        let new = new.trim_end_matches('\n');
        if new != original.trim_end_matches('\n') {
            self.composer.set_text(new);
            self.composer.ed.set_cursor(new.len());
            self.draft_due = Some(Instant::now() + DRAFT_QUIET);
        }
    }

    pub fn spawn_git(&self) {
        // No runtime means a test driving the app by hand.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let cwd = self.opts.cwd.clone();
        let tx = self.app_tx.clone();
        tokio::task::spawn_blocking(move || {
            let run = |args: &[&str]| {
                tuikit::git::command(&cwd)
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
            };
            let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"])
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            let dirty =
                run(&["status", "--porcelain", "-uno"]).is_some_and(|o| !o.stdout.is_empty());
            let _ = tx.send(Msg::Git(GitInfo { branch, dirty }));
        });
    }

    pub fn resume_dialog(&self) -> crate::ui::resume::Resume {
        crate::ui::resume::Resume::new(
            &self.cwd_str(),
            &self.session_id,
            &self.git.branch,
            now_secs(),
        )
    }

    /// Read the session list off the main thread: this directory first, every project when
    /// `ctrl+a` asks for it.
    fn spawn_resume_rows(&self, all: bool) {
        let cwd = PathBuf::from(self.cwd_str());
        let tx = self.app_tx.clone();
        tokio::task::spawn_blocking(move || {
            let root = backend_claude::history::config_dir().join("projects");
            let (only, limit) = if all {
                (None, 300)
            } else {
                (Some(cwd.as_path()), 200)
            };
            backend_claude::sessions::list_details_with(&root, only, limit, &mut |rows| {
                let _ = tx.send(Msg::Resume {
                    all,
                    rows,
                    last: false,
                });
            });
            let _ = tx.send(Msg::Resume {
                all,
                rows: Vec::new(),
                last: true,
            });
        });
    }

    pub fn spawn_files(&mut self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let cwd = self.opts.cwd.clone();
        let tx = self.app_tx.clone();
        let reply = self.app_tx.clone();
        self.file_search = Some(crate::files::FileSearch::spawn(50, move |query, items| {
            let _ = reply.send(Msg::FileHits { query, items });
        }));
        tokio::task::spawn_blocking(move || {
            let mut first = true;
            crate::files::list(&cwd, &mut |chunk| {
                let _ = tx.send(Msg::Files { chunk, first });
                first = false;
            });
            if first {
                // Nothing listed: still replace whatever an earlier listing left.
                let _ = tx.send(Msg::Files {
                    chunk: Vec::new(),
                    first: true,
                });
            }
        });
    }

    /// Hand a query the composer could not score inline to the search worker.
    fn pump_file_query(&mut self) {
        if let Some(q) = self.composer.want_files.take() {
            if let Some(f) = self.file_search.as_mut() {
                f.query(q);
            }
        }
    }

    // ---- sending -------------------------------------------------------------------------

    /// Send a prompt. `shown` still carries chip labels, which is how the images attached to
    /// it are found again.
    fn dispatch(&mut self, text: String, shown: String) {
        if self.backend_down {
            self.unsent = Some(shown.clone());
        }
        let images = self.composer.images_in(&shown);
        self.tr.push_user(text.clone(), shown);
        self.pending = true;
        self.view.sticky = true;
        self.view.cursor = None;
        self.focus = Focus::Composer;
        let _ = self.tx.send(if images.is_empty() {
            Request::Prompt(text)
        } else {
            Request::PromptWith { text, images }
        });
    }

    /// The queue holds what was typed, chips and all; the text is expanded when it is sent.
    fn dispatch_queued(&mut self, shown: String) {
        let full = self.composer.expand_text(&shown);
        self.dispatch(full, shown);
    }

    fn send(&mut self, now: Instant) {
        let has_image = !self.composer.images().is_empty();
        let (full, shown) = self.composer.take();
        if full.trim().is_empty() && !has_image {
            // An empty composer sends the queue head after an interrupt.
            if !self.busy() {
                if let Some(next) = self.queue.pop_front() {
                    self.dispatch_queued(next);
                }
            }
            return;
        }
        store::append_history(&self.cwd_str(), &shown);
        if let Some((name, args)) = commands::parse(&full) {
            let known = self.cmds.iter().find(|c| c.name == name);
            let local = known.and_then(|c| c.local);
            if known.is_some() {
                store::note_command(name);
                self.cmds = self.registry();
            }
            if let Some(l) = local {
                let args = args.to_string();
                self.run_local(l, &args, now);
                return;
            }
        }
        if self.busy() {
            self.queue.push_back(shown);
            let n = self.queue.len();
            self.set_flash(format!("{n} queued"), false, now);
        } else {
            self.dispatch(full, shown);
        }
    }

    fn steer(&mut self, now: Instant) {
        let has_image = !self.composer.images().is_empty();
        let (full, shown) = self.composer.take();
        if full.trim().is_empty() && !has_image {
            return;
        }
        store::append_history(&self.cwd_str(), &shown);
        if self.busy() && has_image {
            // Steering carries text only, so a message with an image waits its turn.
            self.queue.push_back(shown);
            self.set_flash("images cannot steer, queued instead", true, now);
        } else if self.busy() {
            // The CLI folds the line into the running turn at the next tool boundary, so it
            // shows up here right away instead of waiting in the queue.
            self.tr.push_user(full.clone(), shown);
            let _ = self.tx.send(Request::Steer(full));
            self.view.sticky = true;
            self.set_flash("steering", false, now);
        } else {
            self.dispatch(full, shown);
        }
    }

    fn interrupt(&mut self, now: Instant) {
        if let Some(p) = self.perm.take() {
            let _ = self.tx.send(Request::Decide {
                id: p.req.id,
                allow: false,
                scope: DecideScope::Once,
                note: String::new(),
            });
        }
        // The backend refuses what is still waiting when it gets the cancel.
        self.perm_queue.clear();
        let _ = self.tx.send(Request::Cancel);
        self.esc_armed = None;
        self.set_flash("interrupting", true, now);
    }

    // ---- local commands ------------------------------------------------------------------

    fn run_local(&mut self, l: Local, args: &str, now: Instant) {
        match l {
            Local::Model | Local::Effort | Local::Mode => {
                if args.is_empty() {
                    let row = match l {
                        Local::Model => 0,
                        Local::Effort => 1,
                        _ => 2,
                    };
                    self.overlay = Some(Overlay::Settings { row });
                } else {
                    let req = match l {
                        Local::Model => Request::SetModel(args.to_string()),
                        Local::Effort => Request::SetEffort(args.to_string()),
                        _ => Request::SetMode(mode_from_word(args)),
                    };
                    let _ = self.tx.send(req);
                }
            }
            Local::Resume => {
                // CLAUDE_CONFIG_DIR set means the sessions are on disk wherever they are read.
                let on_disk = self.cfg.backend.starts_with("claude")
                    || std::env::var_os("CLAUDE_CONFIG_DIR").is_some();
                if on_disk && tokio::runtime::Handle::try_current().is_ok() {
                    // Claude keeps its sessions on disk, so the dialog reads them itself: that
                    // gives message counts, branches and every project, not only this one.
                    let d = self.resume_dialog();
                    self.overlay = Some(Overlay::Resume(Box::new(d)));
                    self.spawn_resume_rows(false);
                } else {
                    self.want_sessions = true;
                    let _ = self.tx.send(Request::ListSessions);
                }
            }
            Local::New => {
                self.tr.reset();
                self.queue.clear();
                self.todos.clear();
                self.view = View::default();
                let _ = self.tx.send(Request::NewSession);
            }
            Local::Exit => self.should_quit = true,
            Local::Theme => self.overlay = Some(dialogs::themes(self.theme_choice.name())),
            Local::Rail => self.toggle_rail(),
            Local::Detail => self.toggle_detail(),
            Local::Copy => {
                // `/copy` is the last answer, `/copy 2` the one before. Anything else is a typo
                // and copying something the argument did not name would put the wrong text on
                // the clipboard.
                let n = match args.trim() {
                    "" => 1,
                    a => match a.parse::<usize>() {
                        Ok(n) if n >= 1 => n,
                        _ => {
                            self.set_flash("usage: /copy [n], the nth last answer", true, now);
                            return;
                        }
                    },
                } - 1;
                match self.tr.last_assistant_text(n) {
                    Some(t) => self.copy(&t, now),
                    None => self.set_flash("nothing to copy", true, now),
                }
            }
            Local::Export => self.export(args, now),
            Local::Diff => self.open_diff(None),
            Local::Search => {
                self.tr.set_search(args);
                self.jump_match(None, now);
            }
            Local::Mouse => {
                let on = match args {
                    "on" => true,
                    "off" => false,
                    _ => !self.mouse_on,
                };
                self.mouse_on = on;
                self.set_flash(if on { "mouse on" } else { "mouse off" }, false, now);
            }
            Local::Redraw => self.repaint = true,
            Local::Keys => self.overlay = Some(Overlay::Help { scroll: 0 }),
        }
    }

    fn toggle_rail(&mut self) {
        let visible = self.geo.rail_rect.is_some();
        self.rail = if visible { RailMode::Off } else { RailMode::On };
    }

    fn toggle_detail(&mut self) {
        self.detail = !self.detail;
    }

    fn export(&mut self, arg: &str, now: Instant) {
        let name = if arg.is_empty() {
            let id: String = self.session_id.chars().take(8).collect();
            format!(
                "openc-{}.md",
                if id.is_empty() {
                    "session".to_string()
                } else {
                    id
                }
            )
        } else {
            arg.to_string()
        };
        let path = self.opts.cwd.join(&name);
        let mut s = String::new();
        for b in &self.tr.blocks {
            let t = b.text();
            if t.is_empty() {
                continue;
            }
            match &b.kind {
                BKind::User { .. } => s.push_str(&format!("## You\n\n{t}\n\n")),
                BKind::Tools(_) => s.push_str(&format!("```\n{t}```\n\n")),
                _ => s.push_str(&format!("{t}\n\n")),
            }
        }
        // An export holds everything said in the session, tool output included: private to you,
        // and never over a file that is already there.
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(s.as_bytes()));
        match written {
            Ok(()) => self.set_flash(format!("wrote {name}"), false, now),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => self.set_flash(
                format!("{name} exists, name a file that does not (/export <name>)"),
                true,
                now,
            ),
            Err(e) => self.set_flash(format!("export failed: {e}"), true, now),
        }
    }

    /// Copy to the clipboard. The helpers (tmux, wl-copy, xclip) can wait on a process for a
    /// second each, so they run on a worker and the report arrives as [`Msg::Copied`]. Without a
    /// runtime, or under test capture, it is done on the spot.
    pub fn copy(&mut self, text: &str, now: Instant) {
        if crate::clipboard::capturing() || tokio::runtime::Handle::try_current().is_err() {
            let r = crate::clipboard::copy(text, &self.caps);
            self.set_flash(r.msg, r.warn, now);
            return;
        }
        let (text, caps, tx) = (text.to_string(), self.caps.clone(), self.app_tx.clone());
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Msg::Copied(crate::clipboard::gather(&text, &caps)));
        });
    }

    /// Read the clipboard image on a worker: the tools wait up to 1.5 s each, several times.
    fn paste_clipboard_image(&mut self, now: Instant) {
        if tokio::runtime::Handle::try_current().is_err() {
            let r = clip::read_clipboard();
            self.on_msg(Msg::ClipImage(r), now);
            return;
        }
        if self.paste_busy {
            return;
        }
        self.paste_busy = true;
        self.set_flash("reading the clipboard", false, now);
        let tx = self.app_tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Msg::ClipImage(clip::read_clipboard()));
        });
    }

    // ---- keys ----------------------------------------------------------------------------

    fn cx_vh(&self) -> usize {
        self.geo.transcript.height as usize
    }

    pub fn on_paste(&mut self, s: &str) {
        if let Some(ov) = self.overlay.as_mut() {
            ov.paste(s);
            return;
        }
        if let Some(p) = self.perm.as_mut() {
            p.paste(s);
            return;
        }
        if let Some(ed) = self.search_ed.as_mut() {
            ed.paste(s);
            return;
        }
        self.focus = Focus::Composer;
        let cwd = self.opts.cwd.clone();
        self.composer.paste(s, &cwd);
        self.pump_file_query();
        self.draft_due = Some(Instant::now() + DRAFT_QUIET);
    }

    pub fn on_key(&mut self, key: KeyEvent, now: Instant) {
        self.tr.sel = None;
        // A repaint is a repaint wherever you are: in a dialog, on a prompt, in the viewer.
        if keys::lookup(Scope::Global, &key) == Some(Action::Repaint) {
            self.repaint = true;
            return;
        }
        if self.tools_key(key, now) {
            return;
        }
        // Overlays take everything.
        if let Some(ov) = self.overlay.as_mut() {
            let out = ov.on_key(key);
            self.on_overlay(out, now);
            return;
        }
        if let Some(panel) = self.perm.as_mut() {
            // ctrl+c still interrupts; scrolling keys keep working.
            if keys::lookup(Scope::Global, &key) == Some(Action::CtrlC) {
                self.interrupt(now);
                return;
            }
            if matches!(
                keys::lookup(Scope::Global, &key),
                Some(Action::PageUp | Action::PageDown)
            ) {
                self.act(keys::lookup(Scope::Global, &key).expect("checked"), now);
                return;
            }
            let out = panel.on_key(key, now);
            if out == crate::ui::permission::PermOut::Locked {
                // A letter meant for the next message goes to the draft and keeps the lock.
                // Enter, arrows and the rest do nothing.
                if is_text_key(&key) {
                    panel.extend_lock(now);
                    self.last_typed = Some(now);
                    let _ = self.composer.on_key(key, &self.cmds, &self.files);
                    self.pump_file_query();
                    self.draft_due = Some(now + DRAFT_QUIET);
                }
                return;
            }
            self.perm_out(out, now);
            return;
        }
        if self.search_ed.is_some() {
            self.on_search_key(key, now);
            return;
        }
        // Global chords first, but let the composer's popup see esc/tab/enter.
        if let Some(a) = keys::lookup(Scope::Global, &key) {
            // ctrl+p and ctrl+n move the popup selection while one is open.
            let popup_wants = self.focus == Focus::Composer
                && self.composer.popup.is_some()
                && matches!(a, Action::Esc | Action::Palette);
            if !popup_wants {
                // ctrl+d only quits from an empty composer.
                if !(a == Action::CtrlD
                    && self.focus == Focus::Composer
                    && !self.composer.is_empty())
                {
                    self.act(a, now);
                    return;
                }
            }
        }
        match self.focus {
            Focus::Nav => {
                if let Some(a) = keys::lookup(Scope::Nav, &key) {
                    self.act(a, now);
                }
            }
            Focus::Composer => {
                if let Some(a) = keys::lookup(Scope::Composer, &key) {
                    let empty = self.composer.is_empty();
                    let act_now = match a {
                        Action::Send | Action::Newline | Action::Complete => false,
                        // Only with nothing to edit; otherwise the editor moves the caret.
                        Action::PullQueue => empty && !self.queue.is_empty(),
                        Action::ScrollTop | Action::ScrollBottom => empty,
                        _ => true,
                    };
                    if act_now {
                        self.act(a, now);
                        return;
                    }
                }
                if is_text_key(&key) {
                    self.last_typed = Some(now);
                }
                let out = self.composer.on_key(key, &self.cmds, &self.files);
                self.pump_file_query();
                match out {
                    Out::Send => {
                        self.send(now);
                        // Enter ends the message, so whatever happens next is not an
                        // interruption of your typing.
                        self.last_typed = None;
                    }
                    Out::Newline | Out::None | Out::HistoryEdge => {}
                }
                self.draft_due = Some(now + DRAFT_QUIET);
            }
        }
    }

    fn on_search_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ed) = self.search_ed.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.search_ed = None;
                self.tr.search = None;
            }
            KeyCode::Enter => {
                let q = ed.text().to_string();
                self.search_ed = None;
                self.tr.set_search(&q);
                self.jump_match(None, now);
            }
            _ => {
                ed.apply_key(key);
                let q = ed.text().to_string();
                self.tr.set_search(&q);
                // Search as you type: the view follows the first hit.
                self.jump_match(None, now);
            }
        }
    }

    /// Go to a search hit: the first at or after the cursor (`None`), or the next or
    /// previous one (`Some(forward)`).
    fn jump_match(&mut self, step: Option<bool>, now: Instant) {
        if self.tr.search.is_none() {
            return;
        }
        let vh = self.cx_vh();
        let cx = Cx {
            p: &self.p,
            theme: &self.theme,
            g: &self.g,
            width: self.geo.c as usize,
            detail: self.detail,
            now,
            spin: 0,
        };
        let hit = match step {
            None => self.tr.search_jump(self.view.cursor.unwrap_or(0), &cx),
            Some(fwd) => self.tr.search_step(fwd, &cx),
        };
        let Some(hit) = hit else {
            self.set_flash("no matches", true, now);
            return;
        };
        self.view.cursor = Some(hit.block);
        self.focus = Focus::Nav;
        self.tr.reveal_hit(&mut self.view, hit, vh, &cx);
    }

    fn on_overlay(&mut self, out: OvOut, now: Instant) {
        match out {
            OvOut::None => {}
            OvOut::Close => {
                self.overlay = None;
                if let Some(t) = self.theme_before_preview.take() {
                    self.apply_theme(t);
                }
            }
            OvOut::Preview(name) => {
                if let Some(t) = ThemeChoice::parse(&name) {
                    if self.theme_before_preview.is_none() {
                        self.theme_before_preview = Some(self.theme_choice);
                    }
                    self.apply_theme_temp(t);
                }
            }
            OvOut::Step { row, delta } => self.step_setting(row, delta),
            OvOut::Resume(o) => self.on_resume(o, now),
            OvOut::Pick(kind, value) => {
                self.overlay = None;
                match kind {
                    SelKind::Palette => {
                        if let Some(name) = value.strip_prefix("act:") {
                            if let Some(bd) = keys::BINDINGS
                                .iter()
                                .find(|b| format!("{:?}", b.action) == name)
                            {
                                self.act(bd.action, now);
                            }
                        } else if let Some((name, args)) = commands::parse(&value) {
                            let local = self
                                .cmds
                                .iter()
                                .find(|c| c.name == name)
                                .and_then(|c| c.local);
                            match local {
                                Some(l) => self.run_local(l, args, now),
                                // A backend command goes into the composer for arguments.
                                None => {
                                    self.composer.set_text(&format!("{value} "));
                                    self.focus = Focus::Composer;
                                }
                            }
                        }
                    }
                    SelKind::Theme => {
                        if let Some(t) = ThemeChoice::parse(&value) {
                            self.theme_before_preview = None;
                            self.apply_theme(t);
                        }
                    }
                    SelKind::History => {
                        self.composer.set_text(&value);
                        self.focus = Focus::Composer;
                    }
                }
            }
        }
    }

    fn on_resume(&mut self, out: ResumeOut, now: Instant) {
        match out {
            ResumeOut::Pick { id, cwd, other_dir } => {
                self.overlay = None;
                if other_dir {
                    // The running claude is tied to this directory, so say how to get there.
                    let cmd = format!("cd {} && openc --resume {id}", shell_quote(&cwd));
                    self.copy(&cmd, now);
                    self.set_flash("other directory: command copied", false, now);
                } else if id != self.session_id {
                    self.tr.reset();
                    let _ = self.tx.send(Request::LoadSession(id));
                }
            }
            ResumeOut::NeedAll => self.spawn_resume_rows(true),
            ResumeOut::Rename { id, path, title } => {
                let res = backend_claude::sessions::rename(&path, &id, &title);
                if let Some(Overlay::Resume(d)) = self.overlay.as_mut() {
                    match res {
                        Ok(()) => d.renamed(&id, &title),
                        Err(e) => d.flash(format!("rename failed: {e}")),
                    }
                }
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
                    s.title = title;
                }
            }
            ResumeOut::Delete { id, path } => {
                if let Some(Overlay::Resume(d)) = self.overlay.as_mut() {
                    if id == self.session_id {
                        d.flash("this is the open session; leave it first");
                    } else {
                        match backend_claude::sessions::delete(&path) {
                            Ok(()) => d.deleted(&id),
                            Err(e) => d.flash(format!("delete failed: {e}")),
                        }
                    }
                }
                self.sessions.retain(|s| s.id != id);
            }
            ResumeOut::None | ResumeOut::Close => {}
        }
    }

    fn apply_theme_temp(&mut self, t: ThemeChoice) {
        let kind = match t {
            ThemeChoice::Hearth => Kind::Hearth,
            ThemeChoice::Parchment => Kind::Parchment,
            ThemeChoice::Auto => {
                if self
                    .scheme_dark
                    .map_or_else(|| self.caps.light_background() == Some(true), |dark| !dark)
                {
                    Kind::Parchment
                } else {
                    Kind::Hearth
                }
            }
        };
        self.p = Palette::new(kind, self.depth);
        self.theme = self.p.theme();
    }

    /// The terminal said its scheme is dark or light (mode 2031). Only `auto` follows it.
    pub fn on_scheme(&mut self, dark: bool) {
        self.scheme_dark = Some(dark);
        if self.theme_choice == ThemeChoice::Auto && self.theme_before_preview.is_none() {
            self.apply_theme_temp(ThemeChoice::Auto);
        }
    }

    pub fn apply_theme(&mut self, t: ThemeChoice) {
        self.theme_choice = t;
        self.apply_theme_temp(t);
    }

    fn step_setting(&mut self, row: usize, delta: i32) {
        let step = |list: &[String], cur: &str| -> Option<String> {
            if list.is_empty() {
                return None;
            }
            let i = list.iter().position(|x| x == cur).unwrap_or(0) as i32;
            let n = list.len() as i32;
            Some(list[((i + delta).rem_euclid(n)) as usize].clone())
        };
        match row {
            0 => {
                let ids: Vec<String> = self.cfg.models.iter().map(|m| m.id.clone()).collect();
                if let Some(m) = step(&ids, &self.cfg.model) {
                    let _ = self.tx.send(Request::SetModel(m));
                }
            }
            1 => {
                if let Some(e) = step(&self.cfg.efforts, &self.cfg.effort) {
                    let _ = self.tx.send(Request::SetEffort(e));
                }
            }
            _ => {
                if let Some(m) = step(&self.cfg.modes, &self.cfg.mode) {
                    let _ = self.tx.send(Request::SetMode(m));
                }
            }
        }
    }

    pub fn settings_view(&self) -> dialogs::SettingsView {
        let model = model_display(&self.cfg, &self.cfg.model);
        let effort = if self.cfg.effort.is_empty() {
            if self.cfg.efforts.is_empty() {
                "not supported".to_string()
            } else {
                "default".to_string()
            }
        } else {
            self.cfg.effort.clone()
        };
        let mode = crate::ui::status::mode_word(&self.cfg.mode).to_string();
        dialogs::SettingsView {
            rows: vec![
                ("model".into(), model, self.cfg.models.len() > 1),
                ("effort".into(), effort, !self.cfg.efforts.is_empty()),
                ("mode".into(), mode, self.cfg.modes.len() > 1),
            ],
            options: vec![
                self.cfg
                    .models
                    .iter()
                    .map(|m| (model_display(&self.cfg, &m.id), m.id == self.cfg.model))
                    .collect(),
                self.cfg
                    .efforts
                    .iter()
                    .map(|e| (e.clone(), *e == self.cfg.effort))
                    .collect(),
                self.cfg
                    .modes
                    .iter()
                    .map(|m| {
                        (
                            crate::ui::status::mode_word(m).to_string(),
                            *m == self.cfg.mode,
                        )
                    })
                    .collect(),
            ],
        }
    }

    fn cycle_mode(&mut self) {
        let order = ["default", "acceptEdits", "plan"];
        let avail: Vec<&str> = order
            .iter()
            .copied()
            .filter(|m| self.cfg.modes.iter().any(|x| x == m))
            .collect();
        if avail.is_empty() {
            // Not a Claude backend: walk whatever list it has.
            self.step_setting(2, 1);
            return;
        }
        let i = avail.iter().position(|m| *m == self.cfg.mode);
        let next = match i {
            Some(i) => avail[(i + 1) % avail.len()],
            None => avail[0],
        };
        let _ = self.tx.send(Request::SetMode(next.to_string()));
    }

    pub fn act(&mut self, a: Action, now: Instant) {
        let vh = self.cx_vh().max(1);
        macro_rules! cx {
            () => {
                Cx {
                    p: &self.p,
                    theme: &self.theme,
                    g: &self.g,
                    width: self.geo.c as usize,
                    detail: self.detail,
                    now,
                    spin: 0,
                }
            };
        }
        match a {
            Action::CtrlC => {
                if self.busy() {
                    self.interrupt(now);
                } else if !self.composer.is_empty() {
                    let (_, shown) = self.composer.take();
                    store::append_history(&self.cwd_str(), &shown);
                } else if self.quit_armed.is_some_and(|t| now < t + QUIT_ARM) {
                    self.should_quit = true;
                } else {
                    self.quit_armed = Some(now);
                    self.quit_key = "ctrl+c";
                }
            }
            Action::CtrlD => {
                if self.focus == Focus::Nav {
                    self.act(Action::HalfDown, now);
                } else if self.busy() && self.quit_armed.is_none_or(|t| now >= t + QUIT_ARM) {
                    self.quit_armed = Some(now);
                    self.quit_key = "ctrl+d";
                } else {
                    self.should_quit = true;
                }
            }
            Action::Esc => {
                if self.focus == Focus::Nav {
                    self.act(Action::ToComposer, now);
                } else if self.busy() {
                    if self.esc_armed.is_some_and(|t| now < t + ESC_ARM) {
                        self.interrupt(now);
                    } else {
                        self.esc_armed = Some(now);
                    }
                } else if self.tr.has_conversation() {
                    self.enter_nav(now);
                }
            }
            Action::Palette => self.overlay = Some(dialogs::palette(&self.cmds)),
            Action::Settings => self.overlay = Some(Overlay::Settings { row: 0 }),
            Action::CycleMode => self.cycle_mode(),
            Action::Detail => self.toggle_detail(),
            Action::Rail => self.toggle_rail(),
            Action::Repaint => self.repaint = true,
            Action::Suspend => self.suspend = true,
            Action::Help => self.overlay = Some(Overlay::Help { scroll: 0 }),
            Action::PageUp => {
                let cx = cx!();
                self.tr
                    .scroll_by(&mut self.view, -(vh as isize - 1), vh, &cx);
                self.focus = Focus::Nav;
            }
            Action::PageDown => {
                let cx = cx!();
                self.tr.scroll_by(&mut self.view, vh as isize - 1, vh, &cx);
            }
            Action::HalfDown => {
                let cx = cx!();
                self.tr
                    .scroll_by(&mut self.view, (vh / 2) as isize, vh, &cx);
            }
            Action::HalfUp => {
                let cx = cx!();
                self.tr
                    .scroll_by(&mut self.view, -((vh / 2) as isize), vh, &cx);
            }
            Action::Send => self.send(now),
            Action::Newline | Action::Complete | Action::Local => {}
            Action::PullQueue => {
                if let Some(q) = self.queue.pop_back() {
                    self.composer.set_text(&q);
                }
            }
            Action::ScrollTop => self.act(Action::NavTop, now),
            Action::ScrollBottom => self.act(Action::NavBottom, now),
            Action::Steer => self.steer(now),
            Action::HistorySearch => {
                let entries: Vec<String> = self
                    .composer
                    .ed
                    .history_entries()
                    .map(str::to_string)
                    .collect();
                self.overlay = Some(dialogs::history(entries.iter().map(String::as_str)));
            }
            Action::ExternalEditor => {
                self.external = Some(External::Editor(self.composer.expanded()))
            }
            Action::PasteImage => self.paste_clipboard_image(now),
            Action::NavDown | Action::NavUp => {
                let fwd = a == Action::NavDown;
                let cur = self
                    .view
                    .cursor
                    .unwrap_or(if fwd { 0 } else { self.tr.blocks.len() });
                let next = if self.view.cursor.is_none() && fwd {
                    // First j picks the first block on screen.
                    self.first_visible_block()
                } else {
                    self.tr
                        .find_from(cur, fwd, |b| !matches!(b.kind, BKind::Welcome(_)))
                };
                if let Some(n) = next {
                    self.view.cursor = Some(n);
                    let cx = cx!();
                    self.tr.reveal(&mut self.view, n, vh, &cx);
                }
            }
            Action::NavTop => {
                self.tr.to_top(&mut self.view);
                self.view.cursor = self
                    .tr
                    .find_from(0, true, |b| !matches!(b.kind, BKind::Welcome(_)));
                self.focus = Focus::Nav;
            }
            Action::NavBottom => {
                self.tr.to_bottom(&mut self.view);
                self.view.cursor = None;
                self.focus = Focus::Composer;
            }
            Action::PrevTurn | Action::NextTurn => {
                let fwd = a == Action::NextTurn;
                let cur = self
                    .view
                    .cursor
                    .unwrap_or(if fwd { 0 } else { self.tr.blocks.len() });
                if let Some(n) = self.tr.find_from(cur, fwd, |b| b.is_user()) {
                    self.view.cursor = Some(n);
                    let cx = cx!();
                    self.tr.reveal(&mut self.view, n, vh, &cx);
                }
            }
            Action::PrevTool | Action::NextTool => {
                let fwd = a == Action::NextTool;
                let cur = self
                    .view
                    .cursor
                    .unwrap_or(if fwd { 0 } else { self.tr.blocks.len() });
                if let Some(n) = self.tr.find_from(cur, fwd, |b| b.is_tool()) {
                    self.view.cursor = Some(n);
                    let cx = cx!();
                    self.tr.reveal(&mut self.view, n, vh, &cx);
                }
            }
            Action::Toggle => {
                if let Some(c) = self.view.cursor {
                    self.fold(c, false, now);
                }
            }
            Action::ToggleTurn => {
                if let Some(c) = self.view.cursor {
                    self.fold(c, true, now);
                }
            }
            Action::CopyBlock => {
                if let Some(t) = self
                    .view
                    .cursor
                    .and_then(|c| self.tr.blocks.get(c))
                    .map(|b| b.text())
                {
                    self.copy(&t, now);
                }
            }
            Action::CopyTurn => {
                if let Some(c) = self.view.cursor {
                    let t = self.tr.turn_text(c);
                    self.copy(&t, now);
                }
            }
            Action::Pager => {
                if let Some(t) = self
                    .view
                    .cursor
                    .and_then(|c| self.tr.blocks.get(c))
                    .map(|b| b.text())
                {
                    self.external = Some(External::Pager(t));
                }
            }
            Action::DiffView => self.open_diff(self.view.cursor),
            Action::FocusAgent => self.focus_agent(now),
            Action::Rewind => self.start_rewind(now),
            Action::Search => self.search_ed = Some(Editor::new()),
            Action::SearchNext | Action::SearchPrev => {
                self.jump_match(Some(a == Action::SearchNext), now);
            }
            Action::ToComposer => {
                self.focus = Focus::Composer;
                self.view.cursor = None;
                if self.tr.search.is_some() && self.search_ed.is_none() {
                    self.tr.search = None;
                }
            }
        }
    }

    /// Fold or unfold a block (or its whole turn) without moving the row you are looking at.
    fn fold(&mut self, block: usize, turn: bool, now: Instant) {
        let vh = self.cx_vh();
        let detail = self.detail;
        let cx = Cx {
            p: &self.p,
            theme: &self.theme,
            g: &self.g,
            width: self.geo.c as usize,
            detail,
            now,
            spin: 0,
        };
        self.tr.keep_row(&mut self.view, block, vh, &cx, |t| {
            if turn {
                t.toggle_turn(block, detail);
            } else {
                t.toggle(block, detail);
            }
        });
    }

    fn first_visible_block(&mut self) -> Option<usize> {
        self.tr
            .hits
            .iter()
            .map(|h| h.1)
            .find(|b| {
                !matches!(
                    self.tr.blocks.get(*b).map(|x| &x.kind),
                    Some(BKind::Welcome(_))
                )
            })
            .or_else(|| {
                self.tr
                    .find_from(0, true, |b| !matches!(b.kind, BKind::Welcome(_)))
            })
    }

    fn enter_nav(&mut self, now: Instant) {
        let vh = self.cx_vh().max(1);
        self.focus = Focus::Nav;
        // The transcript never autoscrolls in nav mode: pin the view where it is.
        if self.view.sticky {
            let cx = Cx {
                p: &self.p,
                theme: &self.theme,
                g: &self.g,
                width: self.geo.c as usize,
                detail: self.detail,
                now,
                spin: 0,
            };
            let f = self.tr.frame(vh, &mut self.view, &cx);
            if let Some((b, r)) = f.rows.first() {
                self.view.top = (*b, *r);
                self.view.sticky = false;
                self.view.rev_at_unstick = self.tr.rev;
            }
        }
        self.view.cursor = self
            .tr
            .hits
            .iter()
            .rev()
            .map(|h| h.1)
            .find(|b| {
                !matches!(
                    self.tr.blocks.get(*b).map(|x| &x.kind),
                    Some(BKind::Welcome(_))
                )
            })
            .or(self.view.cursor);
    }

    // ---- mouse ---------------------------------------------------------------------------

    pub fn on_mouse(&mut self, m: MouseEvent, now: Instant) {
        if self.tools_mouse(m) {
            return;
        }
        if let Some(ov) = self.overlay.as_mut() {
            let out = ov.on_mouse(m);
            self.on_overlay(out, now);
            return;
        }
        let vh = self.cx_vh().max(1);
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.wheel.push_back(now);
                while self
                    .wheel
                    .front()
                    .is_some_and(|t| now.saturating_duration_since(*t) > Duration::from_millis(300))
                {
                    self.wheel.pop_front();
                }
                let step: isize = if self.wheel.len() >= 6 { 6 } else { 3 };
                let up = m.kind == MouseEventKind::ScrollUp;
                let cx = Cx {
                    p: &self.p,
                    theme: &self.theme,
                    g: &self.g,
                    width: self.geo.c as usize,
                    detail: self.detail,
                    now,
                    spin: 0,
                };
                self.tr
                    .scroll_by(&mut self.view, if up { -step } else { step }, vh, &cx);
                // The wheel only scrolls. Nav mode turns letters into commands, so it starts
                // from a key you pressed (esc, pageup), never from a flick of the wheel.
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let (x, y) = (m.column, m.row);
                let g = self.geo;
                if y >= g.transcript.y && y < g.transcript.bottom() {
                    // Selection starts here; a click without a drag folds on release.
                    self.tr.mouse_down(x, y, now);
                } else if y >= g.dock.y && y < g.dock.bottom() && !self.todos.is_empty() {
                    let open = self.todos_open.unwrap_or(self.default_todos_open());
                    self.todos_open = Some(!open);
                } else if y >= g.composer.y && y < g.composer.bottom() {
                    let (up, down) = g.pads();
                    let area = ratatui::layout::Rect::new(
                        g.composer.x + 2,
                        g.composer.y + up,
                        g.composer.width.saturating_sub(3),
                        g.composer.height.saturating_sub(up + down),
                    );
                    self.composer.ed.click(area, x, y);
                    self.focus = Focus::Composer;
                } else if y == g.status.y && x < g.status.width / 2 {
                    self.overlay = Some(Overlay::Settings { row: 0 });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let g = self.geo;
                // Past the top or bottom edge a drag scrolls a row per event and keeps selecting.
                let edge: isize = if m.row <= g.transcript.y {
                    -1
                } else if m.row.saturating_add(1) >= g.transcript.bottom() {
                    1
                } else {
                    0
                };
                if edge != 0 && self.tr.sel.is_some() {
                    let cx = Cx {
                        p: &self.p,
                        theme: &self.theme,
                        g: &self.g,
                        width: self.geo.c as usize,
                        detail: self.detail,
                        now,
                        spin: 0,
                    };
                    self.tr.scroll_by(&mut self.view, edge, vh, &cx);
                }
                self.tr.mouse_drag(m.column, m.row);
            }
            MouseEventKind::Up(MouseButton::Left) => match self.tr.mouse_up(m.column, m.row) {
                MouseUp::Click { block, head } => {
                    let foldable = self.tr.blocks.get(block).is_some_and(|bl| bl.foldable());
                    if foldable && (head || self.tr.blocks[block].is_tool()) {
                        self.fold(block, false, now);
                    }
                }
                MouseUp::Copy => {
                    let cx = Cx {
                        p: &self.p,
                        theme: &self.theme,
                        g: &self.g,
                        width: self.geo.c as usize,
                        detail: self.detail,
                        now,
                        spin: 0,
                    };
                    if let Some(t) = self.tr.selection_text(&cx) {
                        self.copy(&t, now);
                    }
                }
                MouseUp::Nothing => {}
            },
            _ => {}
        }
    }

    pub fn default_todos_open(&self) -> bool {
        self.size.1 >= 30 && self.geo.rail_rect.is_none()
    }

    // ---- exit ----------------------------------------------------------------------------

    pub fn save_draft(&self) {
        store::save_draft(&self.session_id, &self.composer.expanded());
    }

    /// What is left in the shell after the alt screen closes.
    pub fn exit_summary(&self) -> String {
        let mut s = String::new();
        if let Some(t) = self.tr.last_assistant_text(0) {
            let lines: Vec<&str> = t.lines().collect();
            let keep = lines.len().min(14);
            for l in &lines[lines.len() - keep..] {
                s.push_str(l);
                s.push('\n');
            }
            s.push('\n');
        }
        let u = &self.tr.usage;
        if self.tr.has_conversation() {
            let mut parts = Vec::new();
            if u.context_tokens > 0 {
                parts.push(format!(
                    "{} context",
                    crate::ui::row::fmt_tokens(u.context_tokens)
                ));
            }
            if let Some(c) = u.cost_usd.filter(|c| *c > 0.0) {
                parts.push(format!("${c:.2}"));
            }
            if !parts.is_empty() {
                s.push_str(&parts.join(" · "));
                s.push('\n');
            }
        }
        if !self.session_id.is_empty() && self.tr.has_conversation() {
            s.push_str(&format!("openc --resume {}\n", self.session_id));
        }
        // Written straight to the user's shell after the alternate screen closes, so model text
        // and the session id must not carry a sequence the terminal would act on.
        tuikit::width::plain_text(&s).into_owned()
    }
}

/// A path for `cd`: bare when it is plain, single-quoted otherwise.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn mode_from_word(w: &str) -> String {
    match w {
        "ask" => "default",
        "edits" => "acceptEdits",
        "bypass" => "bypassPermissions",
        other => other,
    }
    .to_string()
}
