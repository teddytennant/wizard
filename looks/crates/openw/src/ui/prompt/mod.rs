// OWNER: prompt (frame, editor, placeholder, meta line, autocomplete popup, history, paste summary)
//! The prompt box shared by home and session: opencode's `component/prompt/index.tsx` and
//! `autocomplete.tsx`. State lives in [`PromptState`]; the free functions draw it and turn keys
//! into editor edits, popup moves and submits.

pub mod fuzzysort;

use std::ops::Range;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tuikit::border::PromptFrame;
use tuikit::editor::{Editor, EditorStyle, KeyOutcome};
use tuikit::paint::{fill, put_str, set_style};
use tuikit::width::{display_width, normalize_newlines, truncate_middle};

use crate::app::App;
use crate::commands::{self, Command, Source};
use crate::keys::Action;

pub const EXAMPLES: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];
pub const SHELL_EXAMPLES: [&str; 3] = ["ls -la", "git status", "pwd"];
const PASTE_LINES: usize = 3;
const PASTE_CHARS: usize = 150;
const HISTORY_FILE: &str = "prompt-history.jsonl";
const STASH_FILE: &str = "prompt-stash.jsonl";
const HISTORY_KEEP: usize = 50;
/// A pasted body larger than this stays in memory for the session but is not written to the
/// history file: every submit would otherwise rewrite megabytes, and 50 entries keep them all.
const PASTE_PERSIST_MAX: usize = 64 * 1024;
/// A draft cleared with ctrl+c is kept in history from this many chars.
const DRAFT_MIN_CHARS: usize = 20;

/// Text that stands in for something bigger while the prompt is edited: a pasted block, or an
/// attached file. `text` is what goes to the backend in place of `token`.
#[derive(Clone, Debug, PartialEq)]
pub struct Paste {
    pub token: String,
    pub text: String,
}

/// A prompt ready to send, see [`PromptState::outgoing`].
pub struct Outgoing {
    pub sent: String,
    pub shown: String,
    pub files: Vec<crate::app::Chip>,
}

/// One remembered prompt: what was typed, the mode it was typed in and the placeholders in it.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub input: String,
    pub shell: bool,
    pub pastes: Vec<Paste>,
}

impl HistoryEntry {
    fn to_json(&self) -> serde_json::Value {
        let parts: Vec<serde_json::Value> = self
            .pastes
            .iter()
            .filter(|p| p.text.len() <= PASTE_PERSIST_MAX)
            .map(|p| {
                serde_json::json!({
                    "type": "text",
                    "text": p.text,
                    "source": { "text": { "value": p.token } },
                })
            })
            .collect();
        let mut v = serde_json::json!({ "input": self.input, "parts": parts });
        if self.shell {
            v["mode"] = "shell".into();
        }
        v
    }

