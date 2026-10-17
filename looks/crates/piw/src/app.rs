// OWNER: shared (state, update, event loop; touch with care, other builders depend on it)
//! App state, the message type, `update`, and the event loop.
//!
//! Everything the UI shows lives in [`App`]. Terminal input and backend events both become calls
//! on it (`on_key`, `update`); the loop draws only when something marked it dirty or an animation
//! deadline passed, so an idle app wakes for nothing.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::transcript::{Part, Role, Transcript};
use agent_core::{
    Backend, BackendHandle, Config, Event, NoticeLevel, Request, SessionInfo, SlashCommand,
    StopReason, ToolCall, ToolStatus,
};
use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::FutureExt;
use ratatui::text::Line;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tuikit::term::{Event as TermEvent, EventPump, TermGuard, Terminal};

use crate::commands::{self, SlashItem};
use crate::keys::{Action, Keymap};
use crate::mock::PiMock;
use crate::settings::Settings;
use crate::system_theme::{Appearance, Reported};
use crate::theme::{self, ColorQuery, PiTheme, TermColors, Tok};
use crate::ui::autocomplete::{AcCtx, Apply, Autocomplete, Kind};
use crate::ui::editor::{EditKey, EditorState};
use crate::ui::loader::{Loader, LoaderKind};
use crate::ui::search::{Search, SearchKey};
use crate::ui::selectors::{SelOut, Selector};
use crate::ui::tools::{ToolCx, ToolTime, UserBash};
use crate::ui::{self, messages, Cx, Lines};

/// The wall clock the selectors see when the app runs under test or for screenshots.
pub const FROZEN_NOW: i64 = 1_790_003_600;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MIN_FRAME: Duration = Duration::from_millis(16);
const DOUBLE: Duration = Duration::from_millis(500);
const SCROLLBAR_LINGER: Duration = Duration::from_millis(1000);

/// What the transcript is made of, in order, after the header.
#[derive(Clone, Debug)]
pub enum Item {
    /// A user or assistant message of the agent-core transcript.
    Msg(usize),
    /// Dim status line; a status that follows a status replaces it in place.
    Status(String),
    /// A dim line that is never replaced (`Session name set: x`).
    Note(String),
    Warning(String),
    Error(String),
    /// `✓ New session started`.
    Accent(String),
    /// Command output: default colour, padding 1.
    Text(String),
    /// Pre-styled rows (`/session`, `/hotkeys`), shown with padding 1.
    Rows(Vec<Line<'static>>),
    /// `/hotkeys` and `/changelog`: blocks that draw their own rules, rebuilt at every width.
    Hotkeys,
    Changelog,
    /// A `!cmd` run, index into `App::bashes`.
    Bash(usize),
    Compaction {
        tokens: Option<u64>,
        summary: String,
    },
}

pub struct Queued {
    pub text: String,
    pub follow_up: bool,
}

pub enum Dock {
    Editor,
    Selector(Selector),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum After {
    NewSession,
    Resumed,
}

/// Messages that reach the app from tasks and the backend.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Backend(Event),
    Branch(Option<String>),
    Files(Vec<String>),
    /// The terminal's answer to a colour query that arrived after startup stopped waiting.
    TermReply(tuikit::input::Reply),
    /// A `!cmd` printed something; the app reads the tail from its job.
    ShellOut {
        idx: usize,
    },
    Shell {
        idx: usize,
        end: crate::shell::ShellEnd,
    },
}

/// Work that needs the terminal, which only the loop owns.
#[derive(Debug, PartialEq)]
pub enum Deferred {
    Suspend,
    Editor,
    /// Ask the terminal for its colours again.
    QueryColors,
    Copy(String),
}

#[derive(Clone, Debug, Default)]
pub struct AppOpts {
    pub cwd: PathBuf,
    pub continue_latest: bool,
    /// `-r`: open the session selector at startup.
    pub pick_session: bool,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub prompts: Vec<String>,
    pub theme: Option<String>,
    pub name: Option<String>,
    pub mock: bool,
    /// Where prompt history lives; `None` keeps it in memory (tests).
    pub state_dir: Option<PathBuf>,
    /// Where `settings.json` lives; `None` keeps settings in memory (tests).
    pub config_dir: Option<PathBuf>,
    pub verbose: bool,
    /// This executable, when it can run as `piw --guard <cmd>`: `!cmd` goes through it, so the
    /// whole process tree ends with piw however piw ends. `None` (tests) runs the shell in a
    /// process group of its own instead.
    pub guard_exe: Option<PathBuf>,
    /// How long a `!cmd` may run; `None` is [`crate::shell::DEFAULT_LIMIT`].
    pub shell_limit: Option<Duration>,
}

/// Scroll position of the transcript. `None` follows the end.
#[derive(Debug, Default)]
pub struct Scroll {
    offset: Option<usize>,
    /// Following stays off at the bottom (`scrollTo(.., {disableFollow: true})`, which search uses)
    /// until the next scroll.
    pinned: bool,
    total: usize,
    view: usize,
    bar_until: Option<Duration>,
}

impl Scroll {
    pub fn set_extent(&mut self, total: usize, view: usize) {
        self.total = total;
        self.view = view;
        if let Some(o) = self.offset {
            if o >= self.max_top() && !self.pinned {
                self.offset = None;
            }
        }
    }

    pub fn max_top(&self) -> usize {
        self.total.saturating_sub(self.view)
    }

    pub fn top(&self) -> usize {
        self.offset
            .map_or(self.max_top(), |o| o.min(self.max_top()))
    }

    pub fn scrolled_up(&self) -> bool {
        self.offset.is_some() && self.max_top() > 0
    }

    pub fn bar_visible(&self, now: Duration) -> bool {
        self.bar_until.is_some_and(|t| now < t)
    }

    pub fn bar_deadline(&self) -> Option<Duration> {
        self.bar_until
    }

    /// The scrollbar shows for a second after wheel or drag activity, not after key scrolling.
    pub fn flash(&mut self, now: Duration) {
        self.bar_until = Some(now + SCROLLBAR_LINGER);
    }

    pub fn up(&mut self, n: usize) {
        let t = self.top();
        self.pinned = false;
        self.offset = Some(t.saturating_sub(n));
    }

    pub fn down(&mut self, n: usize) {
        let t = self.top() + n;
        self.pinned = false;
        self.offset = if t >= self.max_top() { None } else { Some(t) };
    }

    pub fn to(&mut self, row: usize) {
        self.pinned = false;
        self.offset = if row >= self.max_top() {
            None
        } else {
            Some(row)
        };
    }

    /// Scroll to `row` and stop following, even when that is the bottom.
    pub fn pin(&mut self, row: usize) {
        self.pinned = true;
        self.offset = Some(row.min(self.max_top()));
    }

    pub fn to_top(&mut self) {
        self.pinned = false;
        self.offset = Some(0);
    }

    pub fn follow(&mut self) {
        self.pinned = false;
        self.offset = None;
    }

    /// A page is the viewport less four rows of overlap (`PAGE_SCROLL_OVERLAP`).
    pub fn page(&self) -> usize {
        self.view.saturating_sub(4).max(1)
    }

    pub fn view_rows(&self) -> usize {
        self.view
    }
}

struct CacheEntry {
    key: u64,
    lines: Arc<Lines>,
    /// Where a click lands in `lines`.
    regions: Vec<messages::Region>,
}

/// What a click on the transcript toggles. Pi keeps this per component: clicking a tool block
/// flips that block's output (`ctrl+o` sets them all again), a thinking block flips that run, a
/// compaction summary flips its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Click {
    Tool { msg: usize, id: String },
    Think { msg: usize, run: usize },
    Compaction(usize),
}

fn click_for(idx: usize, c: &messages::Click) -> Click {
    match c {
        messages::Click::Tool(id) => Click::Tool {
            msg: idx,
            id: id.clone(),
        },
        messages::Click::Think(n) => Click::Think { msg: idx, run: *n },
        messages::Click::Compaction => Click::Compaction(idx),
    }
}

/// The transcript's rows as shared pieces, so a frame never copies them.
#[derive(Default)]
pub struct Chunks {
    parts: Vec<Arc<Lines>>,
    total: usize,
}

impl Chunks {
    fn push(&mut self, rows: Arc<Lines>) {
        self.total += rows.len();
        self.parts.push(rows);
    }

    pub fn len(&self) -> usize {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// `n` rows from row `top` on.
    pub fn window(&self, top: usize, n: usize) -> impl Iterator<Item = &Line<'static>> {
        let mut skip = top;
        self.parts
            .iter()
            .filter_map(move |p| {
                if skip >= p.len() {
                    skip -= p.len();
                    None
                } else {
                    let from = std::mem::take(&mut skip);
                    Some(p[from..].iter())
                }
            })
            .flatten()
            .take(n)
    }

    pub fn into_lines(self) -> Lines {
        let mut out = Vec::with_capacity(self.total);
        for p in self.parts {
            out.extend(p.iter().cloned());
        }
        out
    }
}

