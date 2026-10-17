//! App state, backend events and keys.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::term::Terminal;
use agent_core::{
    Backend, BackendHandle, Config, DecideScope, Event, HistoryItem, NoticeLevel,
    PermissionRequest, Request, SessionInfo, SlashCommand, StopReason, Todo, ToolCall, ToolKind,
    ToolStatus, Usage,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tuikit::editor::{Editor, KeyOutcome};
use tuikit::term::{Event as TermEvent, EventPump, TermGuard};

use crate::theme::Theme;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The Claude Code release this look reproduces.
pub const CLAUDE_VERSION: &str = "2.1.300";
pub const LOOK: &str = "claude";

pub const SPINNER_FRAMES: [&str; 6] = ["·", "✢", "*", "✶", "✻", "✽"];
pub const VERBS: [&str; 12] = [
    "Deliberating",
    "Sublimating",
    "Considering",
    "Imagining",
    "Combobulating",
    "Lollygagging",
    "Cogitating",
    "Brewing",
    "Pondering",
    "Simmering",
    "Conjuring",
    "Percolating",
];
pub const DONE_VERBS: [&str; 6] = [
    "Worked",
    "Brewed",
    "Baked",
    "Cogitated",
    "Sautéed",
    "Churned",
];
pub const PLACEHOLDERS: [&str; 4] = [
    "Try \"fix lint errors\"",
    "Try \"how does <filepath> work?\"",
    "Try \"create a util logging.py that...\"",
    "Try \"write a test for <filepath>\"",
];
/// Commands the look answers itself, listed in the menu when the backend does not name them.
pub const LOCAL_COMMANDS: &[(&str, &str)] = &[
    ("clear", "Clear conversation history and free up context"),
    ("exit", "Exit the REPL"),
    ("help", "Show help and available commands"),
    ("model", "Set the AI model for Wizard"),
    ("resume", "Resume a previous conversation"),
    (
        "status",
        "Show Wizard status including version, model and session",
    ),
];
/// How long a footer hint stays up, and how quickly a second press must follow to count.
/// What to do when wizard cannot start a session or has gone away.
pub const SETUP_HINT: &str =
    "Run `wizard setup` to choose a provider and model, then start this look again.";
/// Prompt history entries kept on disk and loaded.
const HISTORY_KEEP: usize = 1000;
/// Past this size the history file is cut back to its last entries.
const HISTORY_TRIM_BYTES: u64 = 512 * 1024;
/// The wait before a refused mode change is sent again, and how many times it is.
const MODE_RETRY: Duration = Duration::from_millis(400);
const MODE_RETRIES: u32 = 15;
const HINT_TTL: Duration = Duration::from_millis(800);
/// How long the effort notice stays up after start and after a turn.
/// How long the effort notice stays once the session is up, a turn ends or a model is picked.
const NOTICE_TTL: Duration = Duration::from_secs(30);
const DOUBLE_PRESS: Duration = Duration::from_millis(800);
const FRAME: Duration = Duration::from_millis(120);

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Backend(Event),
    Files(Vec<String>),
    Shell {
        command: String,
        output: String,
        ok: bool,
    },
}

#[derive(Clone, Debug, Default)]
pub struct AppOpts {
    pub cwd: PathBuf,
    pub resume: Option<String>,
    pub continue_latest: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub plan: bool,
    pub prompts: Vec<String>,
    pub backend: BackendKind,
    pub light: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum BackendKind {
    #[default]
    Wizard,
    Mock,
    Replay(PathBuf),
}

/// Monotonic time the tests can pin.
#[derive(Debug)]
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
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Block {
    User {
        text: String,
        bash: bool,
    },
    /// The result of a local or backend command, hanging under the user row before it. `dim`
    /// is the SGR dim attribute, `muted` the grey of secondary text.
    Output {
        text: String,
        accent: String,
        dim: bool,
        muted: bool,
    },
    Assistant {
        text: String,
        time: (u32, u32),
    },
    Thought(String),
    Tool(ToolCall),
    Todos(Vec<Todo>),
    TurnEnd {
        verb: usize,
        secs: u64,
        time: (u32, u32),
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Menu {
    None,
    /// Indexes into `App::commands`.
    Slash {
        items: Vec<usize>,
        sel: usize,
    },
    Mention {
        items: Vec<String>,
        sel: Option<usize>,
    },
}

/// The rewind confirm step's options.
pub const REWIND_CHOICES: [&str; 2] = ["Restore conversation", "Never mind"];

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Dialog {
    Help {
        tab: usize,
        sel: usize,
        focus: bool,
    },
    Model {
        sel: usize,
    },
    Status,
    /// `/mode`: wizard's modes, the one in use ticked.
    Mode {
        sel: usize,
    },
    /// `/effort`: the reasoning efforts the model takes.
    Effort {
        sel: usize,
    },
    Resume {
        sel: usize,
        query: String,
    },
    Permission {
        req: PermissionRequest,
        sel: usize,
    },
    /// Esc Esc on an empty prompt: pick a prompt to go back to before.
    Rewind {
        sel: usize,
    },
    /// `/btw`: a side question and its answer, in a panel; nothing goes in the transcript.
    Btw {
        question: String,
        answer: Option<String>,
        scroll: usize,
    },
    /// The rewind's confirm step for the `nth` prompt.
    RewindConfirm {
        nth: usize,
        sel: usize,
    },
    /// ctrl+r: earlier prompts, filtered by what is typed.
    Search {
        query: String,
        sel: usize,
    },
}

#[derive(Clone, Debug)]
pub struct Busy {
    pub since: Duration,
    pub verb: usize,
    pub chars: usize,
    pub thinking: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModeStep {
    Plan,
    Mode(String),
    Model(String),
    Effort(String),
    /// `/omakase`, sent silently like the plan toggle; it turns plan mode on with it.
    Omakase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    CtrlC,
    CtrlD,
    Esc,
    /// Esc on an empty prompt; a second opens the rewind picker.
    Rewind,
}

pub struct App {
    pub opts: AppOpts,
    pub tx: UnboundedSender<Request>,
    pub msg_tx: UnboundedSender<Msg>,
    pub size: (u16, u16),
    pub theme: Theme,
    pub clock: Clock,
    /// Wall clock override for tests: hour and minute.
    pub fixed_hm: Option<(u32, u32)>,
    /// Unix seconds for relative times; tests pin it.
    pub now_unix: i64,
    pub cfg: Config,
    pub session_id: String,
    pub commands: Vec<SlashCommand>,
    pub sessions: Vec<SessionInfo>,
    pub files: Vec<String>,
    pub blocks: Vec<Block>,
    pub todos: Vec<Todo>,
    pub show_todos: bool,
    pub usage: Usage,
    pub editor: Editor,
    pub bash_mode: bool,
    pub stash: Option<String>,
    pub prompts: Vec<String>,
    pub history_at: Option<usize>,
    pub draft: String,
    pub busy: Option<Busy>,
    pub queued: Vec<String>,
    pub plan: bool,
    /// Omakase on: plan mode whose plan is approved without asking. Put back on a new session
    /// like plan mode.
    pub omakase: bool,
    pub menu: Menu,
    pub shortcuts: bool,
    pub dialog: Option<Dialog>,
    pub detail: bool,
    /// Rows the transcript is scrolled up from its end.
    pub scroll: usize,
    pub hint: Option<(Hint, Duration)>,
    pub notice: Option<String>,
    pub placeholder: usize,
    pub ready: bool,
    pub turns: usize,
    pub fatal: Option<String>,
    pub quit: bool,
    /// Quitting for a `/ui` switch: the look taking over is where the session goes on, so
    /// no resume hint is left behind.
    pub switched: bool,
    pub dirty: bool,
    pub repaint: bool,
    pub suspend: bool,
    pub edit_request: bool,
    /// Open the transcript in the editor (`v` in the detailed view).
    pub view_request: bool,
    /// The resume picker lists every project's sessions, not just this directory's.
    pub resume_all: bool,
    /// Where the terminal cursor goes, set by the last draw.
    pub cursor: Option<(u16, u16)>,
    /// Rows of the last transcript draw: content height and viewport height.
    pub last_view: (usize, usize),
    pending_prompts: Vec<String>,
    /// Link texts and their targets in the last transcript draw, wrapped in OSC 8 after it.
    pub links: std::cell::RefCell<Vec<(String, String)>>,
    /// Where those links landed in the last frame: column, row, text, url, colour.
    pub link_spots: Vec<(u16, u16, String, String, ratatui::style::Color)>,
    /// When the notice above the prompt goes away; `None` keeps it.
    pub notice_until: Option<Duration>,
    /// Names of the commands that come from `~/.wizard/commands`.
    pub custom: Vec<String>,
    /// Subagents from `~/.wizard/subagents`: name and description.
    pub agents: Vec<(String, String)>,
    /// When each entry of `prompts` was sent, Unix seconds.
    pub prompt_times: Vec<i64>,
    rewind_answer: Option<String>,
    /// A note being typed for a permission answer (Tab on the prompt).
    pub amend: Option<String>,
    /// The prompt a `/rewind` is going back to, put back in the input when it lands.
    rewinding: Option<String>,
    /// shift+tab steps not yet sent, the one wizard has not answered, and where they lead.
    mode_steps: std::collections::VecDeque<ModeStep>,
    mode_waiting: Option<ModeStep>,
    mode_target: Option<(String, bool)>,
    /// The effort the model picker shows; applied only when the pick is confirmed.
    pub effort_preview: Option<String>,
    /// Prompt history kept across sessions, and where.
    history_file: Option<PathBuf>,
    /// The directory each entry of `prompts` was sent from.
    pub prompt_dirs: Vec<String>,
    /// The history search shows this project's prompts only.
    pub search_here: bool,
    /// ctrl+x was pressed: ctrl+s next sends the queued prompts now.
    chord: bool,
    /// The clock is pinned (tests).
    pub fixed_now: bool,
    /// Blocks holding a prompt that waited in the queue.
    pub from_queue: Vec<usize>,
    /// When to send a mode change wizard turned down again, and how often it has been.
    mode_retry: Option<Duration>,
    mode_retries: u32,
    /// Wizard said there is no session to change: mode presses wait for the next Ready.
    no_session: bool,
    /// shift+tab presses made before the first session came up.
    early_presses: usize,
    /// A session load or a new session asked for and not yet open: prompts wait for it and
    /// for the choices carried into it.
    loading: bool,
    /// The turn running was started by a command wizard answers itself.
    command_turn: bool,
    /// The running turn is wizard answering /btw.
    btw_turn: bool,
    /// A typed /rewind went through: the transcript is rebuilt when its turn ends.
    reload_after_turn: bool,
    /// The ctrl+r hint: from the second step through history until something is typed.
    pub search_hint: bool,
    /// Something was sent this session, a local command included: no placeholder after it.
    pub submitted: bool,
    /// `/plan` toggles sent for shift+tab whose turn and answer stay out of the transcript.
    silent: usize,
}

impl App {
    pub fn new(opts: AppOpts, tx: UnboundedSender<Request>, msg_tx: UnboundedSender<Msg>) -> App {
        let theme = if opts.light {
            Theme::light()
        } else {
            Theme::dark()
        };
        // plan mode also comes over from the look `/ui` left, through wizard's handoff
        let handed = agent_core::handed_plan();
        let plan = opts.plan || handed.is_some();
        let pending_prompts = opts.prompts.clone();
        let mut app = App {
            opts,
            tx,
            msg_tx,
            size: (80, 24),
            theme,
            clock: Clock::default(),
            fixed_hm: None,
            now_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
            cfg: Config::default(),
            session_id: String::new(),
            commands: LOCAL_COMMANDS
                .iter()
                .map(|(name, description)| SlashCommand {
                    name: name.to_string(),
                    description: description.to_string(),
                    input_hint: String::new(),
                })
                .collect(),
            sessions: Vec::new(),
            files: Vec::new(),
            blocks: Vec::new(),
            todos: Vec::new(),
            show_todos: true,
            usage: Usage::default(),
            editor: Editor::new(),
            bash_mode: false,
            stash: None,
            prompts: Vec::new(),
            history_at: None,
            draft: String::new(),
            busy: None,
            queued: Vec::new(),
            plan,
            omakase: handed == Some("/omakase"),
            menu: Menu::None,
            shortcuts: false,
            dialog: None,
            detail: false,
            scroll: 0,
            hint: None,
            notice: None,
            placeholder: 0,
            ready: false,
            turns: 0,
            fatal: None,
            quit: false,
            switched: false,
            dirty: true,
            repaint: false,
            suspend: false,
            edit_request: false,
            view_request: false,
            resume_all: false,
            cursor: None,
            last_view: (0, 0),
            pending_prompts,
            links: Default::default(),
            link_spots: Vec::new(),
            notice_until: None,
            custom: Vec::new(),
            agents: Vec::new(),
            prompt_times: Vec::new(),
            amend: None,
            rewind_answer: None,
            rewinding: None,
            silent: 0,
            mode_steps: Default::default(),
            mode_waiting: None,
            mode_target: None,
            effort_preview: None,
            history_file: None,
            prompt_dirs: Vec::new(),
            search_here: false,
            chord: false,
            fixed_now: false,
            from_queue: Vec::new(),
            mode_retry: None,
            mode_retries: 0,
            no_session: false,
            early_presses: 0,
            loading: false,
            command_turn: false,
            btw_turn: false,
            reload_after_turn: false,
            search_hint: false,
            submitted: false,
        };
        app.load_home();
        app
    }

    /// Custom commands (`~/.wizard/commands`, then the project's) and subagents. Wizard lists
    /// and expands the commands too; they are read here so help can show them in their own
    /// tab and the menu has them before wizard says.
    fn load_home(&mut self) {
        self.load_history();
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let wizard = home.join(".wizard");
        let mut files = read_dir_sorted(&wizard.join("commands"), "md");
        for (name, text) in read_dir_sorted(&self.opts.cwd.join(".wizard/commands"), "md") {
            files.retain(|(n, _)| *n != name);
            files.push((name, text));
        }
        for (name, text) in files {
            let description = front_matter(&text, "description").unwrap_or_else(|| {
                text.lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && *l != "---")
                    .unwrap_or("")
                    .trim_start_matches('#')
                    .trim()
                    .to_string()
            });
            if !self.commands.iter().any(|c| c.name == name) {
                self.commands.push(SlashCommand {
                    name: name.clone(),
                    description,
                    input_hint: String::new(),
                });
            }
            if !self.custom.contains(&name) {
                self.custom.push(name);
            }
        }
        for (stem, text) in read_dir_sorted(&wizard.join("subagents"), "toml") {
            let name = toml_str(&text, "name").unwrap_or(stem);
            let description = toml_str(&text, "description").unwrap_or_default();
            self.agents.push((name, description));
        }
    }

    /// Prompts sent from any directory before, oldest first, from `history.jsonl` in the state
    /// directory: `CLAUDEW_STATE_DIR`, else `$XDG_STATE_HOME/claudew` or
    /// `~/.local/state/claudew`. The scripted backends keep none unless the variable is set.
    fn load_history(&mut self) {
        let dir = match std::env::var_os("CLAUDEW_STATE_DIR") {
            Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
            _ if self.opts.backend != BackendKind::Wizard => None,
            _ => std::env::var_os("XDG_STATE_HOME")
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(s).join("claudew"))
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/claudew"))
                }),
        };
        let Some(file) = dir.map(|d| d.join("history.jsonl")) else {
            return;
        };
        // read as bytes and decode each line on its own: one bad line costs that line only
        if let Ok(bytes) = std::fs::read(&file) {
            let lines: Vec<String> = bytes
                .split(|b| *b == b'\n')
                .map(|l| String::from_utf8_lossy(l).into_owned())
                .collect();
            let skip = lines.len().saturating_sub(HISTORY_KEEP);
            for line in &lines[skip..] {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let Some(p) = v.get("text").and_then(|t| t.as_str()) else {
                    continue;
                };
                self.prompts.push(p.to_string());
                self.prompt_times
                    .push(v.get("at").and_then(|t| t.as_i64()).unwrap_or(0));
                self.prompt_dirs.push(
                    v.get("cwd")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                );
            }
        }
        self.history_file = Some(file);
    }