    fn from_json(v: &serde_json::Value) -> Option<HistoryEntry> {
        let input = v.get("input")?.as_str()?.to_string();
        let shell = v.get("mode").and_then(|m| m.as_str()) == Some("shell");
        let pastes = v
            .get("parts")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        Some(Paste {
                            token: p.pointer("/source/text/value")?.as_str()?.to_string(),
                            text: p.get("text")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(HistoryEntry {
            input,
            shell,
            pastes,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcKind {
    Slash,
    File,
}

#[derive(Clone, Debug)]
pub enum AcValue {
    Command(Box<Command>),
    File { path: String, dir: bool },
}

#[derive(Clone, Debug)]
pub struct AcItem {
    pub display: String,
    pub desc: String,
    pub value: AcValue,
}

#[derive(Clone, Debug)]
pub struct Autocomplete {
    pub kind: AcKind,
    /// Byte offset of the `/` or `@` that opened the popup.
    pub start: usize,
    pub query: String,
    pub items: Vec<AcItem>,
    pub selected: usize,
    pub offset: usize,
}

pub struct PromptState {
    pub editor: Editor,
    pub shell: bool,
    /// Index into [`EXAMPLES`]; re-rolled when the session changes.
    pub example: usize,
    pub ac: Option<Autocomplete>,
    /// Text the popup was dismissed with esc at; it reopens as soon as the text changes.
    dismissed: Option<String>,
    pub pastes: Vec<Paste>,
    pub mentions: Vec<String>,
    pub stash: Vec<String>,
    history: Vec<HistoryEntry>,
    /// 0 is the draft; `-n` is the n-th newest entry (opencode's `store.index`).
    hist_index: isize,
    history_path: Option<PathBuf>,
    stash_path: Option<PathBuf>,
    /// Rect the editor text was last drawn in, for mouse clicks.
    pub input_rect: Rect,
    /// Candidate files and directories for `@`, built from [`App::files`] when it changes.
    index: FileIndex,
}

impl PromptState {
    pub fn new(example: usize, state_dir: Option<PathBuf>) -> Self {
        let history_path = state_dir.as_ref().map(|d| d.join(HISTORY_FILE));
        let stash_path = state_dir.map(|d| d.join(STASH_FILE));
        let history = history_path
            .as_deref()
            .map(load_history)
            .unwrap_or_default();
        let stash = stash_path.as_deref().map(load_stash).unwrap_or_default();
        let mut editor = Editor::new();
        editor.set_punct_wrap(true);
        PromptState {
            editor,
            shell: false,
            example: example % EXAMPLES.len(),
            ac: None,
            dismissed: None,
            pastes: Vec::new(),
            mentions: Vec::new(),
            stash,
            history,
            hist_index: 0,
            history_path,
            stash_path,
            input_rect: Rect::default(),
            index: FileIndex::default(),
        }
    }

    /// Write the stash after a push or pop.
    pub fn save_stash(&self) {
        if let Some(p) = &self.stash_path {
            let body: String = self
                .stash
                .iter()
                .map(|t| serde_json::json!({ "input": t, "parts": [] }).to_string() + "\n")
                .collect();
            write_state(p, body);
        }
    }

    pub fn text(&self) -> &str {
        self.editor.text()
    }

    pub fn is_empty(&self) -> bool {
        self.editor.is_empty()
    }

    pub fn clear(&mut self) {
        self.editor.clear();
        self.shell = false;
        self.ac = None;
        self.pastes.clear();
        self.mentions.clear();
    }

    pub fn set_text(&mut self, s: &str) {
        self.editor.set_text(s);
    }

    /// Text with paste summaries replaced by what was pasted.
    pub fn expanded(&self) -> String {
        let mut t = self.editor.text().to_string();
        for p in &self.pastes {
            t = t.replacen(&p.token, &p.text, 1);
        }
        t
    }

    /// The prompt as it goes out: `sent` has every placeholder swapped for what it stands for,
    /// `shown` keeps the attachment tokens (`[Image 1]`) as the user block displays them, and
    /// `files` are the chips for the mentions and attachments still in the text.
    pub fn outgoing(&self) -> Outgoing {
        let text = self.editor.text();
        let mut sent = text.to_string();
        let mut shown = text.to_string();
        let mut files: Vec<crate::app::Chip> = Vec::new();
        for p in &self.pastes {
            sent = sent.replacen(&p.token, &p.text, 1);
            if p.token.starts_with("[Image ") || p.token.starts_with("[PDF ") {
                let name = p
                    .text
                    .trim_start_matches('@')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
                if text.contains(&p.token) {
                    files.push(crate::app::Chip {
                        directory: false,
                        name,
                    });
                }
            } else {
                shown = shown.replacen(&p.token, &p.text, 1);
            }
        }
        for m in &self.mentions {
            if text.contains(m.as_str()) {
                // wizard expands `@path` only for a token without whitespace; a name with a
                // space goes out quoted so the model reads the whole name, not `@my`
                if m.contains(char::is_whitespace) {
                    let quoted = format!("@\"{}\"", &m[1..]);
                    sent = sent.replacen(m.as_str(), &quoted, 1);
                }
                let name = m.trim_start_matches('@').to_string();
                let chip = crate::app::Chip {
                    directory: name.ends_with('/'),
                    name,
                };
                if !files.contains(&chip) {
                    files.push(chip);
                }
            }
        }
        Outgoing { sent, shown, files }
    }

    /// What is in the box right now, as a history entry.
    fn current_entry(&self) -> HistoryEntry {
        let t = self.editor.text();
        HistoryEntry {
            input: t.to_string(),
            shell: self.shell,
            pastes: self
                .pastes
                .iter()
                .filter(|p| t.contains(&p.token))
                .cloned()
                .collect(),
        }
    }

    /// Remember the prompt as submitted (call before clearing the box).
    pub fn remember(&mut self) {
        let e = self.current_entry();
        self.append_history(e);
    }

    fn append_history(&mut self, e: HistoryEntry) {
        if self.history.last() == Some(&e) {
            self.hist_index = 0;
            return;
        }
        self.history.push(e.clone());
        self.hist_index = 0;
        let Some(path) = self.history_path.clone() else {
            return;
        };
        if self.history.len() > HISTORY_KEEP {
            let skip = self.history.len() - HISTORY_KEEP;
            self.history.drain(..skip);
            let body: String = self
                .history
                .iter()
                .map(|e| e.to_json().to_string() + "\n")
                .collect();
            write_state(&path, body);
        } else {
            append_state(&path, &(e.to_json().to_string() + "\n"));
        }
    }

    /// ctrl+c on a non-empty box: a draft of 20 chars or more is kept in history.
    pub fn clear_keeping_draft(&mut self) {
        if self.editor.text().trim().chars().count() >= DRAFT_MIN_CHARS || !self.pastes.is_empty() {
            self.remember();
        }
        self.clear();
    }

    /// opencode's `history.move`: `dir` is -1 for older, 1 for newer. Only moves from an empty
    /// box or one that still holds the entry it was filled from.
    fn history_move(&mut self, dir: isize) -> Option<HistoryEntry> {
        if self.history.is_empty() {
            return None;
        }
        let n = self.history.len() as isize;
        let at = |i: isize| -> usize {
            if i >= 0 {
                i as usize
            } else {
                (n + i) as usize
            }
        };
        let current = &self.history[at(self.hist_index)];
        let text = self.editor.text();
        if current.input != text && !text.is_empty() {
            return None;
        }
        let next = self.hist_index + dir;
        if next.abs() > n || next > 0 {
            return None;
        }
        self.hist_index = next;
        if next == 0 {
            return Some(HistoryEntry {
                input: String::new(),
                shell: false,
                pastes: Vec::new(),
            });
        }
        Some(self.history[at(next)].clone())
    }

    fn load_entry(&mut self, e: HistoryEntry, cursor_end: bool) {
        self.editor.set_text(&e.input);
        self.shell = e.shell;
        self.pastes = e.pastes;
        self.mentions.clear();
        if !cursor_end {
            self.editor.set_cursor(0);
        }
    }
}

fn load_history(path: &std::path::Path) -> Vec<HistoryEntry> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut v: Vec<HistoryEntry> = s
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|j| HistoryEntry::from_json(&j))
        .collect();
    let skip = v.len().saturating_sub(HISTORY_KEEP);
    v.drain(..skip);
    v
}

fn load_stash(path: &std::path::Path) -> Vec<String> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    s.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("input").and_then(|i| i.as_str()).map(String::from))
        .collect()
}

/// Runs on the UI thread at every submit once history is full, so no `fsync`: on a busy disk
/// that was seconds before the prompt went out.
fn write_state(path: &std::path::Path, body: String) {
    let _ = crate::private::write_replace(path, body.as_bytes());
}

fn append_state(path: &std::path::Path, line: &str) {
    let _ = crate::private::append(path, line.as_bytes());
}

// ---- geometry --------------------------------------------------------------------------

/// Total rows of the box, cap row included: top pad + input + blank + meta + cap.
pub fn box_height(app: &App, width: u16) -> u16 {
    let inner_w = width.saturating_sub(5).max(1);
    let max_input = (app.size.1 / 3).max(6);
    let n = app.prompt.editor.needed_height(inner_w).clamp(1, max_input);
    n + 4
}

// ---- drawing ---------------------------------------------------------------------------

fn bar_color(app: &App) -> Color {
    if app.leader_pending() {
        app.theme.border
    } else if app.prompt.shell {
        app.theme.primary
    } else {
        app.agent.color(&app.theme)
    }
}

/// Draw the box into `area` (`box_height` rows). Returns the terminal cursor cell.
pub fn draw_box(buf: &mut Buffer, area: Rect, app: &mut App) -> Option<(u16, u16)> {
    if area.height < 3 || area.width < 6 {
        return None;
    }
    let theme = app.theme.clone();
    let bar = bar_color(app);
    tuikit::paint::fill(buf, area, Style::new().bg(theme.background));
    ratatui::widgets::Widget::render(
        PromptFrame {
            border: bar,
            bg: theme.background_element,
        },
        area,
        buf,
    );
    let inner = PromptFrame::inner(area);
    if inner.height < 2 {
        return None;
    }
    let input = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    let meta_y = inner.y + inner.height - 1;
    app.prompt.input_rect = input;

    let pending = app.leader_pending();
    let text_fg = if pending {
        theme.text_muted
    } else {
        theme.text
    };
    let shell = app.prompt.shell;
    // Only the home prompt is given example texts; in a session the box stays empty.
    let placeholder = (!app.in_session()).then(|| {
        let ph = if shell {
            format!(
                "Run a command… \"{}\"",
                SHELL_EXAMPLES[app.prompt.example % SHELL_EXAMPLES.len()]
            )
        } else {
            format!(
                "Ask anything… \"{}\"",
                EXAMPLES[app.prompt.example % EXAMPLES.len()]
            )
        };
        (
            ph,
            Style::new()
                .fg(theme.text_muted)
                .bg(theme.background_element),
        )
    });
    let style = EditorStyle {
        text: Style::new().fg(text_fg).bg(theme.background_element),
        placeholder,
    };
    let info = app.prompt.editor.render(input, buf, &style);
    style_tokens(buf, input, app, &info);

    // Meta row.
    let meta = Rect {
        y: meta_y,
        height: 1,
        ..inner
    };
    draw_meta(buf, meta, app, pending);

    if app.dialogs.is_open() || app.prompt_blurred() {
        None
    } else {
        info.cursor
    }
}

fn draw_meta(buf: &mut Buffer, row: Rect, app: &App, pending: bool) {
    let t = &app.theme;
    let bg = t.background_element;
    let hl = if pending {
        t.border
    } else if app.prompt.shell {
        t.primary
    } else {
        app.agent.color(t)
    };
    let mut x = row.x;
    let name = if app.prompt.shell {
        "Shell".to_string()
    } else {
        app.agent.title().to_string()
    };
    x = put_str(buf, x, row.y, &name, Style::new().fg(hl).bg(bg), row);
    if app.prompt.shell {
        return;
    }
    x = put_str(
        buf,
        x,
        row.y,
        " · ",
        Style::new().fg(t.text_muted).bg(bg),
        row,
    );
    let (model, provider) = app.model_display();
    let model_fg = if pending { t.text_muted } else { t.text };
    x = put_str(buf, x, row.y, &model, Style::new().fg(model_fg).bg(bg), row);
    if !provider.is_empty() {
        x = put_str(
            buf,
            x,
            row.y,
            &format!(" {provider}"),
            Style::new().fg(t.text_muted).bg(bg),
            row,
        );
    }
    // opencode's variant: a dot and the name in bold warning, once one is chosen
    if let Some(v) = app.variant_shown() {
        x = put_str(
            buf,
            x,
            row.y,
            " ·",
            Style::new().fg(t.text_muted).bg(bg),
            row,
        );
        put_str(
            buf,
            x,
            row.y,
            &format!(" {v}"),
            Style::new()
                .fg(t.warning)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
            row,
        );
    }
}

/// Paste summaries and `@file` mentions get their own colors on top of the plain text.
fn style_tokens(buf: &mut Buffer, input: Rect, app: &App, info: &tuikit::editor::RenderInfo) {
    let text = app.prompt.editor.text();
    if text.is_empty() {
        return;
    }
    let mut ranges: Vec<(Range<usize>, Style)> = Vec::new();
    let t = &app.theme;
    for p in &app.prompt.pastes {
        if let Some(i) = text.find(&p.token) {
            ranges.push((
                i..i + p.token.len(),
                Style::new()
                    .fg(t.background)
                    .bg(t.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    for m in &app.prompt.mentions {
        let mut from = 0;
        while let Some(i) = text[from..].find(m.as_str()) {
            let s = from + i;
            ranges.push((
                s..s + m.len(),
                Style::new().fg(t.warning).add_modifier(Modifier::BOLD),
            ));
            from = s + m.len();
        }
    }
    if ranges.is_empty() {
        return;
    }
    let rows = app.prompt.editor.layout(input.width as usize);
    let Some((_, cy)) = info.cursor else { return };
    let cur_row = rows
        .iter()
        .position(|r| {
            app.prompt.editor.cursor() >= r.start && app.prompt.editor.cursor() <= r.next.max(r.end)
        })
        .unwrap_or(0);
    let scroll = cur_row.saturating_sub((cy - input.y) as usize);
    for (range, style) in ranges {
        for (i, row) in rows.iter().enumerate() {
            if i < scroll || i - scroll >= input.height as usize {
                continue;
            }
            let (s, e) = (range.start.max(row.start), range.end.min(row.end));
            if s >= e {
                continue;
            }
            let x0 = input.x + display_width(&text[row.start..s]) as u16;
            let w = display_width(&text[s..e]) as u16;
            set_style(
                buf,
                Rect::new(x0, input.y + (i - scroll) as u16, w, 1),
                style,
            );
        }
    }
}

// ---- autocomplete ----------------------------------------------------------------------

/// The project's files and directories, prepared for matching. Rebuilt when `App::files`
/// changes size.
#[derive(Default)]
struct FileIndex {
    /// [`files_stamp`] of the list the entries were built from.
    built_for: u64,
    entries: Vec<FileEntry>,
}

struct FileEntry {
    /// Directories end with `/`.
    path: String,
    dir: bool,
    full: fuzzysort::Prepared,
    name: fuzzysort::Prepared,
}

/// Cheap identity of a file list: its length and 64 evenly spaced names. A switch to a branch
/// with as many files used to keep the old index, since only the length was compared; hashing
/// all 20,000 names on every keystroke would cost more than the search.
fn files_stamp(files: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    files.len().hash(&mut h);
    let step = (files.len() / 64).max(1);
    for f in files.iter().step_by(step) {
        f.hash(&mut h);
    }
    files.last().hash(&mut h);
    h.finish()
}

fn build_index(files: &[String]) -> Vec<FileEntry> {
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for f in files {
        let mut end = 0;
        while let Some(i) = f[end..].find('/') {
            end += i + 1;
            dirs.insert(f[..end].to_string());
        }
    }
    let mut out: Vec<FileEntry> = Vec::with_capacity(files.len() + dirs.len());
    let mut push = |path: &str, dir: bool| {
        let trimmed = path.trim_end_matches('/');
        let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
        out.push(FileEntry {
            path: path.to_string(),
            dir,
            full: fuzzysort::Prepared::new(path),
            name: fuzzysort::Prepared::new(base),
        });
    };
    for d in &dirs {
        push(d, true);
    }
    for f in files {
        push(f, false);
    }
    out
}

/// What `hide()` does in opencode: leaving a half-typed `/command` deletes it.
fn drop_typed_slash(app: &mut App) {
    let text = app.prompt.editor.text().to_string();
    if !text.ends_with(' ') && text.starts_with('/') {
        let cur = app.prompt.editor.cursor().min(text.len());
        app.prompt.editor.set_text(&text[cur..]);
        app.prompt.editor.set_cursor(0);
    }
}

/// opencode's `mentionTriggerIndex`: the `@` before the cursor, if it starts a word and no
/// whitespace follows it.
fn mention_start(before: &str) -> Option<usize> {
    let i = before.rfind('@')?;
    let prev = before[..i].chars().next_back();
    let query = &before[i..];
    (prev.is_none_or(char::is_whitespace) && !query.contains(char::is_whitespace)).then_some(i)
}

/// Rebuild the popup from the text and cursor. Call after every edit.
pub fn refresh_ac(app: &mut App) {
    let text = app.prompt.editor.text().to_string();
    let cur = app.prompt.editor.cursor();
    let before = &text[..cur];
    let in_session = app.in_session();

    let trigger: Option<(AcKind, usize, String)> = if app.prompt.shell {
        None
    } else if cur > 0 && text.starts_with('/') && !before.contains(char::is_whitespace) {
        Some((AcKind::Slash, 0, before[1..].to_string()))
    } else {
        mention_start(before).map(|i| (AcKind::File, i, before[i + 1..].to_string()))
    };
    let Some((kind, start, query)) = trigger else {
        app.prompt.ac = None;
        app.prompt.dismissed = None;
        return;
    };
    if app.prompt.dismissed.as_deref() == Some(text.as_str()) {
        app.prompt.ac = None;
        return;
    }
    app.prompt.dismissed = None;
    if kind == AcKind::File && app.prompt.ac.is_none() {
        app.refresh_files_if_stale();
    }
    let stamp = files_stamp(&app.files);
    if app.prompt.index.built_for != stamp {
        app.prompt.index = FileIndex {
            built_for: stamp,
            entries: build_index(&app.files),
        };
    }
    let items = match kind {
        AcKind::Slash => slash_items(&app.cmds, &query, in_session),
        AcKind::File => file_items(&app.prompt.index, &query),
    };
    let keep = app
        .prompt
        .ac
        .as_ref()
        .filter(|a| a.kind == kind && a.start == start && a.query == query)
        .map(|a| (a.selected, a.offset));
    let (selected, offset) = keep.unwrap_or((0, 0));
    app.prompt.ac = Some(Autocomplete {
        kind,
        start,
        query,
        items,
        selected,
        offset,
    });
}

/// JavaScript's `localeCompare` for the ASCII names commands have: punctuation before digits
/// before letters, case ignored, then lower case first.
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |c: char| -> (u8, char) {
        if c.is_alphabetic() {
            (2, c.to_lowercase().next().unwrap_or(c))
        } else if c.is_numeric() {
            (1, c)
        } else {
            (0, c)
        }
    };
    let ka: Vec<_> = a.chars().map(key).collect();
    let kb: Vec<_> = b.chars().map(key).collect();
    ka.cmp(&kb).then_with(|| b.cmp(a))
}

fn removing_line_range(q: &str) -> &str {
    q.rfind('#').map_or(q, |i| &q[..i])
}

fn slash_items(cmds: &[Command], query: &str, in_session: bool) -> Vec<AcItem> {
    struct E<'a> {
        display: String,
        desc: &'a str,
        aliases: String,
        cmd: &'a Command,
    }
    let mut entries: Vec<E> = commands::slash_entries(cmds, in_session)
        .into_iter()
        .map(|c| E {
            display: format!("/{}", c.name.as_deref().unwrap_or("")),
            desc: &c.description,
            aliases: c
                .aliases
                .iter()
                .map(|a| format!("/{a}"))
                .collect::<Vec<_>>()
                .join(" "),
            cmd: c,
        })
        .collect();
    // what the backend added comes after opencode's own, so the first screen is opencode's
    entries.sort_by(|a, b| {
        let backend = |e: &E| e.cmd.source == Source::Backend;
        backend(a)
            .cmp(&backend(b))
            .then_with(|| locale_cmp(&a.display, &b.display))
    });
    let max = entries
        .iter()
        .map(|e| e.display.chars().count())
        .max()
        .unwrap_or(0);
    let order: Vec<usize> = if query.is_empty() {
        (0..entries.len()).collect()
    } else {
        let q = removing_line_range(query);
        let prepared: Vec<[Option<fuzzysort::Prepared>; 3]> = entries
            .iter()
            .map(|e| {
                let some = |s: &str| (!s.is_empty()).then(|| fuzzysort::Prepared::new(s));
                [some(&e.display), some(e.desc), some(&e.aliases)]
            })
            .collect();
        let keys: Vec<Vec<Option<&fuzzysort::Prepared>>> = prepared
            .iter()
            .map(|a| a.iter().map(|o| o.as_ref()).collect())
            .collect();
        let search = fuzzysort::Search::new(q);
        let typed = format!("/{query}");
        fuzzysort::go(q, &keys, 0.0, 10, |i, score| {
            // the display key has to have matched for the prefix bonus to count
            let hit = keys[i][0].is_some_and(|p| fuzzysort::single(&search, p).is_some());
            if hit && entries[i].display.starts_with(&typed) {
                score * 2.0
            } else {
                score
            }
        })
    };
    order
        .into_iter()
        .take(if query.is_empty() { usize::MAX } else { 10 })
        .map(|i| {
            let e = &entries[i];
            AcItem {
                display: format!("{:<w$}", e.display, w = max + 2),
                desc: e.desc.trim_start().to_string(),
                value: AcValue::Command(Box::new(e.cmd.clone())),
            }
        })
        .collect()
}

fn file_items(index: &FileIndex, query: &str) -> Vec<AcItem> {
    let item = |e: &FileEntry| AcItem {
        display: e.path.clone(),
        desc: String::new(),
        value: AcValue::File {
            path: e.path.clone(),
            dir: e.dir,
        },
    };
    let q = removing_line_range(query);
    if q.is_empty() {
        // No query: what is at the top of the project, folders first.
        let mut top: Vec<&FileEntry> = index
            .entries
            .iter()
            .filter(|e| e.path.trim_end_matches('/').find('/').is_none())
            .collect();
        top.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| locale_cmp(&a.path, &b.path)));
        return top.into_iter().take(10).map(item).collect();
    }
    let keys: Vec<Vec<Option<&fuzzysort::Prepared>>> = index
        .entries
        .iter()
        .map(|e| vec![Some(&e.full), Some(&e.name)])
        .collect();
    fuzzysort::go(q, &keys, 0.5, 20, |_, s| s)
        .into_iter()
        .take(10)
        .map(|i| item(&index.entries[i]))
        .collect()
}

/// Rows the popup wants above a prompt whose top is at `prompt_top`.
pub fn popup_height(app: &App, prompt_top: u16) -> u16 {
    match &app.prompt.ac {
        Some(a) => (a.items.len().max(1) as u16).min(10).min(prompt_top),
        None => 0,
    }
}

/// Paint the popup in the rows directly above `prompt` (same x and width).
pub fn draw_popup(buf: &mut Buffer, prompt: Rect, app: &mut App) {
    let h = popup_height(app, prompt.y);
    let Some(ac) = app.prompt.ac.as_mut() else {
        return;
    };
    if h == 0 || prompt.width < 4 {
        return;
    }
    let t = &app.theme;
    let area = Rect::new(prompt.x, prompt.y - h, prompt.width, h);
    fill(buf, area, Style::new().bg(t.background));
    let menu = Rect::new(area.x + 1, area.y, area.width - 2, h);
    fill(buf, menu, Style::new().bg(t.background_menu));
    for y in area.y..area.bottom() {
        put_str(
            buf,
            area.x,
            y,
            "┃",
            Style::new().fg(t.border).bg(t.background),
            area,
        );
        put_str(
            buf,
            area.right() - 1,
            y,
            "┃",
            Style::new().fg(t.border).bg(t.background),
            area,
        );
    }
    if ac.items.is_empty() {
        put_str(
            buf,
            menu.x + 1,
            menu.y,
            "No matching items",
            Style::new().fg(t.text_muted).bg(t.background_menu),
            menu,
        );
        return;
    }
    let rows = h as usize;
    if ac.selected < ac.offset {
        ac.offset = ac.selected;
    } else if ac.selected >= ac.offset + rows {
        ac.offset = ac.selected + 1 - rows;
    }
    let inner_w = menu.width.saturating_sub(2) as usize;
    for (n, item) in ac.items.iter().enumerate().skip(ac.offset).take(rows) {
        let y = menu.y + (n - ac.offset) as u16;
        let sel = n == ac.selected;
        let bg = if sel { t.primary } else { t.background_menu };
        let fg = if sel {
            t.selected_foreground(Some(t.primary))
        } else {
            t.text
        };
        let mfg = if sel { fg } else { t.text_muted };
        fill(
            buf,
            Rect::new(menu.x, y, menu.width, 1),
            Style::new().bg(bg),
        );
        let shown = if matches!(item.value, AcValue::File { .. }) {
            truncate_middle(&item.display, inner_w)
        } else {
            item.display.clone()
        };
        let x = put_str(buf, menu.x + 1, y, &shown, Style::new().fg(fg).bg(bg), menu);
        if !item.desc.is_empty() {
            // `display` is already padded to the longest command, so the description just
            // follows it after one space; the row clips it, no ellipsis.
            put_str(buf, x + 1, y, &item.desc, Style::new().fg(mfg).bg(bg), menu);
        }
    }
}

// ---- keys ------------------------------------------------------------------------------

/// What the app should do after the prompt handled a key.
#[derive(Debug, PartialEq)]
pub enum PromptEvent {
    None,
    Submit,
    Action(Action),
    /// The key was not for the prompt.
    Ignored,
}

fn ctrl(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL)
}

/// Close the popup the way opencode's `hide()` does: a half-typed `/command` goes with it.
fn hide_ac(app: &mut App) {
    let kind = app.prompt.ac.as_ref().map(|a| a.kind);
    app.prompt.ac = None;
    if kind == Some(AcKind::Slash) {
        drop_typed_slash(app);
    }
}

/// Keys that act on the popup while it is open. Returns `None` when the key falls through.
fn ac_key(app: &mut App, key: &KeyEvent) -> Option<PromptEvent> {
    let ac = app.prompt.ac.as_mut()?;
    let n = ac.items.len();
    let mv = |ac: &mut Autocomplete, d: isize| {
        if !ac.items.is_empty() {
            ac.selected = (ac.selected as isize + d).rem_euclid(ac.items.len() as isize) as usize;
        }
    };
    match key.code {
        KeyCode::Up => mv(ac, -1),
        KeyCode::Down => mv(ac, 1),
        KeyCode::Char('p') if ctrl(key) => mv(ac, -1),
        KeyCode::Char('n') if ctrl(key) => mv(ac, 1),
        KeyCode::Esc => {
            hide_ac(app);
            app.prompt.dismissed = Some(app.prompt.editor.text().to_string());
            return Some(PromptEvent::None);
        }
        KeyCode::Tab => {
            if n > 0 {
                return Some(complete(app, false));
            }
            // nothing to complete; the key still belongs to the popup
            return Some(PromptEvent::None);
        }
        KeyCode::Enter
            if !key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::ALT)
                && !ctrl(key) =>
        {
            // With nothing listed opencode's select does nothing, so enter does not submit.
            if n > 0 {
                return Some(complete(app, true));
            }
            return Some(PromptEvent::None);
        }
        _ => return None,
    }
    Some(PromptEvent::None)
}

/// Replace `[from, to)` of the text and put the cursor at `cursor`.
fn splice(app: &mut App, from: usize, to: usize, with: &str, cursor: usize) {
    let text = app.prompt.editor.text().to_string();
    let new = format!("{}{}{}", &text[..from], with, &text[to..]);
    app.prompt.editor.set_text(&new);
    app.prompt.editor.set_cursor(cursor);
}

/// Apply the selected popup entry. `enter` is true for enter, false for tab. They do the same
/// thing, except that tab on a directory goes into it instead of mentioning it.
fn complete(app: &mut App, enter: bool) -> PromptEvent {
    let Some(ac) = app.prompt.ac.clone() else {
        return PromptEvent::None;
    };
    let Some(item) = ac.items.get(ac.selected) else {
        return PromptEvent::None;
    };
    let cur = app.prompt.editor.cursor();
    match &item.value {
        AcValue::Command(c) => {
            let c = c.clone();
            if c.source == Source::Frontend {
                // hide() first: the typed `/xx` is removed, then the command runs
                app.prompt.ac = None;
                drop_typed_slash(app);
                return PromptEvent::Action(c.action.clone());
            }
            let name = c.name.clone().unwrap_or_default();
            let text = format!("/{name} ");
            let at = text.len();
            app.prompt.ac = None;
            splice(app, 0, cur, &text, at);
            refresh_ac(app);
        }
        AcValue::File { path, dir } => {
            if *dir && !enter {
                // tab goes into the directory: `@src/` and the popup lists what is inside
                let token = format!("@{path}");
                let at = ac.start + token.len();
                splice(app, ac.start, cur, &token, at);
                refresh_ac(app);
                return PromptEvent::None;
            }
            let mut token = format!("@{path}");
            if !*dir {
                if let Some((_, range)) = ac.query.split_once('#') {
                    token.push('#');
                    token.push_str(range);
                }
            }
            let text = app.prompt.editor.text().to_string();
            let tail = if text[cur..].starts_with(' ') {
                ""
            } else {
                " "
            };
            let at = ac.start + token.len() + tail.len();
            splice(app, ac.start, cur, &format!("{token}{tail}"), at);
            if !app.prompt.mentions.contains(&token) {
                app.prompt.mentions.push(token);
            }
            app.prompt.ac = None;
            refresh_ac(app);
        }
    }
    PromptEvent::None
}

fn history_load(app: &mut App, e: HistoryEntry, to_end: bool) {
    app.prompt.load_entry(e, to_end);
    refresh_ac(app);
}

/// Handle one key aimed at the prompt (everything the app did not claim first).
pub fn handle_key(app: &mut App, key: KeyEvent) -> PromptEvent {
    if let Some(ev) = ac_key(app, &key) {
        if !matches!(ev, PromptEvent::Action(_)) {
            refresh_ac(app);
        }
        return ev;
    }
    let m = key.modifiers;
    match key.code {
        KeyCode::Enter => {
            if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) || ctrl(&key) {
                app.prompt.editor.insert_newline();
                refresh_ac(app);
                return PromptEvent::None;
            }
            return PromptEvent::Submit;
        }
        KeyCode::Char('j') if ctrl(&key) => {
            app.prompt.editor.insert_newline();
            return PromptEvent::None;
        }
        KeyCode::Char('c') if ctrl(&key) => {
            if !app.prompt.is_empty() {
                app.prompt.clear_keeping_draft();
                return PromptEvent::None;
            }
            return PromptEvent::Ignored;
        }
        KeyCode::Char('v') if ctrl(&key) => {
            paste_clipboard(app);
            return PromptEvent::None;
        }
        KeyCode::Char('!')
            if app.prompt.editor.cursor() == 0 && !ctrl(&key) && !app.prompt.shell =>
        {
            app.prompt.shell = true;
            return PromptEvent::None;
        }
        KeyCode::Backspace if app.prompt.shell && app.prompt.editor.cursor() == 0 => {
            app.prompt.shell = false;
            return PromptEvent::None;
        }
        KeyCode::Backspace if delete_token_before_cursor(app) => {
            refresh_ac(app);
            return PromptEvent::None;
        }
        // a paste summary is one unit for the cursor and for delete, like opencode's virtual
        // text; moving or deleting into it a character at a time left `[Pasted ~4 lines ` behind
        // and sent that
        KeyCode::Left if m.is_empty() && step_over_token(app, false) => {
            refresh_ac(app);
            return PromptEvent::None;
        }
        KeyCode::Right if m.is_empty() && step_over_token(app, true) => {
            refresh_ac(app);
            return PromptEvent::None;
        }
        KeyCode::Delete if m.is_empty() && delete_token_after_cursor(app) => {
            refresh_ac(app);
            return PromptEvent::None;
        }
        KeyCode::Up if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            // up walks the history only from the very start of the box; from anywhere else
            // it first goes to the start (first row) or up a row
            if app.prompt.editor.cursor() != 0 {
                if app.prompt.editor.move_up() == KeyOutcome::AtTop {
                    app.prompt.editor.set_cursor(0);
                }
            } else if let Some(e) = app.prompt.history_move(-1) {
                history_load(app, e, false);
            }
            refresh_ac(app);
            return PromptEvent::None;
        }
        KeyCode::Down if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            let end = app.prompt.editor.text().len();
            if app.prompt.editor.cursor() != end {
                if app.prompt.editor.move_down() == KeyOutcome::AtBottom {
                    app.prompt.editor.set_cursor(end);
                }
            } else if let Some(e) = app.prompt.history_move(1) {
                history_load(app, e, true);
            }
            refresh_ac(app);
            return PromptEvent::None;
        }
        _ => {}
    }
    let outcome = app.prompt.editor.apply_key(key);
    if outcome == KeyOutcome::Ignored {
        return PromptEvent::Ignored;
    }
    refresh_ac(app);
    PromptEvent::None
}