pub struct App {
    pub opts: AppOpts,
    pub theme: PiTheme,
    /// What the terminal has said about its colours, which the `system` theme is generated from.
    pub term: TermColors,
    color_query: ColorQuery,
    /// The model and thinking level wizard started with, before piw changed anything: what the
    /// selectors mark `· default`, where Pi marks its saved default.
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    /// The transcript search box (`ctrl+shift+f`); it owns the keyboard while it is open.
    pub search: Option<Search>,
    /// Messages flashed at the top right of the screen, with the time they go away.
    pub flashes: Vec<(String, Duration)>,
    /// Blocks a click expanded or collapsed (tool id), hid or showed (message, thinking run), or
    /// opened (compaction item), over the global settings.
    pub tool_exp: HashMap<(usize, String), bool>,
    pub think_hidden: HashMap<(usize, usize), bool>,
    pub compaction_exp: HashMap<usize, bool>,
    /// Transcript rows a click does something on, rebuilt with the transcript.
    pub click_map: Vec<(std::ops::Range<usize>, Click)>,
    /// Where the left button went down, so a release on the same cell is a click.
    press: Option<(u16, u16)>,
    /// Bumped when a click changes what the cached rows hold.
    block_gen: u64,
    pub settings: Settings,
    pub keymap: Keymap,
    pub size: (u16, u16),
    pub cwd: String,
    pub home: String,
    pub branch: Option<String>,
    pub transcript: Transcript,
    pub items: Vec<Item>,
    mapped: usize,
    pub config: Config,
    pub backend_cmds: Vec<SlashCommand>,
    pub slash: Vec<SlashItem>,
    pub sessions: Vec<SessionInfo>,
    /// The backend has answered a `ListSessions` at least once.
    sessions_listed: bool,
    pub session_name: Option<String>,
    pub editor: EditorState,
    pub autocomplete: Autocomplete,
    pub dock: Dock,
    pub queue: Vec<Queued>,
    pub scroll: Scroll,
    /// `toolOutputExpanded`.
    pub expanded: bool,
    pub hide_thinking: bool,
    pub files: Vec<String>,
    /// Set when a command line option is refused after startup; see [`App::fail_startup`].
    pub startup_error: Option<String>,
    pub files_at: Instant,
    files_cut_told: bool,
    pub tx: UnboundedSender<Request>,
    /// The task that forwards the backend's events; replaced when the backend is started again.
    pub fwd: Option<tokio::task::JoinHandle<()>>,
    pub msg_tx: UnboundedSender<Msg>,
    pub pending: Vec<Deferred>,
    pub quit: bool,
    /// Quitting for a `/ui` switch: the look taking over is where the session goes on, so
    /// no resume hint is left behind.
    pub switched: bool,
    /// `/plan` or `/omakase` to send on the first session: the look `/ui` left was planning.
    handed_plan: Option<&'static str>,
    pub dirty: bool,
    pub backend_ready: bool,
    pub backend_dead: bool,
    /// Right after a compaction the context figure prints `?`.
    pub context_unknown: bool,
    pub bashes: Vec<UserBash>,
    /// The running `!cmd` and the index of its block in `bashes`.
    bash_job: Option<(usize, crate::shell::ShellJob)>,
    bash_ctx: Vec<String>,
    compacting: bool,
    /// Esc was pressed and the turn has not ended yet.
    aborting: bool,
    retrying: bool,
    warned_steer: bool,
    saw_error_notice: bool,
    restored_note: Option<usize>,
    ctrlc_at: Option<Duration>,
    last_wheel: Option<Duration>,
    after_history: Option<After>,
    tool_times: HashMap<String, (Duration, Option<Duration>)>,
    cache: HashMap<usize, CacheEntry>,
    cache_gen: u64,
    prompt_rows: Vec<usize>,
    initial_prompts: Vec<String>,
    continue_pending: bool,
    pick_pending: bool,
    started: Instant,
    pub frozen: Option<Duration>,
    last_anim: Instant,
    pub title_dirty: bool,
}

impl App {
    pub fn new(opts: AppOpts, tx: UnboundedSender<Request>, msg_tx: UnboundedSender<Msg>) -> App {
        let settings = opts
            .config_dir
            .as_deref()
            .map(Settings::load)
            .unwrap_or_default();
        let term = TermColors::default();
        let theme = resolve_theme(opts.theme.as_deref().or(settings.theme.as_deref()), &term);
        let cwd = opts.cwd.display().to_string();
        let home = std::env::var("HOME").unwrap_or_default();
        let mut editor = EditorState::new();
        if let Some(h) = load_history(opts.state_dir.as_deref()) {
            editor.ed.set_history(h);
        }
        let mut autocomplete = Autocomplete::new();
        autocomplete.max_visible = settings.autocomplete_max_visible;
        let mut app = App {
            theme,
            term,
            color_query: ColorQuery::new(),
            default_model: None,
            default_effort: None,
            search: None,
            flashes: Vec::new(),
            tool_exp: HashMap::new(),
            think_hidden: HashMap::new(),
            compaction_exp: HashMap::new(),
            click_map: Vec::new(),
            press: None,
            block_gen: 0,
            hide_thinking: settings.hide_thinking_block,
            settings,
            keymap: Keymap::new(),
            size: (120, 36),
            cwd,
            home,
            branch: None,
            transcript: Transcript::new(),
            items: Vec::new(),
            mapped: 0,
            config: Config::default(),
            backend_cmds: Vec::new(),
            slash: Vec::new(),
            sessions: Vec::new(),
            sessions_listed: false,
            session_name: opts.name.clone(),
            editor,
            autocomplete,
            dock: Dock::Editor,
            queue: Vec::new(),
            scroll: Scroll::default(),
            expanded: false,
            files: Vec::new(),
            startup_error: None,
            files_at: Instant::now(),
            files_cut_told: false,
            tx,
            fwd: None,
            msg_tx,
            pending: Vec::new(),
            quit: false,
            switched: false,
            handed_plan: agent_core::handed_plan(),
            dirty: true,
            backend_ready: false,
            backend_dead: false,
            context_unknown: false,
            bashes: Vec::new(),
            bash_job: None,
            bash_ctx: Vec::new(),
            compacting: false,
            aborting: false,
            retrying: false,
            warned_steer: false,
            saw_error_notice: false,
            restored_note: None,
            ctrlc_at: None,
            last_wheel: None,
            after_history: None,
            tool_times: HashMap::new(),
            cache: HashMap::new(),
            cache_gen: 0,
            prompt_rows: Vec::new(),
            initial_prompts: opts.prompts.clone(),
            continue_pending: opts.continue_latest,
            pick_pending: opts.pick_session,
            started: Instant::now(),
            frozen: None,
            last_anim: Instant::now(),
            title_dirty: true,
            opts,
        };
        app.slash = commands::items(&[]);
        if app.opts.resume.is_some() {
            // `--session <id>` replays a conversation: say so, as `/resume` does
            app.after_history = Some(After::Resumed);
        }
        app
    }

    // ---- small queries -----------------------------------------------------------------

    pub fn clock(&self) -> Duration {
        self.frozen.unwrap_or_else(|| self.started.elapsed())
    }

    pub fn cx(&self, width: u16) -> Cx {
        Cx {
            theme: self.theme.clone(),
            width,
            expanded: self.expanded,
            hide_thinking: self.hide_thinking,
            out_pad: self.settings.output_padding,
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            clock: self.clock(),
            version: self.config.version().to_string(),
        }
    }

    pub fn busy(&self) -> bool {
        self.transcript.busy || self.compacting
    }

    pub fn bash_mode(&self) -> bool {
        self.editor.is_bash()
    }

    pub fn thinking_level(&self) -> &str {
        if self.config.effort.is_empty() {
            "default"
        } else {
            &self.config.effort
        }
    }

    pub fn thinking_tok(&self) -> Tok {
        self.theme.thinking(self.thinking_level())
    }

    /// Written to the terminal with `SetTitle`, so neither the directory name nor the session
    /// name may carry a BEL or ESC that ends the title early and runs what follows.
    pub fn window_title(&self) -> String {
        let one_line = |s: &str| tuikit::width::plain_text(s).replace('\n', " ");
        let base = std::path::Path::new(&self.cwd)
            .file_name()
            .map_or(String::new(), |n| one_line(&n.to_string_lossy()));
        let t = match &self.session_name {
            Some(n) => format!("π - {} - {base}", one_line(n)),
            None => format!("π - {base}"),
        };
        tuikit::width::truncate(&t, 100)
    }

    /// The loader that replaces the editor's top rule, if one is showing.
    pub fn loader(&self, cx: &Cx) -> Option<Loader> {
        if !self.busy() {
            return None;
        }
        let kind = if self.aborting {
            LoaderKind::Aborting
        } else if self.compacting {
            LoaderKind::Compacting
        } else if self.retrying {
            LoaderKind::Retry
        } else {
            LoaderKind::Working
        };
        let tok = if self.bash_mode() {
            Tok::BashMode
        } else {
            self.thinking_tok()
        };
        Some(Loader::new(
            kind,
            cx,
            cx.th().fg(tok),
            &self.keymap.key(Action::Interrupt),
            cx.clock,
        ))
    }

    pub fn last_assistant_text(&self) -> Option<String> {
        self.transcript
            .messages
            .iter()
            .rev()
            .filter(|m| m.role == Role::Assistant)
            .find_map(|m| {
                let t: String = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                (!t.trim().is_empty()).then_some(t)
            })
    }

    // ---- transcript items --------------------------------------------------------------

    fn sync_items(&mut self) {
        let n = self.transcript.messages.len();
        for i in self.mapped..n {
            self.items.push(Item::Msg(i));
        }
        self.mapped = n;
    }

    fn reset_items(&mut self) {
        self.items.clear();
        self.mapped = 0;
        self.cache.clear();
        ui::stream::clear();
        self.cache_gen += 1;
        self.tool_times.clear();
        self.bashes.clear();
        self.bash_ctx.clear();
        self.tool_exp.clear();
        self.think_hidden.clear();
        self.compaction_exp.clear();
        self.click_map.clear();
        self.scroll.follow();
    }

    /// The branch shown in the footer; the agent or a `!` command may have switched it.
    pub fn refresh_branch(&mut self) {
        if let Some(b) = git_branch(&self.opts.cwd) {
            self.branch = Some(b);
        }
    }

    pub fn status(&mut self, text: String) {
        if let Some(Item::Status(prev)) = self.items.last_mut() {
            *prev = text;
            // the rows kept for this item were made from the old text
            self.cache.remove(&(self.items.len() - 1));
        } else {
            self.items.push(Item::Status(text));
        }
        self.dirty = true;
    }

    pub fn warn(&mut self, text: String) {
        self.items.push(Item::Warning(text));
        self.dirty = true;
    }

    pub fn error(&mut self, text: String) {
        self.items.push(Item::Error(text));
        self.dirty = true;
    }

    fn follow_end(&mut self) {
        self.scroll.follow();
    }

    /// Build the whole transcript at `cx.width`: the header, then every item, as one list of rows.
    /// The screen takes [`Chunks`] and reads only the rows it shows.
    pub fn transcript_lines(&mut self, cx: &Cx) -> Lines {
        self.transcript_chunks(cx).into_lines()
    }

