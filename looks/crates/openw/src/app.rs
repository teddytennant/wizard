// OWNER: shared (state, update, event loop; touch with care, three builders depend on it)
//! App state, the message type, `update`, and the event loop.
//!
//! Everything the UI shows lives in [`App`]. Terminal input and backend events both become calls
//! on it (`on_key`, `update`); the loop draws only when something marked it dirty or an animation
//! deadline passed, so an idle app wakes for nothing.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_core::mock::MockBackend;
use agent_core::transcript::{Part, Role, Transcript};
use agent_core::{
    Backend, BackendHandle, Config, Event, NoticeLevel, Request, SessionInfo, SlashCommand,
    StopReason, ToolCall, ToolKind, ToolStatus,
};
use anyhow::Result;
use backend_wizard::WizardBackend;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures_util::FutureExt;
use ratatui::style::Color;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tuikit::scroll::ScrollState;
use tuikit::term::{Event as TermEvent, EventPump, TermGuard, Terminal};
use tuikit::theme::Variant;
use tuikit::toast::{Toast, ToastState};
use tuikit::{Mode, Theme};

use crate::commands::{self, CmdCtx, Command, Source};
use crate::keys::{Action, Keymap, Resolve};
use crate::ui;
use crate::ui::dialogs::{self, DialogStack, Effect, Outcome};
use crate::ui::prompt::{self, PromptEvent, PromptState};
use crate::ui::session::SessionView;

/// The clock dialogs see when the app runs under test.
pub const FROZEN_NOW: i64 = 1_790_003_600;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MIN_FRAME: Duration = Duration::from_millis(16);
const LEADER_TIMEOUT: Duration = Duration::from_millis(2000);
const ESC_WINDOW: Duration = Duration::from_millis(5000);
const WHEEL_ROWS: isize = 3;
/// `connecting…` for this long gets a toast: wizard is not answering `initialize`.
const SLOW_START: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Home,
    Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Build,
    Plan,
}

impl AgentKind {
    pub fn title(self) -> &'static str {
        match self {
            AgentKind::Build => "Build",
            AgentKind::Plan => "Plan",
        }
    }
    pub fn color(self, t: &Theme) -> Color {
        match self {
            AgentKind::Build => t.secondary,
            AgentKind::Plan => t.warning,
        }
    }
    pub fn toggled(self) -> Self {
        match self {
            AgentKind::Build => AgentKind::Plan,
            AgentKind::Plan => AgentKind::Build,
        }
    }
}

/// A `File` / `Directory` chip under a user message.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub directory: bool,
    pub name: String,
}

/// What openw knows about a user message that the transcript does not carry.
#[derive(Clone, Debug, Default)]
pub struct MsgMeta {
    pub at: Option<std::time::SystemTime>,
    pub files: Vec<Chip>,
    /// The message is the `/compact` marker: it renders as a divider, not as text.
    pub compaction: bool,
}

#[derive(Clone, Debug)]
pub struct ViewFlags {
    pub conceal: bool,
    pub timestamps: bool,
    pub thinking_shown: bool,
    pub tool_details: bool,
    pub generic_output: bool,
    pub scrollbar: bool,
    pub tips_hidden: bool,
}

impl Default for ViewFlags {
    fn default() -> Self {
        ViewFlags {
            conceal: true,
            timestamps: false,
            thinking_shown: false,
            tool_details: true,
            generic_output: false,
            scrollbar: false,
            tips_hidden: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarPref {
    Auto,
    Show,
    Hide,
}

#[derive(Clone, Copy, Debug)]
pub struct SidebarState {
    pub shown: bool,
    pub overlay: bool,
}

/// `/undo` over wizard takes three exchanges: list the turns, rewind to the newest, reload the
/// session. The replies are read here instead of landing in the transcript.
#[derive(Debug)]
struct Undo {
    stage: UndoStage,
    /// Reply text collected during the current exchange.
    buf: String,
    /// The user message being taken back; its text returns to the prompt.
    restore: String,
    /// Interrupt first, as opencode does; the listing starts once the turn has ended.
    waiting_for_cancel: bool,
}

#[derive(Debug, PartialEq)]
enum UndoStage {
    Listing,
    Rewinding,
}

/// The newest turn in wizard's `/rewind` listing: its number and the start of its prompt. The
/// reply is a header line, then one `N — prompt · files` line per turn, newest first; the
/// header may also carry the first turn (`rewind to before turn: 7 — fix the build`).
fn parse_rewind_listing(text: &str) -> Option<(String, String)> {
    text.lines().find_map(|l| {
        let l = l.trim();
        let l = l.strip_prefix("rewind to before turn:").unwrap_or(l).trim();
        let (num, prompt) = l.split_once('—')?;
        let num = num.trim();
        (!num.is_empty() && num.chars().all(|c| c.is_ascii_digit()))
            .then(|| (num.to_string(), prompt.trim().to_string()))
    })
}

/// Messages that reach the app from tasks and the backend.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Backend(Event),
    Branch(Option<String>),
    Files(Vec<String>),
    /// A chunk of a running `!cmd`'s output.
    ShellOut {
        id: String,
        text: String,
    },
    /// A `!cmd` ended. `note` is a line to add under the output (timeout, interrupt).
    Shell {
        id: String,
        note: String,
        ok: bool,
        cancelled: bool,
    },
    /// What ctrl+v found on the system clipboard, read off the UI thread.
    Clip(Option<crate::clipboard::Clip>),
}

/// Most output of a `!cmd` kept in the transcript.
const SHELL_OUTPUT_CAP: usize = 256 * 1024;
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);

/// `OPENW_SHELL_TIMEOUT_MS` shortens the limit, for tests.
fn shell_timeout() -> Duration {
    std::env::var("OPENW_SHELL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(SHELL_TIMEOUT, Duration::from_millis)
}

/// A running `!cmd`. The shell leads a process group of its own, so stopping it stops what it
/// started too; dropping the job while it runs (openw quitting, a panic) does the same.
struct ShellJob {
    id: String,
    /// Process group of the shell, 0 once it is over.
    pgid: std::sync::Arc<std::sync::atomic::AtomicI32>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// Output kept so far, to hold the cap.
    kept: usize,
    truncated: bool,
}

impl ShellJob {
    fn signal(&self, sig: i32) {
        let g = self.pgid.load(std::sync::atomic::Ordering::SeqCst);
        if g > 0 {
            // SAFETY: killpg(2) on the group this job created.
            unsafe { libc::killpg(g, sig) };
        }
    }
}

impl Drop for ShellJob {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}

/// Split off the longest valid UTF-8 prefix of `buf`; an incomplete character at the end stays
/// for the next chunk, bytes that can never be valid become U+FFFD.
fn drain_utf8(buf: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(buf) {
            Ok(s) => {
                out.push_str(s);
                buf.clear();
                return out;
            }
            Err(e) => {
                let good = e.valid_up_to();
                out.push_str(&String::from_utf8_lossy(&buf[..good]));
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        buf.drain(..good + n);
                    }
                    None => {
                        buf.drain(..good);
                        return out;
                    }
                }
            }
        }
    }
}