/// Backspace right after a paste summary removes the whole token.
fn delete_token_before_cursor(app: &mut App) -> bool {
    let cur = app.prompt.editor.cursor();
    let text = app.prompt.editor.text().to_string();
    let hit = app
        .prompt
        .pastes
        .iter()
        .position(|p| text[..cur].ends_with(&p.token));
    let Some(i) = hit else { return false };
    let tok = app.prompt.pastes.remove(i).token;
    let start = cur - tok.len();
    splice(app, start, cur, "", start);
    true
}

/// Move the cursor over a whole paste summary next to it. False when there is none there.
fn step_over_token(app: &mut App, forward: bool) -> bool {
    let cur = app.prompt.editor.cursor();
    let text = app.prompt.editor.text();
    let to = app.prompt.pastes.iter().find_map(|p| {
        if forward {
            text[cur..]
                .starts_with(&p.token)
                .then(|| cur + p.token.len())
        } else {
            text[..cur].ends_with(&p.token).then(|| cur - p.token.len())
        }
    });
    match to {
        Some(at) => {
            app.prompt.editor.set_cursor(at);
            true
        }
        None => false,
    }
}

/// Delete removes a paste summary that starts at the cursor whole.
fn delete_token_after_cursor(app: &mut App) -> bool {
    let cur = app.prompt.editor.cursor();
    let text = app.prompt.editor.text().to_string();
    let Some(i) = app
        .prompt
        .pastes
        .iter()
        .position(|p| text[cur..].starts_with(&p.token))
    else {
        return false;
    };
    let tok = app.prompt.pastes.remove(i).token;
    splice(app, cur, cur + tok.len(), "", cur);
    true
}