    /// The transcript as shared pieces, one per item. A finished item is rendered once and its
    /// rows are shared from then on, so a frame costs the number of items (to pick the visible
    /// ones), not the number of rows, and nothing is copied.
    pub fn transcript_chunks(&mut self, cx: &Cx) -> Chunks {
        let mut out = Chunks::default();
        self.prompt_rows.clear();
        if self.settings.quiet_startup != crate::settings::Quiet::On {
            out.push(Arc::new(ui::header::render(cx, &self.keymap)));
        }
        let key_base = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (
                cx.width,
                cx.expanded,
                cx.hide_thinking,
                cx.out_pad,
                &cx.theme.name,
                self.cache_gen,
                self.block_gen,
            )
                .hash(&mut h);
            h.finish()
        };
        self.click_map.clear();
        let items = std::mem::take(&mut self.items);
        let expand_key = self.keymap.key(Action::ToolsExpand);
        for (idx, it) in items.iter().enumerate() {
            // `Some(true)`: keep the rows for next frame; `None`: they change with the clock
            let cached = |cache: &HashMap<usize, CacheEntry>| {
                cache
                    .get(&idx)
                    .filter(|c| c.key == key_base)
                    .map(|c| (c.lines.clone(), c.regions.clone()))
            };
            let (rows, keep, regions): (Arc<Lines>, bool, Vec<messages::Region>) = match it {
                Item::Msg(i) => {
                    let Some(m) = self.transcript.messages.get(*i) else {
                        continue;
                    };
                    match m.role {
                        Role::User => {
                            self.prompt_rows.push(out.len() + usize::from(idx != 0));
                            match cached(&self.cache) {
                                Some((c, r)) => (c, false, r),
                                None => {
                                    let text = match m.parts.first() {
                                        Some(Part::Text(t)) => t.as_str(),
                                        _ => "",
                                    };
                                    // the blank row is only there when the chat already holds
                                    // something (the header and its spacer live outside the chat
                                    // container)
                                    let l = messages::user(text, cx);
                                    let skip = usize::from(idx == 0);
                                    (
                                        Arc::new(l.into_iter().skip(skip).collect()),
                                        true,
                                        Vec::new(),
                                    )
                                }
                            }
                        }
                        _ => {
                            let closed = m.took.is_some();
                            // Pi tags assistant text blocks that carry no tool calls too
                            let marked = !m.parts.iter().any(|p| matches!(p, Part::Tool(_)))
                                && m.parts
                                    .iter()
                                    .any(|p| matches!(p, Part::Text(t) if !t.trim().is_empty()));
                            if marked {
                                self.prompt_rows.push(out.len());
                            }
                            match cached(&self.cache).filter(|_| closed) {
                                Some((c, r)) => (c, false, r),
                                None => {
                                    let times = &self.tool_times;
                                    let now = cx.clock;
                                    // what a click set on this message's blocks
                                    let tools: HashMap<String, bool> = self
                                        .tool_exp
                                        .iter()
                                        .filter(|((m, _), _)| *m == idx)
                                        .map(|((_, id), v)| (id.clone(), *v))
                                        .collect();
                                    let think: HashMap<usize, bool> = self
                                        .think_hidden
                                        .iter()
                                        .filter(|((m, _), _)| *m == idx)
                                        .map(|((_, n), v)| (*n, *v))
                                        .collect();
                                    let (rows, regions) = messages::assistant_with(
                                        m,
                                        cx,
                                        &|c: &ToolCall| tool_time(times, c, now),
                                        messages::Overrides {
                                            tools: Some(&tools),
                                            think: Some(&think),
                                        },
                                    );
                                    (Arc::new(rows), closed, regions)
                                }
                            }
                        }
                    }
                }
                other => match cached(&self.cache) {
                    Some((c, r)) => (c, false, r),
                    None => {
                        let mut regions = Vec::new();
                        let (rows, keep) = match other {
                            Item::Status(t) | Item::Note(t) => (messages::status(t, cx), true),
                            Item::Warning(t) => (messages::warning(t, cx), true),
                            Item::Error(t) => (messages::error(t, cx), true),
                            Item::Accent(t) => (messages::accent_note(t, cx), true),
                            Item::Text(t) => (messages::text_block(t, cx), true),
                            Item::Rows(r) => (messages::lines_block(r), true),
                            Item::Hotkeys => (ui::selectors::hotkeys(cx, &self.keymap), true),
                            Item::Changelog => (ui::selectors::changelog(cx), true),
                            Item::Bash(b) => match self.bashes.get(*b) {
                                Some(b) => {
                                    let time = ToolTime {
                                        elapsed: b.took.unwrap_or_else(|| b.started.elapsed()),
                                        done: !b.running,
                                    };
                                    let tcx = ToolCx::new(cx, time);
                                    // a running command's elapsed time and output move
                                    (ui::tools::render_user_bash(b, &tcx), !b.running)
                                }
                                None => continue,
                            },
                            Item::Compaction { tokens, summary } => {
                                let mut local = cx.clone();
                                if let Some(e) = self.compaction_exp.get(&idx) {
                                    local.expanded = *e;
                                }
                                let rows =
                                    messages::compaction(*tokens, summary, &local, &expand_key);
                                // blank row, padding, content, padding
                                if rows.len() > 3 {
                                    regions.push(messages::Region {
                                        rows: 2..rows.len() - 1,
                                        click: messages::Click::Compaction,
                                    });
                                }
                                (rows, true)
                            }
                            Item::Msg(_) => unreachable!("handled above"),
                        };
                        (Arc::new(rows), keep, regions)
                    }
                },
            };
            for r in &regions {
                self.click_map.push((
                    out.len() + r.rows.start..out.len() + r.rows.end,
                    click_for(idx, &r.click),
                ));
            }
            if keep {
                self.cache.insert(
                    idx,
                    CacheEntry {
                        key: key_base,
                        lines: rows.clone(),
                        regions,
                    },
                );
            }
            out.push(rows);
        }
        self.items = items;
        out
    }

    // ---- backend events ----------------------------------------------------------------

    pub fn send(&mut self, r: Request) {
        if self.tx.send(r).is_err() && !self.backend_dead {
            self.backend_dead = true;
            self.error("The wizard process is not running".into());
        }
    }

    pub fn update(&mut self, msg: Msg) {
        self.dirty = true;
        match msg {
            Msg::Backend(ev) => self.on_event(ev),
            Msg::Branch(b) => self.branch = b,
            Msg::Files(f) => {
                if f.len() >= MAX_FILES && !self.files_cut_told {
                    self.files_cut_told = true;
                    self.warn(format!(
                        "@ lists the first {MAX_FILES} files of this folder; type more of the path to reach the rest"
                    ));
                }
                self.files = f;
                self.refresh_autocomplete();
            }
            Msg::TermReply(r) => {
                self.on_term_reply(r);
            }
            Msg::ShellOut { idx } => self.bash_output(idx),
            Msg::Shell { idx, end } => self.finish_bash(idx, end),
        }
    }

    pub fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        match &ev {
            Event::Ready { config, session_id } => {
                if self.default_model.is_none() && !config.model.is_empty() {
                    self.default_model = Some(config.model.clone());
                }
                if self.default_effort.is_none() && !config.effort.is_empty() {
                    self.default_effort = Some(config.effort.clone());
                }
                self.config = config.clone();
                if let Some(cmd) = self.handed_plan.take() {
                    self.send(Request::Prompt(cmd.into()));
                }
                // wizard confirms `/new` with `Ready` alone; it sends no `History` for an empty
                // session, so the conversation on screen is cleared here
                if self.after_history == Some(After::NewSession) {
                    self.transcript.apply(&Event::History {
                        session_id: session_id.clone(),
                        items: Vec::new(),
                    });
                    self.transcript.apply(&ev);
                    self.session_switched();
                } else {
                    self.transcript.apply(&ev);
                }
                self.aborting = false;
                self.backend_ready = true;
                self.backend_dead = false;
                self.send(Request::ListSessions);
                if let Some(m) = self.opts.model.take() {
                    match commands::find_model(self, &m).map(|o| o.id.clone()) {
                        Some(id) => self.send(Request::SetModel(id)),
                        None => {
                            self.fail_startup(format!(
                                "model \"{m}\" not found; /model lists the ones wizard has"
                            ));
                            return;
                        }
                    }
                }
                if let Some(t) = self.opts.thinking.take() {
                    if !self.config.efforts.is_empty() && !self.config.efforts.contains(&t) {
                        let all = self.config.efforts.join(", ");
                        self.fail_startup(format!(
                            "unknown thinking level \"{t}\". Available levels: {all}"
                        ));
                        return;
                    }
                    self.send(Request::SetEffort(t));
                }
                if self.pick_pending {
                    self.pick_pending = false;
                    self.open_resume();
                }
                let prompts = std::mem::take(&mut self.initial_prompts);
                for p in prompts {
                    self.editor.set_text(&p);
                    self.submit();
                }
            }
            Event::Commands(c) => {
                self.backend_cmds = c.clone();
                self.slash = commands::items(c);
            }
            Event::ConfigChanged(c) => {
                self.config = c.clone();
                self.transcript.apply(&ev);
            }
            Event::Sessions(s) => {
                self.sessions = s.clone();
                self.sessions_listed = true;
                if self.continue_pending {
                    self.continue_pending = false;
                    let mine = self
                        .sessions
                        .iter()
                        .filter(|x| x.cwd.is_empty() || x.cwd == self.cwd)
                        .max_by_key(|x| x.updated)
                        .map(|x| x.id.clone());
                    if let Some(id) = mine {
                        self.after_history = Some(After::Resumed);
                        self.send(Request::LoadSession(id));
                    }
                }
            }
            Event::History { .. } => {
                self.transcript.apply(&ev);
                self.session_switched();
            }
            Event::TurnStart => {
                self.transcript.apply(&ev);
                self.saw_error_notice = false;
                self.sync_items();
            }
            Event::TextDelta(_) | Event::ThoughtDelta(_) => {
                self.retrying = false;
                self.transcript.apply(&ev);
                self.sync_items();
            }
            Event::Tool(c) => {
                self.retrying = false;
                let now = self.clock();
                let e = self.tool_times.entry(c.id.clone()).or_insert((now, None));
                if matches!(c.status, ToolStatus::Completed | ToolStatus::Failed) && e.1.is_none() {
                    e.1 = Some(now);
                }
                self.transcript.apply(&ev);
                self.sync_items();
            }
            Event::Todos(_) | Event::Permission(_) | Event::RewindPreview(_) => {}
            Event::Usage(_) => {
                self.transcript.apply(&ev);
                self.context_unknown = false;
            }
            Event::TurnEnd(reason) => {
                let reason = *reason;
                self.aborting = false;
                self.transcript.apply(&ev);
                self.sync_items();
                self.retrying = false;
                self.compacting = false;
                self.refresh_branch();
                match reason {
                    StopReason::MaxTurns => self.status("Stopped: turn limit reached".into()),
                    StopReason::Error if !self.saw_error_notice => {
                        self.error("Request failed".into())
                    }
                    _ => {}
                }
                if let Some(n) = self.restored_note.take() {
                    self.status(format!(
                        "Restored {n} queued message{} to editor",
                        if n == 1 { "" } else { "s" }
                    ));
                }
                if reason != StopReason::Cancelled {
                    self.flush_queue();
                }
            }
            Event::Notice { level, text } => self.on_notice(*level, text),
            Event::Fatal(text) => {
                self.transcript.busy = false;
                self.compacting = false;
                self.aborting = false;
                self.backend_dead = true;
                self.after_history = None;
                self.error(text.clone());
                self.items.push(Item::Text(
                    "Press ctrl+c to exit, or start a new session with /new".into(),
                ));
                // what was waiting for the turn can no longer be sent: back into the editor
                let n = self.dequeue_all();
                if n > 0 {
                    self.status(format!(
                        "Restored {n} queued message{} to editor",
                        if n == 1 { "" } else { "s" }
                    ));
                }
            }
        }
        if let Dock::Selector(s) = &mut self.dock {
            s.on_event(&ev);
        }
    }

    /// A command line option the backend turned out not to accept (known only once it has
    /// said which models and levels it has): piw leaves and `main` prints this and exits 1.
    fn fail_startup(&mut self, msg: String) {
        self.startup_error = Some(msg);
        self.quit = true;
    }

    /// What follows a session switch the backend confirmed: the old conversation is gone from
    /// the screen, the queue with it, and the line that says what happened.
    fn session_switched(&mut self) {
        self.reset_items();
        self.sync_items();
        self.context_unknown = false;
        self.queue.clear();
        self.aborting = false;
        match self.after_history.take() {
            Some(After::NewSession) => {
                self.session_name = None;
                self.title_dirty = true;
                self.items
                    .push(Item::Accent("✓ New session started".into()))
            }
            Some(After::Resumed) => self.status("Resumed session".into()),
            None => {}
        }
    }

    fn on_notice(&mut self, level: NoticeLevel, text: &str) {
        let text = text.trim_end();
        if agent_core::switched_look(text).is_some_and(|l| l != "pi") {
            // `/ui <look>`: wizard saved it; leaving hands the terminal to that look
            self.quit = true;
            self.switched = true;
            return;
        }
        // a refusal (`Finish or cancel the running turn before switching sessions.`, a session
        // that would not load) means no switch is coming: a later reload must not announce one
        if level != NoticeLevel::Info {
            self.after_history = None;
        }
        if self.compacting && level == NoticeLevel::Info && !text.starts_with("nothing to compact")
        {
            let tokens = first_number_before(text, "token");
            self.items.push(Item::Compaction {
                tokens,
                summary: text.to_string(),
            });
            self.context_unknown = true;
            return;
        }
        match level {
            NoticeLevel::Info => {
                if text.contains('\n') {
                    self.items.push(Item::Text(text.to_string()));
                } else {
                    self.status(text.to_string());
                }
            }
            NoticeLevel::Warn => {
                let t = text.strip_prefix("[wizard] ").unwrap_or(text);
                if t.to_lowercase().contains("retry") {
                    self.retrying = true;
                }
                self.warn(t.to_string());
            }
            NoticeLevel::Error => {
                self.saw_error_notice = true;
                self.error(text.to_string());
            }
        }
    }

    // ---- submit, queue ------------------------------------------------------------------

    pub fn submit(&mut self) {
        self.submit_as(false);
    }

    fn submit_as(&mut self, follow_up: bool) {
        let raw = self.editor.expanded();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.backend_ready && !self.backend_dead {
            self.status("Startup is still in progress".into());
            return;
        }
        self.editor.ed.history_push(&text);
        save_history(self.opts.state_dir.as_deref(), &self.editor);
        self.editor.clear();
        self.autocomplete.reset();
        self.dispatch(text, follow_up);
    }

    /// Run submitted text: `!cmd`, a slash command, or a prompt (queued while the agent works).
    pub fn dispatch(&mut self, text: String, follow_up: bool) {
        if let Some(rest) = text.strip_prefix('!') {
            let (cmd, exclude) = match rest.strip_prefix('!') {
                Some(c) => (c.trim(), true),
                None => (rest.trim(), false),
            };
            if !cmd.is_empty() {
                self.run_bash(cmd.to_string(), exclude);
            }
            return;
        }
        if let Some((name, arg)) = commands::split(&text) {
            let (name, arg) = (name.to_string(), arg.to_string());
            if commands::run(self, &name, &arg) {
                self.follow_end();
                return;
            }
            // a wizard command: it answers with a notice, no user block
            if self.backend_cmds.iter().any(|c| c.name == name) {
                self.send(Request::Prompt(text));
                self.follow_end();
                return;
            }
        }
        if self.backend_dead {
            // there is nobody to answer; keep what was typed instead of showing it as sent
            self.editor.set_text(&text);
            self.error(
                "wizard is not running. Type /new to start it again, or ctrl+c to exit.".into(),
            );
            return;
        }
        if self.busy() {
            if !self.warned_steer && !follow_up {
                self.warned_steer = true;
                self.warn(
                    "wizard cannot steer a running turn; the message is sent when it finishes."
                        .into(),
                );
            }
            if self.compacting {
                self.status("Queued message for after compaction".into());
            }
            self.queue.push(Queued { text, follow_up });
            return;
        }
        self.send_prompt(text);
    }

    pub fn send_prompt(&mut self, text: String) {
        let mut sent = String::new();
        for c in self.bash_ctx.drain(..) {
            sent.push_str(&c);
            sent.push_str("\n\n");
        }
        sent.push_str(&text);
        self.transcript.push_user(&text);
        self.sync_items();
        self.context_unknown = false;
        self.follow_end();
        self.send(Request::Prompt(sent));
    }

    fn flush_queue(&mut self) {
        if self.queue.is_empty() || self.busy() {
            return;
        }
        // steering messages first, then follow-ups
        let i = self.queue.iter().position(|q| !q.follow_up).unwrap_or(0);
        let q = self.queue.remove(i);
        self.dispatch_unqueued(q.text);
    }

    fn dispatch_unqueued(&mut self, text: String) {
        if let Some((name, arg)) = commands::split(&text) {
            let (name, arg) = (name.to_string(), arg.to_string());
            if commands::run(self, &name, &arg) {
                return;
            }
        }
        self.send_prompt(text);
    }

    /// `alt+up`: every queued message back into the editor, steering first.
    pub fn dequeue_all(&mut self) -> usize {
        let n = self.queue.len();
        if n == 0 {
            return 0;
        }
        let mut q = std::mem::take(&mut self.queue);
        q.sort_by_key(|x| x.follow_up);
        let mut text: Vec<String> = q.into_iter().map(|x| x.text).collect();
        // the editor's own text may hold `[paste #1 +15 lines]`; set_text forgets what the marker
        // stands for, so it goes back as the pasted text, not as the marker
        let cur = self.editor.expanded();
        if !cur.trim().is_empty() {
            text.push(cur);
        }
        self.editor.set_text(&text.join("\n\n"));
        n
    }

    // ---- user bash ----------------------------------------------------------------------

    fn run_bash(&mut self, cmd: String, exclude: bool) {
        if self.bashes.last().is_some_and(|b| b.running) {
            self.warn("A bash command is already running. Press Esc to cancel it first.".into());
            return;
        }
        let idx = self.bashes.len();
        self.bashes.push(UserBash {
            cmd: cmd.clone(),
            exclude,
            output: String::new(),
            running: true,
            exit: None,
            cancelled: false,
            started: Instant::now(),
            took: None,
        });
        self.items.push(Item::Bash(idx));
        self.follow_end();
        let tx = self.msg_tx.clone();
        let job = if tokio::runtime::Handle::try_current().is_ok() {
            crate::shell::ShellJob::spawn(
                &cmd,
                &self.opts.cwd,
                self.opts.guard_exe.as_ref(),
                self.opts.shell_limit.unwrap_or(crate::shell::DEFAULT_LIMIT),
                move |m| {
                    let _ = tx.send(match m {
                        crate::shell::ShellMsg::Output => Msg::ShellOut { idx },
                        crate::shell::ShellMsg::End(end) => Msg::Shell { idx, end },
                    });
                },
            )
        } else {
            Err(std::io::Error::other("no async runtime to run it on"))
        };
        match job {
            Ok(j) => self.bash_job = Some((idx, j)),
            Err(e) => {
                let b = &mut self.bashes[idx];
                b.output = e.to_string();
                b.running = false;
                b.exit = Some(127);
                b.took = Some(b.started.elapsed());
            }
        }
    }

    /// Show what the running command has printed so far.
    fn bash_output(&mut self, idx: usize) {
        let Some((text, cut)) = self
            .bash_job
            .as_ref()
            .filter(|(i, _)| *i == idx)
            .map(|(_, j)| j.tail())
        else {
            return;
        };
        if let Some(b) = self.bashes.get_mut(idx).filter(|b| b.running) {
            b.output = shown_output(&text, cut);
            self.dirty = true;
        }
    }

    fn finish_bash(&mut self, idx: usize, end: crate::shell::ShellEnd) {
        // a cancelled run's late result must not take the next command's job with it
        let finished = match self.bash_job.take() {
            Some((i, j)) if i == idx => Some(j),
            other => {
                self.bash_job = other;
                None
            }
        };
        let (text, cut) = finished.map_or_else(Default::default, |j| j.tail());
        let limit = self.opts.shell_limit.unwrap_or(crate::shell::DEFAULT_LIMIT);
        let Some(b) = self.bashes.get_mut(idx) else {
            return;
        };
        if b.cancelled {
            return;
        }
        let mut out = shown_output(&text, cut);
        if end.timed_out {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("(timed out after {})", fmt_limit(limit)));
        }
        b.output = out.clone();
        b.running = false;
        b.exit = end.code;
        b.took = Some(b.started.elapsed());
        if !b.exclude {
            self.bash_ctx.push(format!(
                "Ran `{}`\n```\n{}\n```",
                b.cmd,
                fence_safe(context_tail(&out).trim_end())
            ));
        }
        self.refresh_branch();
        self.dirty = true;
    }

    /// Esc on a running `!cmd`: SIGTERM to its tree, SIGKILL later. The block says so now and a
    /// late result is ignored.
    fn cancel_bash(&mut self) -> bool {
        let Some(b) = self.bashes.last_mut().filter(|b| b.running) else {
            return false;
        };
        b.cancelled = true;
        b.running = false;
        b.took = Some(b.started.elapsed());
        if let Some((_, j)) = self.bash_job.as_mut() {
            j.cancel();
        }
        true
    }

    /// Piw is leaving: end what `!cmd` started.
    pub fn kill_bash(&mut self) {
        self.bash_job = None;
    }

    // ---- commands the table calls --------------------------------------------------------

    pub fn open_selector(&mut self, s: Selector) {
        self.autocomplete.reset();
        self.dock = Dock::Selector(s);
        self.dirty = true;
    }

    pub fn close_selector(&mut self) {
        self.dock = Dock::Editor;
        self.dirty = true;
    }

    pub fn cmd_model(&mut self, arg: &str) {
        if arg.is_empty() {
            commands::open_model_selector(self, "");
            return;
        }
        match commands::find_model(self, arg).map(|m| (m.id.clone(), m.name.clone())) {
            Some((id, name)) => {
                self.send(Request::SetModel(id));
                self.status(format!("Model: {name}"));
            }
            None => commands::open_model_selector(self, arg),
        }
    }

    pub fn cmd_thinking(&mut self, arg: &str) {
        if arg.is_empty() {
            if self.config.efforts.is_empty() {
                self.status("Current model does not support thinking".into());
                return;
            }
            let sel = Selector::thinking_with_default(
                &self.config.efforts,
                &self.config.effort,
                self.default_effort.as_deref(),
            );
            self.open_selector(sel);
            return;
        }
        if self.config.efforts.iter().any(|e| e == arg) {
            self.set_effort(arg.to_string());
        } else {
            let all = self.config.efforts.join(", ");
            self.error(format!(
                "Unknown thinking level \"{arg}\". Available levels: {all}"
            ));
        }
    }

    fn set_effort(&mut self, level: String) {
        self.config.effort = level.clone();
        self.send(Request::SetEffort(level.clone()));
        self.status(format!("Thinking level: {level}"));
    }

    pub fn cycle_thinking(&mut self) {
        let list = self.config.efforts.clone();
        if list.is_empty() {
            self.status("Current model does not support thinking".into());
            return;
        }
        let at = list.iter().position(|e| *e == self.config.effort);
        let next = list[at.map_or(0, |i| (i + 1) % list.len())].clone();
        self.set_effort(next);
    }

    pub fn cycle_model(&mut self, forward: bool) {
        let all: Vec<&agent_core::ModelOption> = self
            .config
            .models
            .iter()
            .filter(|m| {
                self.settings.enabled_models.is_empty()
                    || self.settings.enabled_models.contains(&m.id)
            })
            .collect();
        if all.len() < 2 {
            self.status(if self.settings.enabled_models.is_empty() {
                "Only one model available".into()
            } else {
                "Only one model in scope".into()
            });
            return;
        }
        let at = all
            .iter()
            .position(|m| m.id == self.config.model)
            .unwrap_or(0);
        let n = all.len();
        let next = all[if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        }];
        let (id, name) = (next.id.clone(), next.name.clone());
        self.send(Request::SetModel(id));
        let lvl = self.thinking_level().to_string();
        self.status(format!("Switched to {name} (thinking: {lvl})"));
    }

    pub fn cmd_name(&mut self, arg: &str) {
        if arg.is_empty() {
            match &self.session_name {
                Some(n) => self.status(format!("Session name: {n}")),
                None => self.warn("Usage: /name <name>".into()),
            }
            return;
        }
        self.session_name = Some(arg.to_string());
        self.title_dirty = true;
        self.items
            .push(Item::Note(format!("Session name set: {arg}")));
    }

    pub fn new_session(&mut self) {
        if self.backend_dead {
            self.restart_backend();
            return;
        }
        if self.busy() {
            self.warn("Finish or cancel the running turn before starting a new session.".into());
            return;
        }
        self.after_history = Some(After::NewSession);
        self.send(Request::NewSession);
    }

    /// The wizard process is gone (`Fatal`): `/new` starts another one, as the error line says.
    fn restart_backend(&mut self) {
        if tokio::runtime::Handle::try_current().is_err() {
            self.error("Could not start wizard again: no async runtime".into());
            return;
        }
        match spawn_backend(
            self.opts.cwd.clone(),
            None,
            self.opts.mock,
            self.msg_tx.clone(),
        ) {
            Ok((tx, fwd)) => {
                self.tx = tx;
                self.fwd = Some(fwd);
                self.backend_dead = false;
                self.backend_ready = false;
                self.after_history = Some(After::NewSession);
                self.status("Starting wizard again...".into());
            }
            Err(e) => self.error(format!("Could not start wizard again: {e:#}")),
        }
    }

    pub fn compact(&mut self, arg: &str) {
        if self.busy() {
            // the backend queues the prompt behind the turn, and `compacting` would show a
            // compaction that has not started
            self.warn("Finish or cancel the running turn before compacting.".into());
            return;
        }
        self.compacting = true;
        let p = if arg.is_empty() {
            "/compact".to_string()
        } else {
            format!("/compact {arg}")
        };
        self.send(Request::Prompt(p));
    }

    /// The theme setting in force: the command line wins over the settings file.
    pub fn theme_setting(&self) -> Option<String> {
        self.opts
            .theme
            .clone()
            .or_else(|| self.settings.theme.clone())
    }

    /// Whether the theme depends on what the terminal reports: the `system` theme, which is also
    /// the default, and a `light/dark` pair.
    pub fn theme_follows_terminal(&self) -> bool {
        let setting = self.theme_setting();
        let name = theme::resolve_setting(setting.as_deref(), self.term.appearance());
        matches!(name.as_deref(), None | Some("system")) || setting.is_some_and(|s| s.contains('/'))
    }

    /// Send the colour queries Pi sends at startup (OSC 10, 11, 4 and a DA1 to end the batch).
    pub fn start_color_query(&mut self) {
        self.color_query = ColorQuery::new();
    }

    /// A reply to the colour query. Returns `true` when it completed the batch, which is also what
    /// late replies after the startup timeout do: the theme is rebuilt when they arrive.
    pub fn on_term_reply(&mut self, r: tuikit::input::Reply) -> bool {
        let done = self.color_query.feed(r);
        if done {
            let reported = self.color_query.reported();
            self.apply_terminal_colors(reported);
        }
        done
    }

    /// What arrived when the startup wait ran out. The batch stays open for late replies.
    pub fn color_query_timed_out(&mut self) {
        let reported = self.color_query.reported();
        self.apply_terminal_colors(reported);
    }

    /// `applyTerminalColors`: fold the report into what is known, and rebuild the theme only when
    /// something changed, since a rebuild repaints everything.
    pub fn apply_terminal_colors(&mut self, new: Reported) {
        let merged = theme::merge_reported(&self.term.reported, &new);
        if merged == self.term.reported {
            return;
        }
        self.term.reported = merged;
        self.reapply_theme();
    }

    /// Mode 2031: the terminal says it switched between light and dark. Its colours changed too,
    /// so they are asked for again (the caller writes the query).
    pub fn on_color_scheme(&mut self, dark: bool) {
        let previous = self.term.appearance();
        self.term.scheme = Some(if dark {
            Appearance::Dark
        } else {
            Appearance::Light
        });
        self.color_query = ColorQuery::new();
        self.pending.push(Deferred::QueryColors);
        if self.term.appearance() != previous {
            self.reapply_theme();
        }
    }

    fn reapply_theme(&mut self) {
        if self.theme_follows_terminal() {
            self.theme = resolve_theme(self.theme_setting().as_deref(), &self.term);
            // the cached rows were drawn in the old colours under the same theme name
            self.cache.clear();
            self.cache_gen += 1;
            self.dirty = true;
        }
    }

    /// `/reload`: wizard rereads its skills and scripted tools; piw rereads its settings file and
    /// theme (not keybindings: `keybindings.json` is not read, and the message does not say so). A busy turn refuses, as Pi does.
    pub fn reload(&mut self) {
        if self.busy() {
            self.warn("Wait for the current response to finish before reloading.".into());
            return;
        }
        if let Some(d) = self.opts.config_dir.clone() {
            self.settings = Settings::load(&d);
            self.hide_thinking = self.settings.hide_thinking_block;
            self.autocomplete.max_visible = self.settings.autocomplete_max_visible;
            self.theme = resolve_theme(self.theme_setting().as_deref(), &self.term);
        }
        self.send(Request::Prompt("/reload".into()));
        self.status("Reloaded settings, theme and wizard skills".into());
    }

    pub fn open_settings(&mut self) {
        let sel = Selector::settings(&self.settings, &self.theme.name);
        self.open_selector(sel);
    }

    pub fn open_scoped_models(&mut self) {
        let sel = Selector::scoped(&self.config.models, &self.settings.enabled_models);
        self.open_selector(sel);
    }

    /// Seconds since the epoch; pinned when the clock is frozen (tests, screenshots).
    fn unix_now(&self) -> i64 {
        if self.frozen.is_some() {
            return FROZEN_NOW;
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }

    pub fn open_resume(&mut self) {
        self.send(Request::ListSessions);
        let mut sel = Selector::resume(
            &self.sessions,
            &self.cwd,
            &self.transcript.session_id,
            &self.home,
            self.unix_now(),
        );
        if let Selector::Resume(r) = &mut sel {
            r.set_loading(!self.sessions_listed);
        }
        self.open_selector(sel);
    }

    pub fn show_session_info(&mut self) {
        let t = &self.transcript;
        let mut st = ui::selectors::SessionStats {
            name: self.session_name.as_deref(),
            id: &t.session_id,
            path: format!("~/.wizard/sessions/{}.jsonl", t.session_id),
            usage: t.usage.clone(),
            ..Default::default()
        };
        for m in &t.messages {
            match m.role {
                Role::User => st.users += 1,
                Role::Assistant => st.assistants += 1,
                Role::Notice(_) => {}
            }
            for p in &m.parts {
                if let Part::Tool(c) = p {
                    st.tool_calls += 1;
                    if matches!(c.status, ToolStatus::Completed | ToolStatus::Failed) {
                        st.tool_results += 1;
                    }
                }
            }
        }
        let rows = ui::selectors::session_info(&self.cx(self.size.0), &st);
        self.items.push(Item::Rows(rows));
    }

    pub fn show_changelog(&mut self) {
        self.items.push(Item::Changelog);
    }

    pub fn show_hotkeys(&mut self) {
        self.items.push(Item::Hotkeys);
    }

    /// Apply settings the `/settings` selector changed, live, and write them.
    fn apply_settings(&mut self, s: Settings) {
        self.hide_thinking = s.hide_thinking_block;
        self.autocomplete.max_visible = s.autocomplete_max_visible;
        if let Some(name) = s.theme.as_deref() {
            if name != self.theme.name {
                self.theme = resolve_theme(Some(name), &self.term);
            }
        }
        self.settings = s;
        self.save_settings();
        self.dirty = true;
    }

    // ---- keys ----------------------------------------------------------------------------

    fn autocomplete_ctx_update(&mut self) {
        let models = self.config.models.clone();
        let cwd = self.opts.cwd.clone();
        let ctx = AcCtx {
            commands: &self.slash,
            files: &self.files,
            models: &models,
            cwd: &cwd,
        };
        let text = self.editor.text().to_string();
        let cursor = self.editor.ed.cursor();
        let was_files = self.autocomplete.kind() == Some(&Kind::File);
        self.autocomplete.update(&text, cursor, &ctx);
        if !was_files && self.autocomplete.kind() == Some(&Kind::File) {
            self.refresh_files_if_stale();
        }
    }

    /// The `@` list is a snapshot taken at startup. Opening the popup reads it again when it is
    /// a few seconds old, so a file the agent made since is offered.
    fn refresh_files_if_stale(&mut self) {
        if self.files_at.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.files_at = Instant::now();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (cwd, tx) = (self.opts.cwd.clone(), self.msg_tx.clone());
        rt.spawn_blocking(move || {
            let _ = tx.send(Msg::Files(list_files(&cwd)));
        });
    }

    pub fn refresh_autocomplete(&mut self) {
        if matches!(self.dock, Dock::Editor) {
            self.autocomplete_ctx_update();
        }
    }

    fn apply_completion(&mut self, ap: Apply) {
        let t = self.editor.text().to_string();
        // The popup is rebuilt after every edit, but whatever changes the text from outside the
        // key path (the external editor, a restored queue) could leave one behind. A range that
        // no longer fits is dropped, not sliced.
        if !ap.fits(&t) {
            self.autocomplete.reset();
            return;
        }
        let new = format!("{}{}{}", &t[..ap.range.0], ap.text, &t[ap.range.1..]);
        let at = ap.range.0 + ap.text.len();
        self.editor.set_text(&new);
        self.editor.ed.set_cursor(at);
        if !ap.keep_open {
            self.autocomplete.close(&new);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.dirty = true;
        if Search::is_toggle(&key) {
            self.search = match self.search.take() {
                Some(_) => None,
                None => Some(Search::open(self.scroll.top())),
            };
            return;
        }
        if let Some(mut s) = self.search.take() {
            let keymap = &self.keymap;
            let scrolls = |k: &KeyEvent| {
                matches!(
                    keymap.resolve(k),
                    Some(
                        Action::PageUp
                            | Action::PageDown
                            | Action::Top
                            | Action::Bottom
                            | Action::PreviousPrompt
                            | Action::NextPrompt
                    )
                )
            };
            match s.key(key, self.scroll.top(), scrolls) {
                SearchKey::Close => return,
                SearchKey::Handled => {
                    self.search = Some(s);
                    return;
                }
                SearchKey::Pass => self.search = Some(s),
            }
        }
        self.editor.pad = self.settings.editor_padding_x;
        self.editor.set_width(self.size.0);
        if let Dock::Selector(sel) = &mut self.dock {
            let out = sel.on_key(key);
            self.apply_selector(out);
            return;
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if !ctrl_c {
            self.ctrlc_at = self.ctrlc_at.filter(|_| false);
        }
        // popup keys come before the application bindings
        if self.autocomplete.is_open() {
            match key.code {
                KeyCode::Esc => {
                    let t = self.editor.text().to_string();
                    self.autocomplete.close(&t);
                    return;
                }
                KeyCode::Up => {
                    self.autocomplete.up();
                    return;
                }
                KeyCode::Down => {
                    self.autocomplete.down();
                    return;
                }
                KeyCode::Tab => {
                    if let Some(ap) = self.autocomplete.selected() {
                        self.apply_completion(ap);
                        self.refresh_autocomplete();
                    }
                    return;
                }
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
                {
                    if let Some(ap) = self.autocomplete.selected() {
                        let slash = ap.kind == Kind::Slash;
                        let model_arg = ap.kind == Kind::ModelArg;
                        let keep = ap.keep_open;
                        let before = self.editor.text().to_string();
                        self.apply_completion(ap);
                        // an argument typed out in full is sent, not applied a second time
                        if slash || (model_arg && self.editor.text() == before) {
                            self.submit();
                            return;
                        }
                        // a `/model` argument is applied and a second enter submits it
                        if keep || !model_arg {
                            self.refresh_autocomplete();
                        }
                        return;
                    }
                }
                _ => {}
            }
        }
        if let Some(a) = self.keymap.resolve(&key) {
            if self.on_action(a, &key) {
                self.refresh_autocomplete();
                return;
            }
        }
        match key.code {
            KeyCode::Up
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                if let EditKey::AtTop = self.editor.key(key) {
                    if !self.editor.ed.history_prev() {
                        self.editor.ed.move_line_start();
                    }
                }
            }
            KeyCode::Down
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                if let EditKey::AtBottom = self.editor.key(key) {
                    self.editor.ed.history_next();
                }
            }
            KeyCode::Tab => {
                let text = self.editor.text().to_string();
                let cur = self.editor.ed.cursor();
                let cwd = self.opts.cwd.clone();
                if let Some(ap) = self.autocomplete.tab_path(&text, cur, &cwd) {
                    self.apply_completion(ap);
                }
            }
            _ => {
                self.editor.key(key);
            }
        }
        self.refresh_autocomplete();
    }

    /// Returns `true` when the key was consumed.
    fn on_action(&mut self, a: Action, key: &KeyEvent) -> bool {
        let now = self.clock();
        match a {
            Action::Interrupt => {
                if self.busy() && !self.compacting {
                    let n = self.dequeue_all();
                    self.restored_note = (n > 0).then_some(n);
                    // the backend may take seconds to stop; the loader says so meanwhile
                    self.aborting = true;
                    self.send(Request::Cancel);
                } else if self.cancel_bash() {
                } else if self.bash_mode() {
                    self.editor.clear();
                } else if self.compacting {
                    self.send(Request::Cancel);
                    self.compacting = false;
                    self.error("Compaction cancelled".into());
                } else {
                    return false;
                }
            }
            Action::Clear => {
                let had_text = !self.editor.is_empty();
                if had_text {
                    self.editor.clear();
                    self.ctrlc_at = Some(now);
                } else if self
                    .ctrlc_at
                    .is_some_and(|t| now.saturating_sub(t) < DOUBLE)
                {
                    self.quit = true;
                } else {
                    self.ctrlc_at = Some(now);
                }
            }
            Action::Exit => {
                if self.editor.is_empty() {
                    self.quit = true;
                } else {
                    return false;
                }
            }
            Action::Suspend => self.pending.push(Deferred::Suspend),
            Action::ExternalEditor => self.pending.push(Deferred::Editor),
            Action::PasteImage => match read_clipboard_text() {
                Some(t) => self.editor.paste(&t),
                None => self.warn("wizard does not accept images over ACP.".into()),
            },
            Action::ThinkingCycle => self.cycle_thinking(),
            Action::ThinkingToggle => {
                self.hide_thinking = !self.hide_thinking;
                // Pi's `setHideThinkingBlock` drops every block's own choice
                self.think_hidden.clear();
                self.block_gen += 1;
                self.settings.hide_thinking_block = self.hide_thinking;
                self.save_settings();
                self.status(format!(
                    "Thinking blocks: {}",
                    if self.hide_thinking {
                        "hidden"
                    } else {
                        "visible"
                    }
                ));
            }
            Action::ModelSelect => commands::open_model_selector(self, ""),
            Action::ModelCycleForward => self.cycle_model(true),
            Action::ModelCycleBackward => self.cycle_model(false),
            Action::ToolsExpand => {
                self.expanded = !self.expanded;
                // `setToolsExpanded` sets every component, so a click's choice is gone
                self.tool_exp.clear();
                self.compaction_exp.clear();
                self.block_gen += 1;
                self.status(format!(
                    "Tool output: {}",
                    if self.expanded {
                        "expanded"
                    } else {
                        "collapsed"
                    }
                ));
            }
            Action::MessageCopy => match self.last_assistant_text() {
                Some(t) => {
                    self.pending.push(Deferred::Copy(t));
                    // Pi flashes ` Copied! ` at the top right; the status line is /copy's
                    self.flash("Copied!");
                }
                None => self.error("No agent messages to copy yet.".into()),
            },
            Action::FollowUp => {
                if self.busy() {
                    self.submit_as(true);
                } else {
                    self.submit();
                }
            }
            Action::Dequeue => {
                let n = self.dequeue_all();
                if n == 0 {
                    self.status("No queued messages to restore".into());
                } else {
                    self.status(format!(
                        "Restored {n} queued message{} to editor",
                        if n == 1 { "" } else { "s" }
                    ));
                }
            }
            Action::PageUp => {
                let p = self.scroll.page();
                self.scroll.up(p);
            }
            Action::PageDown => {
                let p = self.scroll.page();
                self.scroll.down(p);
            }
            Action::Top => self.scroll.to_top(),
            Action::Bottom => self.scroll.follow(),
            // the row of a user block's first (padding) line is the OSC 133 mark Pi searches for
            Action::PreviousPrompt => {
                let top = self.scroll.top();
                if let Some(&r) = self.prompt_rows.iter().rev().find(|&&r| r < top) {
                    self.scroll.to(r);
                }
            }
            Action::NextPrompt => {
                let top = self.scroll.top();
                if let Some(&r) = self.prompt_rows.iter().find(|&&r| r > top) {
                    self.scroll.to(r);
                }
            }
            Action::NewLine => self.editor.ed.insert_newline(),
            Action::Submit => {
                // a trailing backslash makes a newline for terminals without shift+enter
                if self.editor.text().ends_with('\\') {
                    self.editor.ed.backspace();
                    self.editor.ed.insert_newline();
                } else {
                    self.submit();
                }
            }
            Action::Tab => return false,
        }
        let _ = key;
        true
    }

    fn apply_selector(&mut self, out: SelOut) {
        match out {
            SelOut::None => {}
            SelOut::Close => self.close_selector(),
            SelOut::SetModel(id) => {
                let name = self
                    .config
                    .models
                    .iter()
                    .find(|m| m.id == id)
                    .map_or(id.clone(), |m| m.name.clone());
                self.send(Request::SetModel(id));
                self.close_selector();
                self.status(format!("Model: {name}"));
            }
            SelOut::SetThinking(level) => {
                self.close_selector();
                self.set_effort(level);
            }
            SelOut::Resume(id) => {
                self.close_selector();
                self.after_history = Some(After::Resumed);
                self.send(Request::LoadSession(id));
            }
            SelOut::Warning(t) => {
                self.warn(t);
            }
            SelOut::Settings(s) => self.apply_settings(*s),
            SelOut::Theme(name) => {
                self.theme = resolve_theme(Some(&name), &self.term);
            }
            SelOut::Scoped(ids) => self.settings.enabled_models = ids,
            SelOut::ScopedSave(ids) => {
                self.settings.enabled_models = ids;
                self.save_settings();
            }
        }
    }

    fn save_settings(&self) {
        if let Some(d) = &self.opts.config_dir {
            self.settings.save(d);
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        self.dirty = true;
        if let Some(s) = &mut self.search {
            s.paste(text, self.scroll.top());
            return;
        }
        match &mut self.dock {
            Dock::Selector(s) => s.on_paste(text),
            Dock::Editor => {
                self.editor.pad = self.settings.editor_padding_x;
                self.editor.set_width(self.size.0);
                self.editor.paste(text);
                self.refresh_autocomplete();
            }
        }
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        let now = self.clock();
        match ev.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let n = self.wheel_lines(now, ev.modifiers.contains(KeyModifiers::ALT));
                if ev.kind == MouseEventKind::ScrollUp {
                    self.scroll.up(n);
                } else {
                    self.scroll.down(n);
                }
                self.scroll.flash(now);
                self.dirty = true;
            }
            // the jump indicator sits on the last transcript row
            MouseEventKind::Down(_)
                if self.scroll.scrolled_up() && ev.row as usize + 1 == self.scroll_view_rows() =>
            {
                self.scroll.follow();
                self.dirty = true;
            }
            MouseEventKind::Down(MouseButton::Left) => self.press = Some((ev.column, ev.row)),
            // a release on the cell the press was on is a click, anywhere else a drag
            MouseEventKind::Up(MouseButton::Left)
                if self.press.take() == Some((ev.column, ev.row)) =>
            {
                self.click(ev.row);
            }
            _ => {}
        }
    }

    /// A left click on screen row `row`: the block under it flips, if it has a result to show or
    /// hide.
    fn click(&mut self, row: u16) {
        if row as usize >= self.scroll_view_rows() {
            return;
        }
        let at = self.scroll.top() + row as usize;
        let Some((_, target)) = self
            .click_map
            .iter()
            .find(|(r, _)| r.contains(&at))
            .cloned()
        else {
            return;
        };
        match target {
            Click::Tool { msg, id } => {
                let now = self
                    .tool_exp
                    .get(&(msg, id.clone()))
                    .copied()
                    .unwrap_or(self.expanded);
                self.tool_exp.insert((msg, id), !now);
            }
            Click::Think { msg, run } => {
                let now = self
                    .think_hidden
                    .get(&(msg, run))
                    .copied()
                    .unwrap_or(self.hide_thinking);
                self.think_hidden.insert((msg, run), !now);
            }
            Click::Compaction(idx) => {
                let now = self
                    .compaction_exp
                    .get(&idx)
                    .copied()
                    .unwrap_or(self.expanded);
                self.compaction_exp.insert(idx, !now);
            }
        }
        self.block_gen += 1;
        self.cache.clear();
        self.dirty = true;
    }

    /// Pi's `auto` wheel setting: one line for an isolated notch, more for a fast spin
    /// (notches 100 ms apart are 1, 50 ms are 2, 20 ms are 5); alt moves five times as far.
    fn wheel_lines(&mut self, now: Duration, alt: bool) -> usize {
        let gap = self.last_wheel.map(|t| now.saturating_sub(t));
        self.last_wheel = Some(now);
        let n = match gap {
            Some(g) if g <= Duration::from_millis(30) => 5,
            Some(g) if g <= Duration::from_millis(70) => 2,
            _ => 1,
        };
        if alt {
            n * 5
        } else {
            n
        }
    }

    fn scroll_view_rows(&self) -> usize {
        self.scroll.view_rows()
    }

    pub fn on_resize(&mut self, w: u16, h: u16) {
        self.size = (w, h);
        self.dirty = true;
    }

    /// Deadline bookkeeping; called when the loop wakes without input.
    /// Show `msg` at the top right for a second (Pi's alternate-screen flash).
    pub fn flash(&mut self, msg: &str) {
        let until = self.clock() + Duration::from_millis(1000);
        self.flashes.push((msg.to_string(), until));
        self.dirty = true;
    }

    pub fn on_tick(&mut self, now: Instant) {
        let clock = self.clock();
        let n = self.flashes.len();
        self.flashes.retain(|(_, until)| clock < *until);
        if self.flashes.len() != n {
            self.dirty = true;
        }
        if self.animating()
            && now.duration_since(self.last_anim) >= tuikit::spinner::BRAILLE_INTERVAL
        {
            self.last_anim = now;
            self.dirty = true;
        }
        if let Some(t) = self.scroll.bar_deadline() {
            if self.clock() >= t {
                self.dirty = true;
            }
        }
    }

    /// Something on screen changes with the clock: the spinner, `Elapsed`, the scrollbar fade.
    pub fn animating(&self) -> bool {
        self.busy() || self.bashes.last().is_some_and(|b| b.running)
    }

    /// Next instant the loop must wake at with no input, or `None` to sleep until something
    /// happens.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        let mut w: Option<Instant> = None;
        let mut take = |t: Instant| w = Some(w.map_or(t, |c| c.min(t)));
        if let Some(until) = self.flashes.iter().map(|(_, u)| *u).min() {
            take(now + until.saturating_sub(self.clock()));
        }
        if self.animating() {
            take((self.last_anim + tuikit::spinner::BRAILLE_INTERVAL).max(now));
        }
        if let Some(t) = self.scroll.bar_deadline() {
            let left = t.saturating_sub(self.clock());
            if !left.is_zero() {
                take(now + left);
            }
        }
        w
    }

    // ---- exit ----------------------------------------------------------------------------

    /// What is printed on the normal screen after the alternate screen closes: the whole
    /// document, the empty editor box, the footer, then the resume hint.
    pub fn epilogue(&mut self, width: u16) -> String {
        let cx = self.cx(width);
        let mut lines = self.transcript_lines(&cx);
        // leaving for another look: the editor and footer belong to a UI that is gone
        if !self.switched {
            lines.push(ui::blank());
            let rule = ui::rule(width, cx.th().fg(self.thinking_tok()));
            lines.push(rule.clone());
            lines.push(ui::blank());
            lines.push(rule);
            lines.extend(ui::footer::render(self, &cx));
        }
        let mut out = String::new();
        for l in &lines {
            out.push_str(&ui::ansi_line(l));
            out.push_str("\r\n");
        }
        if !self.transcript.session_id.is_empty() && !self.switched {
            // Pi writes this with SGR 2 whatever the theme, not with its `dim` colour
            let dim = ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM);
            out.push_str("\r\n");
            out.push_str(&ui::ansi_line(&Line::from(vec![
                ui::span("To resume this session:", dim),
                ratatui::text::Span::raw(format!(
                    " wizard-ui-pi --session {}",
                    tuikit::width::shell_word(&self.transcript.session_id)
                )),
            ])));
            out.push_str("\r\n");
        }
        out
    }
}

