//! TUI state machine: application state, slash commands, and the main loop.
//! Rendering lives in [`crate::ui`]; raw events in [`crate::event`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentEvent, DoneReason};
use crate::cli::Cli;
use crate::commands::CustomCommand;
use crate::config::{Config, Mode};
use crate::event::{Event, EventLoop};

/// One rendered entry in the chat transcript.
#[derive(Debug)]
pub enum TranscriptEntry {
    User(String),
    Assistant(String),
    /// Model reasoning ("thinking") that preceded an assistant reply,
    /// rendered dimmed.
    Thinking(String),
    /// Collapsible tool invocation card.
    ToolCard {
        name: String,
        args: Value,
        /// `None` while the tool is still running.
        output: Option<String>,
        is_error: bool,
        collapsed: bool,
    },
    /// System notice (mode switch, errors).
    Notice(String),
}

/// What the input line is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Composing a chat message.
    #[default]
    Chat,
    /// Composing a `/slash` command.
    Command,
}

/// Parsed `/slash` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    /// Clear the conversation (respawns a fresh bridge).
    Clear,
    /// `/model [tag]` — show current model, or switch to `tag`.
    Model(Option<String>),
    /// `/mode [genie|sovereign]` — show the picker, or switch mode.
    Mode(Option<Mode>),
    /// `/evolve [start|status]` — drive AHE's harness-evolution loop.
    Evolve(EvolveSlash),
    Quit,
}

/// What a `/evolve` invocation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvolveSlash {
    /// Preflight and launch the evolve loop (default, also `/evolve start`).
    Start,
    /// Summarize the latest experiment's progress.
    Status,
}

impl SlashCommand {
    /// Parse a `/...` input line. `None` when `input` is not a slash
    /// command; `Some(Err(msg))` for an unknown command or bad arguments.
    pub fn parse(input: &str) -> Option<Result<Self, String>> {
        let input = input.trim();
        let rest = input.strip_prefix('/')?;
        let mut parts = rest.split_whitespace();
        let command = parts.next().unwrap_or("");
        let args: Vec<&str> = parts.collect();

        let parsed = match command {
            "help" => Ok(Self::Help),
            "clear" => Ok(Self::Clear),
            "model" => Ok(Self::Model(args.first().map(|s| s.to_string()))),
            "mode" => match args.first() {
                None => Ok(Self::Mode(None)),
                Some(&"genie") => Ok(Self::Mode(Some(Mode::Genie))),
                Some(&"sovereign") => Ok(Self::Mode(Some(Mode::Sovereign))),
                Some(other) => Err(format!("unknown mode '{other}' (genie|sovereign)")),
            },
            "genie" => Ok(Self::Mode(Some(Mode::Genie))),
            "sovereign" => Ok(Self::Mode(Some(Mode::Sovereign))),
            "evolve" => match args.first() {
                None | Some(&"start") => Ok(Self::Evolve(EvolveSlash::Start)),
                Some(&"status") => Ok(Self::Evolve(EvolveSlash::Status)),
                Some(other) => Err(format!("unknown evolve action '{other}' (start|status)")),
            },
            "quit" | "q" | "exit" => Ok(Self::Quit),
            other => Err(format!("unknown command '/{other}' — try /help")),
        };
        Some(parsed)
    }
}

/// One entry in the slash-command completion table. Drives the suggestion
/// popup and the inline ghost-text prediction.
#[derive(Debug)]
pub struct CommandSpec {
    pub name: &'static str,
    /// Argument hint shown after the name (e.g. `[tag]`).
    pub args: &'static str,
    pub description: &'static str,
    /// Completion appends a trailing space and waits for arguments instead
    /// of submitting immediately.
    pub takes_args: bool,
}

/// All slash commands, in display order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "model",
        args: "[tag]",
        description: "show or switch the model",
        takes_args: false,
    },
    CommandSpec {
        name: "mode",
        args: "[genie|sovereign]",
        description: "pick or switch personality mode",
        takes_args: false,
    },
    CommandSpec {
        name: "genie",
        args: "",
        description: "switch to genie mode",
        takes_args: false,
    },
    CommandSpec {
        name: "sovereign",
        args: "",
        description: "switch to sovereign mode",
        takes_args: false,
    },
    CommandSpec {
        name: "evolve",
        args: "[start|status]",
        description: "drive AHE's harness-evolution loop",
        takes_args: false,
    },
    CommandSpec {
        name: "clear",
        args: "",
        description: "clear the conversation",
        takes_args: false,
    },
    CommandSpec {
        name: "help",
        args: "",
        description: "show available commands and keys",
        takes_args: false,
    },
    CommandSpec {
        name: "quit",
        args: "",
        description: "exit wizard",
        takes_args: false,
    },
];

/// One row in the suggestion popup: a builtin [`CommandSpec`] or a custom
/// command loaded from `~/.wizard/commands/` / `<project>/.wizard/commands/`.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub name: String,
    /// Argument hint shown after the name.
    pub args: String,
    pub description: String,
    /// Completion appends a trailing space and waits for arguments instead
    /// of submitting immediately.
    pub takes_args: bool,
}