async fn run_shell_job(
    id: String,
    cmd: String,
    cwd: String,
    pgid: std::sync::Arc<std::sync::atomic::AtomicI32>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    tx: UnboundedSender<Msg>,
) {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;
    let end = |note: String, ok: bool, cancelled: bool| Msg::Shell {
        id: id.clone(),
        note,
        ok,
        cancelled,
    };
    let spawned = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(end(e.to_string(), false, false));
            return;
        }
    };
    let group = child.id().map_or(0, |p| p as i32);
    pgid.store(group, Ordering::SeqCst);
    let (otx, mut orx) = tokio::sync::mpsc::unbounded_channel::<String>();
    fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
        mut r: R,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        tokio::spawn(async move {
            let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
            loop {
                match r.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        let t = drain_utf8(&mut buf);
                        if !t.is_empty() && tx.send(t).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
    if let Some(o) = child.stdout.take() {
        pump(o, otx.clone());
    }
    if let Some(e) = child.stderr.take() {
        pump(e, otx.clone());
    }
    drop(otx);
    let forward = |text: String| {
        let _ = tx.send(Msg::ShellOut {
            id: id.clone(),
            text,
        });
    };
    let limit = shell_timeout();
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let (note, ok, cancelled) = loop {
        tokio::select! {
            Some(t) = orx.recv() => forward(t),
            st = child.wait() => {
                // a background child that kept the pipes open must not hold the result back
                let grace = tokio::time::sleep(Duration::from_millis(150));
                tokio::pin!(grace);
                loop {
                    tokio::select! {
                        t = orx.recv() => match t { Some(t) => forward(t), None => break },
                        _ = &mut grace => break,
                    }
                }
                break (String::new(), st.map(|s| s.success()).unwrap_or(false), false);
            }
            _ = &mut deadline => {
                terminate(&mut child, group).await;
                break (format!("timed out after {}", fmt_limit(limit)), false, false);
            }
            _ = &mut stop => {
                terminate(&mut child, group).await;
                break ("interrupted".to_string(), false, true);
            }
        }
    };
    pgid.store(0, Ordering::SeqCst);
    let _ = tx.send(end(note, ok, cancelled));
}

fn fmt_limit(d: Duration) -> String {
    if d.as_millis().is_multiple_of(1000) {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// SIGTERM the whole group, SIGKILL it if the shell is still there 1.5 s later.
async fn terminate(child: &mut tokio::process::Child, group: i32) {
    if group > 0 {
        // SAFETY: killpg(2) on the group created for this job.
        unsafe { libc::killpg(group, libc::SIGTERM) };
    }
    if tokio::time::timeout(Duration::from_millis(1500), child.wait())
        .await
        .is_err()
    {
        if group > 0 {
            // SAFETY: as above.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
        let _ = child.wait().await;
    }
}

/// Work that needs the terminal, which only the loop owns.
#[derive(Debug, PartialEq)]
pub enum Deferred {
    Suspend,
    Editor,
    Copy(String),
}

#[derive(Clone, Debug, Default)]
pub struct AppOpts {
    pub cwd: PathBuf,
    pub continue_latest: bool,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub theme: Option<String>,
    pub mock: bool,
    /// Where history and settings live; `None` keeps everything in memory (tests).
    pub state_dir: Option<PathBuf>,
    /// Picks the placeholder example and the tip.
    pub seed: u64,
    /// Where `state.json` (theme, favorites, pins) lives; `None` keeps it in memory (tests).
    pub config_dir: Option<PathBuf>,
    /// `~/.wizard`, read for MCP servers, skills and plugins; `None` means none (tests).
    pub wizard_dir: Option<PathBuf>,
}

pub struct App {
    pub opts: AppOpts,
    pub theme: Theme,
    pub keymap: Keymap,
    pub route: Route,
    pub size: (u16, u16),
    pub cwd: String,
    pub branch: Option<String>,
    pub transcript: Transcript,
    pub config: Config,
    pub backend_cmds: Vec<SlashCommand>,
    pub cmds: Vec<Command>,
    pub sessions: Vec<SessionInfo>,
    pub titles: HashMap<String, String>,
    /// What the dialogs remember (theme, favorites, recent models, pins, renames).
    pub prefs: dialogs::prefs::Prefs,
    /// When each stashed prompt was stashed, parallel to `prompt.stash`.
    pub stash_at: Vec<i64>,
    pub prompt: PromptState,
    pub dialogs: DialogStack,
    /// Permissions and questions waiting for an answer; the first one is shown.
    pub asks: std::collections::VecDeque<dialogs::ask::Ask>,
    /// Questions answered or dismissed, for whoever sent them (`agent-core` has no event or
    /// request for questions, so nothing here is sent to the backend).
    pub question_results: Vec<dialogs::ask::AskOutcome>,
    pub toasts: ToastState,
    pub scroll: ScrollState,
    pub view: SessionView,
    pub agent: AgentKind,
    pub msg_agents: HashMap<usize, AgentKind>,
    /// Per user message: send time, file chips, compaction marker. Keyed by message index.
    pub msg_meta: HashMap<usize, MsgMeta>,
    pub flags: ViewFlags,
    /// `[Pasted ~N lines]` summaries; off with the palette's `Disable paste summary`.
    pub paste_summary: bool,
    pub sidebar_pref: SidebarPref,
    pub sb: ui::sidebar::SideState,
    /// Index of the subagent run being viewed (see `ui::header`), `None` in the parent session.
    pub child: Option<usize>,
    pub tip: usize,
    pub files: Vec<String>,
    pub tx: UnboundedSender<Request>,
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
    /// A retry or warning wizard reported mid-turn; the busy hint row shows it in `error`.
    pub busy_note: Option<String>,
    leader_at: Option<Instant>,
    esc_at: Option<Instant>,
    started: Instant,
    pub frozen: Option<Duration>,
    last_anim: Instant,
    continue_pending: bool,
    initial_prompt: Option<String>,
    silent_notices: u32,
    undo: Option<Undo>,
    shell_seq: u32,
    /// The `!cmd` that is running, if any.
    shell: Option<ShellJob>,
    files_cut_told: bool,
    pub title_dirty: bool,
    /// When the second `esc` sent `Cancel`; cleared when the turn ends or the session reloads.
    pub interrupt_at: Option<Instant>,
    /// Last time the backend said anything, for the "not responding" hint.
    pub last_backend: Instant,
    slow_start_warned: bool,
    /// When `files` was last read; `@` re-reads it when it is stale.
    files_at: Instant,
    /// Effort the backend started with, so a change made here can be shown.
    effort_at_start: Option<String>,
    /// Model the user picked (or `-m` named), applied again whenever wizard resets it.
    wanted_model: Option<String>,
    /// A save to `state.json` or `kv.json` failed and the user has been told.
    save_failed: bool,
}

impl App {
    pub fn new(opts: AppOpts, tx: UnboundedSender<Request>, msg_tx: UnboundedSender<Msg>) -> App {
        // opencode leaves languages it has no parser for uncoloured; so does openw
        tuikit::syntax::set_opencode_parity(true);
        // and draws a tab as two cells, in the prompt and the transcript alike
        tuikit::width::set_tab_width(2);
        // and wraps a word together with the spaces after it
        tuikit::width::set_spaces_fit(true);
        let kv = load_kv(opts.state_dir.as_deref());
        let prefs_loaded = opts
            .config_dir
            .as_deref()
            .map(dialogs::prefs::Prefs::load)
            .unwrap_or_default();
        let theme_name = opts
            .theme
            .clone()
            .or_else(|| prefs_loaded.theme.clone())
            .or_else(|| kv.get("theme").and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_else(|| tuikit::theme::DEFAULT_THEME.to_string());
        let mode = match kv.get("theme_mode").and_then(|v| v.as_str()) {
            Some("light") => Mode::Light,
            _ => Mode::Dark,
        };
        let theme =
            Theme::builtin_mode(&theme_name, mode).unwrap_or_else(|| Theme::default_theme(mode));
        let cwd = opts.cwd.display().to_string();
        let keymap = Keymap::new();
        let flags = ViewFlags {
            tips_hidden: kv
                .get("tips_hidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            ..Default::default()
        };
        let sidebar_pref = match kv.get("sidebar").and_then(|v| v.as_str()) {
            Some("hide") => SidebarPref::Hide,
            _ => SidebarPref::Auto,
        };
        let seed = opts.seed as usize;
        let prompt = PromptState::new(seed, opts.state_dir.clone());
        let mut app = App {
            continue_pending: opts.continue_latest,
            initial_prompt: opts.prompt.clone(),
            theme,
            keymap,
            route: Route::Home,
            size: (120, 36),
            cwd,
            branch: None,
            transcript: Transcript::new(),
            config: Config::default(),
            backend_cmds: Vec::new(),
            cmds: Vec::new(),
            sessions: Vec::new(),
            titles: prefs_loaded.titles.clone().into_iter().collect(),
            prefs: prefs_loaded,
            stash_at: Vec::new(),
            prompt,
            dialogs: DialogStack::default(),
            asks: std::collections::VecDeque::new(),
            question_results: Vec::new(),
            toasts: ToastState::default(),
            scroll: ScrollState::new(),
            view: SessionView::default(),
            agent: AgentKind::Build,
            msg_agents: HashMap::new(),
            msg_meta: HashMap::new(),
            flags,
            paste_summary: true,
            sidebar_pref,
            sb: Default::default(),
            child: None,
            tip: seed / 3,
            files: Vec::new(),
            tx,
            msg_tx,
            pending: Vec::new(),
            quit: false,
            switched: false,
            handed_plan: agent_core::handed_plan(),
            dirty: true,
            backend_ready: false,
            backend_dead: false,
            busy_note: None,
            leader_at: None,
            esc_at: None,
            started: Instant::now(),
            frozen: None,
            last_anim: Instant::now(),
            silent_notices: 0,
            undo: None,
            shell_seq: 0,
            shell: None,
            files_cut_told: false,
            title_dirty: true,
            interrupt_at: None,
            last_backend: Instant::now(),
            slow_start_warned: false,
            files_at: Instant::now(),
            effort_at_start: None,
            wanted_model: None,
            save_failed: false,
            opts,
        };
        app.rebuild_commands();
        app
    }

    // ---- small queries -----------------------------------------------------------------

    pub fn in_session(&self) -> bool {
        self.route == Route::Session
    }

    pub fn leader_pending(&self) -> bool {
        self.leader_at.is_some()
    }

    pub fn esc_armed(&self) -> bool {
        self.esc_at.is_some()
    }

    /// The prompt has no focus while a dialog is open.
    pub fn prompt_blurred(&self) -> bool {
        self.dialogs.is_open()
    }

    pub fn anim_elapsed(&self) -> Duration {
        if self.prefs.disable_animations {
            return Duration::ZERO;
        }
        self.frozen.unwrap_or_else(|| self.started.elapsed())
    }

    pub fn sidebar_state(&self) -> SidebarState {
        if self.route != Route::Session || self.child.is_some() {
            return SidebarState {
                shown: false,
                overlay: false,
            };
        }
        let wide = self.size.0 > 120;
        match self.sidebar_pref {
            SidebarPref::Auto => SidebarState {
                shown: wide,
                overlay: false,
            },
            SidebarPref::Show => SidebarState {
                shown: true,
                overlay: !wide,
            },
            SidebarPref::Hide => SidebarState {
                shown: false,
                overlay: false,
            },
        }
    }

    /// `(model name, provider)` as the prompt shows them.
    pub fn model_display(&self) -> (String, String) {
        if self.config.model.is_empty() {
            return if self.backend_ready {
                ("No provider selected".into(), "Connect a provider".into())
            } else {
                ("connecting…".into(), String::new())
            };
        }
        match self
            .config
            .models
            .iter()
            .find(|m| m.id == self.config.model)
        {
            Some(m) => (m.name.clone(), m.provider.clone()),
            None => match self.config.model.split_once('/') {
                Some((p, n)) => (n.to_string(), p.to_string()),
                None => (self.config.model.clone(), String::new()),
            },
        }
    }

    /// The effort (opencode's variant) when it is not what wizard started with. `ctrl+t`
    /// changes it with no other sign, and `/status` is the only place that said so.
    pub fn variant_shown(&self) -> Option<&str> {
        let now = self.config.effort.as_str();
        let start = self.effort_at_start.as_deref()?;
        (!now.is_empty() && now != start).then_some(now)
    }

    /// Display name for a model id recorded on a message.
    pub fn display_name(&self, id: &str) -> String {
        match self.config.models.iter().find(|m| m.id == id) {
            Some(m) => m.name.clone(),
            None => id
                .rsplit_once('/')
                .map_or(id.to_string(), |(_, n)| n.to_string()),
        }
    }

    pub fn agent_for(&self, msg_idx: usize) -> AgentKind {
        (0..=msg_idx)
            .rev()
            .find(|i| {
                self.transcript
                    .messages
                    .get(*i)
                    .is_some_and(|m| m.role == Role::User)
            })
            .and_then(|i| self.msg_agents.get(&i).copied())
            .unwrap_or(self.agent)
    }

    pub fn session_title(&self) -> String {
        if let Some(t) = self.titles.get(&self.transcript.session_id) {
            return t.clone();
        }
        if let Some(s) = self
            .sessions
            .iter()
            .find(|s| s.id == self.transcript.session_id)
        {
            if !s.title.trim().is_empty() {
                return s.title.clone();
            }
        }
        let first = self
            .transcript
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| match m.parts.first() {
                Some(Part::Text(t)) => Some(t.lines().next().unwrap_or("").to_string()),
                _ => None,
            });
        first
            .map(|t| tuikit::width::truncate(&t, 50))
            .unwrap_or_else(|| "New session".into())
    }

    /// Copy of what dialogs need, taken when one opens. Under test (`frozen`) the clock and the
    /// time zone are pinned so snapshots do not move.
    pub fn dialog_ctx(&self) -> dialogs::Ctx {
        let (now, off) = if self.frozen.is_some() {
            (FROZEN_NOW, 0)
        } else {
            let now = dialogs::clock::unix_now();
            (now, dialogs::clock::local_offset(now))
        };
        let msgs = &self.transcript.messages;
        let mut users = Vec::new();
        for (i, m) in msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
        {
            let text = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let at = if self.frozen.is_some() {
                now - 60 * (msgs.len() - i) as i64
            } else {
                now - m.started.elapsed().as_secs() as i64
            };
            users.push(dialogs::UserMsg {
                index: i,
                turn: users.len() + 1,
                text,
                at,
            });
        }
        dialogs::Ctx {
            version: self.config.version().to_string(),
            cwd: self.cwd.clone(),
            config: self.config.clone(),
            prefs: self.prefs.clone(),
            sessions: self.sessions.clone(),
            session_id: self.transcript.session_id.clone(),
            session_title: self.session_title(),
            in_session: self.in_session(),
            now,
            utc_offset: off,
            stash: self
                .prompt
                .stash
                .iter()
                .enumerate()
                .map(|(i, t)| dialogs::StashItem {
                    text: t.clone(),
                    at: self.stash_at.get(i).copied().unwrap_or(now),
                })
                .collect(),
            theme: self.theme.name.clone(),
            agent: Some(self.agent),
            palette_key: self.keymap.first(&Action::Palette),
            wizard_dir: self.opts.wizard_dir.clone(),
            users,
            export: dialogs::export::Options {
                thinking: true,
                tool_details: self.flags.tool_details,
                assistant_metadata: true,
                open_without_saving: false,
            },
        }
    }

    fn save_prefs(&mut self) {
        let Some(dir) = self.opts.config_dir.clone() else {
            return;
        };
        if let Err(e) = self.prefs.save(&dir) {
            self.save_error(&dir, &e);
        }
    }

    /// Settings that cannot be written are told once; every later save would say the same.
    fn save_error(&mut self, dir: &std::path::Path, e: &std::io::Error) {
        if !self.save_failed {
            self.save_failed = true;
            self.toast(
                Variant::Warning,
                format!("Could not save settings in {}: {e}", dir.display()),
            );
        }
    }

    pub fn cmd_ctx(&self) -> CmdCtx<'_> {
        CmdCtx {
            in_session: self.in_session(),
            // opencode counts the session it is in, saved or not
            has_sessions: !self.sessions.is_empty() || self.in_session(),
            // opencode has no model, hence no variants, until something has been used
            has_variants: !self.config.efforts.is_empty()
                && (self.in_session() || !self.sessions.is_empty()),
            sidebar_visible: self.sidebar_state().shown,
            conceal: self.flags.conceal,
            thinking_shown: self.flags.thinking_shown,
            timestamps: self.flags.timestamps,
            tool_details: self.flags.tool_details,
            generic_output: self.flags.generic_output,
            scrollbar: self.flags.scrollbar,
            prompt_has_text: !self.prompt.is_empty(),
            stash_nonempty: !self.prompt.stash.is_empty(),
            tips_hidden: self.flags.tips_hidden,
            light: self.theme.mode == Mode::Light,
            backend: &self.backend_cmds,
            connected: !self.config.models.is_empty(),
            title_enabled: !self.prefs.disable_title,
            animations: !self.prefs.disable_animations,
        }
    }

    fn rebuild_commands(&mut self) {
        let cmds = commands::build(&self.cmd_ctx(), &self.keymap);
        self.cmds = cmds;
    }

    // ---- toasts and requests -----------------------------------------------------------

    pub fn toast(&mut self, v: Variant, msg: impl Into<String>) {
        self.toasts.show(Toast::new(v, msg), Instant::now());
        self.dirty = true;
    }

    pub fn send(&mut self, r: Request) {
        if self.tx.send(r).is_err() && !self.backend_dead {
            self.backend_dead = true;
            self.toast(Variant::Error, "The wizard process is not running");
        }
    }

    // ---- backend events ----------------------------------------------------------------

    pub fn update(&mut self, msg: Msg) {
        self.dirty = true;
        match msg {
            Msg::Backend(ev) => {
                self.last_backend = Instant::now();
                self.on_event(ev)
            }
            Msg::Branch(b) => self.branch = b,
            Msg::Files(f) => {
                if f.len() >= MAX_FILES && !self.files_cut_told {
                    self.files_cut_told = true;
                    self.toast(
                        Variant::Info,
                        format!("@ lists the first {MAX_FILES} files of this folder"),
                    );
                }
                self.files = f;
                prompt::refresh_ac(self);
            }
            Msg::ShellOut { id, text } => self.shell_output(&id, &text),
            Msg::Shell {
                id,
                note,
                ok,
                cancelled,
            } => self.finish_shell(&id, note, ok, cancelled),
            Msg::Clip(c) => prompt::finish_paste_clipboard(self, c),
        }
    }

    pub fn on_event(&mut self, ev: Event) {
        if self.undo_intercepts(&ev) {
            return;
        }
        self.dialogs.on_event(&ev);
        match &ev {
            Event::Ready { config, .. } => {
                self.config = config.clone();
                self.transcript.apply(&ev);
                self.backend_ready = true;
                self.backend_dead = false;
                self.send(Request::ListSessions);
                if self.effort_at_start.is_none() {
                    self.effort_at_start = Some(config.effort.clone());
                }
                if let Some(m) = self.opts.model.take() {
                    self.wanted_model = Some(m.clone());
                    self.send(Request::SetModel(m));
                } else {
                    self.restore_model();
                }
                if let Some(cmd) = self.handed_plan.take() {
                    self.agent = AgentKind::Plan;
                    self.silent_notices += 1;
                    self.send(Request::Prompt(cmd.into()));
                }
                if let Some(p) = self.initial_prompt.take() {
                    self.prompt.set_text(&p);
                    self.submit();
                }
            }
            Event::Commands(c) => {
                self.backend_cmds = c.clone();
                self.rebuild_commands();
            }
            Event::Permission(req) => {
                let p = dialogs::ask::PermissionPrompt::new(req.clone());
                self.asks
                    .push_back(dialogs::ask::Ask::Permission(Box::new(p)));
                self.enter_session();
                self.scroll.to_bottom();
            }
            Event::ConfigChanged(c) => {
                // `/model x` or `/provider use` typed here is a pick too
                if !c.model.is_empty() {
                    self.wanted_model = Some(c.model.clone());
                }
                self.config = c.clone();
                self.transcript.apply(&ev);
            }
            Event::Sessions(list) => {
                self.sessions = list.clone();
                if self.continue_pending {
                    self.continue_pending = false;
                    let best = list
                        .iter()
                        .filter(|s| s.cwd == self.cwd || s.cwd.is_empty())
                        .max_by_key(|s| s.updated)
                        .map(|s| s.id.clone());
                    match best {
                        Some(id) => self.send(Request::LoadSession(id)),
                        None => {
                            self.toast(Variant::Warning, "No earlier session in this directory")
                        }
                    }
                }
                self.rebuild_commands();
            }
            Event::History { items, .. } => {
                self.transcript.apply(&ev);
                self.interrupt_at = None;
                self.esc_at = None;
                self.msg_agents.clear();
                self.msg_meta.clear();
                self.view = SessionView::default();
                ui::tools::reset_caches();
                self.child = None;
                self.route = if items.is_empty() {
                    Route::Home
                } else {
                    Route::Session
                };
                self.scroll.to_bottom();
                self.title_dirty = true;
            }
            Event::Notice { text, .. }
                if agent_core::switched_look(text).is_some_and(|l| l != "opencode") =>
            {
                // `/ui <look>`: wizard saved it; leaving hands the terminal to that look
                self.quit = true;
                self.switched = true;
            }
            Event::Notice { level, text } => match level {
                NoticeLevel::Info => {
                    if text.to_lowercase().contains("nothing to compact") {
                        // no compaction happened, so no divider either
                        let newest = self
                            .msg_meta
                            .iter()
                            .filter(|(_, m)| m.compaction)
                            .map(|(i, _)| *i)
                            .max();
                        if let Some(m) = newest.and_then(|i| self.msg_meta.get_mut(&i)) {
                            m.compaction = false;
                        }
                    }
                    if let Some(plan) = agent_core::plan_mode(text) {
                        self.agent = if plan {
                            AgentKind::Plan
                        } else {
                            AgentKind::Build
                        };
                    }
                    if self.silent_notices > 0 {
                        self.silent_notices -= 1;
                        self.toast(Variant::Info, text.clone());
                    } else {
                        self.transcript.apply(&ev);
                        self.enter_session();
                    }
                }
                NoticeLevel::Warn => {
                    self.silent_notices = self.silent_notices.saturating_sub(1);
                    if self.transcript.busy {
                        // opencode shows retries as the spinner's message, not as a toast
                        self.busy_note = Some(text.clone());
                    } else {
                        self.toast(Variant::Warning, text.clone());
                    }
                }
                NoticeLevel::Error => {
                    self.transcript.apply(&ev);
                    self.enter_session();
                    self.toasts
                        .show(Toast::new(Variant::Error, text.clone()), Instant::now());
                }
            },
            Event::Fatal(text) => {
                self.transcript.apply(&ev);
                self.settle_tools();
                self.interrupt_at = None;
                self.esc_at = None;
                self.enter_session();
                self.backend_dead = true;
                self.toast(
                    Variant::Error,
                    tuikit::width::truncate(text.lines().next().unwrap_or("backend stopped"), 100),
                );
            }
            Event::TurnStart => {
                self.transcript.apply(&ev);
            }
            Event::TurnEnd(reason) => {
                self.transcript.apply(&ev);
                self.esc_at = None;
                self.interrupt_at = None;
                self.busy_note = None;
                if *reason != StopReason::EndTurn {
                    self.settle_tools();
                }
                // a turn may have switched branches or added files
                self.refresh_workspace();
            }
            _ => {
                if matches!(
                    ev,
                    Event::TextDelta(_) | Event::ThoughtDelta(_) | Event::Tool(_)
                ) {
                    self.busy_note = None;
                }
                self.transcript.apply(&ev);
            }
        }
    }

    /// The advertised id a typed model name stands for (`grok-4.6` for `xai-oauth/grok-4.6`);
    /// what was typed when nothing matches.
    fn model_id_for(&self, typed: &str) -> String {
        let hits: Vec<&agent_core::ModelOption> = self
            .config
            .models
            .iter()
            .filter(|m| m.id == typed || m.name == typed)
            .collect();
        match hits.as_slice() {
            [m] => m.id.clone(),
            _ => typed.to_string(),
        }
    }

    /// opencode starts on the model you last picked, wizard starts on its own default and goes
    /// back to it on every session switch. Pick again whenever they differ, as long as the
    /// model is one wizard offers.
    fn restore_model(&mut self) {
        // `/ui` from another look hands over the model that session was on
        let handed_off = std::env::var_os("WIZARD_HANDOFF").is_some();
        let want = match self.wanted_model.clone() {
            Some(m) => Some(m),
            None if handed_off => None,
            None => self.prefs.recent.first().cloned(),
        };
        let Some(want) = want else { return };
        let known = self.config.models.iter().any(|m| m.id == want);
        if known && self.config.model != want {
            self.wanted_model = Some(want.clone());
            self.send(Request::SetModel(want));
        }
    }

    /// `/undo`: take back the last message with wizard's `/rewind`. Redo does not exist there.
    fn start_undo(&mut self) {
        if self.undo.is_some() {
            return;
        }
        let Some(restore) = self
            .transcript
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(i, m)| {
                m.role == Role::User && !self.msg_meta.get(i).is_some_and(|x| x.compaction)
            })
            .and_then(|(_, m)| match m.parts.first() {
                Some(Part::Text(t)) if !t.trim().is_empty() => Some(t.clone()),
                _ => None,
            })
        else {
            return;
        };
        if self.backend_dead {
            self.toast(Variant::Error, "wizard is not running; restart openw");
            return;
        }
        let busy = self.transcript.busy;
        self.undo = Some(Undo {
            stage: UndoStage::Listing,
            buf: String::new(),
            restore,
            waiting_for_cancel: busy,
        });
        if busy {
            self.send(Request::Cancel);
        } else {
            self.send(Request::Prompt("/rewind".into()));
        }
    }

    /// Feed backend events to a running `/undo` instead of the transcript. True when consumed.
    fn undo_intercepts(&mut self, ev: &Event) -> bool {
        let Some(u) = self.undo.as_mut() else {
            return false;
        };
        match ev {
            Event::TurnStart if !u.waiting_for_cancel => true,
            // a slash command's output arrives as one info notice just before the turn ends
            Event::Notice {
                level: NoticeLevel::Info,
                text,
            } if !u.waiting_for_cancel => {
                u.buf.push_str(text);
                true
            }
            Event::TurnEnd(_) if u.waiting_for_cancel => {
                // the interrupted turn is over; the transcript keeps it until the reload
                u.waiting_for_cancel = false;
                self.transcript.apply(ev);
                self.send(Request::Prompt("/rewind".into()));
                true
            }
            Event::TurnEnd(_) => {
                // a backend that answers `/rewind` as plain text streams it into the transcript;
                // the turn must still close or the footer stays busy for good
                if self.transcript.busy {
                    self.transcript.apply(ev);
                }
                let buf = std::mem::take(&mut u.buf);
                match u.stage {
                    UndoStage::Listing => match parse_rewind_listing(&buf) {
                        Some((turn, prompt)) => {
                            let ours = u.restore.lines().next().unwrap_or("").trim();
                            // the listing cuts prompts at 120 chars; both sides must agree on
                            // where the newest turn starts
                            let n = prompt.chars().count().min(ours.chars().count()).min(60);
                            let same = prompt.chars().take(n).eq(ours.chars().take(n));
                            if same {
                                u.stage = UndoStage::Rewinding;
                                self.send(Request::Prompt(format!("/rewind {turn}")));
                            } else {
                                self.undo = None;
                                self.toast(
                                    Variant::Warning,
                                    "The last turn does not match; nothing undone",
                                );
                            }
                        }
                        None => {
                            self.undo = None;
                            self.toast(Variant::Info, "Nothing to undo");
                        }
                    },
                    UndoStage::Rewinding => {
                        let restore = std::mem::take(&mut u.restore);
                        self.undo = None;
                        let id = self.transcript.session_id.clone();
                        self.send(Request::LoadSession(id));
                        self.prompt.set_text(&restore);
                        self.toast(Variant::Info, buf.trim().to_string());
                    }
                }
                true
            }
            // errors and notices during the exchange stay visible
            _ => false,
        }
    }

    /// Re-read the git branch and the file list for `@` in the background. A no-op without a
    /// tokio runtime (tests).
    pub fn refresh_workspace(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.files_at = Instant::now();
        let cwd = self.opts.cwd.clone();
        let tx = self.msg_tx.clone();
        rt.spawn_blocking(move || {
            let _ = tx.send(Msg::Branch(git_branch(&cwd)));
            let _ = tx.send(Msg::Files(list_files(&cwd)));
        });
    }

    /// The `@` list is a snapshot; opening the popup re-reads it when it is a few seconds old,
    /// so a file made since is offered.
    pub fn refresh_files_if_stale(&mut self) {
        if self.files_at.elapsed() >= Duration::from_secs(3) {
            self.files_at = Instant::now();
            self.refresh_workspace();
        }
    }

    /// Rows the waiting permission or question wants, or `None` when there is none (or when a
    /// fullscreen permission covers the page, which leaves the bottom block empty).
    pub fn ask_height(&self) -> Option<u16> {
        let a = self.asks.front()?;
        Some(a.height(self.size, &self.theme))
    }

    pub fn draw_ask(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        area: ratatui::layout::Rect,
    ) -> Option<(u16, u16)> {
        let theme = self.theme.clone();
        let screen = ratatui::layout::Rect::new(0, 0, self.size.0, self.size.1);
        self.asks
            .front_mut()
            .and_then(|a| a.draw(buf, area, screen, &theme))
    }

    /// Show a question. `agent-core` has no event for them, so this is how a backend that has
    /// questions, or a test, puts one on screen; the answer lands in `question_results`.
    pub fn ask_question(&mut self, q: dialogs::ask::QuestionPrompt) {
        self.asks
            .push_back(dialogs::ask::Ask::Question(Box::new(q)));
        self.enter_session();
        self.scroll.to_bottom();
        self.dirty = true;
    }

    fn ask_key(&mut self, key: KeyEvent) {
        use dialogs::ask::AskOutcome;
        let Some(ask) = self.asks.front_mut() else {
            return;
        };
        match ask.handle_key(key) {
            AskOutcome::Stay => {}
            AskOutcome::Send(r) => {
                self.asks.pop_front();
                self.send(r);
                self.scroll.to_bottom();
            }
            o => {
                self.asks.pop_front();
                self.question_results.push(o);
                // only the newest answers are ever read; do not keep every one
                if self.question_results.len() > 16 {
                    self.question_results.remove(0);
                }
            }
        }
    }

    fn enter_session(&mut self) {
        if self.route == Route::Home {
            self.route = Route::Session;
        }
    }

    // ---- input -------------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        let now = Instant::now();
        if !self.dialogs.is_open() {
            if let Some(_t) = self.leader_at.take() {
                if key.code == KeyCode::Esc {
                    return;
                }
                let a = match self.keymap.resolve(true, &key) {
                    Resolve::Action(Action::TipsToggle)
                    | Resolve::Action(Action::ToggleConceal)
                        if matches!(key.code, KeyCode::Char('h')) =>
                    {
                        Some(self.keymap.leader_h(!self.in_session()))
                    }
                    Resolve::Action(a) => Some(a),
                    _ => None,
                };
                // opencode turns the exit bindings off while the prompt has text; ctrl+c and
                // ctrl+d already follow that, so `<leader>q` must too
                let a = a.filter(|a| *a != Action::Exit || self.prompt.is_empty());
                if let Some(a) = a {
                    self.run_action(a);
                }
                return;
            }
        }
        if self.view.sel.is_some() && key.code == KeyCode::Esc {
            self.view.sel = None;
            return;
        }
        if !self.asks.is_empty() && !self.dialogs.is_open() {
            // The palette stays reachable; everything else is for the prompt that is waiting.
            let palette = self.keymap.resolve(false, &key) == Resolve::Action(Action::Palette);
            if !palette {
                self.ask_key(key);
                return;
            }
        }
        if self.dialogs.is_open() {
            // ctrl+c closes a dialog like esc does
            let out = match self.dialogs.top_mut() {
                Some(d) => d.handle_key(key),
                None => return,
            };
            self.apply_outcome(out);
            return;
        }
        let ac_open = self.prompt.ac.is_some();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let ac_key = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter | KeyCode::Esc
        ) || (ctrl && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('n')));
        if ac_open && ac_key {
            let ev = prompt::handle_key(self, key);
            self.after_prompt(ev);
            return;
        }
        match self.keymap.resolve(false, &key) {
            Resolve::Pending => {
                self.leader_at = Some(now);
            }
            Resolve::Action(a) => {
                if self.claims(&a) {
                    self.run_key_action(a, now);
                } else {
                    let ev = prompt::handle_key(self, key);
                    self.after_prompt(ev);
                }
            }
            Resolve::None => {
                let ev = prompt::handle_key(self, key);
                self.after_prompt(ev);
            }
        }
    }

    /// Whether a bound key means the action right now, or belongs to the prompt instead.
    fn claims(&self, a: &Action) -> bool {
        let empty = self.prompt.is_empty();
        match a {
            Action::Exit => empty,
            Action::Interrupt => self.prompt.shell || self.transcript.busy,
            // opencode scrolls on home/end even with text in the box
            Action::First | Action::Last => self.in_session(),
            Action::PageUp | Action::PageDown => self.in_session(),
            Action::Rename => self.in_session(),
            Action::LineUp | Action::LineDown | Action::HalfPageUp | Action::HalfPageDown => {
                self.in_session()
            }
            Action::AgentCycle | Action::AgentCycleReverse => true,
            Action::ChildFirst => self.in_session() && self.child.is_none(),
            Action::ChildParent | Action::ChildPrev | Action::ChildNext => self.child.is_some(),
            _ => true,
        }
    }

    fn run_key_action(&mut self, a: Action, now: Instant) {
        if a == Action::Interrupt {
            if self.prompt.shell {
                self.prompt.shell = false;
            } else if self
                .esc_at
                .is_some_and(|t| now.duration_since(t) < ESC_WINDOW)
            {
                self.esc_at = None;
                self.interrupt(now);
            } else {
                self.esc_at = Some(now);
            }
            return;
        }
        self.run_action(a);
    }

    /// The second `esc`. A running `!cmd` is killed here; for the backend it sends `Cancel`,
    /// which wizard honours at its next boundary, so the footer says `interrupting…` until the
    /// turn really ends. Prompts queued behind the turn are dropped by the backend, so their
    /// bubbles go too and their text comes back instead of staying as messages that never ran.
    fn interrupt(&mut self, now: Instant) {
        if self.cancel_shell() {
            self.interrupt_at.get_or_insert(now);
            return;
        }
        let dropped = self.drop_queued();
        self.interrupt_at.get_or_insert(now);
        self.send(Request::Cancel);
        if dropped.is_empty() {
            return;
        }
        let n = dropped.len();
        if self.prompt.is_empty() {
            self.prompt.set_text(&dropped.join("\n\n"));
            prompt::refresh_ac(self);
            self.toast(
                Variant::Warning,
                format!(
                    "Interrupted; {n} queued message{} not sent, back in the prompt",
                    if n == 1 { "" } else { "s" }
                ),
            );
        } else {
            self.toast(
                Variant::Warning,
                format!(
                    "Interrupted; {n} queued message{} not sent (up arrow brings {} back)",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "it" } else { "them" }
                ),
            );
        }
    }

    /// Remove the user bubbles that sit behind the open turn, the ones marked `QUEUED`, and
    /// return their text. The backend drops them on `Cancel`, so they would never get an answer.
    fn drop_queued(&mut self) -> Vec<String> {
        let msgs = &self.transcript.messages;
        let Some(open) = msgs
            .iter()
            .rposition(|m| m.role == Role::Assistant && m.took.is_none())
        else {
            return Vec::new();
        };
        let queued: Vec<usize> = (open + 1..msgs.len())
            .filter(|i| msgs[*i].role == Role::User)
            .collect();
        let mut texts = Vec::new();
        for i in queued.iter().rev() {
            let m = self.transcript.messages.remove(*i);
            self.msg_meta.remove(i);
            self.msg_agents.remove(i);
            if let Some(Part::Text(t)) = m.parts.into_iter().next() {
                if !t.is_empty() {
                    texts.push(t);
                }
            }
        }
        texts.reverse();
        self.transcript.rev += 1;
        texts
    }

    /// Tool rows that never got their end: the turn was cut short or the backend died.
    fn settle_tools(&mut self) {
        for m in &mut self.transcript.messages {
            for p in &mut m.parts {
                if let Part::Tool(c) = p {
                    if matches!(c.status, ToolStatus::Pending | ToolStatus::Running) {
                        c.status = ToolStatus::Failed;
                    }
                }
            }
        }
        self.transcript.rev += 1;
    }

    /// A tool of the open turn is running; it can be quiet for a long time without being stuck.
    fn tool_running(&self) -> bool {
        self.transcript
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && m.took.is_none())
            .is_some_and(|m| {
                m.parts.iter().any(|p| {
                    matches!(p, Part::Tool(c) if matches!(c.status, ToolStatus::Pending | ToolStatus::Running))
                })
            })
    }

    /// How long the backend may stay silent mid-turn, with no tool running, before the footer
    /// says so.
    const STALL_AFTER: Duration = Duration::from_secs(60);

    /// What the footer shows in place of `esc interrupt`, and whether it is a warning.
    pub fn interrupt_status(&self) -> Option<(String, bool)> {
        if !self.transcript.busy {
            return None;
        }
        if let Some(t) = self.interrupt_at {
            // the second interrupt after three seconds restarts wizard
            let text = if t.elapsed() >= Duration::from_secs(3) && self.shell.is_none() {
                "interrupting… esc esc to force"
            } else {
                "interrupting…"
            };
            return Some((text.to_string(), false));
        }
        if !self.backend_dead
            && self.shell.is_none()
            && !self.tool_running()
            && self.last_backend.elapsed() >= Self::STALL_AFTER
        {
            return Some(("not responding, esc esc interrupts".to_string(), true));
        }
        None
    }

    fn after_prompt(&mut self, ev: PromptEvent) {
        match ev {
            PromptEvent::Submit => self.submit(),
            PromptEvent::Action(a) => self.run_action(a),
            PromptEvent::Ignored | PromptEvent::None => {}
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        self.dirty = true;
        if let (Some(a), false) = (self.asks.front_mut(), self.dialogs.is_open()) {
            a.handle_paste(text);
        } else if self.dialogs.is_open() {
            let out = match self.dialogs.top_mut() {
                Some(d) => d.handle_paste(text),
                None => return,
            };
            self.apply_outcome(out);
        } else {
            prompt::handle_paste(self, text);
        }
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if matches!(ev.kind, MouseEventKind::Moved) && !self.dialogs.is_open() {
            return;
        }
        self.dirty = true;
        if self.dialogs.is_open() {
            // A click on the dim layer closes the dialog, as in opencode; everything else goes
            // to the dialog, hover and release included.
            if matches!(ev.kind, MouseEventKind::Up(MouseButton::Left)) {
                let o = tuikit::dialog::last_outer();
                let inside = ev.column >= o.x
                    && ev.column < o.right()
                    && ev.row >= o.y
                    && ev.row < o.bottom();
                if !inside && !o.is_empty() {
                    let out = match self.dialogs.top_mut() {
                        Some(d) => d.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                        None => return,
                    };
                    self.apply_outcome(out);
                    return;
                }
            }
            if matches!(ev.kind, MouseEventKind::Drag(_)) {
                return;
            }
            let out = match self.dialogs.top_mut() {
                Some(d) => d.handle_mouse(ev),
                None => return,
            };
            self.apply_outcome(out);
            return;
        }
        // a drag that began in the transcript keeps its events, wherever the pointer goes
        let dragging = self.view.press.is_some() || self.view.sb_grab.is_some();
        if self.in_session() && !dragging && ui::sidebar::on_mouse(self, ev) {
            return;
        }
        let (col, row) = (ev.column, ev.row);
        let g = self.in_session().then(|| ui::session::geometry(self));
        let in_rect = |r: ratatui::layout::Rect| {
            row >= r.y && row < r.bottom() && col >= r.x && col < r.right()
        };
        match ev.kind {
            MouseEventKind::ScrollUp if self.in_session() => self.scroll.scroll_by(-WHEEL_ROWS),
            MouseEventKind::ScrollDown if self.in_session() => self.scroll.scroll_by(WHEEL_ROWS),
            MouseEventKind::Down(MouseButton::Left) => {
                let r = self.prompt.input_rect;
                if in_rect(r) {
                    self.prompt.editor.click(r, col, row);
                    self.view.sel = None;
                } else if let Some(g) = g {
                    let sb_col = ui::session::scrollbar_col(g.main_w);
                    if self.flags.scrollbar
                        && col == sb_col
                        && row >= g.transcript.y
                        && row < g.transcript.bottom()
                    {
                        self.view.scrollbar_press(&mut self.scroll, row);
                    } else if in_rect(g.transcript) {
                        self.view.press = Some((col, row));
                        self.view.dragged = false;
                        self.view.sel = None;
                    } else {
                        self.view.sel = None;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.view.sb_grab.is_some() {
                    self.view.scrollbar_drag(&mut self.scroll, row);
                } else if let (Some(p), Some(g)) = (self.view.press, g) {
                    // keep the head inside the transcript
                    let t = g.transcript;
                    let head = (
                        col.clamp(t.x, t.right().saturating_sub(1)),
                        row.clamp(t.y, t.bottom().saturating_sub(1)),
                    );
                    if head != p || self.view.dragged {
                        self.view.dragged = true;
                        self.view.sel = Some(ui::session::Selection { anchor: p, head });
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.view.sb_grab = None;
                let press = self.view.press.take();
                if self.view.dragged && self.view.sel.is_some() {
                    self.view.dragged = false;
                    self.view.copy_pending = true;
                } else if let Some(p) = press {
                    // a click: toggles a thought or tool block under it
                    if p == (col, row) && self.in_session() {
                        self.view.click(row);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn on_resize(&mut self, w: u16, h: u16) {
        self.size = (w, h);
        self.dirty = true;
    }

    /// Deadline bookkeeping; called when the loop wakes without input.
    pub fn on_tick(&mut self, now: Instant) {
        if self.toasts.tick(now) {
            self.dirty = true;
        }
        if self
            .leader_at
            .is_some_and(|t| now.duration_since(t) >= LEADER_TIMEOUT)
        {
            self.leader_at = None;
            self.dirty = true;
        }
        if self
            .esc_at
            .is_some_and(|t| now.duration_since(t) >= ESC_WINDOW)
        {
            self.esc_at = None;
            self.dirty = true;
        }
        if !self.backend_ready
            && !self.backend_dead
            && !self.slow_start_warned
            && now.duration_since(self.started) >= SLOW_START
        {
            self.slow_start_warned = true;
            self.toast(Variant::Warning, "wizard is slow to answer; ctrl+c quits");
        }
        if self.animating()
            && now.duration_since(self.last_anim) >= tuikit::spinner::SCANNER_INTERVAL
        {
            self.last_anim = now;
            self.dirty = true;
        }
    }

    /// Something on screen changes with the clock (the busy scanner, tool and thought spinners).
    pub fn animating(&self) -> bool {
        self.transcript.busy
    }

    /// Next instant the loop must wake at with no input, or `None` to sleep until something
    /// happens.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        let mut w: Option<Instant> = None;
        let mut take = |t: Instant| w = Some(w.map_or(t, |c| c.min(t)));
        if let Some(t) = self.toasts.deadline() {
            take(t);
        }
        if let Some(t) = self.leader_at {
            take(t + LEADER_TIMEOUT);
        }
        if let Some(t) = self.esc_at {
            take(t + ESC_WINDOW);
        }
        if !self.backend_ready && !self.backend_dead && !self.slow_start_warned {
            take(self.started + SLOW_START);
        }
        if self.animating() {
            take((self.last_anim + tuikit::spinner::SCANNER_INTERVAL).max(now));
        }
        w
    }

    // ---- submit ------------------------------------------------------------------------

    pub fn submit(&mut self) {
        let raw = self.prompt.expanded();
        let text = raw.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.prompt.shell && matches!(text.as_str(), "exit" | "quit" | ":q") {
            self.quit = true;
            return;
        }
        if self.prompt.shell {
            self.prompt.remember();
            self.prompt.clear();
            self.run_shell(&text);
            return;
        }
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest
                .split_once(char::is_whitespace)
                .map_or((rest, ""), |(n, a)| (n, a.trim()));
            if let Some(cmd) = commands::find(&self.cmds, name).cloned() {
                if cmd.source == Source::Frontend {
                    self.prompt.remember();
                    self.prompt.clear();
                    self.run_command(&cmd, args);
                    return;
                }
            }
        }
        if self.backend_dead {
            // the draft stays in the box instead of vanishing with nothing shown
            self.toast(Variant::Error, "wizard is not running; restart openw");
            return;
        }
        let out = self.prompt.outgoing();
        self.prompt.remember();
        self.prompt.clear();
        let meta = MsgMeta {
            files: out.files,
            ..Default::default()
        };
        self.send_prompt_with(out.shown.trim().to_string(), text, meta);
    }

    /// Put a user block in the transcript and hand the text to the backend.
    pub fn send_prompt(&mut self, text: String) {
        self.send_prompt_with(text.clone(), text, MsgMeta::default());
    }

    /// Like [`send_prompt`](Self::send_prompt) when the block shows something other than what
    /// is sent: `[Image 1]` on screen, `@/path/to.png` on the wire.
    pub fn send_prompt_with(&mut self, shown: String, sent: String, mut meta: MsgMeta) {
        if self.backend_dead {
            self.toast(Variant::Error, "wizard is not running; restart openw");
            return;
        }
        self.transcript.push_user(&shown);
        let idx = self.transcript.messages.len() - 1;
        self.msg_agents.insert(idx, self.agent);
        meta.at = Some(std::time::SystemTime::now());
        self.msg_meta.insert(idx, meta);
        self.route = Route::Session;
        self.scroll.to_bottom();
        self.send(Request::Prompt(sent));
        self.title_dirty = true;
    }

    /// `/compact`: a divider in the transcript, then wizard's reply under a `Compaction` label.
    fn compact(&mut self, args: &str) {
        let sent = if args.is_empty() {
            "/compact".to_string()
        } else {
            format!("/compact {args}")
        };
        self.send_prompt_with(
            String::new(),
            sent,
            MsgMeta {
                compaction: true,
                ..Default::default()
            },
        );
    }

    fn run_command(&mut self, cmd: &Command, args: &str) {
        match (&cmd.action, args.is_empty()) {
            (Action::Models, false) => {
                let id = self.model_id_for(args);
                if self.config.models.iter().any(|m| m.id == id) {
                    self.wanted_model = Some(id.clone());
                    self.prefs.push_recent(&id);
                    self.save_prefs();
                }
                self.send(Request::SetModel(args.to_string()))
            }
            (Action::Themes, false) => match Theme::builtin_mode(args, self.theme.mode) {
                Some(_) => self.run_effect(Effect::SetTheme(args.to_string())),
                None => self.toast(Variant::Error, format!("No theme named {args}")),
            },
            (Action::Compact, _) => self.compact(args),
            (Action::Template(name), _) => self.run_template(name, args),
            (a, _) => self.run_action(a.clone()),
        }
    }

    /// `/init` and `/review`: opencode runs them as prompt templates, and the user block shows
    /// the command as typed.
    fn run_template(&mut self, name: &str, args: &str) {
        let shown = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        self.send_prompt_with(shown, commands::template(name, args), MsgMeta::default());
    }

    fn run_shell(&mut self, cmd: &str) {
        if self.transcript.busy {
            self.toast(Variant::Warning, "Wait for the current turn to finish");
            return;
        }
        self.shell_seq += 1;
        let id = format!("sh-{}", self.shell_seq);
        let call = ToolCall {
            id: id.clone(),
            name: "bash".into(),
            kind: ToolKind::Execute,
            title: cmd.to_string(),
            input: serde_json::json!({ "command": cmd }),
            status: ToolStatus::Running,
            ..Default::default()
        };
        self.transcript.apply(&Event::Tool(call));
        self.enter_session();
        self.scroll.to_bottom();
        let pgid = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        self.shell = Some(ShellJob {
            id: id.clone(),
            pgid: pgid.clone(),
            stop: Some(stop_tx),
            kept: 0,
            truncated: false,
        });
        tokio::spawn(run_shell_job(
            id,
            cmd.to_string(),
            self.cwd.clone(),
            pgid,
            stop_rx,
            self.msg_tx.clone(),
        ));
    }

    /// The tool row of the running `!cmd`, newest message first.
    fn shell_call_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.transcript
            .messages
            .iter_mut()
            .rev()
            .flat_map(|m| m.parts.iter_mut())
            .find_map(|p| match p {
                Part::Tool(c) if c.id == id => Some(c),
                _ => None,
            })
    }

    fn shell_output(&mut self, id: &str, text: &str) {
        let Some(job) = self.shell.as_mut().filter(|j| j.id == id) else {
            return;
        };
        let room = SHELL_OUTPUT_CAP.saturating_sub(job.kept);
        let mut take = text.len().min(room);
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        job.kept += take;
        job.truncated |= take < text.len();
        let chunk = text[..take].to_string();
        if let Some(c) = self.shell_call_mut(id) {
            c.output.get_or_insert_with(String::new).push_str(&chunk);
            self.transcript.rev += 1;
        }
    }

    /// Stop the running `!cmd`: the group gets SIGTERM now, the task escalates to SIGKILL.
    fn cancel_shell(&mut self) -> bool {
        let Some(job) = self.shell.as_mut() else {
            return false;
        };
        job.signal(libc::SIGTERM);
        if let Some(stop) = job.stop.take() {
            let _ = stop.send(());
        }
        true
    }

    /// Kill what a running `!cmd` started. Called when openw is leaving.
    pub fn kill_shell(&mut self) {
        self.shell = None;
    }

    fn finish_shell(&mut self, id: &str, note: String, ok: bool, cancelled: bool) {
        let truncated = self
            .shell
            .as_ref()
            .is_some_and(|j| j.id == id && j.truncated);
        if self.shell.as_ref().is_some_and(|j| j.id == id) {
            self.shell = None;
        }
        self.interrupt_at = None;
        self.esc_at = None;
        let Some(c) = self.shell_call_mut(id) else {
            return;
        };
        c.status = if ok {
            ToolStatus::Completed
        } else {
            ToolStatus::Failed
        };
        let out = c.output.get_or_insert_with(String::new);
        for line in [
            truncated.then_some("[output truncated]"),
            Some(note.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter(|l| !l.is_empty())
        {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(line);
        }
        self.transcript.rev += 1;
        let reason = if cancelled {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        };
        self.transcript.apply(&Event::TurnEnd(reason));
        // shell runs show no duration on the completion line
        if let Some(m) = self.transcript.messages.last_mut() {
            m.took = Some(Duration::ZERO);
        }
    }

    // ---- actions -----------------------------------------------------------------------

    pub fn open_palette(&mut self) {
        let p = dialogs::Palette::new(&self.cmd_ctx(), &self.keymap);
        self.dialogs.push(Box::new(p));
    }

    pub fn run_action(&mut self, a: Action) {
        self.dirty = true;
        match a {
            Action::Leader | Action::Interrupt => {}
            Action::Exit => self.quit = true,
            Action::Palette => self.open_palette(),
            Action::Suspend => self.pending.push(Deferred::Suspend),
            Action::ChildFirst => ui::header::enter_first(self),
            Action::ChildParent => ui::header::leave(self),
            Action::ChildPrev => ui::header::step(self, -1),
            Action::ChildNext => ui::header::step(self, 1),
            Action::TipsToggle => {
                if !self.in_session() {
                    self.flags.tips_hidden = !self.flags.tips_hidden;
                    self.save_kv();
                    self.rebuild_commands();
                }
            }
            Action::Editor => self.pending.push(Deferred::Editor),
            Action::Themes => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::themes::open(&ctx));
            }
            Action::SidebarToggle => {
                if self.in_session() {
                    let shown = self.sidebar_state().shown;
                    self.sidebar_pref = if shown {
                        SidebarPref::Hide
                    } else if self.size.0 > 120 {
                        SidebarPref::Auto
                    } else {
                        SidebarPref::Show
                    };
                    self.save_kv();
                }
            }
            Action::Status => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::status::open(&ctx));
            }
            Action::Debug => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::debug::open(&ctx));
            }
            Action::Export => {
                if self.in_session() {
                    let ctx = self.dialog_ctx();
                    self.dialogs.push(dialogs::export::open(&ctx));
                } else {
                    self.toast(Variant::Info, "Nothing to export yet");
                }
            }
            Action::NewSession => self.new_session(),
            Action::Sessions => {
                self.send(Request::ListSessions);
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::sessions::open(&ctx));
            }
            Action::Timeline => {
                if self.in_session() {
                    let ctx = self.dialog_ctx();
                    self.dialogs.push(dialogs::timeline::open(&ctx));
                }
            }
            Action::Rename => {
                let t = self.session_title();
                self.dialogs.push(dialogs::rename::open(&t));
            }
            Action::Compact => {
                // session only, like the palette entry; the chord is not gated by the table
                if self.in_session() {
                    self.compact("")
                }
            }
            // opencode: page keys move half a screen, ctrl+alt+u/d a quarter
            Action::PageUp => self.scroll.half_page_up(),
            Action::PageDown => self.scroll.half_page_down(),
            Action::HalfPageUp => {
                let q = (self.scroll.viewport() / 4).max(1) as isize;
                self.scroll.scroll_by(-q)
            }
            Action::HalfPageDown => {
                let q = (self.scroll.viewport() / 4).max(1) as isize;
                self.scroll.scroll_by(q)
            }
            Action::LineUp => self.scroll.scroll_by(-1),
            Action::LineDown => self.scroll.scroll_by(1),
            Action::First => self.scroll.to_top(),
            Action::Last => self.scroll.to_bottom(),
            Action::CopyLast => {
                let text = self
                    .transcript
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Assistant)
                    .map(|m| {
                        m.parts
                            .iter()
                            .filter_map(|p| {
                                if let Part::Text(t) = p {
                                    Some(t.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    });
                match text.filter(|t| !t.trim().is_empty()) {
                    Some(t) => {
                        self.pending.push(Deferred::Copy(t));
                        self.toast(Variant::Success, "Message copied to clipboard!");
                    }
                    None => self.toast(Variant::Info, "No assistant message to copy"),
                }
            }
            Action::CopyTranscript => {
                let md = self.transcript_markdown();
                self.pending.push(Deferred::Copy(md));
                self.toast(Variant::Success, "Session transcript copied to clipboard!");
            }
            Action::Undo => self.start_undo(),
            Action::Redo => self.toast(
                Variant::Info,
                "wizard cannot bring back rewound turns, so there is nothing to redo",
            ),
            Action::ToggleConceal => self.flags.conceal = !self.flags.conceal,
            Action::ToggleThinking => self.flags.thinking_shown = !self.flags.thinking_shown,
            Action::ToggleTimestamps => self.flags.timestamps = !self.flags.timestamps,
            Action::ToggleToolDetails => self.flags.tool_details = !self.flags.tool_details,
            Action::ToggleScrollbar => self.flags.scrollbar = !self.flags.scrollbar,
            Action::ToggleGenericOutput => self.flags.generic_output = !self.flags.generic_output,
            Action::Models => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::models::open(&ctx));
            }
            Action::ModelCycle | Action::ModelCycleReverse => {
                // Recently used models first, as opencode's cycle; all models when the
                // current one is not among them.
                let all: Vec<String> = self.config.models.iter().map(|m| m.id.clone()).collect();
                let recent: Vec<String> = self
                    .prefs
                    .recent
                    .iter()
                    .filter(|r| all.contains(r))
                    .cloned()
                    .collect();
                let pool = if recent.len() > 1 && recent.contains(&self.config.model) {
                    recent
                } else {
                    all
                };
                if pool.len() > 1 {
                    let cur = pool
                        .iter()
                        .position(|m| *m == self.config.model)
                        .unwrap_or(0);
                    let n = if a == Action::ModelCycle {
                        (cur + 1) % pool.len()
                    } else {
                        (cur + pool.len() - 1) % pool.len()
                    };
                    let id = pool[n].clone();
                    self.wanted_model = Some(id.clone());
                    self.prefs.push_recent(&id);
                    self.save_prefs();
                    self.send(Request::SetModel(id));
                } else {
                    self.toasts.show(
                        Toast::new(Variant::Info, "Add a favorite model to use this shortcut")
                            .lasting(Duration::from_secs(3)),
                        Instant::now(),
                    );
                }
            }
            Action::Agents => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::agents::open(&ctx));
            }
            Action::AgentCycle | Action::AgentCycleReverse => self.set_agent(self.agent.toggled()),
            Action::VariantCycle => {
                let e = &self.config.efforts;
                if e.is_empty() {
                    self.toast(Variant::Info, "This model has no variants");
                } else {
                    let cur = e.iter().position(|x| *x == self.config.effort).unwrap_or(0);
                    let next = e[(cur + 1) % e.len()].clone();
                    self.send(Request::SetEffort(next));
                }
            }
            Action::Variants => {
                if self.config.efforts.is_empty() {
                    self.toast(Variant::Info, "This model has no variants");
                } else {
                    let d = dialogs::models::variant(&self.config.efforts, &self.config.effort);
                    self.dialogs.push(d);
                }
            }
            Action::Help => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::help::open(&ctx.palette_key));
            }
            Action::Skills => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::skills::open(&ctx));
            }
            Action::Mcps => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::mcp::open(&ctx));
            }
            Action::Connect => self.dialogs.push(dialogs::connect::open()),
            Action::Diff => {
                // there is no viewer; say what happens instead of leaving a bare `/diff` bubble
                self.toast(
                    Variant::Info,
                    "openw has no diff viewer; this runs wizard's /diff",
                );
                self.send_prompt("/diff".into())
            }
            Action::StashPush => {
                if !self.prompt.is_empty() {
                    let t = self.prompt.expanded();
                    self.prompt.stash.push(t);
                    self.prompt.save_stash();
                    self.stash_at.push(dialogs::clock::unix_now());
                    self.prompt.clear();
                    self.toast(Variant::Info, "Prompt stashed");
                }
            }
            Action::StashPop => {
                if let Some(t) = self.prompt.stash.pop() {
                    self.prompt.save_stash();
                    self.stash_at.pop();
                    self.prompt.set_text(&t);
                }
            }
            Action::StashList => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::stash::open(&ctx));
            }
            Action::ToggleMode => {
                self.theme = self.theme.toggled();
                self.save_kv();
            }
            Action::Fork => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::timeline::fork(&ctx));
            }
            Action::Share => self.toast(Variant::Info, "wizard has no session sharing"),
            Action::Plugins => {
                let ctx = self.dialog_ctx();
                self.dialogs.push(dialogs::plugins::open(&ctx));
            }
            Action::InstallPlugin => self.dialogs.push(dialogs::plugins::install()),
            Action::LockThemeMode => {
                self.toast(Variant::Info, "openw has no automatic theme mode to lock")
            }
            Action::OpenDocs => self.toast(
                Variant::Info,
                "Docs: github.com/teddytennant/wizard/tree/main/docs",
            ),
            Action::DebugPanel => self.toast(Variant::Info, "openw has no debug panel"),
            Action::Console => self.toast(Variant::Info, "openw has no console"),
            Action::HeapSnapshot => self.toast(Variant::Info, "openw has no heap snapshot"),
            Action::ToggleTerminalTitle => {
                self.prefs.disable_title = !self.prefs.disable_title;
                self.title_dirty = true;
                self.save_prefs();
            }
            Action::ToggleAnimations => {
                self.prefs.disable_animations = !self.prefs.disable_animations;
                self.save_prefs();
            }
            Action::Unsupported(what) => {
                self.toast(Variant::Info, format!("openw has no {what} setting"))
            }
            Action::MoveSession => self.toast(
                Variant::Info,
                "wizard sessions stay in the directory they started in",
            ),
            Action::SwitchPinned(n) => {
                let idx = n.saturating_sub(1) as usize;
                match self.prefs.pinned.get(idx).cloned() {
                    Some(id) if id != self.transcript.session_id => {
                        self.send(Request::LoadSession(id))
                    }
                    Some(_) => {}
                    None => self.toast(Variant::Info, "No pinned session in that slot"),
                }
            }
            Action::Backend(name) => self.send_prompt(format!("/{name}")),
            Action::InsertSlash(name) => {
                self.prompt.set_text(&format!("/{name} "));
                prompt::refresh_ac(self);
            }
            Action::Template(name) => self.run_template(name, ""),
        }
        self.rebuild_commands();
    }

    pub fn set_agent(&mut self, a: AgentKind) {
        if a == self.agent {
            return;
        }
        self.agent = a;
        // wizard's plan mode is a toggle; its confirmation is shown as a toast, not a message
        self.silent_notices += 1;
        self.send(Request::Prompt("/plan".into()));
    }

    pub fn new_session(&mut self) {
        if self.transcript.busy {
            self.toast(Variant::Warning, "Interrupt the current turn first");
            return;
        }
        if self.backend_dead {
            self.toast(Variant::Error, "wizard is not running; restart openw");
            return;
        }
        self.send(Request::NewSession);
        let model = std::mem::take(&mut self.transcript.model);
        self.transcript = Transcript::new();
        self.transcript.model = model;
        self.msg_agents.clear();
        self.msg_meta.clear();
        self.view = SessionView::default();
        ui::tools::reset_caches();
        self.child = None;
        self.route = Route::Home;
        self.prompt.clear();
        self.prompt.example = (self.prompt.example + 1) % prompt::EXAMPLES.len();
        self.tip = self.tip.wrapping_add(1);
        self.title_dirty = true;
    }

    pub fn apply_outcome(&mut self, out: Outcome) {
        let Outcome { nav, effects } = out;
        self.dialogs.apply(nav);
        for e in effects {
            self.run_effect(e);
        }
        self.dirty = true;
    }

    fn run_effect(&mut self, e: Effect) {
        match e {
            Effect::Run(a) => self.run_action(a),
            Effect::Request(r) => self.send(r),
            Effect::Toast(v, m) => self.toast(v, m),
            Effect::PreviewTheme(n) => self.apply_theme(&n),
            Effect::SetTheme(n) => {
                self.apply_theme(&n);
                self.prefs.theme = Some(n);
                self.save_prefs();
                self.save_kv();
            }
            Effect::SetAgent(a) => self.set_agent(a),
            Effect::Rename(t) => {
                let id = self.transcript.session_id.clone();
                self.rename_session(id, t);
            }
            Effect::RenameSession { id, title } => self.rename_session(id, title),
            Effect::InsertPrompt(t) => self.prompt.set_text(&t),
            Effect::RestorePrompt(t) => self.prompt.set_text(&t),
            Effect::Export { path, options } => {
                let name = |m: &str| self.display_name(m);
                let md = dialogs::export::markdown(
                    &self.transcript,
                    &self.session_title(),
                    &options,
                    &name,
                );
                if options.open_without_saving {
                    // Nothing is written; the text goes to the clipboard to paste where wanted.
                    self.pending.push(Deferred::Copy(md));
                    self.toast(Variant::Success, "Session transcript copied to clipboard!");
                } else {
                    // relative to --cwd, and never over a file that is already there
                    match dialogs::export::write_new(&self.opts.cwd, &path, &md) {
                        Ok(written) => self.toast(
                            Variant::Success,
                            format!("Session exported to {}", written.display()),
                        ),
                        Err(e) => self.toast(Variant::Error, format!("Export failed: {e}")),
                    }
                }
            }
            Effect::RecordModel(id) => {
                self.wanted_model = Some(id.clone());
                self.prefs.push_recent(&id);
                self.save_prefs();
            }
            Effect::ToggleFavorite(id) => {
                self.prefs.toggle_favorite(&id);
                self.save_prefs();
            }
            Effect::TogglePin(id) => {
                self.prefs.toggle_pin(&id);
                self.save_prefs();
            }
            Effect::DeleteSession(id) => {
                self.sessions.retain(|s| s.id != id);
                self.prefs.pinned.retain(|p| *p != id);
                self.prefs.titles.remove(&id);
                self.titles.remove(&id);
                self.save_prefs();
                if let Some(dir) = self.opts.wizard_dir.as_ref().map(|d| d.join("sessions")) {
                    if let Err(e) = dialogs::sessions::delete_session_file(&dir, &id) {
                        self.toast(Variant::Error, format!("Failed to delete session: {e}"));
                    }
                }
            }
            Effect::StashRemove(i) => {
                if i < self.prompt.stash.len() {
                    self.prompt.stash.remove(i);
                    if i < self.stash_at.len() {
                        self.stash_at.remove(i);
                    }
                    self.prompt.save_stash();
                }
            }
            Effect::StashPop(i) => {
                if i < self.prompt.stash.len() {
                    let t = self.prompt.stash.remove(i);
                    if i < self.stash_at.len() {
                        self.stash_at.remove(i);
                    }
                    self.prompt.save_stash();
                    self.prompt.set_text(&t);
                }
            }
            Effect::Copy(t) => self.pending.push(Deferred::Copy(t)),
            Effect::JumpToMessage(i) => {
                if let Some(row) = self.view.row_of_msg.get(&i).copied() {
                    // Leave one row of the previous block above the message.
                    let want = row.saturating_sub(1) as isize;
                    self.scroll.scroll_by(want - self.scroll.offset() as isize);
                }
            }
        }
    }

    fn rename_session(&mut self, id: String, title: String) {
        self.titles.insert(id.clone(), title.clone());
        self.prefs.titles.insert(id, title);
        self.save_prefs();
        self.title_dirty = true;
    }

    fn apply_theme(&mut self, name: &str) {
        if let Some(t) = Theme::builtin_mode(name, self.theme.mode) {
            self.theme = t;
            self.dirty = true;
        }
    }

    // ---- persistence -------------------------------------------------------------------

    fn save_kv(&mut self) {
        let Some(dir) = self.opts.state_dir.clone() else {
            return;
        };
        let v = serde_json::json!({
            "theme": self.theme.name,
            "theme_mode": if self.theme.mode == Mode::Light { "light" } else { "dark" },
            "tips_hidden": self.flags.tips_hidden,
            "sidebar": if self.sidebar_pref == SidebarPref::Hide { "hide" } else { "auto" },
        });
        if let Err(e) = crate::private::write_atomic(&dir.join("kv.json"), v.to_string().as_bytes())
        {
            self.save_error(&dir, &e);
        }
    }

    // ---- exports -----------------------------------------------------------------------

    pub fn transcript_markdown(&self) -> String {
        let mut out = format!("# {}\n\n", self.session_title());
        for m in &self.transcript.messages {
            match m.role {
                Role::User => out.push_str("## User\n\n"),
                Role::Assistant => out.push_str(&format!(
                    "## Assistant ({})\n\n",
                    self.display_name(&m.model)
                )),
                Role::Notice(_) => out.push_str("## Notice\n\n"),
            }
            for p in &m.parts {
                match p {
                    Part::Text(t) => {
                        out.push_str(t.trim());
                        out.push_str("\n\n");
                    }
                    Part::Thought { .. } => {}
                    Part::Tool(c) => out.push_str(&format!("`{} {}`\n\n", c.name, c.title)),
                }
            }
        }
        out
    }

    /// Text printed on the normal screen after the alt screen closes.
    pub fn epilogue(&self) -> Option<String> {
        if self.transcript.messages.is_empty() || self.switched {
            return None;
        }
        let mut s = String::from("\n");
        for r in ui::logo::epilogue_rows() {
            s.push_str("  ");
            s.push_str(&r);
            s.push('\n');
        }
        s.push('\n');
        // Written to the shell after the alternate screen closes: the title is the user's first
        // prompt or a backend string, and the id comes from a backend, so neither may carry
        // a sequence the terminal would act on.
        s.push_str(&format!(
            "  \x1b[90mSession   \x1b[0m\x1b[1m{}\x1b[0m\n",
            one_line(&self.session_title())
        ));
        if !self.transcript.session_id.is_empty() {
            s.push_str(&format!(
                "  \x1b[90mContinue  \x1b[0m\x1b[1mwizard-ui-opencode -s {}\x1b[0m\n",
                shell_word(&self.transcript.session_id)
            ));
        }
        s.push('\n');
        Some(s)
    }

    /// Window title: `openw` on home, `OC | title` in a session, like opencode's.
    pub fn window_title(&self) -> String {
        if !self.in_session() {
            return "openw".into();
        }
        let t = one_line(&self.session_title());
        let t = if t.chars().count() > 40 {
            format!("{}…", t.chars().take(37).collect::<String>())
        } else {
            t
        };
        format!("OC | {t}")
    }
}