fn tool_time(
    times: &HashMap<String, (Duration, Option<Duration>)>,
    c: &ToolCall,
    now: Duration,
) -> ToolTime {
    match times.get(&c.id) {
        Some((start, Some(end))) => ToolTime {
            elapsed: end.saturating_sub(*start),
            done: true,
        },
        Some((start, None)) => ToolTime {
            elapsed: now.saturating_sub(*start),
            done: false,
        },
        None => ToolTime {
            elapsed: Duration::ZERO,
            done: matches!(c.status, ToolStatus::Completed | ToolStatus::Failed),
        },
    }
}

/// `--use-theme` or the settings file's theme, whichever applies. Nothing set means Pi's
/// default, the `system` theme.
fn resolve_theme(setting: Option<&str>, term: &TermColors) -> PiTheme {
    let name = theme::resolve_setting(setting, term.appearance());
    match name.as_deref() {
        None | Some("system") => PiTheme::system(&term.reported, term.appearance()),
        // an unreadable theme falls back to the system theme without a word, as Pi's does
        Some(n) => {
            load_theme(n).unwrap_or_else(|| PiTheme::system(&term.reported, term.appearance()))
        }
    }
}

/// `Ok` when `--use-theme` / `--theme` names something piw can load: `dark`, `light`, `system`,
/// or a Pi theme file.
pub fn check_theme(name: &str) -> Result<(), String> {
    if matches!(name, "dark" | "light" | "system") {
        return Ok(());
    }
    match std::fs::read_to_string(name) {
        Ok(src) => PiTheme::from_json(&src)
            .map(|_| ())
            .map_err(|e| format!("theme {name}: {e}")),
        Err(_) => Err(format!(
            "unknown theme \"{name}\" (dark, light, system, or the path of a theme file)"
        )),
    }
}