    fn remember(&mut self, text: &str) {
        let cwd = self.opts.cwd.to_string_lossy().into_owned();
        self.prompts.push(text.to_string());
        self.prompt_times.push(self.now_unix);
        self.prompt_dirs.push(cwd.clone());
        let Some(file) = &self.history_file else {
            return;
        };
        let line = serde_json::json!({ "text": text, "at": self.now_unix, "cwd": cwd });
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // a file cut off mid-line gets its new line on a line of its own
        let len = std::fs::metadata(file).map_or(0, |m| m.len());
        let mut lead = "";
        if len > 0 {
            use std::io::{Read, Seek, SeekFrom};
            if let Ok(mut f) = std::fs::File::open(file) {
                let mut last = [0u8; 1];
                if f.seek(SeekFrom::End(-1)).is_ok()
                    && f.read_exact(&mut last).is_ok()
                    && last[0] != b'\n'
                {
                    lead = "\n";
                }
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
        {
            use std::io::Write;
            let _ = writeln!(f, "{lead}{line}");
        }
        // keep the file to the last HISTORY_KEEP entries
        if len > HISTORY_TRIM_BYTES {
            if let Ok(bytes) = std::fs::read(file) {
                let lines: Vec<&[u8]> = bytes
                    .split(|b| *b == b'\n')
                    .filter(|l| !l.is_empty())
                    .collect();
                if lines.len() > HISTORY_KEEP {
                    let mut kept = lines[lines.len() - HISTORY_KEEP..].join(&b'\n');
                    kept.push(b'\n');
                    let tmp = file.with_extension("jsonl.tmp");
                    if std::fs::write(&tmp, kept).is_ok() {
                        let _ = std::fs::rename(&tmp, file);
                    }
                }
            }
        }
    }

    /// Wizard's version, as the backend reports it (`wizard 3.8.10`).
    /// The reply's text is coming in. The text is the progress then, so the spinner and the
    /// row above it step aside.
    pub fn streaming(&self) -> bool {
        self.busy.is_some() && matches!(self.blocks.last(), Some(Block::Assistant { .. }))
    }

    /// An edit's diff placed in its file, unless the edit failed: then nothing changed there.
    fn placed(&self, mut call: ToolCall) -> ToolCall {
        if call.status != ToolStatus::Failed {
            if let Some(d) = call.diff.take() {
                call.diff = Some(place_diff(&self.opts.cwd, d));
            }
        }
        call
    }

    /// Empty until wizard has said which it is; never this crate's own version.
    pub fn wizard_version(&self) -> String {
        self.cfg
            .backend
            .strip_prefix("wizard ")
            .unwrap_or("")
            .to_string()
    }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }

    pub fn now_hm(&self) -> (u32, u32) {
        self.fixed_hm.unwrap_or_else(local_hm)
    }

    /// The model's display name, as the backend lists it.
    pub fn model_name(&self) -> String {
        if self.cfg.model.is_empty() {
            return self.configured("model");
        }
        self.cfg
            .models
            .iter()
            .find(|m| m.id == self.cfg.model)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| self.cfg.model.clone())
    }

    pub fn provider(&self) -> String {
        if self.cfg.model.is_empty() {
            return self.configured("active_provider");
        }
        self.cfg
            .models
            .iter()
            .find(|m| m.id == self.cfg.model)
            .map(|m| m.provider.clone())
            .unwrap_or_default()
    }