// ---- helpers -----------------------------------------------------------------------------

fn load_kv(dir: Option<&std::path::Path>) -> serde_json::Value {
    dir.and_then(|d| std::fs::read_to_string(d.join("kv.json")).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
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
        MockBackend::spawn_with(
            cwd,
            resume,
            agent_core::mock::MockOpts {
                todo_tool: true,
                no_sessions: std::env::var_os("OPENW_MOCK_SESSIONS").is_some_and(|v| v == "none"),
            },
        )?
    } else {
        WizardBackend::spawn(cwd, resume)?
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

/// `s` as one line of plain text, safe to write to the real terminal outside ratatui.
fn one_line(s: &str) -> String {
    tuikit::width::plain_text(s).replace('\n', " ")
}

/// `s` as one shell word: bare when it is plain, else single-quoted, so a pasted
/// `openw -s <id>` cannot run anything the id spells.
fn shell_word(s: &str) -> String {
    let id = one_line(s);
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        id
    } else {
        format!("'{}'", id.replace('\'', "'\\''"))
    }
}

/// Current git branch, read from `.git/HEAD` walking up from `cwd`.
pub fn git_branch(cwd: &std::path::Path) -> Option<String> {
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        let head = if git.is_dir() {
            git.join("HEAD")
        } else if git.is_file() {
            // worktree: `gitdir: <path>`
            let s = std::fs::read_to_string(&git).ok()?;
            // a submodule's pointer is relative to the directory that holds the `.git` file
            dir.join(s.trim().strip_prefix("gitdir:")?.trim())
                .join("HEAD")
        } else {
            continue;
        };
        let s = std::fs::read_to_string(head).ok()?;
        let s = s.trim();
        return Some(match s.strip_prefix("ref: refs/heads/") {
            Some(b) => b.to_string(),
            None => s.chars().take(7).collect(),
        });
    }
    None
}