fn load_theme(name: &str) -> Option<PiTheme> {
    if let Some(t) = PiTheme::builtin(name) {
        return Some(t);
    }
    let src = std::fs::read_to_string(name).ok()?;
    PiTheme::from_json(&src).ok()
}

fn first_number_before(text: &str, word: &str) -> Option<u64> {
    let i = text.find(word)?;
    let head = text[..i].trim_end();
    let digits: String = head
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter(|c| *c != ',')
        .collect();
    digits.parse().ok()
}

/// What the model and the screen get of a command's output: escape sequences gone (whole, with
/// their bodies), `\r` normalised, tabs as three spaces like Pi, and the last 2000 lines.
pub fn clean_output(s: &str) -> String {
    let s = tuikit::width::normalize_newlines(s).replace('\t', "   ");
    let out = tuikit::width::plain_text(&s);
    let lines: Vec<&str> = out.lines().collect();
    if lines.len() > 2000 {
        lines[lines.len() - 2000..].join("\n")
    } else {
        out.into_owned()
    }
}

fn fmt_limit(d: Duration) -> String {
    if d.as_millis().is_multiple_of(1000) {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// The tail the runner kept, cleaned, with a first line saying that the start is gone.
fn shown_output(tail: &str, cut: bool) -> String {
    let body = clean_output(tail);
    let more = cut || tail.lines().count() > 2000;
    if more {
        format!("[earlier output not kept]\n{body}")
    } else {
        body
    }
}

/// The last 50 KB of `s` for the model's context, from a line start (Pi's limit for `!`).
fn context_tail(s: &str) -> &str {
    const LIMIT: usize = 50 * 1024;
    if s.len() <= LIMIT {
        return s;
    }
    let mut at = s.len() - LIMIT;
    while !s.is_char_boundary(at) {
        at += 1;
    }
    let t = &s[at..];
    t.find('\n').map_or(t, |i| &t[i + 1..])
}

/// `s` with any run of backticks that could close a fence of three made harmless.
fn fence_safe(s: &str) -> String {
    s.replace("```", "`\u{200b}``")
}

fn read_clipboard_text() -> Option<String> {
    for (bin, args) in [
        ("wl-paste", &["-n"][..]),
        ("xclip", &["-selection", "clipboard", "-o"][..]),
        ("pbpaste", &[][..]),
    ] {
        if let Ok(o) = std::process::Command::new(bin).args(args).output() {
            if o.status.success() && !o.stdout.is_empty() {
                return Some(String::from_utf8_lossy(&o.stdout).into_owned());
            }
        }
    }
    None
}

fn load_history(dir: Option<&std::path::Path>) -> Option<Vec<String>> {
    // one invalid byte must not cost the whole history
    let bytes = std::fs::read(dir?.join("history.json")).ok()?;
    serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()
}

/// Entries longer than this stay out of the history file (a pasted file, a key someone just
/// pasted); they are still in this session's history.
const HISTORY_ENTRY_MAX: usize = 64 * 1024;
const HISTORY_FILE_MAX: usize = 1024 * 1024;

fn save_history(dir: Option<&std::path::Path>, ed: &EditorState) {
    let Some(dir) = dir else { return };
    let all: Vec<&str> = ed.ed.history_entries().collect();
    let mut keep: Vec<&str> = all[all.len().saturating_sub(100)..]
        .iter()
        .copied()
        .filter(|e| e.len() <= HISTORY_ENTRY_MAX)
        .collect();
    // oldest first out until the file is a size the UI thread can rewrite without noticing
    while keep.iter().map(|e| e.len()).sum::<usize>() > HISTORY_FILE_MAX {
        keep.remove(0);
    }
    if let Ok(t) = serde_json::to_string(&keep) {
        // owner-only, replaced whole: it holds whatever was pasted into the prompt
        let _ = tuikit::private::write_replace(&dir.join("history.json"), t.as_bytes());
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
        PiMock::spawn(cwd, resume)?
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
            let s = std::fs::read_to_string(&git).ok()?;
            PathBuf::from(s.trim().strip_prefix("gitdir:")?.trim()).join("HEAD")
        } else {
            continue;
        };
        let s = std::fs::read_to_string(head).ok()?;
        let s = s.trim();
        return Some(match s.strip_prefix("ref: refs/heads/") {
            Some(b) => b.to_string(),
            None => "detached".to_string(),
        });
    }
    None
}

/// Most files the `@` popup indexes.
pub const MAX_FILES: usize = 20_000;

/// Files for the `@` popup: git's view of the tree (hardened, NUL separated), or a shallow walk
/// outside a repo.
pub fn list_files(cwd: &std::path::Path) -> Vec<String> {
    tuikit::git::project_files(cwd, MAX_FILES)
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

fn draw_frame(terminal: &mut Terminal, app: &mut App) -> std::io::Result<()> {
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    let mut out = std::io::stdout();
    crossterm::queue!(out, BeginSynchronizedUpdate)?;
    let depth = tuikit::depth::current();
    terminal.draw(|f| {
        ui::draw(f.buffer_mut(), app);
        tuikit::depth::reduce(f.buffer_mut(), depth);
    })?;
    crossterm::execute!(out, EndSynchronizedUpdate)?;
    if app.title_dirty {
        app.title_dirty = false;
        crossterm::execute!(out, crossterm::terminal::SetTitle(app.window_title()))?;
    }
    Ok(())
}

fn editor_text(content: &str) -> &str {
    content
        .strip_suffix('\n')
        .filter(|b| !b.contains('\n'))
        .unwrap_or(content)
}

/// The colour query Pi sends: OSC 10, 11, 4;0 to 4;15, then DA1.
pub fn query_colors() {
    let mut out = std::io::stdout();
    let _ = out.write_all(theme::COLOR_QUERY.as_bytes());
    let _ = out.flush();
}

fn run_editor(app: &mut App, guard: &mut TermGuard, input: &tuikit::input::InputControl) {
    use tuikit::input::{external_editor, EditorOutcome};
    let out = external_editor(
        guard,
        input,
        &app.opts.cwd,
        "piw",
        &app.editor.expanded(),
        Some("nano"),
    );
    match out {
        EditorOutcome::NoEditor => {}
        EditorOutcome::Failed(m) => app.error(m),
        // an emptied file empties the prompt, as it does in Pi
        EditorOutcome::Edited(t) => app.editor.set_text(editor_text(&t)),
    }
    // whatever the popup showed was for the text before the editor
    app.autocomplete.reset();
}

fn suspend(guard: &mut TermGuard) {
    guard.suspend();
    #[cfg(unix)]
    {
        let _ = signal_hook::low_level::raise(signal_hook::consts::SIGTSTP);
    }
    let _ = guard.resume();
}

/// Run until the app quits or the terminal closes.
pub async fn run(
    app: &mut App,
    terminal: &mut Terminal,
    guard: &mut TermGuard,
    mut msg_rx: UnboundedReceiver<Msg>,
    input: tuikit::input::Input,
) -> Result<()> {
    // Our own reader, not crossterm's: it hears the terminal's colour replies and scheme
    // reports, decodes what tmux sends for shift+enter, and can be parked while `$EDITOR` has
    // the terminal.
    let input_ctl = input.control();
    // The pump only carries the keyboard. Backend messages are read from `msg_rx` here instead
    // of being forwarded into the pump's queue, so a key is never stuck behind a backlog of
    // deltas: terminal events are always looked at first.
    let (_no_msgs, pump_rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    let mut pump = EventPump::<Msg>::spawn_raw(input, Duration::ZERO, pump_rx);
    if let Ok((w, h)) = crossterm::terminal::size() {
        app.size = (w, h);
    }
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut fresh_bytes = 0usize;
    loop {
        let now = Instant::now();
        // a frame is also due when enough text has come in, however soon the last one was: the
        // stream is applied far faster than it can be laid out
        if app.dirty
            && (now.duration_since(last_draw) >= MIN_FRAME
                || fresh_bytes >= crate::pump::FRAME_BYTES)
        {
            app.dirty = false;
            fresh_bytes = 0;
            draw_frame(terminal, app)?;
            last_draw = Instant::now();
        }
        for d in std::mem::take(&mut app.pending) {
            match d {
                Deferred::Suspend => {
                    suspend(guard);
                    *terminal = guard.terminal()?;
                }
                Deferred::Editor => {
                    run_editor(app, guard, &input_ctl);
                    *terminal = guard.terminal()?;
                }
                Deferred::QueryColors => query_colors(),
                Deferred::Copy(t) => osc52_copy(&t),
            }
            app.dirty = true;
        }
        if app.quit {
            break;
        }
        let deadline = if app.dirty {
            Some(last_draw + MIN_FRAME)
        } else {
            app.next_wake(now)
        };
        let wait = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d.into()).await,
                None => std::future::pending().await,
            }
        };
        let first = tokio::select! {
            biased;
            e = pump.next() => e,
            m = msg_rx.recv() => m.map(TermEvent::Msg),
            () = wait => Some(TermEvent::Tick),
        };
        let Some(first) = first else { break };
        // Apply what is queued for one slice, then go and draw: a flood of deltas must not hold
        // the screen or the keyboard until it is over. Runs of text deltas become one append.
        let mut held: Option<TermEvent<Msg>> = Some(first);
        let mut closed = false;
        crate::pump::drain_slice(
            || {
                let e = match held.take() {
                    Some(e) => e,
                    // the keyboard first, then the backend
                    None => match pump.next().now_or_never() {
                        Some(Some(e)) => e,
                        Some(None) => {
                            closed = true;
                            return None;
                        }
                        None => TermEvent::Msg(msg_rx.try_recv().ok()?),
                    },
                };
                let (e, rest) =
                    crate::pump::merge_deltas(e, || msg_rx.try_recv().ok().map(TermEvent::Msg));
                held = rest;
                Some(e)
            },
            |e| {
                let w = crate::pump::weight(&e);
                if !app.quit {
                    handle(app, e)
                }
                fresh_bytes += w;
                w
            },
            crate::pump::SLICE,
            crate::pump::FRAME_BYTES,
        );
        // an event held back by the last merge is handled first on the next pass
        if let Some(e) = held.take() {
            handle(app, e);
        }
        if closed {
            app.quit = true;
        }
    }
    Ok(())
}