/// A pasted file path as terminals send it: quotes around it, `\ ` for spaces, or `file://`.
fn pasted_filepath(value: &str) -> String {
    let raw = value.trim_matches(|c| c == '\'' || c == '"');
    if let Some(rest) = raw.strip_prefix("file://") {
        return percent_decode(rest);
    }
    let mut out = String::new();
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// `%20` and friends in a `file://` URL.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttachKind {
    Image,
    Pdf,
    Svg,
}

fn attach_kind(path: &str) -> Option<AttachKind> {
    match std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" => Some(AttachKind::Image),
        "pdf" => Some(AttachKind::Pdf),
        "svg" => Some(AttachKind::Svg),
        _ => None,
    }
}

/// Insert a placeholder token (plus a space) that stands for `text` until the prompt is sent.
fn insert_placeholder(app: &mut App, token: String, text: String) {
    app.prompt.editor.paste(&format!("{token} "));
    app.prompt.pastes.push(Paste { token, text });
}

/// How many attachments of this kind the prompt already holds (the `N` in `[Image N]`).
fn attached(app: &App, label: &str) -> usize {
    app.prompt
        .pastes
        .iter()
        .filter(|p| p.token.starts_with(label))
        .count()
}

pub fn handle_paste(app: &mut App, raw: &str) {
    let s = normalize_newlines(raw).into_owned();
    let content = s.trim();
    let path = pasted_filepath(content);
    let is_url = path.starts_with("http://") || path.starts_with("https://");
    if !is_url && std::path::Path::new(&path).is_file() {
        match attach_kind(&path) {
            Some(AttachKind::Svg) => {
                if let Ok(svg) = std::fs::read_to_string(&path) {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map_or(String::new(), |n| n.to_string_lossy().into_owned());
                    insert_placeholder(app, format!("[SVG: {name}]"), svg);
                    refresh_ac(app);
                    return;
                }
            }
            Some(kind) => {
                let label = if kind == AttachKind::Pdf {
                    "[PDF "
                } else {
                    "[Image "
                };
                let token = format!("{label}{}]", attached(app, label) + 1);
                // wizard reads `@path` out of the prompt text; ACP has no image block
                insert_placeholder(app, token, format!("@{path}"));
                refresh_ac(app);
                return;
            }
            None => {}
        }
    }
    let lines = content.matches('\n').count() + 1;
    if (lines >= PASTE_LINES || content.chars().count() > PASTE_CHARS) && app.paste_summary {
        let mut token = format!("[Pasted ~{lines} lines]");
        // Two identical tokens would be indistinguishable when expanding.
        if app.prompt.pastes.iter().any(|p| p.token == token) {
            token = format!("[Pasted ~{lines} lines #{}]", app.prompt.pastes.len() + 1);
        }
        insert_placeholder(app, token, content.to_string());
    } else {
        app.prompt.editor.paste(&s);
    }
    refresh_ac(app);
}