impl From<&CommandSpec> for Suggestion {
    fn from(spec: &CommandSpec) -> Self {
        Self {
            name: spec.name.to_string(),
            args: spec.args.to_string(),
            description: spec.description.to_string(),
            takes_args: spec.takes_args,
        }
    }
}

impl From<&CustomCommand> for Suggestion {
    fn from(command: &CustomCommand) -> Self {
        let takes_args = command.expects_args();
        Self {
            name: command.name.clone(),
            args: if takes_args {
                "[args]".to_string()
            } else {
                String::new()
            },
            description: command
                .description
                .clone()
                .unwrap_or_else(|| "custom command".to_string()),
            takes_args,
        }
    }
}

/// True when `name` is a builtin command word ([`COMMANDS`] plus the parse
/// aliases that have no table entry). Unknown words fall through to the
/// model as a normal prompt.
fn is_builtin_command(name: &str) -> bool {
    COMMANDS.iter().any(|spec| spec.name == name) || matches!(name, "q" | "exit")
}

/// What an open [`Picker`] selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Mode,
}

/// One selectable row in a picker popup.
#[derive(Debug)]
pub struct PickerItem {
    /// Value applied on selection (the mode name).
    pub value: String,
    /// Secondary text shown dimmed next to the value.
    pub detail: String,
    /// Marks the currently active item.
    pub current: bool,
}

/// An interactive selection popup (↑/↓ to move, Enter to select, Esc to
/// cancel).
#[derive(Debug)]
pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub items: Vec<PickerItem>,
    pub selected: usize,
}

/// Status bar contents.
#[derive(Debug, Default)]
pub struct StatusLine {
    pub model: String,
    pub mode: Mode,
    /// Current step within the running turn (0 when idle).
    pub step: u32,
    /// True while a turn is streaming.
    pub busy: bool,
}

/// Full TUI state. [`crate::ui::draw`] renders it; [`App::handle_event`]
/// mutates it.
#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub mode: Mode,
    pub input: String,
    /// Cursor position in `input`, in characters.
    pub cursor: usize,
    pub input_mode: InputMode,
    pub transcript: Vec<TranscriptEntry>,
    /// Partial assistant text of the in-flight turn (moved into the
    /// transcript when the turn ends).
    pub streaming: String,
    /// Partial model reasoning of the in-flight turn, rendered dimmed and
    /// flushed to the transcript alongside `streaming`.
    pub streaming_thinking: String,
    pub status: StatusLine,
    /// Transcript scroll offset from the bottom (0 = pinned to latest).
    pub scroll: u16,
    pub should_quit: bool,
    /// Tick counter driving the busy spinner.
    pub tick: u64,
    /// Matching commands (builtin [`COMMANDS`] plus custom commands) for the
    /// current `/input`, shown as the suggestion popup.
    pub suggestions: Vec<Suggestion>,
    /// Highlighted row in `suggestions`.
    pub suggestion_index: usize,
    /// Custom commands loaded from `~/.wizard/commands/` and
    /// `<project>/.wizard/commands/` (set by `run_tui`).
    pub custom_commands: Vec<CustomCommand>,
    /// Project root `@file` references resolve against.
    pub project_root: PathBuf,
    /// Open selection popup (mode picker), if any.
    pub picker: Option<Picker>,
    /// Previously submitted inputs, oldest first (↑/↓ recall).
    pub history: Vec<String>,
    /// Position while browsing `history`; `None` when composing fresh input.
    history_pos: Option<usize>,
    /// The in-progress input saved when history browsing starts.
    history_draft: String,
    /// When the in-flight turn started (drives the elapsed-time display).
    pub turn_started: Option<Instant>,
    /// Verb shown next to the busy spinner ("Conjuring…"); re-rolled at the
    /// start of each busy period by [`App::roll_spinner_verb`].
    pub spinner_verb: String,
    /// Number of verb rolls so far, mixed into the roll seed so back-to-back
    /// turns starting on the same tick still draw fresh verbs.
    verb_rolls: u64,
}

impl App {
    pub fn new(config: Config) -> Self {
        let mode = config.mode;
        let spinner_verb = config.ui.spinner_verb(0).to_string();
        let status = StatusLine {
            model: config.model.clone(),
            mode,
            step: 0,
            busy: false,
        };
        Self {
            config,
            mode,
            input: String::new(),
            cursor: 0,
            input_mode: InputMode::default(),
            transcript: Vec::new(),
            streaming: String::new(),
            streaming_thinking: String::new(),
            status,
            scroll: 0,
            should_quit: false,
            tick: 0,
            suggestions: Vec::new(),
            suggestion_index: 0,
            custom_commands: Vec::new(),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            picker: None,
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            turn_started: None,
            spinner_verb,
            verb_rolls: 0,
        }
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
        self.transcript
            .push(TranscriptEntry::Notice(message.into()));
    }