fn handle(app: &mut App, ev: TermEvent<Msg>) {
    match ev {
        TermEvent::Key(k) => app.on_key(k),
        TermEvent::Mouse(m) => app.on_mouse(m),
        TermEvent::Paste(s) => app.on_paste(&s),
        TermEvent::Resize(w, h) => app.on_resize(w, h),
        TermEvent::FocusGained => {
            app.branch = git_branch(&app.opts.cwd);
            app.dirty = true;
        }
        TermEvent::FocusLost => {}
        TermEvent::ColorScheme(dark) => app.on_color_scheme(dark),
        TermEvent::Tick => app.on_tick(Instant::now()),
        TermEvent::Msg(m) => app.update(m),
    }
    // A wake with nothing to do must not repaint.
    app.on_tick(Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_is_read_from_the_notice() {
        assert_eq!(
            first_number_before("Compacted from 1,261 tokens", "token"),
            Some(1261)
        );
        assert_eq!(first_number_before("done", "token"), None);
    }

    #[test]
    fn output_is_stripped_and_normalised() {
        assert_eq!(
            clean_output("\x1b[31mred\x1b[0m\r\nok\t.\n"),
            "red\nok   .\n"
        );
    }

    #[test]
    fn scroll_follows_until_moved() {
        let mut s = Scroll::default();
        s.set_extent(100, 20);
        assert_eq!(s.top(), 80);
        s.up(5);
        assert_eq!(s.top(), 75);
        assert!(s.scrolled_up());
        s.down(5);
        assert!(!s.scrolled_up());
    }
}