    /// A top-level setting from wizard's config, for when no session came up to say it.
    fn configured(&self, key: &str) -> String {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".wizard/config.toml"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml_str(&t, key))
            .unwrap_or_default()
    }

    /// Which entry of the shift+tab cycle is on: an index into the backend's modes, or `None`
    /// for plan mode.
    /// Where the presses so far lead: an index into the modes, or `None` for plan.
    pub fn shown_index(&self) -> Option<usize> {
        let (mode, plan) = self.shown_mode();
        if plan {
            return None;
        }
        Some(self.cfg.modes.iter().position(|m| *m == mode).unwrap_or(0))
    }

    /// Where wizard confirmed it is.
    pub fn mode_index(&self) -> Option<usize> {
        let (mode, plan) = (self.cfg.mode.clone(), self.plan);
        if plan {
            return None;
        }
        Some(self.cfg.modes.iter().position(|m| *m == mode).unwrap_or(0))
    }

    fn output(&mut self, text: impl Into<String>, accent: impl Into<String>) {
        self.blocks.push(Block::Output {
            text: text.into(),
            accent: accent.into(),
            dim: false,
            muted: false,
        });
        self.scroll = 0;
    }

    // ---- backend ------------------------------------------------------------------------

    pub fn update(&mut self, msg: Msg) {
        self.dirty = true;
        match msg {
            Msg::Backend(ev) => self.on_event(ev),
            Msg::Files(f) => {
                self.files = f;
                self.refresh_menu();
            }
            Msg::Shell {
                command,
                output,
                ok,
            } => {
                self.blocks.push(Block::Tool(ToolCall {
                    id: format!("shell-{}", self.blocks.len()),
                    name: "Bash".into(),
                    kind: ToolKind::Execute,
                    title: command.clone(),
                    input: serde_json::json!({ "command": command, "local": true }),
                    status: if ok {
                        ToolStatus::Completed
                    } else {
                        ToolStatus::Failed
                    },
                    output: Some(output),
                    ..Default::default()
                }));
                self.busy = None;
            }
        }
    }

    pub fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        match ev {
            Event::Ready { session_id, config } => {
                // wizard opens every session, new, loaded or rewound, on its config's defaults:
                // what the user chose (flags at first, then picks and presses) goes back on it
                let first = !self.ready;
                let (mode, plan) = if first {
                    (config.mode.clone(), self.plan)
                } else {
                    self.shown_mode()
                };
                let omakase = std::mem::take(&mut self.omakase);
                let (model, effort) = if first {
                    (self.opts.model.clone(), self.opts.effort.clone())
                } else {
                    (Some(self.cfg.model.clone()), Some(self.cfg.effort.clone()))
                };
                self.session_id = session_id;
                self.cfg = config;
                self.plan = false;
                self.loading = false;
                self.no_session = false;
                self.silent = 0;
                self.mode_steps.clear();
                self.mode_waiting = None;
                self.mode_target = None;
                self.mode_retries = 0;
                self.mode_retry = None;
                if first {
                    self.ready = true;
                    self.notice = self.effort_notice();
                    self.notice_until = Some(self.clock.elapsed() + NOTICE_TTL);
                }
                // a model switch rebuilds wizard's agent, so it goes before mode and plan
                let model = model.map(|m| self.model_id(&m));
                if let Some(m) = model.filter(|m| !m.is_empty() && *m != self.cfg.model) {
                    self.mode_steps.push_back(ModeStep::Model(m));
                }
                if let Some(e) = effort.filter(|e| !e.is_empty() && *e != self.cfg.effort) {
                    self.mode_steps.push_back(ModeStep::Effort(e));
                }
                if omakase {
                    // `/omakase` turns plan mode on itself
                    self.go_to((mode, false));
                    self.mode_steps.push_back(ModeStep::Omakase);
                    let base = self.shown_mode().0;
                    self.mode_target = Some((base, true));
                    self.pump_modes();
                } else {
                    self.go_to((mode, plan));
                }
                // shift+tab pressed before the session existed
                for _ in 0..std::mem::take(&mut self.early_presses) {
                    self.cycle_mode();
                }
                // prompts given before the session: held until all of the above is confirmed
                for p in std::mem::take(&mut self.pending_prompts) {
                    self.submit(p);
                }
                self.pump_modes();
            }
            Event::Commands(mut c) => {
                for custom in self
                    .commands
                    .iter()
                    .filter(|x| self.custom.contains(&x.name))
                {
                    if !c.iter().any(|x| x.name == custom.name) {
                        c.push(custom.clone());
                    }
                }
                for (name, description) in LOCAL_COMMANDS {
                    if !c.iter().any(|x| x.name == *name) {
                        c.push(SlashCommand {
                            name: name.to_string(),
                            description: description.to_string(),
                            input_hint: String::new(),
                        });
                    }
                }
                self.commands = c;
                self.refresh_menu();
            }
            Event::ConfigChanged(c) => {
                self.cfg = c;
                // only the change asked for answers the wait, not some other config update
                let answered = match &self.mode_waiting {
                    Some(ModeStep::Mode(m)) => *m == self.cfg.mode,
                    Some(ModeStep::Model(m)) => *m == self.cfg.model,
                    Some(ModeStep::Effort(e)) => *e == self.cfg.effort,
                    _ => false,
                };
                if answered {
                    self.mode_waiting = None;
                    self.mode_retries = 0;
                    self.pump_modes();
                }
            }
            Event::Sessions(s) => self.sessions = s,
            Event::History { session_id, items } => {
                self.session_id = session_id;
                self.blocks.clear();
                self.from_queue.clear();
                let time = self.now_hm();
                for item in items {
                    match item {
                        HistoryItem::User(text) => {
                            self.turns += 1;
                            self.blocks.push(Block::User { text, bash: false })
                        }
                        HistoryItem::Assistant(text) => {
                            self.blocks.push(Block::Assistant { text, time })
                        }
                        HistoryItem::Thought(t) => self.blocks.push(Block::Thought(t)),
                        HistoryItem::Tool(t) => {
                            let t = self.placed(t);
                            self.blocks.push(Block::Tool(t))
                        }
                    }
                }
                self.scroll = 0;
            }
            // the side question's turn is not the user's: no spinner, nothing to interrupt
            Event::TurnStart if self.btw_turn => {}
            Event::TurnStart if self.silent > 0 && self.busy.is_none() => {}
            Event::TurnStart => {
                if self.busy.is_none() {
                    self.start_busy();
                }
            }
            Event::TextDelta(t) if self.btw_turn => {
                if let Some(Dialog::Btw { answer, .. }) = &mut self.dialog {
                    answer.get_or_insert_with(String::new).push_str(&t);
                }
            }
            Event::TextDelta(t) => {
                if let Some(b) = &mut self.busy {
                    b.chars += t.len();
                    b.thinking = false;
                }
                let time = self.now_hm();
                match self.blocks.last_mut() {
                    Some(Block::Assistant { text, .. }) => text.push_str(&t),
                    _ => self.blocks.push(Block::Assistant { text: t, time }),
                }
            }
            Event::ThoughtDelta(t) => {
                if let Some(b) = &mut self.busy {
                    b.chars += t.len();
                    b.thinking = true;
                }
                match self.blocks.last_mut() {
                    Some(Block::Thought(text)) => text.push_str(&t),
                    _ => self.blocks.push(Block::Thought(t)),
                }
            }
            Event::Tool(call) => {
                let call = self.placed(call);
                if let Some(b) = &mut self.busy {
                    b.thinking = false;
                }
                let at = self.blocks.iter().position(|b| match b {
                    Block::Tool(t) => t.id == call.id,
                    _ => false,
                });
                match at {
                    Some(i) => self.blocks[i] = Block::Tool(call),
                    None => self.blocks.push(Block::Tool(call)),
                }
            }
            Event::Todos(t) => {
                self.todos = t.clone();
                match self.blocks.last_mut() {
                    Some(Block::Todos(old)) => *old = t,
                    _ => self.blocks.push(Block::Todos(t)),
                }
            }
            Event::Usage(u) => self.usage = u,
            // the side question's turn: it never ends the user's turn, and nothing was sent
            // behind it, so a held prompt or a mode change goes next
            Event::TurnEnd(_) if self.btw_turn => {
                self.btw_turn = false;
                self.pump_modes();
            }
            Event::TurnEnd(_) if self.silent > 0 && self.busy.is_none() => {
                self.silent -= 1;
                if matches!(self.mode_waiting, Some(ModeStep::Plan | ModeStep::Omakase)) {
                    self.mode_waiting = None;
                }
                self.pump_modes();
            }
            Event::TurnEnd(reason) => {
                self.end_turn(reason);
                self.pump_modes();
            }
            // wizard turned a mode change down because something was running; try it again
            // wizard turned a mode change down because a turn or its follow-up was still in
            // flight: try again shortly, and again, until it takes
            Event::Notice { text, .. }
                if text.starts_with("Options can only be changed") && self.waiting_on_option() =>
            {
                if let Some(step) = self.mode_waiting.take() {
                    self.mode_steps.push_front(step);
                }
                self.mode_retries += 1;
                if self.mode_retries > MODE_RETRIES {
                    self.mode_failed("wizard stayed busy");
                } else {
                    self.mode_retry = Some(self.clock.elapsed() + MODE_RETRY);
                }
            }
            // no session yet: hold the change until there is one
            Event::Notice { text, .. } if text == "No session yet." && self.waiting_on_option() => {
                if let Some(step) = self.mode_waiting.take() {
                    self.mode_steps.push_front(step);
                }
                self.no_session = true;
            }
            Event::Notice { text, .. }
                if text.starts_with("Could not change the setting") && self.waiting_on_option() =>
            {
                let why = text.trim_start_matches("Could not change the setting: ");
                // a model or effort carried into a session that will not take it is skipped;
                // the mode and plan steps after it still go
                let what = match self.mode_waiting.take() {
                    Some(ModeStep::Model(_)) => Some("Model"),
                    Some(ModeStep::Effort(_)) => Some("Effort"),
                    other => {
                        self.mode_waiting = other;
                        None
                    }
                };
                match what {
                    Some(what) => {
                        self.mode_retries = 0;
                        self.blocks.push(Block::Notice {
                            level: NoticeLevel::Warn,
                            text: format!("{what} not changed: {why}"),
                        });
                        self.pump_modes();
                    }
                    None => self.mode_failed(why),
                }
            }
            // a load or a new session that failed: nothing is coming to wait for
            Event::Notice { text, .. }
                if self.loading
                    && (text.starts_with("Could not load session")
                        || text.starts_with("New session failed")
                        || text.starts_with("Finish or cancel the running turn")) =>
            {
                self.loading = false;
                self.blocks.push(Block::Notice {
                    level: NoticeLevel::Error,
                    text,
                });
                self.pump_modes();
            }
            Event::Notice { text, .. }
                if agent_core::switched_look(&text).is_some_and(|l| l != LOOK) =>
            {
                self.quit = true;
                self.switched = true;
            }
            // a typed /rewind: once its turn ends the transcript is rebuilt from the session
            Event::Notice { text, .. }
                if self.rewinding.is_none() && text.starts_with("rewound to before turn") =>
            {
                self.reload_after_turn = true;
            }
            Event::Notice { text, .. } if self.rewinding.is_some() => {
                self.rewind_answer = Some(text);
            }
            // the answer to /btw goes in its panel
            Event::Notice { text, .. } if self.btw_turn => {
                if let Some(Dialog::Btw { answer, .. }) = &mut self.dialog {
                    *answer = Some(text);
                }
            }
            Event::Notice { level, text } => {
                let low = text.to_lowercase();
                // /plan and /omakase answers say where plan mode is; leaving omakase stays in it
                if let Some(on) = agent_core::plan_mode(&text) {
                    self.plan = on;
                    let t = text.trim();
                    if t.starts_with("omakase on") {
                        self.omakase = true;
                    } else if t.starts_with("omakase off") || !on {
                        self.omakase = false;
                    }
                    // the line wizard sends when an approved plan leaves plan mode mid-turn: the
                    // approval already shows. A typed command's answer is always shown
                    if t == "plan mode off" && self.busy.is_some() && !self.slash_turn() {
                        return;
                    }
                }
                if self.silent > 0 && level == NoticeLevel::Info {
                    return;
                }
                let after_command = matches!(
                    self.blocks.last(),
                    Some(Block::User { text, .. }) if text.starts_with('/')
                );
                if after_command && level == NoticeLevel::Info {
                    let dim = low.starts_with("compact");
                    self.blocks.push(Block::Output {
                        text,
                        accent: String::new(),
                        dim,
                        muted: false,
                    });
                } else {
                    let setup = text.starts_with("could not start a session");
                    self.blocks.push(Block::Notice { level, text });
                    if setup {
                        self.busy = None;
                        self.blocks.push(Block::Notice {
                            level: NoticeLevel::Info,
                            text: SETUP_HINT.into(),
                        });
                    }
                }
            }
            // wizard is gone: nothing is running any more, and nothing typed can reach it
            Event::Fatal(e) => {
                self.busy = None;
                self.queued.clear();
                self.notice = None;
                self.blocks.push(Block::Notice {
                    level: NoticeLevel::Error,
                    text: e.clone(),
                });
                self.blocks.push(Block::Notice {
                    level: NoticeLevel::Info,
                    text: SETUP_HINT.into(),
                });
                self.fatal = Some(e);
            }
            Event::Permission(req) => {
                self.dialog = Some(Dialog::Permission { req, sel: 0 });
            }
            Event::RewindPreview(_) => {}
        }
    }

    fn effort_notice(&self) -> Option<String> {
        effort_notice_text(&self.cfg.effort)
    }

    fn start_busy(&mut self) {
        let n = self.turns + self.blocks.len();
        self.busy = Some(Busy {
            since: self.clock.elapsed(),
            verb: n % VERBS.len(),
            chars: 0,
            thinking: false,
        });
        self.notice = self.effort_notice();
        self.notice_until = None;
    }

    fn end_turn(&mut self, reason: StopReason) {
        if std::mem::take(&mut self.reload_after_turn) {
            self.busy = None;
            self.blocks.clear();
            self.from_queue.clear();
            self.load(Some(self.session_id.clone()));
            return;
        }
        if let Some(text) = self.rewinding.take() {
            self.finish_rewind(text);
            return;
        }
        let Some(busy) = self.busy.take() else { return };
        self.notice_until = Some(self.clock.elapsed() + NOTICE_TTL);
        let secs = (self.clock.elapsed().saturating_sub(busy.since)).as_secs();
        // a slash line wizard ran as a command; one it handed to the model gets the done line
        let since: Vec<&Block> = self
            .blocks
            .iter()
            .rev()
            .take_while(|b| !matches!(b, Block::User { .. }))
            .collect();
        let command_turn = self
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                Block::User { text, bash } => Some(text.starts_with('/') && !bash),
                _ => None,
            })
            .unwrap_or(false)
            && !since
                .iter()
                .any(|b| matches!(b, Block::Assistant { .. } | Block::Tool(_)));
        match reason {
            StopReason::Cancelled => {
                // calls still out when the turn stopped fold away like finished ones
                for b in &mut self.blocks {
                    if let Block::Tool(t) = b {
                        if matches!(t.status, ToolStatus::Pending | ToolStatus::Running) {
                            t.status = ToolStatus::Failed;
                            t.output = Some(agent_core::INTERRUPTED_OUTPUT.into());
                        }
                    }
                }
                // Esc on a plan is a rejection, not an interruption: the plan stays, boxed
                let rejected_plan = self
                    .blocks
                    .iter()
                    .rev()
                    .take_while(|b| !matches!(b, Block::User { .. }))
                    .find_map(|b| match b {
                        Block::Tool(t) => Some(crate::ui::transcript::is_plan_call(t)),
                        _ => None,
                    })
                    .unwrap_or(false);
                if rejected_plan {
                    self.blocks.push(Block::TurnEnd {
                        verb: busy.verb % DONE_VERBS.len(),
                        secs,
                        time: self.now_hm(),
                    });
                    return;
                }
                self.blocks.push(Block::Output {
                    text: "Interrupted · What should Wizard do instead?".into(),
                    accent: String::new(),
                    dim: false,
                    muted: true,
                });
                let used_tools = self
                    .blocks
                    .iter()
                    .rev()
                    .take_while(|b| !matches!(b, Block::User { .. }))
                    .any(|b| matches!(b, Block::Tool(_)));
                if used_tools {
                    self.blocks.push(Block::TurnEnd {
                        verb: busy.verb % DONE_VERBS.len(),
                        secs,
                        time: self.now_hm(),
                    });
                }
            }
            _ if command_turn => {}
            _ => self.blocks.push(Block::TurnEnd {
                verb: busy.verb % DONE_VERBS.len(),
                secs,
                time: self.now_hm(),
            }),
        }
    }

    // ---- prompts ------------------------------------------------------------------------

    /// Send `text` as the user typed it: a local command, a shell line or a prompt.
    pub fn submit(&mut self, text: String) {
        let text = text.trim_end().to_string();
        if text.is_empty() {
            return;
        }
        self.scroll = 0;
        self.submitted = true;
        self.search_hint = false;
        self.history_at = None;
        if self.bash_mode {
            self.bash_mode = false;
            if self.prompts.last() != Some(&text) {
                self.remember(&text);
            }
            self.run_shell(text);
            return;
        }
        let (name, arg) = match text.strip_prefix('/') {
            Some(rest) => {
                let (n, a) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                (n.to_string(), a.trim().to_string())
            }
            None => (String::new(), String::new()),
        };
        if matches!(name.as_str(), "exit" | "quit") {
            self.quit = true;
            return;
        }
        if self.fatal.is_some() {
            self.blocks.push(Block::User { text, bash: false });
            self.blocks.push(Block::Notice {
                level: NoticeLevel::Error,
                text: "Not sent: there is no session to send it to.".into(),
            });
            return;
        }
        // a turn running, or a mode change not yet confirmed: the prompt waits for it, so it
        // never runs in a mode on the way to the one pressed
        // panels the look opens itself (help, status, the pickers) need no wizard mode
        let local = matches!(
            name.as_str(),
            "help" | "status" | "resume" | "continue" | "config" | "settings"
        ) || (matches!(name.as_str(), "model" | "mode" | "rewind" | "effort")
            && arg.is_empty());
        if self.busy.is_some() || (self.modes_pending() && !local) {
            self.queued.push(text);
            return;
        }
        // history is what was sent: a queued prompt counts once it goes, not when typed
        if self.prompts.last() != Some(&text) {
            self.remember(&text);
        }
        match name.as_str() {
            "exit" | "quit" => {
                self.quit = true;
                return;
            }
            // a side question: answered in a panel, kept out of the conversation
            "btw" if !arg.is_empty() => {
                self.dialog = Some(Dialog::Btw {
                    question: arg.clone(),
                    answer: None,
                    scroll: 0,
                });
                self.btw_turn = true;
                self.send(Request::Prompt(text));
                return;
            }
            // plan mode through the same steps as shift+tab, so it is never entered on chat
            "plan" if arg.is_empty() && !self.cfg.modes.is_empty() => {
                self.blocks.push(Block::User { text, bash: false });
                let (mode, plan) = self.shown_mode();
                if plan {
                    self.output("Already in plan mode.", String::new());
                } else {
                    self.go_to((mode, true));
                    self.output("Enabled plan mode", String::new());
                }
                return;
            }
            "plan" if matches!(arg.as_str(), "on" | "off") && !self.cfg.modes.is_empty() => {
                self.blocks.push(Block::User { text, bash: false });
                let (mode, plan) = self.shown_mode();
                let on = arg == "on";
                if on != plan {
                    self.go_to((mode, on));
                }
                let said = if on {
                    "Enabled plan mode"
                } else {
                    "Disabled plan mode"
                };
                self.output(said, String::new());
                return;
            }
            // `/plan <text>`: plan mode stays on (or goes on) and the text is the prompt, sent
            // once wizard is in plan mode
            "plan" if !self.cfg.modes.is_empty() => {
                let (mode, plan) = self.shown_mode();
                if !plan {
                    self.go_to((mode, true));
                }
                self.submit(arg);
                return;
            }
            // the same picker as Esc Esc
            "rewind" if arg.is_empty() => {
                if self.rewind_targets().is_empty() {
                    self.blocks.push(Block::User { text, bash: false });
                    self.output("Nothing to rewind to yet", String::new());
                } else {
                    self.dialog = Some(Dialog::Rewind { sel: usize::MAX });
                }
                return;
            }
            // a model or effort typed out goes the picker's way, with the picker's answer
            "model" if self.model_index(&arg).is_some() => {
                if let Some(i) = self.model_index(&arg) {
                    self.apply_model(i, text);
                }
                return;
            }
            "effort" if arg.is_empty() && !self.cfg.efforts.is_empty() => {
                let sel = self
                    .cfg
                    .efforts
                    .iter()
                    .position(|e| *e == self.cfg.effort)
                    .unwrap_or(0);
                self.dialog = Some(Dialog::Effort { sel });
                return;
            }
            "effort" if self.cfg.efforts.contains(&arg) => {
                self.blocks.push(Block::User { text, bash: false });
                self.set_effort(arg);
                return;
            }
            "config" | "settings" => {
                self.blocks.push(Block::User { text, bash: false });
                self.output(
                    "Wizard has no settings panel here: /model and /mode change this session, \
                     and ~/.wizard/config.toml (or `wizard setup`) holds the defaults",
                    String::new(),
                );
                return;
            }
            "mode" if arg.is_empty() && !self.cfg.modes.is_empty() => {
                let (mode, _) = self.shown_mode();
                let sel = self.cfg.modes.iter().position(|m| *m == mode).unwrap_or(0);
                self.dialog = Some(Dialog::Mode { sel });
                return;
            }
            "mode" if self.cfg.modes.contains(&arg) => {
                self.blocks.push(Block::User { text, bash: false });
                self.pick_mode_named(arg);
                return;
            }
            "clear" | "new" | "reset" => {
                self.notice = None;
                self.from_queue.clear();
                self.blocks.clear();
                self.todos.clear();
                self.turns = 0;
                self.usage = Usage::default();
                self.blocks.push(Block::User { text, bash: false });
                self.load(None);
                return;
            }
            "help" => {
                self.dialog = Some(Dialog::Help {
                    tab: 0,
                    sel: 0,
                    focus: false,
                });
                return;
            }
            "status" => {
                self.dialog = Some(Dialog::Status);
                return;
            }
            "model" if arg.is_empty() && !self.cfg.models.is_empty() => {
                self.open_model();
                return;
            }
            "resume" | "continue" if arg.is_empty() => {
                self.sessions.clear();
                self.send(Request::ListSessions);
                self.dialog = Some(Dialog::Resume {
                    sel: 0,
                    query: String::new(),
                });
                return;
            }
            "resume" | "continue" => {
                self.blocks.push(Block::User { text, bash: false });
                self.load(Some(arg));
                return;
            }
            _ => {}
        }
        self.blocks.push(Block::User {
            text: text.clone(),
            bash: false,
        });
        self.turns += 1;
        // the user's own commands are prompts for the model once wizard expands them
        self.command_turn = !name.is_empty() && !self.custom.contains(&name);
        self.start_busy();
        self.send(Request::Prompt(text));
    }

    fn run_shell(&mut self, command: String) {
        self.blocks.push(Block::User {
            text: command.clone(),
            bash: true,
        });
        self.start_busy();
        let tx = self.msg_tx.clone();
        let cwd = self.opts.cwd.clone();
        std::thread::spawn(move || {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                .output();
            let (output, ok) = match out {
                Ok(o) => {
                    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                    s.push_str(&String::from_utf8_lossy(&o.stderr));
                    (s.trim_end().to_string(), o.status.success())
                }
                Err(e) => (e.to_string(), false),
            };
            let _ = tx.send(Msg::Shell {
                command,
                output,
                ok,
            });
        });
    }

    fn open_model(&mut self) {
        let sel = self
            .cfg
            .models
            .iter()
            .position(|m| m.id == self.cfg.model)
            .unwrap_or(0);
        self.dialog = Some(Dialog::Model { sel });
    }

    // ---- menus --------------------------------------------------------------------------

    /// The `@token` the cursor is in or just after, as (byte start, query).
    fn mention_at_cursor(&self) -> Option<(usize, String)> {
        let text = self.editor.text();
        let upto = &text[..self.editor.cursor().min(text.len())];
        let start = upto.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + 1);
        let token = &upto[start..];
        token.strip_prefix('@').map(|q| (start, q.to_string()))
    }

    pub fn refresh_menu(&mut self) {
        let text = self.editor.text().to_string();
        // a recalled `/help` is history, not a command being typed
        if self.bash_mode || self.history_at.is_some() {
            self.menu = Menu::None;
            return;
        }
        if let Some(q) = text.strip_prefix('/') {
            if !q.contains(char::is_whitespace) {
                let items = slash_matches(&self.commands, q);
                let sel = match &self.menu {
                    Menu::Slash { sel, items: old } if *old == items => *sel,
                    _ => 0,
                };
                self.menu = Menu::Slash { items, sel };
                return;
            }
        }
        if let Some((_, q)) = self.mention_at_cursor() {
            let items = mention_matches(&self.files, &self.agents, &q);
            let sel = match &self.menu {
                Menu::Mention { sel, items: old } if *old == items => *sel,
                _ => None,
            };
            self.menu = Menu::Mention { items, sel };
            return;
        }
        self.menu = Menu::None;
    }

    fn menu_move(&mut self, down: bool) {
        match &mut self.menu {
            Menu::Slash { items, sel } if !items.is_empty() => {
                let n = items.len();
                *sel = if down {
                    (*sel + 1) % n
                } else {
                    (*sel + n - 1) % n
                };
            }
            Menu::Mention { items, sel } if !items.is_empty() => {
                let n = items.len();
                *sel = Some(match *sel {
                    None if down => 0,
                    None => n - 1,
                    Some(i) if down => (i + 1) % n,
                    Some(i) => (i + n - 1) % n,
                });
            }
            _ => {}
        }
    }

    /// Put the selected menu entry into the input. Returns whether there was one to take.
    fn menu_accept(&mut self, first_if_none: bool) -> bool {
        match self.menu.clone() {
            Menu::Slash { items, sel } => {
                let Some(cmd) = items.get(sel).and_then(|i| self.commands.get(*i)) else {
                    return false;
                };
                self.editor.set_text(&format!("/{} ", cmd.name));
                self.menu = Menu::None;
                true
            }
            Menu::Mention { items, sel } => {
                let pick = match sel {
                    Some(i) => items.get(i),
                    None if first_if_none => items.first(),
                    None => None,
                };
                let (Some(path), Some((start, _))) = (pick.cloned(), self.mention_at_cursor())
                else {
                    return false;
                };
                let text = self.editor.text().to_string();
                let cursor = self.editor.cursor().min(text.len());
                let path = match path.strip_prefix(AGENT) {
                    Some(a) => format!("agent-{}", a.split('\t').next().unwrap_or("")),
                    None => path,
                };
                let dir = path.ends_with('/');
                let insert = if dir {
                    format!("@{path}")
                } else {
                    format!("@{path} ")
                };
                let new = format!("{}{}{}", &text[..start], insert, &text[cursor..]);
                self.editor.set_text(&new);
                self.editor.set_cursor(start + insert.len());
                self.menu = Menu::None;
                if dir {
                    self.refresh_menu();
                }
                true
            }
            Menu::None => false,
        }
    }

    // ---- keys ---------------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let now = self.clock.elapsed();
        let chord = std::mem::take(&mut self.chord);
        let prev_hint = self
            .hint
            .filter(|(_, at)| now.saturating_sub(*at) < DOUBLE_PRESS)
            .map(|(h, _)| h);
        if matches!(key.code, KeyCode::Esc)
            || (ctrl && matches!(key.code, KeyCode::Char('c' | 'd')))
        {
            self.hint = None;
        }

        if ctrl && key.code == KeyCode::Char('c') {
            self.ctrl_c(prev_hint);
            if self.dialog.is_none() {
                self.flush_queue();
            }
            return;
        }
        if self.detail {
            return self.detail_key(key);
        }
        if self.dialog.is_some() {
            self.dialog_key(key);
            // a picker a queued command opened is answered: the rest of the queue goes on
            if self.dialog.is_none() {
                self.flush_queue();
            }
            return;
        }
        match key.code {
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => {
                if prev_hint == Some(Hint::CtrlD) {
                    self.quit = true;
                } else {
                    self.hint = Some((Hint::CtrlD, self.clock.elapsed()));
                }
            }
            KeyCode::Char('o') if ctrl => {
                self.detail = true;
                self.scroll = 0;
            }
            KeyCode::Char('t') if ctrl => self.show_todos = !self.show_todos,
            KeyCode::Char('r') if ctrl => {
                self.dialog = Some(Dialog::Search {
                    query: String::new(),
                    sel: 0,
                })
            }
            KeyCode::End if ctrl => self.scroll = 0,
            KeyCode::Char('l') if ctrl => self.repaint = true,
            KeyCode::Char('z') if ctrl && !shift => self.suspend = true,
            KeyCode::Char('g') if ctrl => self.edit_request = true,
            KeyCode::Char('s') if ctrl && chord => {
                self.chord = false;
                if self.busy.is_some() && !self.queued.is_empty() {
                    self.send(Request::Cancel);
                }
            }
            KeyCode::Char('x') if ctrl => self.chord = true,
            // ctrl+enter, where the terminal reports it, sends the queue now too
            KeyCode::Enter if ctrl && !self.queued.is_empty() => {
                if self.busy.is_some() {
                    self.send(Request::Cancel);
                }
            }
            KeyCode::Char('s') if ctrl => self.toggle_stash(),
            KeyCode::Char('p') if alt => self.open_model(),
            KeyCode::BackTab => self.cycle_mode(),
            KeyCode::PageUp => self.scroll_by(self.page() as isize),
            KeyCode::PageDown => self.scroll_by(-(self.page() as isize)),
            KeyCode::Esc => self.escape(prev_hint),
            KeyCode::Enter if shift || alt => self.insert_newline(),
            KeyCode::Char('j') if ctrl => self.insert_newline(),
            KeyCode::Enter => self.enter(),
            KeyCode::Tab => {
                if !self.complete_ui() {
                    self.menu_accept(true);
                }
            }
            KeyCode::Up if self.menu_open() => self.menu_move(false),
            KeyCode::Down if self.menu_open() => self.menu_move(true),
            KeyCode::Char('?') if self.editor.is_empty() && !self.bash_mode => {
                self.shortcuts = !self.shortcuts;
            }
            KeyCode::Char('!') if self.editor.is_empty() && !self.bash_mode => {
                self.bash_mode = true;
                self.shortcuts = false;
            }
            KeyCode::Backspace if self.editor.is_empty() && (self.bash_mode || self.shortcuts) => {
                self.bash_mode = false;
                self.shortcuts = false;
            }
            _ => {
                self.shortcuts = false;
                match self.editor.apply_key(key) {
                    KeyOutcome::AtTop if key.code == KeyCode::Up && !self.queued.is_empty() => {
                        let back = std::mem::take(&mut self.queued).join("\n");
                        self.editor.set_text(&back);
                    }
                    KeyOutcome::AtTop if key.code == KeyCode::Up => self.history(-1),
                    KeyOutcome::AtBottom if key.code == KeyCode::Down => self.history(1),
                    _ => {
                        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
                            self.history_at = None;
                            self.search_hint = false;
                        }
                    }
                }
                self.refresh_menu();
            }
        }
    }

    fn menu_open(&self) -> bool {
        match &self.menu {
            Menu::None => false,
            Menu::Slash { items, .. } => !items.is_empty(),
            Menu::Mention { items, .. } => !items.is_empty(),
        }
    }

    fn page(&self) -> usize {
        (self.last_view.1 / 2).max(1)
    }

    pub fn scroll_by(&mut self, up: isize) {
        let max = self.last_view.0.saturating_sub(self.last_view.1) as isize;
        self.scroll = (self.scroll as isize + up).clamp(0, max.max(0)) as usize;
    }

    fn ctrl_c(&mut self, prev: Option<Hint>) {
        if self.dialog.is_some() {
            self.close_dialog();
            return;
        }
        if self.detail {
            self.detail = false;
            return;
        }
        if prev == Some(Hint::CtrlC) && self.editor.is_empty() {
            self.quit = true;
            return;
        }
        if self.editor.is_empty() && self.busy.is_some() {
            self.send(Request::Cancel);
            self.hint = Some((Hint::CtrlC, self.clock.elapsed()));
            return;
        }
        self.editor.clear();
        self.bash_mode = false;
        self.shortcuts = false;
        self.history_at = None;
        self.menu = Menu::None;
        self.hint = Some((Hint::CtrlC, self.clock.elapsed()));
    }

    fn escape(&mut self, prev: Option<Hint>) {
        if self.shortcuts {
            self.shortcuts = false;
        } else if self.menu_open() {
            self.menu = Menu::None;
        } else if self.bash_mode && self.editor.is_empty() {
            self.bash_mode = false;
        } else if self.busy.is_some() {
            self.send(Request::Cancel);
        } else if !self.editor.is_empty() {
            if prev == Some(Hint::Esc) {
                self.editor.clear();
                self.menu = Menu::None;
            } else {
                self.hint = Some((Hint::Esc, self.clock.elapsed()));
            }
        } else if prev == Some(Hint::Rewind) {
            if !self.rewind_targets().is_empty() {
                self.dialog = Some(Dialog::Rewind { sel: usize::MAX });
            }
        } else {
            self.hint = Some((Hint::Rewind, self.clock.elapsed()));
        }
    }

    fn insert_newline(&mut self) {
        self.editor.insert_newline();
        self.refresh_menu();
    }

    fn enter(&mut self) {
        if self.editor.text().ends_with('\\') && self.editor.cursor() == self.editor.text().len() {
            self.editor.backspace();
            self.editor.insert_newline();
            return;
        }
        // what was typed wins when it names a command, or when nothing's name matches it (a
        // description match is a suggestion, not something to run on Enter)
        let typed = self.editor.text().trim().to_string();
        let word = typed
            .strip_prefix('/')
            .map(|r| r.split_whitespace().next().unwrap_or("").to_lowercase());
        let named = word.as_ref().is_some_and(|w| {
            self.commands.iter().any(|c| c.name.to_lowercase() == *w)
                || !self
                    .commands
                    .iter()
                    .any(|c| c.name.to_lowercase().contains(w.as_str()))
        });
        if named && matches!(self.menu, Menu::Slash { .. }) {
            self.editor.clear();
            self.menu = Menu::None;
            self.submit(typed);
            return;
        }
        if let Menu::Slash { items, sel } = &self.menu {
            if let Some(cmd) = items.get(*sel).and_then(|i| self.commands.get(*i)) {
                let text = format!("/{}", cmd.name);
                self.editor.clear();
                self.menu = Menu::None;
                self.submit(text);
                return;
            }
        }
        if matches!(self.menu, Menu::Mention { sel: Some(_), .. }) && self.menu_accept(false) {
            return;
        }
        let text = self.editor.text().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.editor.clear();
        self.menu = Menu::None;
        self.submit(text);
        if let Some(s) = self.stash.take() {
            self.editor.set_text(&s);
        }
    }

    /// `/ui <part>` + Tab: the next look whose name starts with what is typed, round the list.
    fn complete_ui(&mut self) -> bool {
        const LOOKS: [&str; 7] = [
            "wizard", "codex", "grok", "opencode", "pi", "jcode", "claude",
        ];
        let text = self.editor.text().to_string();
        let Some(part) = text.strip_prefix("/ui ") else {
            return false;
        };
        let part = part.trim();
        let at = LOOKS.iter().position(|l| *l == part);
        let next = match at {
            // already a full name: Tab moves on to the next look
            Some(i) => LOOKS[(i + 1) % LOOKS.len()],
            None => match LOOKS.iter().find(|l| l.starts_with(part)) {
                Some(l) => l,
                None => return false,
            },
        };
        self.editor.set_text(&format!("/ui {next}"));
        self.menu = Menu::None;
        true
    }

    fn toggle_stash(&mut self) {
        if self.editor.is_empty() {
            if let Some(s) = self.stash.take() {
                self.editor.set_text(&s);
            }
        } else {
            self.stash = Some(self.editor.text().to_string());
            self.editor.clear();
        }
        self.refresh_menu();
    }

    /// The prompts Up and Down step through: this project's, oldest first. History from other
    /// directories stays out of the arrows (ctrl+r can reach it).
    pub fn arrow_history(&self) -> Vec<usize> {
        let here = self.opts.cwd.to_string_lossy();
        (0..self.prompts.len())
            .filter(|&i| {
                self.prompt_dirs
                    .get(i)
                    .is_none_or(|d| d.is_empty() || *d == here)
            })
            .collect()
    }

    fn history(&mut self, step: isize) {
        let list = self.arrow_history();
        if list.is_empty() {
            return;
        }
        let n = list.len();
        let next = match (self.history_at, step) {
            (None, s) if s < 0 => {
                self.draft = self.editor.text().to_string();
                Some(n - 1)
            }
            (None, _) => None,
            (Some(0), s) if s < 0 => Some(0),
            (Some(i), s) if s < 0 => Some(i - 1),
            (Some(i), _) if i + 1 >= n => None,
            (Some(i), _) => Some(i + 1),
        };
        if next == self.history_at {
            return;
        }
        // the reference shows it once you are moving through history, not on the first step
        if self.history_at.is_some() {
            self.search_hint = true;
        }
        self.history_at = next;
        match next {
            Some(i) => {
                let t = self.prompts[list[i]].clone();
                self.editor.set_text(&t);
            }
            None => {
                let d = std::mem::take(&mut self.draft);
                self.editor.set_text(&d);
            }
        }
        self.menu = Menu::None;
    }

    /// Flip wizard's plan mode without a transcript entry.
    fn toggle_plan(&mut self) {
        self.silent += 1;
        self.send(Request::Prompt("/plan".into()));
    }

    /// Where the presses so far lead, not yet all confirmed by wizard.
    pub fn shown_mode(&self) -> (String, bool) {
        self.mode_target
            .clone()
            .unwrap_or_else(|| (self.cfg.mode.clone(), self.plan))
    }

    /// shift+tab: the next of wizard's modes, then plan, then back. Wizard takes one change at
    /// a time and none during a turn, so the steps queue and each waits for its answer.
    fn cycle_mode(&mut self) {
        // no session yet, so no modes to step through: count the press and replay it on Ready
        if !self.ready {
            self.early_presses += 1;
            return;
        }
        let modes = self.cfg.modes.clone();
        let (mode, plan) = self.shown_mode();
        let at = modes.iter().position(|m| *m == mode).unwrap_or(0);
        let next = if plan {
            (modes.first().cloned().unwrap_or(mode.clone()), false)
        } else if at + 1 < modes.len() {
            (modes[at + 1].clone(), false)
        } else {
            (mode.clone(), true)
        };
        self.go_to(next);
    }

    /// Queue the steps from where the presses so far lead to `next` (mode, plan).
    fn go_to(&mut self, next: (String, bool)) {
        let modes = self.cfg.modes.clone();
        let (mode, plan) = self.shown_mode();
        // plan mode reads the project, so it never sits on chat, which has no project tools
        let next = if next.1 && next.0 == "chat" {
            let base = modes
                .iter()
                .find(|m| *m != "chat")
                .cloned()
                .unwrap_or(next.0);
            (base, true)
        } else {
            next
        };
        // into plan: the base mode first, so plan sits on it; out of plan: plan off first
        let base = (next.0 != mode).then(|| ModeStep::Mode(next.0.clone()));
        let toggle = (next.1 != plan).then_some(ModeStep::Plan);
        let steps = if next.1 {
            [base, toggle]
        } else {
            [toggle, base]
        };
        self.mode_steps.extend(steps.into_iter().flatten());
        self.mode_target = Some(next);
        self.pump_modes();
    }

    /// A mode change wizard would not make: the rest of the presses go too, the footer goes
    /// back to the mode wizard is in, and a prompt held for them returns to the input unsent.
    fn mode_failed(&mut self, why: &str) {
        self.mode_waiting = None;
        self.mode_steps.clear();
        self.mode_target = None;
        self.mode_retries = 0;
        self.mode_retry = None;
        self.blocks.push(Block::Notice {
            level: NoticeLevel::Warn,
            text: format!("Mode not changed: {why}"),
        });
        if !self.queued.is_empty() && self.editor.is_empty() {
            let back = std::mem::take(&mut self.queued).join("\n");
            self.editor.set_text(&back);
        }
    }

    /// A mode change is pressed and not yet confirmed: prompts wait for it.
    pub fn modes_pending(&self) -> bool {
        self.mode_waiting.is_some()
            || !self.mode_steps.is_empty()
            || self.silent > 0
            || self.early_presses > 0
            || self.loading
            || self.btw_turn
    }

    /// The id wizard reports for `m`, a model id or a name (`--model grok-4`), so the wait for
    /// it is answered by the config update that follows.
    fn model_id(&self, m: &str) -> String {
        let models = &self.cfg.models;
        if m.contains('/') || models.iter().any(|x| x.id == m) {
            return m.to_string();
        }
        let current = self.cfg.model.split('/').next().unwrap_or("");
        models
            .iter()
            .filter(|x| x.name == m)
            .max_by_key(|x| x.provider == current)
            .map_or_else(|| m.to_string(), |x| x.id.clone())
    }

    /// The turn running is one wizard answers as a command, not a prompt for the model.
    fn slash_turn(&self) -> bool {
        self.command_turn
    }

    /// Waiting on a session option (mode, model or effort), not on a plan toggle.
    fn waiting_on_option(&self) -> bool {
        matches!(
            self.mode_waiting,
            Some(ModeStep::Mode(_) | ModeStep::Model(_) | ModeStep::Effort(_))
        )
    }

    /// Open a saved session, or a new one with `None`; prompts wait until it is open.
    fn load(&mut self, id: Option<String>) {
        self.loading = true;
        self.send(match id {
            Some(id) => Request::LoadSession(id),
            None => Request::NewSession,
        });
    }

    fn pump_modes(&mut self) {
        // before the session exists there is nothing to change; Ready runs this again
        if self.mode_waiting.is_some()
            || self.busy.is_some()
            || self.btw_turn
            || self.loading
            || !self.ready
            || self.no_session
        {
            return;
        }
        match self.mode_steps.pop_front() {
            Some(ModeStep::Plan) => {
                self.mode_waiting = Some(ModeStep::Plan);
                self.toggle_plan();
            }
            Some(ModeStep::Mode(m)) => {
                self.mode_waiting = Some(ModeStep::Mode(m.clone()));
                self.send(Request::SetMode(m));
            }
            Some(ModeStep::Omakase) => {
                self.mode_waiting = Some(ModeStep::Omakase);
                self.silent += 1;
                self.send(Request::Prompt("/omakase".into()));
            }
            Some(ModeStep::Model(m)) => {
                self.mode_waiting = Some(ModeStep::Model(m.clone()));
                self.send(Request::SetModel(m));
            }
            Some(ModeStep::Effort(e)) => {
                self.mode_waiting = Some(ModeStep::Effort(e.clone()));
                self.send(Request::SetEffort(e));
            }
            None => {
                self.mode_target = None;
                self.flush_queue();
            }
        }
    }

    /// Send the next queued prompt, once the turn is over and every mode change pressed
    /// before it has been confirmed: a prompt queued after shift+tab runs in that mode.
    fn flush_queue(&mut self) {
        // one the look answers itself (a picker, /config) starts no turn: the next goes too
        // nor past a picker one of them opened: the rest wait until it is answered
        while self.busy.is_none()
            && self.dialog.is_none()
            && !self.modes_pending()
            && !self.queued.is_empty()
        {
            let next = self.queued.remove(0);
            let at = self.blocks.len();
            self.submit(next);
            if matches!(self.blocks.get(at), Some(Block::User { .. })) {
                self.from_queue.push(at);
            }
        }
    }

    fn detail_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('o') if ctrl => self.detail = false,
            KeyCode::Esc | KeyCode::Char('q') => self.detail = false,
            KeyCode::Char('?') => self.shortcuts = !self.shortcuts,
            KeyCode::Char('v') => self.view_request = true,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(self.page() as isize),
            KeyCode::PageDown => self.scroll_by(-(self.page() as isize)),
            _ => {}
        }
        if !self.detail {
            self.scroll = 0;
        }
    }

    /// What the rewind's confirm step offers. Wizard's /rewind restores the conversation (and
    /// the files the later turns changed); its /compact summarizes everything, not from or up
    /// to a point, so the reference's two summarize options have nothing behind them here.
    fn rewind_choice(&mut self, nth: usize, choice: usize) {
        self.dialog = None;
        if REWIND_CHOICES[choice] == "Restore conversation" {
            self.start_rewind(nth);
        }
    }

    fn close_dialog(&mut self) {
        match self.dialog.take() {
            Some(Dialog::RewindConfirm { nth, .. }) => {
                self.dialog = Some(Dialog::Rewind { sel: nth });
            }
            Some(Dialog::Model { .. }) => {
                self.effort_preview = None;
                self.show_effort_notice();
                self.blocks.push(Block::User {
                    text: "/model ".into(),
                    bash: false,
                });
                let name = self.model_name();
                self.output(format!("Kept model as {name}"), name);
            }
            Some(Dialog::Effort { .. }) => {
                self.blocks.push(Block::User {
                    text: "/effort ".into(),
                    bash: false,
                });
                let e = self.cfg.effort.clone();
                self.output(format!("Kept effort level as {e}"), String::new());
            }
            Some(Dialog::Mode { .. }) => {
                self.blocks.push(Block::User {
                    text: "/mode ".into(),
                    bash: false,
                });
                let (mode, _) = self.shown_mode();
                self.output(format!("Kept mode as {mode}"), String::new());
            }
            Some(Dialog::Resume { .. }) => {
                self.resume_all = false;
                self.blocks.push(Block::User {
                    text: "/resume ".into(),
                    bash: false,
                });
                self.output("Resume cancelled", "");
            }
            // Esc on a permission prompt turns the call down and stops the turn
            Some(Dialog::Permission { req, .. }) => {
                self.amend = None;
                self.send(Request::Decide {
                    id: req.id,
                    allow: false,
                    scope: DecideScope::Once,
                    note: String::new(),
                });
                self.send(Request::Cancel);
            }
            _ => {}
        }
    }

    /// Sessions the resume picker lists for `query`: this directory's first.
    pub fn resume_list(&self, query: &str) -> Vec<&SessionInfo> {
        let q = query.to_lowercase();
        let cwd = self.opts.cwd.to_string_lossy();
        let mut v: Vec<&SessionInfo> = self
            .sessions
            .iter()
            .filter(|s| s.id != self.session_id)
            .filter(|s| self.resume_all || s.cwd.is_empty() || s.cwd == cwd)
            .filter(|s| q.is_empty() || s.title.to_lowercase().contains(&q))
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.updated));
        v
    }

    fn dialog_key(&mut self, key: KeyEvent) {
        let Some(dialog) = self.dialog.clone() else {
            return;
        };
        if let (Some(mut note), Dialog::Permission { req, sel }) = (self.amend.clone(), &dialog) {
            match key.code {
                KeyCode::Esc => self.amend = None,
                KeyCode::Enter => {
                    let choice = permission_options(req)[*sel].1;
                    self.decide(req.clone(), choice);
                }
                KeyCode::Backspace => {
                    note.pop();
                    self.amend = Some(note);
                }
                KeyCode::Char(c) => {
                    note.push(c);
                    self.amend = Some(note);
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Esc {
            return self.close_dialog();
        }
        match dialog {
            Dialog::Help { tab, sel, focus } => {
                let n = crate::ui::dialogs::sorted_commands(self)
                    .iter()
                    .filter(|c| self.custom.contains(&c.name) == (tab == 2))
                    .count();
                let (tab, sel, focus) = match key.code {
                    KeyCode::Tab | KeyCode::Right => ((tab + 1) % 3, 0, false),
                    KeyCode::BackTab | KeyCode::Left => ((tab + 2) % 3, 0, false),
                    KeyCode::Down if tab > 0 && n > 0 && !focus => (tab, sel, true),
                    KeyCode::Down if tab > 0 && n > 0 => (tab, (sel + 1) % n, true),
                    KeyCode::Up if tab > 0 && focus && sel == 0 => (tab, 0, false),
                    KeyCode::Up if tab > 0 && focus => (tab, sel - 1, true),
                    _ => (tab, sel, focus),
                };
                self.dialog = Some(Dialog::Help { tab, sel, focus });
            }
            Dialog::Model { sel } => {
                let n = self.cfg.models.len().max(1);
                match key.code {
                    KeyCode::Down => self.dialog = Some(Dialog::Model { sel: (sel + 1) % n }),
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::Model {
                            sel: (sel + n - 1) % n,
                        })
                    }
                    KeyCode::Left => self.step_effort(-1),
                    KeyCode::Right => self.step_effort(1),
                    KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => {
                        self.pick_model(c as usize - '1' as usize)
                    }
                    KeyCode::Enter => self.pick_model(sel),
                    _ => {}
                }
            }
            Dialog::Mode { sel } => {
                let n = self.cfg.modes.len().max(1);
                match key.code {
                    KeyCode::Down => self.dialog = Some(Dialog::Mode { sel: (sel + 1) % n }),
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::Mode {
                            sel: (sel + n - 1) % n,
                        })
                    }
                    KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => {
                        self.pick_mode(c as usize - '1' as usize)
                    }
                    KeyCode::Enter => self.pick_mode(sel),
                    _ => {}
                }
            }
            Dialog::Effort { sel } => {
                let n = self.cfg.efforts.len().max(1);
                match key.code {
                    KeyCode::Down => self.dialog = Some(Dialog::Effort { sel: (sel + 1) % n }),
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::Effort {
                            sel: (sel + n - 1) % n,
                        })
                    }
                    KeyCode::Enter => {
                        if let Some(e) = self.cfg.efforts.get(sel).cloned() {
                            self.dialog = None;
                            self.blocks.push(Block::User {
                                text: "/effort ".into(),
                                bash: false,
                            });
                            self.set_effort(e);
                        }
                    }
                    _ => {}
                }
            }
            Dialog::Status => {
                if key.code == KeyCode::Enter {
                    self.dialog = None;
                }
            }
            Dialog::Resume { sel, mut query } => {
                let ids: Vec<String> = self
                    .resume_list(&query)
                    .iter()
                    .map(|s| s.id.clone())
                    .collect();
                let n = ids.len().max(1);
                let mut sel = sel.min(n - 1);
                match key.code {
                    KeyCode::Down => sel = (sel + 1) % n,
                    KeyCode::Up => sel = (sel + n - 1) % n,
                    KeyCode::Backspace => {
                        query.pop();
                        sel = 0;
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.resume_all = !self.resume_all;
                        sel = 0;
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        sel = 0;
                    }
                    KeyCode::Enter => {
                        if let Some(id) = ids.get(sel) {
                            self.dialog = None;
                            self.blocks.clear();
                            self.from_queue.clear();
                            self.load(Some(id.clone()));
                        }
                        return;
                    }
                    _ => {}
                }
                self.dialog = Some(Dialog::Resume { sel, query });
            }
            Dialog::Permission { req, sel } => {
                let options = permission_options(&req);
                let n = options.len();
                match key.code {
                    KeyCode::Down => {
                        self.dialog = Some(Dialog::Permission {
                            req,
                            sel: (sel + 1) % n,
                        })
                    }
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::Permission {
                            req,
                            sel: (sel + n - 1) % n,
                        })
                    }
                    KeyCode::Char(c @ '1'..='3') if (c as usize - '1' as usize) < n => {
                        self.decide(req, options[c as usize - '1' as usize].1)
                    }
                    KeyCode::Enter => self.decide(req, options[sel].1),
                    KeyCode::Tab => self.amend = Some(String::new()),
                    _ => {}
                }
            }
            Dialog::Rewind { sel } => {
                let n = self.rewind_targets().len() + 1;
                let at = sel.min(n - 1);
                match key.code {
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::Rewind {
                            sel: at.saturating_sub(1),
                        })
                    }
                    KeyCode::Down => {
                        self.dialog = Some(Dialog::Rewind {
                            sel: (at + 1).min(n - 1),
                        })
                    }
                    KeyCode::Enter if at + 1 < n => {
                        self.dialog = Some(Dialog::RewindConfirm { nth: at, sel: 0 });
                    }
                    KeyCode::Enter => self.dialog = None,
                    _ => {}
                }
            }
            Dialog::Btw {
                question,
                answer,
                scroll,
            } => {
                let scroll = match key.code {
                    KeyCode::Up => scroll.saturating_sub(1),
                    KeyCode::Down => scroll + 1,
                    _ => scroll,
                };
                self.dialog = Some(Dialog::Btw {
                    question,
                    answer,
                    scroll,
                });
            }
            Dialog::RewindConfirm { nth, sel } => {
                let n = REWIND_CHOICES.len();
                match key.code {
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::RewindConfirm {
                            nth,
                            sel: sel.saturating_sub(1),
                        })
                    }
                    KeyCode::Down => {
                        self.dialog = Some(Dialog::RewindConfirm {
                            nth,
                            sel: (sel + 1).min(n - 1),
                        })
                    }
                    KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => {
                        self.rewind_choice(nth, c as usize - '1' as usize)
                    }
                    KeyCode::Enter => self.rewind_choice(nth, sel),
                    _ => {}
                }
            }
            Dialog::Search { mut query, sel } => {
                let n = self.search_hits(&query).len();
                match key.code {
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::Search {
                            query,
                            sel: (sel + 1).min(n.saturating_sub(1)),
                        })
                    }
                    KeyCode::Down => {
                        self.dialog = Some(Dialog::Search {
                            query,
                            sel: sel.saturating_sub(1),
                        })
                    }
                    KeyCode::Enter => {
                        let hit = self
                            .search_hits(&query)
                            .get(sel)
                            .map(|&i| self.prompts[i].clone());
                        self.dialog = None;
                        if let Some(text) = hit {
                            self.editor.set_text(&text);
                            self.refresh_menu();
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.dialog = Some(Dialog::Search { query, sel: 0 });
                    }
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.search_here = !self.search_here;
                        self.dialog = Some(Dialog::Search { query, sel: 0 });
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        self.dialog = Some(Dialog::Search { query, sel: 0 });
                    }
                    _ => {}
                }
            }
        }
    }

    /// Earlier prompts matching `query`, newest first, as indexes into `prompts`.
    pub fn search_hits(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        let here = self.opts.cwd.to_string_lossy();
        (0..self.prompts.len())
            .rev()
            .filter(|&i| self.prompts[i].to_lowercase().contains(&q))
            .filter(|&i| !self.search_here || self.prompt_dirs.get(i).is_some_and(|d| *d == here))
            .collect()
    }

    /// The prompts the rewind picker offers, oldest first: what was typed, not commands.
    pub fn rewind_targets(&self) -> Vec<String> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::User { text, bash: false } if !text.starts_with('/') => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The files the `nth` rewind prompt's turn changed: name, lines added, lines removed.
    pub fn changes_after(&self, nth: usize) -> Vec<(String, usize, usize)> {
        let mut seen = 0;
        let mut inside = false;
        let mut out: Vec<(String, usize, usize)> = Vec::new();
        for b in &self.blocks {
            match b {
                Block::User { text, bash: false } if !text.starts_with('/') => {
                    if inside {
                        break;
                    }
                    inside = seen == nth;
                    seen += 1;
                }
                Block::Tool(t) if inside => {
                    let Some(d) = &t.diff else { continue };
                    let old = d.old.clone().unwrap_or_default();
                    let diff = similar::TextDiff::from_lines(&old, &d.new);
                    let (mut a, mut r) = (0, 0);
                    for ch in diff.iter_all_changes() {
                        match ch.tag() {
                            similar::ChangeTag::Insert => a += 1,
                            similar::ChangeTag::Delete => r += 1,
                            similar::ChangeTag::Equal => {}
                        }
                    }
                    let name = d.path.rsplit('/').next().unwrap_or(&d.path).to_string();
                    match out.iter_mut().find(|(n, _, _)| *n == name) {
                        Some(e) => {
                            e.1 += a;
                            e.2 += r;
                        }
                        None => out.push((name, a, r)),
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Ask wizard to go back to before the `nth` prompt the picker lists. Wizard numbers its
    /// turns in the session file; the prompt is found there by its first line.
    fn start_rewind(&mut self, nth: usize) {
        let targets = self.rewind_targets();
        let Some(text) = targets.get(nth).cloned() else {
            return;
        };
        let markers = std::env::var_os("HOME")
            .map(|h| {
                rewind_markers(
                    &PathBuf::from(h)
                        .join(".wizard/sessions")
                        .join(format!("{}.jsonl", self.session_id)),
                )
            })
            .unwrap_or_default();
        let first = |s: &str| s.lines().next().unwrap_or("").trim().to_string();
        let earlier = targets[..nth]
            .iter()
            .filter(|t| first(t) == first(&text))
            .count();
        let turn = markers
            .iter()
            .filter(|(_, p)| first(p) == first(&text))
            .nth(earlier)
            .map(|(t, _)| *t);
        match turn {
            Some(turn) => {
                self.rewinding = Some(text);
                self.rewind_answer = None;
                self.start_busy();
                self.send(Request::Prompt(format!("/rewind {turn}")));
            }
            None => self.blocks.push(Block::Notice {
                level: NoticeLevel::Error,
                text: "Cannot rewind: wizard has no checkpoint for that prompt".into(),
            }),
        }
    }

    fn finish_rewind(&mut self, text: String) {
        self.busy = None;
        let answer = self.rewind_answer.take().unwrap_or_default();
        if answer.starts_with("rewound to before turn") {
            self.blocks.clear();
            self.from_queue.clear();
            self.load(Some(self.session_id.clone()));
            // the rewound prompt comes back to the input, unless something is typed there now
            if self.editor.is_empty() {
                self.editor.set_text(&text);
            }
        } else {
            let why = if answer.is_empty() {
                "wizard did not answer".to_string()
            } else {
                answer
            };
            self.blocks.push(Block::Notice {
                level: NoticeLevel::Error,
                text: format!("Rewind failed: {why}"),
            });
        }
    }

    fn decide(&mut self, req: PermissionRequest, choice: Choice) {
        self.dialog = None;
        // an approved plan ends plan mode in wizard
        if is_plan(&req) && choice != Choice::No {
            self.plan = false;
        }
        let note = self.amend.take().unwrap_or_default();
        self.send(Request::Decide {
            id: req.id,
            allow: choice != Choice::No,
            scope: if choice == Choice::Always {
                DecideScope::Always
            } else {
                DecideScope::Once
            },
            note,
        });
    }

    fn show_effort_notice(&mut self) {
        self.notice = self.effort_notice();
        self.notice_until = Some(self.clock.elapsed() + NOTICE_TTL);
    }

    fn pick_mode(&mut self, i: usize) {
        let Some(m) = self.cfg.modes.get(i).cloned() else {
            return;
        };
        self.dialog = None;
        self.blocks.push(Block::User {
            text: "/mode ".into(),
            bash: false,
        });
        self.pick_mode_named(m);
    }

    /// Through the same steps as shift+tab, so a prompt sent next waits for it. Plan mode stays
    /// on unless the mode is chat, which has no project tools to plan with.
    fn pick_mode_named(&mut self, m: String) {
        let (mode, plan) = self.shown_mode();
        if m == mode {
            self.output(format!("Kept mode as {m}"), String::new());
            return;
        }
        let plan = plan && m != "chat";
        self.output(format!("Set mode to {m}"), String::new());
        self.go_to((m, plan));
    }

    /// Where `m`, an id or a name, is in the model list.
    fn model_index(&self, m: &str) -> Option<usize> {
        let id = self.model_id(m);
        self.cfg.models.iter().position(|x| x.id == id)
    }

    fn pick_model(&mut self, i: usize) {
        self.apply_model(i, "/model ".into());
    }

    /// Effort for this session, through the same queue as a mode change, so a refusal while
    /// wizard is busy is retried and a prompt sent next waits for it.
    fn set_effort(&mut self, e: String) {
        if e != self.cfg.effort {
            self.mode_steps.push_back(ModeStep::Effort(e.clone()));
            self.cfg.effort = e.clone();
            self.pump_modes();
        }
        self.show_effort_notice();
        self.output(
            format!("Set effort level to {e} for this session only"),
            String::new(),
        );
    }

    /// The model picked at `i` (and the effort the picker shows), answered as the picker does;
    /// `typed` is the line echoed above the answer.
    fn apply_model(&mut self, i: usize, typed: String) {
        let Some(m) = self.cfg.models.get(i).cloned() else {
            return;
        };
        self.dialog = None;
        self.show_effort_notice();
        let effort = self.effort_preview.take().filter(|e| *e != self.cfg.effort);
        self.blocks.push(Block::User {
            text: typed,
            bash: false,
        });
        if m.id == self.cfg.model && effort.is_none() {
            self.output(format!("Kept model as {}", m.name), m.name);
            return;
        }
        // queued like a mode change: retried while wizard is busy, and a prompt waits for it
        if m.id != self.cfg.model {
            self.mode_steps.push_back(ModeStep::Model(m.id.clone()));
            self.cfg.model = m.id;
        }
        let mut text = format!("Set model to {} for this session only", m.name);
        if let Some(e) = effort {
            self.mode_steps.push_back(ModeStep::Effort(e.clone()));
            self.cfg.effort = e.clone();
            self.show_effort_notice();
            text.push_str(&format!(" with {e} effort"));
        }
        self.output(text, m.name);
        self.pump_modes();
    }

    /// ←/→ in the model picker: only what the picker shows changes until a pick is confirmed.
    fn step_effort(&mut self, step: isize) {
        let n = self.cfg.efforts.len() as isize;
        if n == 0 {
            return;
        }
        let current = self
            .effort_preview
            .clone()
            .unwrap_or_else(|| self.cfg.effort.clone());
        let at = self
            .cfg
            .efforts
            .iter()
            .position(|e| *e == current)
            .unwrap_or(0) as isize;
        self.effort_preview = Some(self.cfg.efforts[(at + step).clamp(0, n - 1) as usize].clone());
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_by(3),
            MouseEventKind::ScrollDown => self.scroll_by(-3),
            _ => return,
        }
        self.dirty = true;
    }

    pub fn on_paste(&mut self, s: &str) {
        if self.dialog.is_some() || self.detail {
            return;
        }
        self.editor.paste(s);
        self.shortcuts = false;
        self.refresh_menu();
        self.dirty = true;
    }

    pub fn on_resize(&mut self, w: u16, h: u16) {
        self.size = (w, h);
        self.scroll = 0;
        self.dirty = true;
    }

    /// Expire the footer hint and advance the spinner. Returns nothing; sets `dirty`.
    pub fn on_tick(&mut self) {
        if self.mode_retry.is_some_and(|at| self.clock.elapsed() >= at) {
            self.mode_retry = None;
            self.pump_modes();
        }
        if !self.fixed_now {
            self.now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(self.now_unix, |d| d.as_secs() as i64);
        }
        let now = self.clock.elapsed();
        if self.notice_until.is_some_and(|until| now >= until) {
            self.notice = None;
            self.notice_until = None;
            self.dirty = true;
        }
        if let Some((_, at)) = self.hint {
            if now.saturating_sub(at) >= HINT_TTL {
                self.hint = None;
                self.dirty = true;
            }
        }
        if self.busy.is_some() && self.dialog.is_none() {
            self.dirty = true;
        }
    }

    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        if self.busy.is_some() && self.dialog.is_none() {
            return Some(now + FRAME);
        }
        let elapsed = self.clock.elapsed();
        let hint = self.hint.map(|(_, at)| at + HINT_TTL);
        [hint, self.notice_until, self.mode_retry]
            .into_iter()
            .flatten()
            .min()
            .map(|at| now + at.saturating_sub(elapsed).max(Duration::from_millis(10)))
    }

    /// What is printed to the normal screen after the UI is gone.
    pub fn epilogue(&self) -> String {
        if self.blocks.is_empty() || self.session_id.is_empty() || self.switched {
            return "\n".to_string();
        }
        // wizard can only reopen the latest session, so it is named as that
        let lead = if self.started_by_wizard() {
            "Resume the most recent session with:"
        } else {
            "Resume this session with:"
        };
        format!(
            "\n\x1b[2m{lead}\x1b[0m\n\x1b[2m{}\x1b[0m\n",
            self.resume_command()
        )
    }

    fn started_by_wizard(&self) -> bool {
        std::env::var_os("CLAUDEW_WIZARD_BIN").is_some_and(|b| !b.is_empty())
    }

    /// What the user types to come back. Started by `wizard` (it sets CLAUDEW_WIZARD_BIN),
    /// that is `wizard --resume`, which opens the session just closed, the latest in this
    /// directory. Run on its own, it is this binary with the id.
    pub fn resume_command(&self) -> String {
        if self.started_by_wizard() {
            "wizard --resume".to_string()
        } else {
            format!("wizard-ui-claude --resume {}", self.session_id)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Yes,
    Always,
    No,
}

/// The numbered answers a permission prompt offers.
/// Wizard's one question: a finished plan from `exit_plan`.
pub fn is_plan(req: &PermissionRequest) -> bool {
    req.input.get("plan").is_some_and(|p| p.is_string())
}

pub fn permission_options(req: &PermissionRequest) -> Vec<(String, Choice)> {
    if is_plan(req) {
        return vec![
            ("Yes, proceed".to_string(), Choice::Yes),
            ("No, keep planning".to_string(), Choice::No),
        ];
    }
    let mut v = vec![("Yes".to_string(), Choice::Yes)];
    if !req.rule.is_empty() {
        let what = match req.kind {
            ToolKind::Edit => "Yes, allow all edits during this session".to_string(),
            _ => format!("Yes, and don\u{2019}t ask again for: {}", req.rule),
        };
        v.push((what, Choice::Always));
    }
    v.push(("No".to_string(), Choice::No));
    v
}

/// An edit's diff with the whole file around it. Wizard sends only the replaced snippet, so
/// it is found in the file: the new text once the edit is made, the old text before. With the
/// file on both sides the diff shows real line numbers and the lines around the change.
pub fn place_diff(cwd: &Path, d: agent_core::FileDiff) -> agent_core::FileDiff {
    let Some(old) = d.old.clone().filter(|o| !o.is_empty() && !d.new.is_empty()) else {
        return d;
    };
    let path = if Path::new(&d.path).is_absolute() {
        PathBuf::from(&d.path)
    } else {
        cwd.join(&d.path)
    };
    let Ok(file) = std::fs::read_to_string(&path) else {
        return d;
    };
    if file == d.new || file == old {
        return d;
    }
    let (before, after) = if file.contains(&d.new) && !file.contains(&old) {
        (file.replacen(&d.new, &old, 1), file)
    } else if file.contains(&old) {
        let after = file.replacen(&old, &d.new, 1);
        (file, after)
    } else {
        return d;
    };
    agent_core::FileDiff {
        path: d.path,
        old: Some(before),
        new: after,
    }
}

/// `(turn, prompt)` pairs from a wizard session file, for `/rewind <turn>`.
pub fn rewind_markers(path: &Path) -> Vec<(u64, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            Some((
                v.get("turn")?.as_u64()?,
                v.get("prompt")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Files in `dir` ending in `.{ext}`, by name: (stem, contents).
fn read_dir_sorted(dir: &Path, ext: &str) -> Vec<(String, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(String, String)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != ext {
                return None;
            }
            let stem = p.file_stem()?.to_string_lossy().into_owned();
            Some((stem, std::fs::read_to_string(&p).ok()?))
        })
        .collect();
    v.sort();
    v
}

/// `key: value` from a markdown file's front matter.
fn front_matter(text: &str, key: &str) -> Option<String> {
    let body = text.strip_prefix("---")?;
    let end = body.find("\n---")?;
    body[..end].lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
    })
}

/// A top-level `key = "value"` from a TOML file.
fn toml_str(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        let v = v.trim();
        (k.trim() == key && v.starts_with('"')).then(|| v.trim_matches('"').to_string())
    })
}

/// `◐ medium · /effort`, the notice over the prompt; none for wizard's unnamed default.
pub fn effort_notice_text(effort: &str) -> Option<String> {
    if matches!(effort, "" | "default") {
        return None;
    }
    Some(format!("{} {effort} · /effort", effort_glyph(effort)))
}

pub fn effort_glyph(effort: &str) -> &'static str {
    match effort {
        "low" | "minimal" | "none" => "○",
        "high" => "●",
        "xhigh" | "max" => "◉",
        _ => "◐",
    }
}