    /// Recompute [`InputMode`] from the input text, then refresh the command
    /// suggestions.
    fn sync_input_mode(&mut self) {
        self.input_mode = if self.input.trim_start().starts_with('/') {
            InputMode::Command
        } else {
            InputMode::Chat
        };
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
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    fn set_input(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.input = text;
        self.sync_input_mode();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.sync_input_mode();
    }

    fn insert_char(&mut self, c: char) {
        let index = self.byte_index();
        self.input.insert(index, c);
        self.cursor += 1;
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
        self.cursor -= 1;
        let index = self.byte_index();
        self.input.remove(index);
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.input.chars().count() {
            let index = self.byte_index();
            self.input.remove(index);
        }
    }

    /// Delete the word before the cursor (Ctrl-W).
    fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        while self.cursor > 0 && chars[self.cursor - 1].is_whitespace() {
            self.delete_back();
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

    // --- input history (↑/↓ recall, shell-style) ---

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.history_draft = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(pos) => pos - 1,
        };
        self.set_input(self.history[pos].clone());
        self.history_pos = Some(pos);
    }

    fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(pos) if pos + 1 < self.history.len() => {
                self.set_input(self.history[pos + 1].clone());
                self.history_pos = Some(pos + 1);
            }
            Some(_) => {
                let draft = std::mem::take(&mut self.history_draft);
                self.set_input(draft);
                self.history_pos = None;
            }
        }
    }

    /// Move any in-flight streaming text into the transcript. Reasoning
    /// flushes first: it streams before the visible reply.
    fn flush_streaming(&mut self) {
        if !self.streaming_thinking.is_empty() {
            let text = std::mem::take(&mut self.streaming_thinking);
            self.transcript.push(TranscriptEntry::Thinking(text));
        }
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.transcript.push(TranscriptEntry::Assistant(text));
        }
    }

    /// Toggle the expansion of the most recent finished tool card (Ctrl-T).
    fn toggle_last_tool_card(&mut self) {
        for entry in self.transcript.iter_mut().rev() {
            if let TranscriptEntry::ToolCard { collapsed, .. } = entry {
                *collapsed = !*collapsed;
                return;
            }
        }
    }

    /// Dispatch one event from the merged stream. Returns the user action
    /// the main loop must perform (start a turn, run a slash command, ...).
    pub fn handle_event(&mut self, event: Event) -> Result<Option<AppAction>> {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scroll = self.scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll = self.scroll.saturating_sub(3);
                    }
                    _ => {}
                }
                Ok(None)
            }
            Event::Paste(text) => {
                self.insert_str(&text);
                self.sync_input_mode();
                Ok(None)
            }
            Event::Resize(_, _) => Ok(None),
            Event::Tick => {
                self.tick = self.tick.wrapping_add(1);
                Ok(None)
            }
            Event::Agent(agent_event) => {
                self.handle_agent_event(agent_event);
                Ok(None)
            }
            Event::Notice(text) => {
                self.notice(text);
                Ok(None)
            }
        }
    }

    /// Keyboard handling for the current [`InputMode`]. Priority: global
    /// chords, open picker, then line editing.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        if key.kind == KeyEventKind::Release {
            return Ok(None);
        }

        // Global chords, regardless of input mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('d') => {
                    self.should_quit = true;
                    return Ok(None);
                }
                KeyCode::Char('u') => {
                    // Readline-style: kill from the line start to the cursor.
                    let index = self.byte_index();
                    self.input.drain(..index);
                    self.cursor = 0;
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('w') => {
                    self.delete_word_back();
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('a') => {
                    self.cursor = 0;
                    return Ok(None);
                }
                KeyCode::Char('e') => {
                    self.cursor = self.input.chars().count();
                    return Ok(None);
                }
                KeyCode::Char('k') => {
                    let index = self.byte_index();
                    self.input.truncate(index);
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('t') => {
                    self.toggle_last_tool_card();
                    return Ok(None);
                }
                _ => {}
            }
        }

        // An open picker captures navigation keys.
        if let Some(picker) = self.picker.as_mut() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab => {
                    picker.selected = if picker.selected == 0 {
                        picker.items.len().saturating_sub(1)
                    } else {
                        picker.selected - 1
                    };
                }
                KeyCode::Down | KeyCode::Tab => {
                    picker.selected = if picker.selected + 1 >= picker.items.len() {
                        0
                    } else {
                        picker.selected + 1
                    };
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
                        PickerKind::Mode => {
                            let mode = if item.value == "sovereign" {
                                Mode::Sovereign
                            } else {
                                Mode::Genie
                            };
                            AppAction::Command(SlashCommand::Mode(Some(mode)))
                        }
                    };
                    return Ok(Some(action));
                }
                _ => {}
            }
            return Ok(None);
        }

        let suggesting = !self.suggestions.is_empty();
        let action = match key.code {
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
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
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
            KeyCode::Esc => {
                if self.scroll > 0 {
                    self.scroll = 0;
                } else {
                    self.clear_input();
                }
                None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                None
            }
            KeyCode::Up => {
                // While browsing history, ↑/↓ keep navigating history even
                // when a recalled slash command repopulates suggestions.
                if suggesting && self.history_pos.is_none() {
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
                if suggesting && self.history_pos.is_none() {
                    self.suggestion_index = if self.suggestion_index + 1 >= self.suggestions.len() {
                        0
                    } else {
                        self.suggestion_index + 1
                    };
                } else {
                    self.history_next();
                }
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
                    // Completed to e.g. "/review " — wait for the arguments.
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

    /// Submit `input` as a user prompt: record it verbatim in history and
    /// the transcript, hand the preprocessed form (custom-command and `@file`
    /// expansion) to the agent.
    fn submit_prompt(&mut self, input: String) -> Option<AppAction> {
        if self.status.busy {
            // Rejected input never ran; do not record it in history.
            self.notice("the agent is busy — wait for the current turn to finish");
            return None;
        }
        let expanded =
            crate::commands::preprocess(&input, &self.custom_commands, &self.project_root);
        self.push_history(&input);
        self.clear_input();
        self.transcript.push(TranscriptEntry::User(input));
        self.scroll = 0;
        Some(AppAction::Submit(expanded))
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

    /// Record a submitted input for ↑/↓ recall (skipping immediate repeats).
    fn push_history(&mut self, input: &str) {
        if self.history.last().map(String::as_str) != Some(input) {
            self.history.push(input.to_string());
        }
    }

    /// Fold an agent event into the transcript / status.
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => {
                self.streaming.push_str(&delta);
            }
            AgentEvent::ThinkingDelta(delta) => {
                self.streaming_thinking.push_str(&delta);
            }
            AgentEvent::ToolStarted { name, args } => {
                self.flush_streaming();
                self.transcript.push(TranscriptEntry::ToolCard {
                    name,
                    args,
                    output: None,
                    is_error: false,
                    collapsed: false,
                });
            }
            AgentEvent::ToolFinished { name, output } => {
                let card = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match entry {
                        TranscriptEntry::ToolCard {
                            name: card_name,
                            output: slot,
                            is_error,
                            collapsed,
                            ..
                        } if *card_name == name && slot.is_none() => {
                            Some((slot, is_error, collapsed))
                        }
                        _ => None,
                    });
                match card {
                    Some((slot, is_error, collapsed)) => {
                        *is_error = output.is_error;
                        // Long, successful outputs start collapsed; errors
                        // stay expanded so they are visible.
                        *collapsed = !output.is_error && output.content.lines().count() > 6;
                        *slot = Some(output.content);
                    }
                    None => {
                        // No matching running card — record the result
                        // standalone.
                        self.transcript.push(TranscriptEntry::ToolCard {
                            name,
                            args: Value::Null,
                            output: Some(output.content),
                            is_error: output.is_error,
                            collapsed: false,
                        });
                    }
                }
            }
            AgentEvent::StepCompleted { step } => {
                self.status.step = step;
            }
            AgentEvent::Error(message) => {
                self.flush_streaming();
                self.notice(format!("error: {message}"));
            }
            AgentEvent::Done { reason } => {
                self.flush_streaming();
                self.status.busy = false;
                self.turn_started = None;
                match reason {
                    DoneReason::Completed => {}
                    DoneReason::Stopped => self.notice("turn stopped"),
                }
            }
        }
    }
}