/// Most files the `@` popup indexes.
const MAX_FILES: usize = 20_000;

/// Files for the `@` popup: git's view of the tree, or a shallow walk outside a repo.
pub fn list_files(cwd: &std::path::Path) -> Vec<String> {
    tuikit::git::project_files(cwd, MAX_FILES)
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
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
    out
}

/// Copy through the terminal (OSC 52, wrapped for tmux and screen) and, when there is one, the
/// system clipboard tool, the way opencode does both.
fn osc52_copy(text: &str) {
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let passthrough = format!("\x1bPtmux;\x1b{seq}\x1b\\");
    let mut out = std::io::stdout();
    let payload = if std::env::var_os("TMUX").is_some() {
        format!("{seq}{passthrough}")
    } else if std::env::var_os("STY").is_some() {
        passthrough
    } else {
        seq
    };
    let _ = out.write_all(payload.as_bytes());
    let _ = out.flush();
    native_copy(text);
}

fn native_copy(text: &str) {
    // The helper may be slow to read or never read (a stopped compositor), so the write and the
    // wait both happen off the UI thread.
    let text = text.to_string();
    std::thread::spawn(move || {
        use std::process::{Command, Stdio};
        let candidates: [(&str, &[&str]); 4] = [
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
            ("pbcopy", &[]),
        ];
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        for (cmd, args) in candidates {
            if cmd == "wl-copy" && !wayland {
                continue;
            }
            let Ok(mut child) = Command::new(cmd)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            // wl-copy and xclip fork the server; reaping the front half keeps no zombie
            let _ = child.wait();
            return;
        }
    });
}