/// Commands for what is typed after the slash: names that start with it, then names that
/// contain it, then descriptions that do.
pub fn slash_matches(commands: &[SlashCommand], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..commands.len()).collect();
    }
    let q = query.to_lowercase();
    let mut ranked: Vec<(u8, usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let name = c.name.to_lowercase();
            if name == q {
                Some((0, i))
            } else if name.starts_with(&q) {
                Some((1, i))
            } else if name.contains(&q) {
                Some((2, i))
            } else if c.description.to_lowercase().contains(&q) {
                Some((3, i))
            } else {
                None
            }
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|(_, i)| i).collect()
}

/// Paths for an `@` query: the top level when nothing is typed, otherwise every path that
/// matches, best first.
/// A mention entry that names a subagent: the marker, then name and description joined by a
/// tab.
pub const AGENT: char = '\u{1}';

/// Paths and subagents for an `@` query: the top level and then the agents when nothing is
/// typed, otherwise directories that start with it, up to two agents that match, then the
/// other paths.
pub fn mention_matches(files: &[String], agents: &[(String, String)], query: &str) -> Vec<String> {
    const MAX: usize = 15;
    let agent = |(n, d): &(String, String)| format!("{AGENT}{n}\t{d}");
    if query.is_empty() {
        let mut top: Vec<&String> = files
            .iter()
            .filter(|f| !f.trim_end_matches('/').contains('/'))
            .collect();
        // hidden entries after the rest
        top.sort_by_key(|f| (f.starts_with('.'), !f.ends_with('/'), f.to_lowercase()));
        let mut out: Vec<String> = top.into_iter().cloned().collect();
        out.extend(agents.iter().map(agent));
        out.truncate(MAX);
        return out;
    }
    let q = query.to_lowercase();
    let mut ranked: Vec<(u8, usize, String)> = files
        .iter()
        .filter_map(|f| {
            let low = f.to_lowercase();
            let base = low.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            let rank = if low.starts_with(&q) && f.ends_with('/') {
                0
            } else if low.starts_with(&q) {
                3
            } else if base.starts_with(&q) {
                4
            } else if low.contains(&q) {
                5
            } else {
                return None;
            };
            Some((rank, f.len(), f.clone()))
        })
        .collect();
    // at most two agents between the paths that start with the query and the rest
    let mut found: Vec<(u8, String)> = agents
        .iter()
        .filter_map(|a| {
            let low = a.0.to_lowercase();
            let rank = if low.starts_with(&q) {
                1
            } else if low.contains(&q) {
                2
            } else {
                return None;
            };
            Some((rank, agent(a)))
        })
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    ranked.extend(found.into_iter().take(2).map(|(rank, a)| (rank, 0, a)));
    ranked.sort_by_key(|x| (x.0, x.1));
    ranked.into_iter().take(MAX).map(|(_, _, f)| f).collect()
}