/// Side effects the main loop performs on behalf of [`App`] (the app itself
/// stays synchronous and side-effect free).
#[derive(Debug)]
pub enum AppAction {
    /// Start an agent turn with this user message.
    Submit(String),
    /// Execute a parsed slash command.
    Command(SlashCommand),
}

// ---------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
    )
    .context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

fn restore_terminal() -> Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen,
    )
    .context("leaving alternate screen")?;
    crossterm::terminal::disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

/// Restore the terminal if (and only if) raw mode is active. Safe to call
/// from a panic hook — it does nothing when the TUI never started.
pub fn restore_terminal_best_effort() {
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = restore_terminal();
    }
}

/// Restores the terminal when the main loop unwinds or errors out.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_best_effort();
    }
}

// ---------------------------------------------------------------------------
// TUI entry point
// ---------------------------------------------------------------------------

const HELP_TEXT: &str = "available commands:\n  \
/help                       show this help\n  \
/clear                      clear the conversation\n  \
/model [tag]                show the current model, or switch to a tag\n  \
/mode [genie|sovereign]     pick or switch personality mode\n  \
/genie · /sovereign         switch mode directly\n  \
/evolve [start|status]      drive AHE's harness-evolution loop\n  \
/quit                       exit\n\
keys:\n  \
Tab / →                     accept command completion\n  \
↑ / ↓                       select suggestion · browse input history\n  \
PgUp/PgDn · mouse wheel     scroll the transcript\n  \
Ctrl-T                      toggle the last tool card\n  \
Ctrl-A/E Home/End ←/→       move cursor   ·  Ctrl-W/U/K kill word/to start/to end\n  \
Ctrl-C                      quit";