// ---- the loop ----------------------------------------------------------------------------

fn draw_frame(terminal: &mut Terminal, app: &mut App) -> std::io::Result<()> {
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    let mut out = std::io::stdout();
    crossterm::queue!(out, BeginSynchronizedUpdate)?;
    terminal.draw(|f| {
        let cur = ui::draw(f.buffer_mut(), app);
        if let Some(p) = cur {
            f.set_cursor_position(p);
        }
    })?;
    crossterm::execute!(out, EndSynchronizedUpdate)?;
    if app.title_dirty {
        app.title_dirty = false;
        if !app.prefs.disable_title {
            crossterm::execute!(out, crossterm::terminal::SetTitle(app.window_title()))?;
        }
    }
    Ok(())
}

/// opencode's `normalizePromptContent`: one trailing newline goes only when the text is a
/// single line, so a file the editor ended with a newline does not leave an empty second row.
fn normalize_editor_text(content: &str) -> &str {
    for nl in ["\r\n", "\n"] {
        if let Some(body) = content.strip_suffix(nl) {
            return if body.contains(['\n', '\r']) {
                content
            } else {
                body
            };
        }
    }
    content
}

fn reapply_cursor(app: &App, guard: &mut TermGuard) {
    if let Some(rgb) = tuikit::theme::rgb_of(app.theme.text) {
        let _ = guard.set_cursor_color(rgb);
    }
}