/// Files and directories under `cwd`, relative, directories with a trailing slash. Hidden
/// entries are listed but not walked into, as the reference lists `.wizard/`; `.git` and
/// build output are left out.
pub fn list_files(cwd: &Path) -> Vec<String> {
    const LIMIT: usize = 5000;
    let mut out = Vec::new();
    let mut stack = vec![cwd.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" || name == "target" || name == "node_modules" {
                continue;
            }
            let hidden = name.starts_with('.');
            let path = e.path();
            let Ok(rel) = path.strip_prefix(cwd) else {
                continue;
            };
            let rel = rel.to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                out.push(format!("{rel}/"));
                if !hidden {
                    stack.push(path);
                }
            } else {
                out.push(rel);
            }
            if out.len() >= LIMIT {
                return out;
            }
        }
    }
    out
}

/// The editor `v` and ctrl+g open, by its short name.
pub fn editor_name() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .and_then(|v| {
            let cmd = v.split_whitespace().next()?.to_string();
            Some(cmd.rsplit('/').next().unwrap_or(&cmd).to_string())
        })
        .unwrap_or_else(|| "nano".to_string())
}

/// The editor as Claude Code names it in its hints: `Nvim` for nvim, `nano` as it is.
pub fn editor_display() -> String {
    let name = editor_name();
    match name.as_str() {
        "nvim" => "Nvim".into(),
        "vim" => "Vim".into(),
        "emacs" | "emacsclient" => "Emacs".into(),
        "code" => "VS Code".into(),
        "hx" => "Helix".into(),
        _ => name,
    }
}