/// ctrl+v: pull text or an image off the system clipboard. Terminals cannot deliver image
/// bytes through a bracketed paste, so this is how a copied screenshot gets in.
fn paste_clipboard(app: &mut App) {
    // the helpers can take a second each; the answer comes back as `Msg::Clip`
    crate::clipboard::read_in_background(app.msg_tx.clone());
}

/// What the clipboard helper found, delivered by `Msg::Clip`.
pub fn finish_paste_clipboard(app: &mut App, clip: Option<crate::clipboard::Clip>) {
    match clip {
        Some(crate::clipboard::Clip::Text(t)) => handle_paste(app, &t),
        Some(crate::clipboard::Clip::Image { path }) => {
            let token = format!("[Image {}]", attached(app, "[Image ") + 1);
            insert_placeholder(app, token, format!("@{path}"));
            refresh_ac(app);
        }
        None => app.toast(
            tuikit::theme::Variant::Info,
            "The clipboard has nothing to paste",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_round_trips_on_disk_with_mode_and_pastes() {
        let dir = std::env::temp_dir().join(format!("openw-hist-{}", std::process::id()));
        let mut p = PromptState::new(0, Some(dir.clone()));
        p.editor.set_text("first");
        p.remember();
        p.editor.set_text("see [Pasted ~3 lines]");
        p.pastes.push(Paste {
            token: "[Pasted ~3 lines]".into(),
            text: "a\nb\nc".into(),
        });
        p.remember();
        p.clear();
        p.editor.set_text("ls");
        p.shell = true;
        p.remember();
        let again = PromptState::new(0, Some(dir.clone()));
        let got: Vec<(&str, bool, usize)> = again
            .history
            .iter()
            .map(|e| (e.input.as_str(), e.shell, e.pastes.len()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("first", false, 0),
                ("see [Pasted ~3 lines]", false, 1),
                ("ls", true, 0)
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn history_move_follows_opencode() {
        let mut p = PromptState::new(0, None);
        for t in ["one", "two", "three"] {
            p.editor.set_text(t);
            p.remember();
        }
        p.clear();
        // older from an empty box
        assert_eq!(p.history_move(-1).unwrap().input, "three");
        p.editor.set_text("three");
        assert_eq!(p.history_move(-1).unwrap().input, "two");
        // text that is not the entry it was loaded from blocks moving
        p.editor.set_text("edited");
        assert!(p.history_move(-1).is_none());
        p.editor.set_text("two");
        assert_eq!(p.history_move(1).unwrap().input, "three");
        p.editor.set_text("three");
        // past the newest entry the box empties
        assert_eq!(p.history_move(1).unwrap().input, "");
        assert!(p.history_move(1).is_none());
    }

    #[test]
    fn a_long_cleared_draft_is_kept() {
        let mut p = PromptState::new(0, None);
        p.editor.set_text("short");
        p.clear_keeping_draft();
        assert!(p.history.is_empty());
        p.editor
            .set_text("a draft that is longer than twenty chars");
        p.clear_keeping_draft();
        assert_eq!(p.history.len(), 1);
    }

    #[test]
    fn pasted_paths_lose_quotes_escapes_and_file_urls() {
        assert_eq!(pasted_filepath("'/a/b c.png'"), "/a/b c.png");
        assert_eq!(pasted_filepath("/a/b\\ c.png"), "/a/b c.png");
        assert_eq!(pasted_filepath("file:///a/b%20c.png"), "/a/b c.png");
        assert_eq!(pasted_filepath("/plain.png"), "/plain.png");
    }

    #[test]
    fn expanded_swaps_paste_tokens() {
        let mut p = PromptState::new(0, None);
        p.editor.set_text("see [Pasted ~3 lines] ok");
        p.pastes.push(Paste {
            token: "[Pasted ~3 lines]".into(),
            text: "a\nb\nc".into(),
        });
        assert_eq!(p.expanded(), "see a\nb\nc ok");
    }

    #[test]
    fn theme_colors_are_the_opencode_ones() {
        use crate::app::AgentKind;
        let t = tuikit::Theme::default_theme(tuikit::Mode::Dark);
        assert_eq!(AgentKind::Build.color(&t), Color::Rgb(0x5c, 0x9c, 0xf5));
        assert_eq!(AgentKind::Plan.color(&t), Color::Rgb(0xf5, 0xa7, 0x42));
    }
}