fn run_editor(app: &mut App, guard: &mut TermGuard, input: &tuikit::input::InputControl) {
    use tuikit::input::{external_editor, EditorOutcome};
    let out = external_editor(
        guard,
        input,
        &app.opts.cwd,
        "openw",
        &app.prompt.expanded(),
        None,
    );
    reapply_cursor(app, guard);
    match out {
        EditorOutcome::NoEditor => {
            app.toast(Variant::Info, "Set $VISUAL or $EDITOR to use an editor")
        }
        EditorOutcome::Failed(m) => app.toast(Variant::Error, m),
        EditorOutcome::Edited(t) => {
            // an empty file leaves the prompt as it was
            if !t.is_empty() {
                app.prompt.pastes.clear();
                app.prompt.set_text(normalize_editor_text(&t));
            }
        }
    }
}

fn suspend(app: &App, guard: &mut TermGuard) {
    guard.suspend();
    #[cfg(unix)]
    {
        let _ = signal_hook::low_level::raise(signal_hook::consts::SIGTSTP);
    }
    let _ = guard.resume();
    reapply_cursor(app, guard);
}

/// Run until the app quits or the terminal closes.
pub async fn run(
    app: &mut App,
    terminal: &mut Terminal,
    guard: &mut TermGuard,
    mut msg_rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    // Our own reader, not crossterm's, because it can be paused while `$EDITOR` has the terminal.
    let input = tuikit::input::Input::spawn()?;
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
                    suspend(app, guard);
                    // ratatui's clear() asks the terminal for the cursor position, which the event
                    // reader would eat; a fresh terminal repaints everything on the next draw
                    *terminal = guard.terminal()?;
                }
                Deferred::Editor => {
                    run_editor(app, guard, &input_ctl);
                    // ratatui's clear() asks the terminal for the cursor position, which the event
                    // reader would eat; a fresh terminal repaints everything on the next draw
                    *terminal = guard.terminal()?;
                }
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
        // coming back to the window is when the branch or the files most likely changed
        TermEvent::FocusGained => app.refresh_workspace(),
        TermEvent::FocusLost | TermEvent::ColorScheme(_) => {}
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
    fn editor_text_loses_one_trailing_newline_only_when_single_line() {
        assert_eq!(normalize_editor_text("hello\n"), "hello");
        assert_eq!(normalize_editor_text("hello\r\n"), "hello");
        assert_eq!(normalize_editor_text("a\nb\n"), "a\nb\n");
        assert_eq!(normalize_editor_text("hello"), "hello");
        assert_eq!(normalize_editor_text("\n"), "");
    }

    #[test]
    fn utf8_is_cut_only_at_character_boundaries() {
        let mut buf = "é".as_bytes()[..1].to_vec();
        assert_eq!(drain_utf8(&mut buf), "");
        buf.push("é".as_bytes()[1]);
        assert_eq!(drain_utf8(&mut buf), "é");
        let mut buf = b"a\xffb".to_vec();
        assert_eq!(drain_utf8(&mut buf), "a\u{FFFD}b");
        assert!(buf.is_empty());
    }

    #[test]
    fn rewind_listing_parses_header_and_entry_forms() {
        assert_eq!(
            parse_rewind_listing("rewind to before turn:\n7 — fix the build · a.rs\n6 — older"),
            Some(("7".into(), "fix the build · a.rs".into()))
        );
        assert_eq!(
            parse_rewind_listing("rewind to before turn: 3 — one line form"),
            Some(("3".into(), "one line form".into()))
        );
        assert_eq!(parse_rewind_listing("nothing to rewind"), None);
    }
}