fn local_hm() -> (u32, u32) {
    // SAFETY: `time` and `localtime_r` only write to the values passed in.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour as u32, tm.tm_min as u32)
    }
}

/// `9:31 PM`; `pad` gives `09:31 PM` as the detailed transcript prints it.
pub fn clock_text((h, m): (u32, u32), pad: bool) -> String {
    let h12 = match h % 12 {
        0 => 12,
        x => x,
    };
    let ap = if h < 12 { "AM" } else { "PM" };
    if pad {
        format!("{h12:02}:{m:02} {ap}")
    } else {
        format!("{h12}:{m:02} {ap}")
    }
}

/// Start the chosen backend and forward its events into `msg_tx`. Returns the request sender
/// and the forwarding task, which ends when the backend does.
pub fn spawn_backend(
    opts: &AppOpts,
    msg_tx: UnboundedSender<Msg>,
) -> Result<(UnboundedSender<Request>, tokio::task::JoinHandle<()>)> {
    let cwd = opts.cwd.clone();
    let resume = opts.resume.clone();
    let BackendHandle { tx, mut rx } = match &opts.backend {
        BackendKind::Mock => agent_core::mock::MockBackend::spawn(cwd, resume)?,
        BackendKind::Replay(path) => crate::replay::spawn(path)?,
        BackendKind::Wizard => backend_wizard::WizardBackend::spawn_with(cwd, resume, true, true)?,
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

fn draw_frame(terminal: &mut Terminal, app: &mut App) -> Result<()> {
    let before = app.link_spots.clone();
    let frame = |terminal: &mut Terminal, app: &mut App| -> std::io::Result<()> {
        terminal.draw(|f| {
            let area = f.area();
            app.size = (area.width, area.height);
            crate::ui::draw(f.buffer_mut(), app);
            if let Some(c) = app.cursor {
                f.set_cursor_position(c);
            }
        })?;
        Ok(())
    };
    frame(terminal, app)?;
    // a link drawn last frame and gone now leaves its attribute on cells the diff does not
    // rewrite: the whole screen goes again
    if before.iter().any(|s| !app.link_spots.contains(s)) {
        terminal.clear()?;
        frame(terminal, app)?;
    }
    if !app.link_spots.is_empty() {
        use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
        use crossterm::style::{Print, ResetColor, SetForegroundColor};
        let mut out = std::io::stdout();
        let _ = crossterm::queue!(out, SavePosition);
        for (x, y, text, url, color) in &app.link_spots {
            let _ = crossterm::queue!(out, MoveTo(*x, *y), Print(format!("\x1b]8;;{url}\x1b\\")));
            if let ratatui::style::Color::Indexed(n) = color {
                let _ = crossterm::queue!(
                    out,
                    SetForegroundColor(crossterm::style::Color::AnsiValue(*n))
                );
            }
            let _ = crossterm::queue!(out, Print(text), ResetColor, Print("\x1b]8;;\x1b\\"));
        }
        let _ = crossterm::queue!(out, RestorePosition);
        let _ = std::io::Write::flush(&mut out);
    }
    Ok(())
}

/// Run until the app quits or the terminal closes.
pub async fn run(
    app: &mut App,
    terminal: &mut Terminal,
    guard: &mut TermGuard,
    msg_rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    use futures_util::FutureExt;
    let input = tuikit::input::Input::spawn()?;
    let input_ctl = input.control();
    let mut pump = EventPump::<Msg>::spawn_raw(input, Duration::ZERO, msg_rx);
    loop {
        if app.repaint {
            app.repaint = false;
            terminal.clear()?;
            app.dirty = true;
        }
        if app.dirty {
            app.dirty = false;
            draw_frame(terminal, app)?;
        }
        if app.quit {
            break;
        }
        let first = match app.next_wake(Instant::now()) {
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
        if std::mem::take(&mut app.suspend) {
            guard.suspend();
            // SAFETY: raising a signal on our own process.
            unsafe {
                libc::raise(libc::SIGTSTP);
            }
            let _ = guard.resume();
            app.repaint = true;
        }
        if std::mem::take(&mut app.view_request) {
            let w = app.size.0 as usize;
            let text: Vec<String> = crate::ui::transcript::rows(app, w, true)
                .iter()
                .map(|r| r.text().trim_end().to_string())
                .collect();
            let cwd = app.opts.cwd.clone();
            let _ = tuikit::input::external_editor(
                guard,
                &input_ctl,
                &cwd,
                "claudew-transcript",
                &text.join("\n"),
                Some("nano"),
            );
            app.repaint = true;
        }
        if std::mem::take(&mut app.edit_request) {
            let text = app.editor.text().to_string();
            let cwd = app.opts.cwd.clone();
            let out = tuikit::input::external_editor(
                guard,
                &input_ctl,
                &cwd,
                "claudew",
                &text,
                Some("nano"),
            );
            match out {
                tuikit::input::EditorOutcome::Edited(t) => {
                    app.editor.set_text(t.trim_end_matches('\n'));
                    app.refresh_menu();
                }
                tuikit::input::EditorOutcome::NoEditor => {
                    app.notice = Some("set $EDITOR to edit the prompt in it".into())
                }
                tuikit::input::EditorOutcome::Failed(e) => app.notice = Some(e),
            }
            app.repaint = true;
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
        TermEvent::FocusGained => app.repaint = true,
        TermEvent::FocusLost | TermEvent::ColorScheme(_) | TermEvent::Tick => {}
        TermEvent::Msg(m) => app.update(m),
    }
    app.on_tick();
}