/// Spawn a fresh bridge-backed agent from the current config.
async fn spawn_agent(config: &Config) -> Result<Agent> {
    let mut bridge = config.bridge_config()?;
    // For xAI OAuth, resolve a fresh bearer up front and keep the source for
    // per-turn refresh; otherwise the key already lives in the bridge config.
    let tokens = if config.is_oauth() {
        let source = std::sync::Arc::new(crate::auth::xai_oauth::XaiTokenSource::new()?);
        bridge.api_key = source.bearer().await?;
        Some(source)
    } else {
        None
    };
    Agent::spawn(bridge, config.mode, tokens).await
}

/// Interactive entry point: set up the terminal, spawn the NexAU bridge,
/// pre-fill `cli.prompt` if given, and drive the [`EventLoop`] until quit.
/// Restores the terminal on exit and on panic.
pub async fn run_tui(config: Config, cli: Cli) -> Result<i32> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        anyhow::bail!("wizard needs an interactive terminal");
    }

    let project_root = std::env::current_dir().context("resolving project root")?;

    let mut agent_slot: Option<Agent> = Some(spawn_agent(&config).await?);
    let mut agent_task: Option<JoinHandle<Agent>> = None;

    let mut app = App::new(config);
    app.project_root = project_root.clone();
    app.custom_commands = crate::commands::load(&project_root);
    if let Some(prompt) = cli.prompt.clone() {
        app.set_input(prompt);
    }

    let mut events = EventLoop::new(Duration::from_millis(100));
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    loop {
        terminal.draw(|frame| crate::ui::draw(frame, &app))?;

        let Some(event) = events.next().await else {
            break;
        };

        let turn_done = matches!(&event, Event::Agent(AgentEvent::Done { .. }));

        let action = app.handle_event(event)?;
        if let Some(action) = action {
            match action {
                AppAction::Submit(input) => match agent_slot.take() {
                    Some(mut agent) => {
                        app.status.busy = true;
                        app.status.step = 0;
                        app.streaming.clear();
                        app.streaming_thinking.clear();
                        app.turn_started = Some(Instant::now());
                        app.roll_spinner_verb();

                        // Bridge AgentEvent -> Event::Agent for the UI loop.
                        let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
                        let forward = events.sender();
                        tokio::spawn(async move {
                            while let Some(agent_event) = agent_rx.recv().await {
                                if forward.send(Event::Agent(agent_event)).await.is_err() {
                                    break;
                                }
                            }
                        });

                        agent_task = Some(tokio::spawn(async move {
                            let fallback = agent_tx.clone();
                            if let Err(err) = agent.run_turn(&input, agent_tx).await {
                                // run_turn normally ends with Done itself;
                                // on a hard error make sure the UI unblocks.
                                let _ = fallback
                                    .send(AgentEvent::Error(format!("turn failed: {err:#}")))
                                    .await;
                                let _ = fallback
                                    .send(AgentEvent::Done {
                                        reason: DoneReason::Stopped,
                                    })
                                    .await;
                            }
                            agent
                        }));
                    }
                    None => app.notice("the agent is busy — wait for the current turn to finish"),
                },
                AppAction::Command(command) => {
                    CommandContext {
                        app: &mut app,
                        agent_slot: &mut agent_slot,
                        notices: events.sender(),
                    }
                    .run(command)
                    .await;
                }
            }
        }

        if turn_done && let Some(handle) = agent_task.take() {
            match handle.await {
                Ok(agent) => agent_slot = Some(agent),
                Err(err) => {
                    // The turn task panicked and took the agent with it;
                    // respawn a fresh bridge so the session can continue.
                    app.notice(format!("agent task crashed: {err}"));
                    match spawn_agent(&app.config).await {
                        Ok(agent) => {
                            agent_slot = Some(agent);
                            app.notice("agent restarted");
                        }
                        Err(err) => app.notice(format!(
                            "could not restart the agent: {err:#} — /quit and relaunch"
                        )),
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    if let Some(agent) = agent_slot.take() {
        agent.shutdown().await;
    }

    drop(_guard);
    restore_terminal_best_effort();
    Ok(0)
}

/// Everything a slash command may touch, borrowed from the main loop for
/// the duration of one dispatch.
struct CommandContext<'a> {
    app: &'a mut App,
    agent_slot: &'a mut Option<Agent>,
    /// Sender for posting notices from background tasks (e.g. `/evolve`)
    /// back into the transcript without blocking the main loop.
    notices: mpsc::Sender<Event>,
}

impl CommandContext<'_> {
    /// Execute one slash command against the running stack.
    async fn run(mut self, command: SlashCommand) {
        match command {
            SlashCommand::Help => self.app.notice(HELP_TEXT),
            SlashCommand::Quit => self.app.should_quit = true,
            SlashCommand::Clear => self.clear().await,
            SlashCommand::Model(None) => self.show_model(),
            SlashCommand::Model(Some(tag)) => self.switch_model(tag).await,
            SlashCommand::Mode(None) => self.open_mode_picker(),
            SlashCommand::Mode(Some(mode)) => self.switch_mode(mode),
            SlashCommand::Evolve(action) => self.run_evolve(action),
        }
    }

    /// `/evolve [start|status]`: drive AHE's evolve loop on a background task
    /// so the (subprocess + file I/O) work never blocks the UI. The result —
    /// the launched session, a status summary, or an error — comes back as an
    /// [`Event::Notice`].
    fn run_evolve(&mut self, action: EvolveSlash) {
        let evolve = match self.app.config.evolve_ready() {
            Ok(cfg) => cfg.clone(),
            Err(err) => {
                self.app.notice(format!("{err:#}"));
                return;
            }
        };
        self.app.notice(match action {
            EvolveSlash::Start => "evolve: preflighting and launching…",
            EvolveSlash::Status => "evolve: reading latest experiment…",
        });
        let notices = self.notices.clone();
        tokio::spawn(async move {
            let work = tokio::task::spawn_blocking(move || match action {
                EvolveSlash::Start => crate::evolve::start(&evolve).map(|session| {
                    format!(
                        "evolve launched in tmux session '{session}' — \
                         attach: tmux attach -t {session} · /evolve status for progress"
                    )
                }),
                EvolveSlash::Status => crate::evolve::status(&evolve),
            })
            .await;
            let text = match work {
                Ok(Ok(message)) => message,
                Ok(Err(err)) => format!("evolve error: {err:#}"),
                Err(err) => format!("evolve task failed: {err}"),
            };
            let _ = notices.send(Event::Notice(text)).await;
        });
    }

    /// True (with a notice) when the agent cannot be touched right now (a
    /// turn is running).
    fn agent_unavailable(&mut self, action: &str) -> bool {
        if self.app.status.busy {
            self.app
                .notice(format!("cannot {action} while a turn is running"));
            true
        } else {
            false
        }
    }

    /// `/clear`: respawn a fresh bridge and wipe the transcript.
    async fn clear(&mut self) {
        if self.agent_unavailable("clear") {
            return;
        }
        if let Some(agent) = self.agent_slot.as_mut()
            && let Err(err) = agent.clear().await
        {
            self.app.notice(format!("failed to clear: {err:#}"));
            return;
        }
        self.app.transcript.clear();
        self.app.streaming.clear();
        self.app.streaming_thinking.clear();
        self.app.scroll = 0;
        self.app.notice("conversation cleared");
    }

    /// `/model` with no argument: report the active model.
    fn show_model(&mut self) {
        self.app.notice(format!(
            "model: {} — switch with /model <tag>",
            self.app.status.model
        ));
    }

    /// `/model <tag>`: respawn the bridge with the new model (resets history).
    async fn switch_model(&mut self, tag: String) {
        if self.agent_unavailable("switch models") {
            return;
        }
        let Some(agent) = self.agent_slot.as_mut() else {
            self.app
                .notice("the agent is unavailable — try again in a moment");
            return;
        };
        match agent.set_model(tag.clone()).await {
            Ok(()) => {
                self.app.config.model = tag.clone();
                self.app.status.model = tag.clone();
                self.app.transcript.clear();
                self.app.streaming.clear();
                self.app.streaming_thinking.clear();
                self.app.scroll = 0;
                self.app
                    .notice(format!("switched to model {tag} (conversation reset)"));
            }
            Err(err) => self.app.notice(format!("failed to switch model: {err:#}")),
        }
    }

    /// `/mode` with no argument: open the interactive mode picker.
    fn open_mode_picker(&mut self) {
        let items = vec![
            PickerItem {
                value: "genie".to_string(),
                detail: "interactive — acts on each turn".to_string(),
                current: self.app.mode == Mode::Genie,
            },
            PickerItem {
                value: "sovereign".to_string(),
                detail: "autonomous — framed for longer, self-directed work".to_string(),
                current: self.app.mode == Mode::Sovereign,
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

    /// `/mode <name>` (or a picker selection): switch the personality mode.
    fn switch_mode(&mut self, mode: Mode) {
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_mode(mode);
        }
        self.app.mode = mode;
        self.app.config.mode = mode;
        self.app.status.mode = mode;
        self.app.notice(format!("switched to {mode} mode"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(Config::default())
    }

    fn press(app: &mut App, code: KeyCode) -> Option<AppAction> {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
            .expect("key handled")
    }

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn spinner_verb_starts_from_the_default_list() {
        let app = app();
        assert!(
            crate::config::UiConfig::DEFAULT_SPINNER_VERBS.contains(&app.spinner_verb.as_str())
        );
    }

    #[test]
    fn spinner_verb_is_stable_within_a_busy_period() {
        let mut app = app();
        app.tick = 17;
        app.roll_spinner_verb();
        let during = app.spinner_verb.clone();
        app.tick += 5;
        assert_eq!(app.spinner_verb, during);
    }

    #[test]
    fn spinner_verb_rerolls_across_busy_periods() {
        let mut app = app();
        let mut seen = std::collections::HashSet::new();
        for turn in 0..40u64 {
            app.tick = turn * 13;
            app.roll_spinner_verb();
            seen.insert(app.spinner_verb.clone());
        }
        assert!(seen.len() > 1, "verb never varied across busy periods");
    }

    #[test]
    fn slash_filters_suggestions_by_prefix() {
        let mut app = app();
        type_str(&mut app, "/mo");
        let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["model", "mode"]);
        assert_eq!(app.input_mode, InputMode::Command);
    }

    #[test]
    fn suggestions_hide_once_args_are_typed() {
        let mut app = app();
        type_str(&mut app, "/mode genie");
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn arrow_keys_cycle_suggestions_with_wraparound() {
        let mut app = app();
        type_str(&mut app, "/mo");
        assert_eq!(app.suggestion_index, 0);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.suggestion_index, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.suggestion_index, 0);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.suggestion_index, 1);
    }

    #[test]
    fn tab_completes_the_selected_suggestion() {
        let mut app = app();
        // "/mod" prefix-matches both model and mode; Tab completes the top.
        type_str(&mut app, "/mod");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "/model");
        assert_eq!(app.cursor, "/model".chars().count());
    }

    #[test]
    fn enter_completes_and_runs_argless_commands() {
        let mut app = app();
        type_str(&mut app, "/he");
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Help))
        ));
        assert!(app.input.is_empty());
    }

    #[test]
    fn exactly_typed_command_wins_over_longer_completion() {
        // "model" prefix-matches the typed "mode"; Enter must still run
        // /mode itself, not complete to /model.
        let mut app = app();
        type_str(&mut app, "/mode");
        assert_eq!(app.suggestions[0].name, "mode");
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Mode(None)))
        ));
    }

    fn custom(name: &str, template: &str, description: Option<&str>) -> CustomCommand {
        CustomCommand {
            name: name.to_string(),
            description: description.map(str::to_string),
            template: template.to_string(),
            path: PathBuf::new(),
        }
    }

    #[test]
    fn custom_commands_appear_in_suggestions_after_builtins() {
        let mut app = app();
        app.custom_commands = vec![custom(
            "models-report",
            "Report on $ARGUMENTS",
            Some("report"),
        )];
        type_str(&mut app, "/mo");
        let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["model", "mode", "models-report"]);
        let spec = &app.suggestions[2];
        assert_eq!(spec.description, "report");
        assert!(spec.takes_args);
    }

    #[test]
    fn typed_custom_command_submits_the_expanded_prompt() {
        let mut app = app();
        app.custom_commands = vec![custom("review", "Review $1 with care.", None)];
        type_str(&mut app, "/review src/app.rs");
        let action = press(&mut app, KeyCode::Enter);
        let Some(AppAction::Submit(prompt)) = action else {
            panic!("expected a submit, got {action:?}");
        };
        assert_eq!(prompt, "Review src/app.rs with care.");
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::User(text)) if text == "/review src/app.rs"
        ));
    }

    #[test]
    fn unknown_slash_command_passes_through_as_a_prompt() {
        let mut app = app();
        type_str(&mut app, "/frobnicate the build");
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Submit(prompt)) if prompt == "/frobnicate the build"
        ));
    }

    #[test]
    fn builtin_command_with_bad_args_keeps_its_usage_notice() {
        let mut app = app();
        type_str(&mut app, "/mode warlock");
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(text)) if text.contains("unknown mode")
        ));
    }

    #[test]
    fn submit_expands_at_file_references() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ctx.txt"), "the context\n").unwrap();
        let mut app = app();
        app.project_root = tmp.path().to_path_buf();
        type_str(&mut app, "use @ctx.txt here");
        let action = press(&mut app, KeyCode::Enter);
        let Some(AppAction::Submit(prompt)) = action else {
            panic!("expected a submit, got {action:?}");
        };
        assert!(prompt.contains("the context"), "got: {prompt}");
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::User(text)) if text == "use @ctx.txt here"
        ));
    }

    #[test]
    fn tab_completes_at_paths_from_the_directory_listing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("readme.md"), "x").unwrap();
        std::fs::create_dir(tmp.path().join("reach")).unwrap();
        let mut app = app();
        app.project_root = tmp.path().to_path_buf();

        // Common prefix of readme.md / reach.
        type_str(&mut app, "see @re");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "see @rea");

        // Unique file completes fully.
        type_str(&mut app, "d");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "see @readme.md");
    }

    #[test]
    fn tab_completes_unique_directory_with_a_trailing_slash() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sources")).unwrap();
        std::fs::write(tmp.path().join("sources").join("inner.rs"), "x").unwrap();
        let mut app = app();
        app.project_root = tmp.path().to_path_buf();
        type_str(&mut app, "@so");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "@sources/");
        type_str(&mut app, "in");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "@sources/inner.rs");
    }

    #[test]
    fn genie_and_sovereign_parse_as_mode_switches() {
        assert_eq!(
            SlashCommand::parse("/genie"),
            Some(Ok(SlashCommand::Mode(Some(Mode::Genie))))
        );
        assert_eq!(
            SlashCommand::parse("/sovereign"),
            Some(Ok(SlashCommand::Mode(Some(Mode::Sovereign))))
        );
    }

    #[test]
    fn evolve_parses_default_and_explicit_actions() {
        assert_eq!(
            SlashCommand::parse("/evolve"),
            Some(Ok(SlashCommand::Evolve(EvolveSlash::Start)))
        );
        assert_eq!(
            SlashCommand::parse("/evolve start"),
            Some(Ok(SlashCommand::Evolve(EvolveSlash::Start)))
        );
        assert_eq!(
            SlashCommand::parse("/evolve status"),
            Some(Ok(SlashCommand::Evolve(EvolveSlash::Status)))
        );
        assert!(matches!(
            SlashCommand::parse("/evolve frobnicate"),
            Some(Err(message)) if message.contains("unknown evolve action")
        ));
    }

    #[test]
    fn model_parses_with_and_without_a_tag() {
        assert_eq!(
            SlashCommand::parse("/model"),
            Some(Ok(SlashCommand::Model(None)))
        );
        assert_eq!(
            SlashCommand::parse("/model grok-4"),
            Some(Ok(SlashCommand::Model(Some("grok-4".to_string()))))
        );
    }

    #[test]
    fn done_stopped_notes_the_turn_stopped() {
        let mut app = app();
        app.status.busy = true;
        app.handle_agent_event(AgentEvent::Done {
            reason: DoneReason::Stopped,
        });
        assert!(!app.status.busy);
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(text)) if text == "turn stopped"
        ));
    }

    #[test]
    fn tool_started_then_finished_fills_the_card() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "shell".to_string(),
            args: serde_json::json!({"cmd": "ls"}),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "shell".to_string(),
            output: crate::agent::ToolOutput {
                content: "file.txt".to_string(),
                is_error: false,
            },
        });
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::ToolCard { output: Some(text), is_error: false, .. })
                if text == "file.txt"
        ));
    }

    #[test]
    fn cursor_editing_inserts_mid_line() {
        let mut app = app();
        type_str(&mut app, "helo");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.input, "hello");
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Delete);
        assert_eq!(app.input, "ello");
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.input, "ell");
    }

    #[test]
    fn history_recall_restores_draft() {
        let mut app = app();
        type_str(&mut app, "first message");
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "second message");
        press(&mut app, KeyCode::Enter);

        type_str(&mut app, "draft");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "second message");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "first message");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "second message");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn picker_navigation_wraps_and_enter_selects() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Mode,
            title: " select mode ".to_string(),
            items: vec![
                PickerItem {
                    value: "genie".to_string(),
                    detail: String::new(),
                    current: true,
                },
                PickerItem {
                    value: "sovereign".to_string(),
                    detail: String::new(),
                    current: false,
                },
            ],
            selected: 0,
        });

        press(&mut app, KeyCode::Up);
        assert_eq!(app.picker.as_ref().expect("open").selected, 1);
        let action = press(&mut app, KeyCode::Enter);
        match action {
            Some(AppAction::Command(SlashCommand::Mode(Some(mode)))) => {
                assert_eq!(mode, Mode::Sovereign);
            }
            other => panic!("expected mode switch, got {other:?}"),
        }
        assert!(app.picker.is_none());
    }

    #[test]
    fn picker_escape_cancels() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Mode,
            title: " select mode ".to_string(),
            items: vec![PickerItem {
                value: "genie".to_string(),
                detail: String::new(),
                current: true,
            }],
            selected: 0,
        });
        press(&mut app, KeyCode::Esc);
        assert!(app.picker.is_none());
    }

    #[test]
    fn backtab_in_a_picker_navigates() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Mode,
            title: " select mode ".to_string(),
            items: vec![
                PickerItem {
                    value: "genie".to_string(),
                    detail: String::new(),
                    current: true,
                },
                PickerItem {
                    value: "sovereign".to_string(),
                    detail: String::new(),
                    current: false,
                },
            ],
            selected: 0,
        });
        let action = press(&mut app, KeyCode::BackTab);
        assert!(action.is_none(), "the picker captured the key");
        assert_eq!(app.picker.as_ref().expect("open").selected, 1);
    }

    #[test]
    fn ctrl_w_kills_previous_word() {
        let mut app = app();
        type_str(&mut app, "fix the parser bug");
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .expect("key handled");
        assert_eq!(app.input, "fix the parser ");
    }

    #[test]
    fn history_recall_of_slash_command_keeps_browsing_history() {
        let mut app = app();
        type_str(&mut app, "older message");
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "/model");
        press(&mut app, KeyCode::Enter);

        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "/model");
        // The recalled slash command repopulates suggestions; ↑ must keep
        // walking history instead of cycling them.
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "older message");
    }

    #[test]
    fn unbound_ctrl_chords_do_not_insert_literal_chars() {
        let mut app = app();
        type_str(&mut app, "abc");
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .expect("key handled");
        assert_eq!(app.input, "abc");
    }

    #[test]
    fn busy_submit_is_not_recorded_in_history() {
        let mut app = app();
        app.status.busy = true;
        type_str(&mut app, "queued message");
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
        assert!(app.history.is_empty());
    }

    #[test]
    fn ctrl_u_kills_to_line_start_keeping_tail() {
        let mut app = app();
        type_str(&mut app, "hello world");
        for _ in 0..6 {
            press(&mut app, KeyCode::Left);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .expect("key handled");
        assert_eq!(app.input, " world");
        assert_eq!(app.cursor, 0);
    }
}
