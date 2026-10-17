//! Multi-line prompt input model: text, cursor, readline editing, soft-wrap aware vertical
//! movement, undo/redo, and a history ring. The model is not a widget; [`Editor::render`] draws
//! it into a rect and reports the cursor and the height it wants so the host can grow its box.
//!
//! Enter, Tab and Escape are never consumed by [`Editor::apply_key`]. The host decides what
//! submits and what inserts a newline (`insert_newline`).

use crate::paint::put_str;
use crate::width::{
    cell_for_mode, display_width, grapheme_width, tab_width as tab_cols, wrap_cells, Cell, WrapMode,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::collections::VecDeque;
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

const UNDO_LIMIT: usize = 200;
const HISTORY_LIMIT: usize = 500;

/// What [`Editor::apply_key`] did with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Not an editing key; the host should handle it.
    Ignored,
    /// Consumed (text or cursor may have changed).
    Handled,
    /// Up was pressed on the first visual row. Nothing moved; the host may walk history.
    AtTop,
    /// Down was pressed on the last visual row.
    AtBottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    cursor: usize,
}

#[derive(Clone, Debug)]
pub struct VisualRow {
    /// Byte range of the row in the text, including hanging spaces up to `next`.
    pub start: usize,
    /// End of the drawn content (hanging spaces excluded on wrapped rows).
    pub end: usize,
    /// Start of the following row, or the end of the logical line.
    pub next: usize,
    /// Last row of its logical line.
    pub last_in_line: bool,
}

#[derive(Debug, Default)]
struct History {
    entries: VecDeque<String>,
    pos: Option<usize>,
    draft: String,
    prefix: String,
    /// Pi mode: where the cursor was in the draft, which comes back with it.
    draft_cursor: usize,
}

#[derive(Debug)]
pub struct Editor {
    text: String,
    cursor: usize,
    goal_col: Option<usize>,
    width: usize,
    scroll: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_kind: Option<EditKind>,
    kill: String,
    history: History,
    /// Wrap after `/ - .` and the like, as opencode's textarea does.
    punct_wrap: bool,
    /// Wrap like Pi's editor (see [`WrapMode::PiEditor`]); wins over `punct_wrap`.
    pi_wrap: bool,
    /// Pi's key semantics, see [`Editor::set_pi_keys`].
    pi_keys: bool,
    pi: PiState,
}

/// What Pi's editor remembers between keys that the opencode-style editor does not.
#[derive(Debug, Default)]
struct PiState {
    /// Killed text, newest last. Consecutive kills grow one entry.
    ring: Vec<String>,
    /// The last thing done was a kill, so the next one accumulates into the same entry.
    kill_chain: bool,
    /// Byte range of the text a yank just inserted, which `yank_pop` replaces.
    yanked: Option<(usize, usize)>,
    /// Paste markers, which words and kills treat as one unit.
    atomic: Vec<String>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

fn is_word(g: &str) -> bool {
    g.chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

impl Editor {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            goal_col: None,
            width: 0,
            scroll: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_kind: None,
            kill: String::new(),
            history: History::default(),
            punct_wrap: false,
            pi_wrap: false,
            pi_keys: false,
            pi: PiState::default(),
        }
    }

    pub fn with_text(text: &str) -> Self {
        let mut e = Self::new();
        e.text = sanitize_input(text);
        e.cursor = e.text.len();
        e
    }

    // ---- inspection -----------------------------------------------------------------------

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Break rows after `/ - . , ; : ? ( ) [ ] { } \\` too, not only at spaces.
    pub fn set_punct_wrap(&mut self, on: bool) {
        self.punct_wrap = on;
    }

    /// Wrap the way Pi's editor does: spaces count toward the row width.
    pub fn set_pi_wrap(&mut self, on: bool) {
        self.pi_wrap = on;
    }

    /// Pi's editing behaviour where it differs from opencode's: word motion and word kills that
    /// stop at punctuation and at line ends, a kill ring that grows over consecutive kills with
    /// `alt+y` to cycle it, and history that puts the cursor at the start going up and the end
    /// going down and only starts when the editor is empty or the cursor is at the very start.
    /// Additive: with this off nothing changes.
    pub fn set_pi_keys(&mut self, on: bool) {
        self.pi_keys = on;
    }

    /// Text that is one unit to word motion and kills (Pi's paste markers). Only read in Pi mode.
    pub fn set_atomic_markers(&mut self, markers: Vec<String>) {
        self.pi.atomic = markers;
    }

    /// First visual row drawn by the last `render`.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Byte offset of the cursor (always on a grapheme boundary).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    /// `(line, column)` of the cursor, both zero-based; the column counts graphemes.
    pub fn line_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.matches('\n').count();
        let ls = before.rfind('\n').map_or(0, |i| i + 1);
        (line, before[ls..].graphemes(true).count())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Wrap width used for vertical movement before the first render.
    pub fn set_wrap_width(&mut self, w: u16) {
        self.width = w as usize;
    }

    // ---- boundaries -----------------------------------------------------------------------

    fn prev_boundary(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let mut c = GraphemeCursor::new(pos, self.text.len(), true);
        c.prev_boundary(&self.text, 0).ok().flatten().unwrap_or(0)
    }

    fn next_boundary(&self, pos: usize) -> usize {
        if pos >= self.text.len() {
            return self.text.len();
        }
        let mut c = GraphemeCursor::new(pos, self.text.len(), true);
        c.next_boundary(&self.text, 0)
            .ok()
            .flatten()
            .unwrap_or(self.text.len())
    }

    fn line_start(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.text[pos..]
            .find('\n')
            .map_or(self.text.len(), |i| pos + i)
    }

    fn word_left_from(&self, mut pos: usize) -> usize {
        while pos > 0 {
            let p = self.prev_boundary(pos);
            if is_word(&self.text[p..pos]) {
                break;
            }
            pos = p;
        }
        while pos > 0 {
            let p = self.prev_boundary(pos);
            if !is_word(&self.text[p..pos]) {
                break;
            }
            pos = p;
        }
        pos
    }

    fn word_right_from(&self, mut pos: usize) -> usize {
        let n = self.text.len();
        while pos < n {
            let q = self.next_boundary(pos);
            if is_word(&self.text[pos..q]) {
                break;
            }
            pos = q;
        }
        while pos < n {
            let q = self.next_boundary(pos);
            if !is_word(&self.text[pos..q]) {
                break;
            }
            pos = q;
        }
        pos
    }

    // ---- undo bookkeeping -----------------------------------------------------------------

    fn record(&mut self, kind: EditKind) {
        if self.last_kind != Some(kind) || kind == EditKind::Other {
            self.undo.push(Snapshot {
                text: self.text.clone(),
                cursor: self.cursor,
            });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_kind = Some(kind);
        self.goal_col = None;
        self.history.pos = None;
        self.pi.kill_chain = false;
        self.pi.yanked = None;
    }

    fn moved(&mut self) {
        self.last_kind = None;
        // Pi keeps browsing history while the cursor moves inside the recalled text
        if !self.pi_keys {
            self.history.pos = None;
        }
        self.pi.kill_chain = false;
        self.pi.yanked = None;
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot {
            text: std::mem::replace(&mut self.text, s.text),
            cursor: self.cursor,
        });
        self.cursor = s.cursor.min(self.text.len());
        self.last_kind = None;
        self.goal_col = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot {
            text: std::mem::replace(&mut self.text, s.text),
            cursor: self.cursor,
        });
        self.cursor = s.cursor.min(self.text.len());
        self.last_kind = None;
        self.goal_col = None;
        true
    }

    // ---- editing --------------------------------------------------------------------------

    /// Insert one character. Control characters other than tab and newline are refused.
    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            return self.insert_newline();
        }
        if c.is_control() && c != '\t' {
            return;
        }
        self.record(EditKind::Insert);
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        // Undo steps by word: a space ends the group.
        if c.is_whitespace() {
            self.last_kind = None;
        }
    }

    pub fn insert_newline(&mut self) {
        self.record(EditKind::Other);
        self.text.insert(self.cursor, '\n');
        self.cursor += 1;
        self.last_kind = None;
    }

    /// Insert text as a single undo step. `\r\n` and `\r` become `\n`; other control
    /// characters are dropped.
    pub fn paste(&mut self, s: &str) {
        let clean = sanitize_input(s);
        if clean.is_empty() {
            return;
        }
        self.record(EditKind::Other);
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.last_kind = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.record(EditKind::Delete);
        let p = self.prev_boundary(self.cursor);
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.record(EditKind::Delete);
        let q = self.next_boundary(self.cursor);
        self.text.replace_range(self.cursor..q, "");
    }

    fn kill_range(&mut self, a: usize, b: usize) {
        if a >= b {
            return;
        }
        self.record(EditKind::Other);
        self.kill = self.text[a..b].to_string();
        self.text.replace_range(a..b, "");
        self.cursor = a;
        self.last_kind = None;
    }

    pub fn delete_word_back(&mut self) {
        if self.pi_keys {
            return self.pi_delete_word_back();
        }
        let a = self.word_left_from(self.cursor);
        self.kill_range(a, self.cursor);
    }

    pub fn delete_word_forward(&mut self) {
        if self.pi_keys {
            return self.pi_delete_word_forward();
        }
        let b = self.word_right_from(self.cursor);
        self.kill_range(self.cursor, b);
    }

    /// Ctrl+K: to the end of the line; at the end of a line, join with the next one.
    pub fn kill_to_line_end(&mut self) {
        if self.pi_keys {
            return self.pi_kill_to_line_end();
        }
        let le = self.line_end(self.cursor);
        let end = if le == self.cursor {
            (le + 1).min(self.text.len())
        } else {
            le
        };
        self.kill_range(self.cursor, end);
    }

    /// Ctrl+U: to the start of the line; at the start of a line, join with the previous one.
    pub fn kill_to_line_start(&mut self) {
        if self.pi_keys {
            return self.pi_kill_to_line_start();
        }
        let ls = self.line_start(self.cursor);
        let start = if ls == self.cursor {
            ls.saturating_sub(1)
        } else {
            ls
        };
        self.kill_range(start, self.cursor);
    }

    pub fn yank(&mut self) {
        if self.pi_keys {
            return self.pi_yank();
        }
        if self.kill.is_empty() {
            return;
        }
        let k = self.kill.clone();
        self.paste(&k);
    }

    /// Replace the whole text (history recall, external edit). Cursor goes to the end.
    pub fn set_text(&mut self, s: &str) {
        let clean = sanitize_input(s);
        if clean == self.text {
            self.cursor = self.text.len();
            return;
        }
        self.record(EditKind::Other);
        self.text = clean;
        self.cursor = self.text.len();
        self.last_kind = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Take the text for submission: remembers it in history, empties the editor and drops
    /// undo state.
    pub fn submit(&mut self) -> String {
        let out = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.scroll = 0;
        self.undo.clear();
        self.redo.clear();
        self.last_kind = None;
        self.goal_col = None;
        self.history_push(&out);
        self.history.pos = None;
        out
    }

    // ---- movement -------------------------------------------------------------------------

    pub fn move_left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
        self.goal_col = None;
        self.moved();
    }

    pub fn move_right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
        self.goal_col = None;
        self.moved();
    }

    pub fn word_left(&mut self) {
        if self.pi_keys {
            return self.pi_word_left();
        }
        self.cursor = self.word_left_from(self.cursor);
        self.goal_col = None;
        self.moved();
    }

    pub fn word_right(&mut self) {
        if self.pi_keys {
            return self.pi_word_right();
        }
        self.cursor = self.word_right_from(self.cursor);
        self.goal_col = None;
        self.moved();
    }

    pub fn move_line_start(&mut self) {
        self.cursor = self.line_start(self.cursor);
        self.goal_col = None;
        self.moved();
    }

    pub fn move_line_end(&mut self) {
        self.cursor = self.line_end(self.cursor);
        self.goal_col = None;
        self.moved();
    }

    pub fn move_doc_start(&mut self) {
        self.cursor = 0;
        self.goal_col = None;
        self.moved();
    }

    pub fn move_doc_end(&mut self) {
        self.cursor = self.text.len();
        self.goal_col = None;
        self.moved();
    }

    pub fn set_cursor(&mut self, byte: usize) {
        let mut p = byte.min(self.text.len());
        while !self.text.is_char_boundary(p) {
            p -= 1;
        }
        // Snap back to a grapheme boundary.
        let mut c = GraphemeCursor::new(p, self.text.len(), true);
        if !c.is_boundary(&self.text, 0).unwrap_or(true) {
            p = self.prev_boundary(p);
        }
        self.cursor = p;
        self.goal_col = None;
        self.moved();
    }

    // ---- layout ---------------------------------------------------------------------------

    /// Visual rows at `width` (0 means no wrapping). The cursor is needed because a cursor at
    /// the end of a completely full row gets a row of its own to sit on.
    pub fn layout(&self, width: usize) -> Vec<VisualRow> {
        let mut rows = Vec::new();
        let mut ls = 0;
        loop {
            let le = self.line_end(ls);
            let line = &self.text[ls..le];
            if line.is_empty() || width == 0 {
                rows.push(VisualRow {
                    start: ls,
                    end: le,
                    next: le,
                    last_in_line: true,
                });
            } else {
                let gs: Vec<(usize, &str)> = line.grapheme_indices(true).collect();
                let mode = if self.pi_wrap {
                    WrapMode::PiEditor
                } else if self.punct_wrap {
                    WrapMode::WordPunct
                } else {
                    WrapMode::Word
                };
                let cells: Vec<Cell> = gs.iter().map(|(_, g)| tab_cell(g, mode)).collect();
                let starts = wrap_cells(&cells, width, mode);
                let n = starts.len();
                for (r, &st) in starts.iter().enumerate() {
                    let b0 = gs[st].0;
                    let last = r + 1 == n;
                    let b1 = if last {
                        line.len()
                    } else {
                        gs[starts[r + 1]].0
                    };
                    let mut end = b1;
                    if !last {
                        end = b0 + line[b0..b1].trim_end_matches([' ', '\t']).len();
                        if end == b0 {
                            end = b1; // a row of nothing but spaces keeps them
                        }
                    }
                    rows.push(VisualRow {
                        start: ls + b0,
                        end: ls + end,
                        next: ls + b1,
                        last_in_line: last,
                    });
                }
                // Cursor parked after a full last row needs somewhere to be.
                let last = rows.last().cloned().unwrap();
                if self.cursor == le
                    && le > ls
                    && row_width(&self.text[last.start..last.end]) >= width
                {
                    rows.push(VisualRow {
                        start: le,
                        end: le,
                        next: le,
                        last_in_line: true,
                    });
                    let n = rows.len();
                    rows[n - 2].last_in_line = false;
                }
            }
            if le >= self.text.len() {
                break;
            }
            ls = le + 1;
        }
        rows
    }

    fn row_of(&self, rows: &[VisualRow], pos: usize) -> usize {
        for (i, r) in rows.iter().enumerate() {
            if pos >= r.start && (pos < r.next || (r.last_in_line && pos <= r.next)) {
                return i;
            }
        }
        rows.len().saturating_sub(1)
    }

    fn col_in_row(&self, row: &VisualRow, pos: usize) -> usize {
        row_width(&self.text[row.start..pos.clamp(row.start, row.next)])
    }

    /// Byte offset in `row` closest to column `goal` without leaving the row.
    fn pos_at_col(&self, row: &VisualRow, goal: usize) -> usize {
        let mut col = 0;
        let mut pos = row.start;
        for (b, g) in self.text[row.start..row.end].grapheme_indices(true) {
            let w = tab_width(g);
            if col + w > goal {
                // The goal falls inside this grapheme: pick the nearer edge.
                pos = if w > 0 && goal - col >= w.div_ceil(2) {
                    row.start + b + g.len()
                } else {
                    row.start + b
                };
                break;
            }
            col += w;
            pos = row.start + b + g.len();
        }
        // On a wrapped row the cursor must stay on this row, so it cannot sit at the very end
        // when that offset belongs to the next row.
        let max = if row.last_in_line || row.end < row.next {
            row.end
        } else {
            self.prev_boundary(row.end).max(row.start)
        };
        pos.min(max)
    }

    pub fn move_up(&mut self) -> KeyOutcome {
        self.move_vertical(-1)
    }

    pub fn move_down(&mut self) -> KeyOutcome {
        self.move_vertical(1)
    }

    fn move_vertical(&mut self, dir: isize) -> KeyOutcome {
        let rows = self.layout(self.width);
        let cur = self.row_of(&rows, self.cursor);
        let target = cur as isize + dir;
        if target < 0 {
            return KeyOutcome::AtTop;
        }
        if target as usize >= rows.len() {
            return KeyOutcome::AtBottom;
        }
        let goal = self
            .goal_col
            .unwrap_or_else(|| self.col_in_row(&rows[cur], self.cursor));
        self.cursor = self.pos_at_col(&rows[target as usize], goal);
        self.goal_col = Some(goal);
        self.last_kind = None;
        KeyOutcome::Handled
    }

    // ---- keys -----------------------------------------------------------------------------

    /// Apply a readline-style key. See the module docs for what is left to the host.
    pub fn apply_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if key.kind == KeyEventKind::Release {
            return KeyOutcome::Ignored;
        }
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let alt = m.contains(KeyModifiers::ALT);
        let shift = m.contains(KeyModifiers::SHIFT);
        use KeyCode::*;
        match key.code {
            Char(c) if ctrl && !alt => match c.to_ascii_lowercase() {
                'a' => self.move_line_start(),
                'e' => self.move_line_end(),
                'b' => self.move_left(),
                'f' => self.move_right(),
                'k' => self.kill_to_line_end(),
                'u' => self.kill_to_line_start(),
                'w' => self.delete_word_back(),
                'd' => {
                    if self.text.is_empty() {
                        return KeyOutcome::Ignored;
                    }
                    self.delete_forward();
                }
                // Pi gets ctrl+h as its own key (the kitty protocol) and ignores it; a terminal
                // whose backspace sends 0x08 arrives as Backspace, see input.rs
                'h' if self.pi_keys => return KeyOutcome::Ignored,
                'h' => self.backspace(),
                'y' => self.yank(),
                // opencode's input_redo is ctrl+. (and super+shift+z, which has no terminal form)
                '.' => {
                    self.redo();
                }
                'z' if shift => {
                    self.redo();
                }
                'z' | '_' | '-' | '7' => {
                    self.undo();
                }
                _ => return KeyOutcome::Ignored,
            },
            Char(c) if alt && !ctrl => match c.to_ascii_lowercase() {
                'b' => self.word_left(),
                'f' => self.word_right(),
                'd' => self.delete_word_forward(),
                'h' => self.delete_word_back(),
                'y' if self.pi_keys => self.pi_yank_pop(),
                '<' => self.move_doc_start(),
                '>' => self.move_doc_end(),
                _ => return KeyOutcome::Ignored,
            },
            // AltGr reports as ctrl+alt on some layouts and produces a real character.
            Char(c) if ctrl && alt => self.insert_char(c),
            Char(c) if !ctrl && !alt => self.insert_char(c),
            Backspace if alt || ctrl => self.delete_word_back(),
            Backspace => self.backspace(),
            Delete if alt || ctrl => self.delete_word_forward(),
            Delete => self.delete_forward(),
            Left if ctrl || alt => self.word_left(),
            Right if ctrl || alt => self.word_right(),
            Left => self.move_left(),
            Right => self.move_right(),
            Home if ctrl => self.move_doc_start(),
            End if ctrl => self.move_doc_end(),
            Home => self.move_line_start(),
            End => self.move_line_end(),
            Up if !ctrl && !alt && self.pi_keys => return self.pi_up(),
            Down if !ctrl && !alt && self.pi_keys => return self.pi_down(),
            Up if !ctrl && !alt => return self.move_up(),
            Down if !ctrl && !alt => return self.move_down(),
            _ => return KeyOutcome::Ignored,
        }
        KeyOutcome::Handled
    }

    // ---- history --------------------------------------------------------------------------

    /// Add an entry. Empty entries and a repeat of the newest entry are skipped.
    pub fn history_push(&mut self, entry: &str) {
        // Pi stores the trimmed text
        let entry = if self.pi_keys { entry.trim() } else { entry };
        if entry.trim().is_empty() || self.history.entries.back().is_some_and(|e| e == entry) {
            return;
        }
        self.history.entries.push_back(entry.to_string());
        while self.history.entries.len() > HISTORY_LIMIT {
            self.history.entries.pop_front();
        }
    }

    pub fn history_entries(&self) -> impl Iterator<Item = &str> {
        self.history.entries.iter().map(String::as_str)
    }

    pub fn set_history(&mut self, entries: impl IntoIterator<Item = String>) {
        self.history.entries.clear();
        for e in entries {
            self.history_push(&e);
        }
        self.history.pos = None;
    }

    /// Older entry that starts with whatever was left of the cursor when navigation began.
    /// Returns false when there is nothing older.
    pub fn history_prev(&mut self) -> bool {
        if self.pi_keys {
            return self.pi_history_step(-1);
        }
        let h = &mut self.history;
        let from = match h.pos {
            Some(p) => p,
            None => {
                h.draft = self.text.clone();
                h.prefix = self.text[..self.cursor].to_string();
                h.entries.len()
            }
        };
        let found = (0..from)
            .rev()
            .find(|&i| h.entries[i].starts_with(&h.prefix) && h.entries[i] != self.text);
        let Some(i) = found else { return false };
        let entry = h.entries[i].clone();
        self.load_history(entry, Some(i));
        true
    }

    /// Start a history walk from a draft nothing in the history starts with. The walk then goes
    /// through every entry; `history_next` brings the draft back. False when already walking
    /// (the prefix walk ran out; going further would lose the place) or when there is nothing.
    pub fn history_prev_any(&mut self) -> bool {
        if self.pi_keys {
            return self.pi_history_step(-1);
        }
        let h = &mut self.history;
        if h.pos.is_some() {
            return false;
        }
        let Some(i) = (0..h.entries.len())
            .rev()
            .find(|&i| h.entries[i] != self.text)
        else {
            return false;
        };
        h.draft = self.text.clone();
        h.prefix = String::new();
        let entry = h.entries[i].clone();
        self.load_history(entry, Some(i));
        true
    }

    /// Newer entry, or the original draft once past the newest. False when not navigating.
    pub fn history_next(&mut self) -> bool {
        if self.pi_keys {
            return self.pi_history_step(1);
        }
        let h = &mut self.history;
        let Some(p) = h.pos else { return false };
        let found = (p + 1..h.entries.len()).find(|&i| h.entries[i].starts_with(&h.prefix));
        match found {
            Some(i) => {
                let entry = h.entries[i].clone();
                self.load_history(entry, Some(i));
            }
            None => {
                let draft = std::mem::take(&mut h.draft);
                self.load_history(draft, None);
            }
        }
        true
    }

    fn load_history(&mut self, text: String, pos: Option<usize>) {
        let prefix = self.history.prefix.clone();
        let draft = self.history.draft.clone();
        self.text = text;
        self.cursor = self.text.len();
        self.goal_col = None;
        self.last_kind = None;
        self.history.pos = pos;
        self.history.prefix = prefix;
        self.history.draft = draft;
    }

    // ---- rendering ------------------------------------------------------------------------

    /// Rows the content needs at `width`, uncapped. Ask this before choosing the box height.
    pub fn needed_height(&self, width: u16) -> u16 {
        self.layout(width as usize).len().min(u16::MAX as usize) as u16
    }

    /// Height for a box that grows from `min` to `max` rows.
    pub fn grown_height(&self, width: u16, min: u16, max: u16) -> u16 {
        self.needed_height(width).clamp(min.min(max), max)
    }

    /// Draw into `area`. Scrolls to keep the cursor visible when the content is taller than the
    /// area. Returns the absolute cursor cell (if it is inside `area`) and the full height.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, style: &EditorStyle) -> RenderInfo {
        self.width = area.width as usize;
        let rows = self.layout(self.width);
        let needed = rows.len().min(u16::MAX as usize) as u16;
        if area.is_empty() {
            return RenderInfo {
                cursor: None,
                needed_height: needed,
            };
        }
        let cur_row = self.row_of(&rows, self.cursor);
        let h = area.height as usize;
        if cur_row < self.scroll {
            self.scroll = cur_row;
        } else if cur_row >= self.scroll + h {
            self.scroll = cur_row + 1 - h;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(h));

        if self.text.is_empty() {
            if let Some((ph, ph_style)) = &style.placeholder {
                put_str(buf, area.x, area.y, ph, *ph_style, area);
            }
        } else {
            for (i, row) in rows.iter().enumerate().skip(self.scroll).take(h) {
                let y = area.y + (i - self.scroll) as u16;
                let mut x = area.x;
                for g in self.text[row.start..row.end].graphemes(true) {
                    if g == "\t" {
                        x = put_str(buf, x, y, &" ".repeat(tab_cols()), style.text, area);
                    } else {
                        x = put_str(buf, x, y, g, style.text, area);
                    }
                }
            }
        }
        let col = self
            .col_in_row(&rows[cur_row], self.cursor)
            .min(self.width.saturating_sub(1));
        let cy = cur_row.saturating_sub(self.scroll);
        let cursor = (cy < h).then(|| (area.x + col as u16, area.y + cy as u16));
        RenderInfo {
            cursor,
            needed_height: needed,
        }
    }

    /// Move the cursor to a clicked cell of the last rendered `area`.
    pub fn click(&mut self, area: Rect, x: u16, y: u16) {
        if x < area.x || y < area.y || y >= area.bottom() || self.width == 0 {
            return;
        }
        let rows = self.layout(self.width);
        let ri = (y - area.y) as usize + self.scroll;
        let Some(row) = rows.get(ri) else {
            self.move_doc_end();
            return;
        };
        self.cursor = self.pos_at_col(row, (x - area.x) as usize);
        self.goal_col = None;
        self.moved();
    }
}

#[derive(Clone, Debug, Default)]
pub struct EditorStyle {
    pub text: Style,
    pub placeholder: Option<(String, Style)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderInfo {
    /// Absolute terminal cell for the cursor, `None` if it is outside the drawn rows.
    pub cursor: Option<(u16, u16)>,
    /// Rows the whole content needs at the rendered width.
    pub needed_height: u16,
}

fn tab_cell(g: &str, mode: WrapMode) -> Cell {
    let mut c = cell_for_mode(g, mode);
    if g == "\t" {
        c.width = tab_cols() as u8;
    }
    c
}

fn tab_width(g: &str) -> usize {
    if g == "\t" {
        tab_cols()
    } else {
        grapheme_width(g)
    }
}

fn row_width(s: &str) -> usize {
    display_width(s)
}

/// Pasted and programmatic text: `\n` newlines, tabs kept, other control characters dropped.
fn sanitize_input(s: &str) -> String {
    let s = crate::width::normalize_newlines(s);
    s.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

// ---- Pi's editor ---------------------------------------------------------------------------
//
// Ports of pi-tui's `word-navigation.js`, the kill ring in `kill-ring.js` and the history and
// arrow-key rules of `components/editor.js`. Word segments come from UAX #29 like Pi's
// `Intl.Segmenter({granularity: "word"})`; a segment is word-like when it holds a letter or a
// digit, and a word-like segment is still cut at ASCII punctuation (`foo.bar` is two stops).

/// `PUNCTUATION_REGEX` of pi-tui's `utils.js`.
fn is_pi_punct(c: char) -> bool {
    "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`".contains(c)
}

#[derive(Clone, Copy, Debug)]
struct PiSeg<'a> {
    text: &'a str,
    atomic: bool,
}

impl PiSeg<'_> {
    /// `isWhitespaceChar`: `/\s/` is not anchored, so a segment with any space in it counts.
    fn is_space(&self) -> bool {
        self.text.chars().any(char::is_whitespace)
    }
    fn is_word(&self) -> bool {
        self.text.chars().any(char::is_alphanumeric)
    }
}

impl Editor {
    /// Word segments of `s` with every paste marker merged into one atomic segment, as Pi's
    /// `segmentWithMarkers` does.
    fn pi_segments<'a>(&self, s: &'a str) -> Vec<PiSeg<'a>> {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for m in &self.pi.atomic {
            if m.is_empty() {
                continue;
            }
            let mut from = 0;
            while let Some(i) = s[from..].find(m.as_str()) {
                spans.push((from + i, from + i + m.len()));
                from += i + m.len();
            }
        }
        spans.sort_unstable();
        let mut out = Vec::new();
        let mut span = 0;
        for (i, seg) in s.split_word_bound_indices() {
            while span < spans.len() && spans[span].1 <= i {
                span += 1;
            }
            match spans.get(span) {
                Some(&(a, b)) if i >= a && i < b => {
                    if i == a || out.last().is_none_or(|l: &PiSeg| !l.atomic) {
                        out.push(PiSeg {
                            text: &s[a..b],
                            atomic: true,
                        });
                    }
                }
                _ => out.push(PiSeg {
                    text: seg,
                    atomic: false,
                }),
            }
        }
        out
    }

    /// `findWordBackward`: where one word left of the end of `before` lands, in bytes.
    fn pi_find_word_backward(&self, before: &str) -> usize {
        let mut segs = self.pi_segments(before);
        let mut cur = before.len();
        while let Some(l) = segs.last() {
            if l.atomic || !l.is_space() {
                break;
            }
            cur -= l.text.len();
            segs.pop();
        }
        let Some(last) = segs.last().copied() else {
            return cur;
        };
        if last.atomic {
            cur -= last.text.len();
        } else if last.is_word() {
            // inside one word-like segment, stop after its last punctuation character
            match last
                .text
                .char_indices()
                .rev()
                .find(|(_, c)| is_pi_punct(*c))
            {
                None => cur -= last.text.len(),
                Some((i, c)) => cur -= last.text.len() - (i + c.len_utf8()),
            }
        } else {
            while let Some(l) = segs.last() {
                if l.atomic || l.is_word() || l.is_space() {
                    break;
                }
                cur -= l.text.len();
                segs.pop();
            }
        }
        cur
    }

    /// `findWordForward`: where one word right of the start of `after` lands, in bytes.
    fn pi_find_word_forward(&self, after: &str) -> usize {
        let segs = self.pi_segments(after);
        let mut it = segs.iter().copied().peekable();
        let mut cur = 0;
        while let Some(n) = it.peek() {
            if n.atomic || !n.is_space() {
                break;
            }
            cur += n.text.len();
            it.next();
        }
        let Some(next) = it.peek().copied() else {
            return cur;
        };
        if next.atomic {
            cur += next.text.len();
        } else if next.is_word() {
            cur += next
                .text
                .char_indices()
                .find(|(_, c)| is_pi_punct(*c))
                .map_or(next.text.len(), |(i, _)| i);
        } else {
            while let Some(n) = it.peek() {
                if n.atomic || n.is_word() || n.is_space() {
                    break;
                }
                cur += n.text.len();
                it.next();
            }
        }
        cur
    }

    fn pi_word_left(&mut self) {
        let ls = self.line_start(self.cursor);
        if self.cursor == ls {
            // at the start of a line: to the end of the previous one, and no further
            if ls > 0 {
                self.cursor = ls - 1;
            }
        } else {
            self.cursor = ls + self.pi_find_word_backward(&self.text[ls..self.cursor]);
        }
        self.goal_col = None;
        self.moved();
    }

    fn pi_word_right(&mut self) {
        let le = self.line_end(self.cursor);
        if self.cursor >= le {
            if le < self.text.len() {
                self.cursor = le + 1;
            }
        } else {
            self.cursor += self.pi_find_word_forward(&self.text[self.cursor..le]);
        }
        self.goal_col = None;
        self.moved();
    }

    // ---- kill ring ---------------------------------------------------------------------------

    /// `killRing.push`: a kill right after a kill joins the newest entry, in front of it when
    /// the text went backwards and behind it when it went forwards.
    fn pi_ring_push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        match self.pi.ring.last_mut() {
            Some(last) if accumulate => {
                if prepend {
                    last.insert_str(0, text);
                } else {
                    last.push_str(text);
                }
            }
            _ => self.pi.ring.push(text.to_string()),
        }
    }

    /// Remove `a..b` as a kill. `chain` is whether the previous key was a kill too.
    fn pi_kill(&mut self, a: usize, b: usize, prepend: bool, chain: bool) {
        self.record(EditKind::Other);
        let killed = self.text[a..b].to_string();
        self.pi_ring_push(&killed, prepend, chain);
        self.text.replace_range(a..b, "");
        self.cursor = a;
        self.last_kind = None;
        self.pi.kill_chain = true;
    }

    fn pi_delete_word_back(&mut self) {
        let chain = self.pi.kill_chain;
        let ls = self.line_start(self.cursor);
        if self.cursor == ls {
            // at the start of a line the newline is what goes
            if ls > 0 {
                self.pi_kill(ls - 1, ls, true, chain);
            }
            return;
        }
        let from = ls + self.pi_find_word_backward(&self.text[ls..self.cursor]);
        self.pi_kill(from, self.cursor, true, chain);
    }

    fn pi_delete_word_forward(&mut self) {
        let chain = self.pi.kill_chain;
        let le = self.line_end(self.cursor);
        if self.cursor >= le {
            if le < self.text.len() {
                self.pi_kill(le, le + 1, false, chain);
            }
            return;
        }
        let to = self.cursor + self.pi_find_word_forward(&self.text[self.cursor..le]);
        self.pi_kill(self.cursor, to, false, chain);
    }

    fn pi_kill_to_line_end(&mut self) {
        let chain = self.pi.kill_chain;
        let le = self.line_end(self.cursor);
        if self.cursor < le {
            self.pi_kill(self.cursor, le, false, chain);
        } else if le < self.text.len() {
            self.pi_kill(le, le + 1, false, chain);
        }
    }

    fn pi_kill_to_line_start(&mut self) {
        let chain = self.pi.kill_chain;
        let ls = self.line_start(self.cursor);
        if self.cursor > ls {
            self.pi_kill(ls, self.cursor, true, chain);
        } else if ls > 0 {
            self.pi_kill(ls - 1, ls, true, chain);
        }
    }

    fn pi_insert_yanked(&mut self, text: &str) {
        self.record(EditKind::Other);
        let start = self.cursor;
        self.text.insert_str(start, text);
        self.cursor = start + text.len();
        self.last_kind = None;
        self.pi.yanked = Some((start, self.cursor));
    }

    fn pi_yank(&mut self) {
        let Some(t) = self.pi.ring.last().cloned() else {
            return;
        };
        self.pi_insert_yanked(&t);
    }

    /// `alt+y`: replace what the last yank inserted with the next older kill.
    fn pi_yank_pop(&mut self) {
        let Some((a, b)) = self.pi.yanked else {
            return;
        };
        if self.pi.ring.len() <= 1 {
            return;
        }
        self.record(EditKind::Other);
        self.text.replace_range(a..b, "");
        self.cursor = a;
        let last = self.pi.ring.pop().unwrap_or_default();
        self.pi.ring.insert(0, last);
        let t = self.pi.ring.last().cloned().unwrap_or_default();
        self.text.insert_str(a, &t);
        self.cursor = a + t.len();
        self.last_kind = None;
        self.pi.yanked = Some((a, self.cursor));
    }

    // ---- arrows and history --------------------------------------------------------------------

    /// `up` on the first row: history when the editor is empty, already browsing, or the cursor
    /// is at the very start; otherwise the start of the line. Handled here, not by the host.
    fn pi_up(&mut self) -> KeyOutcome {
        let rows = self.layout(self.width);
        if self.row_of(&rows, self.cursor) == 0 {
            if self.text.is_empty() || self.history.pos.is_some() || self.cursor == 0 {
                // at the oldest entry this does nothing, as in Pi
                self.pi_history_step(-1);
            } else {
                self.move_line_start();
            }
            return KeyOutcome::Handled;
        }
        self.move_vertical(-1)
    }

    /// `down`: history only while browsing; on the last row it goes to the end of the line.
    fn pi_down(&mut self) -> KeyOutcome {
        let rows = self.layout(self.width);
        if self.row_of(&rows, self.cursor) + 1 >= rows.len() {
            if self.history.pos.is_some() {
                self.pi_history_step(1);
            } else {
                self.move_line_end();
            }
            return KeyOutcome::Handled;
        }
        self.move_vertical(1)
    }

    /// `navigateHistory`: -1 goes to an older entry with the cursor at its start, 1 to a newer
    /// one with the cursor at its end, and past the newest back to the draft, cursor included.
    fn pi_history_step(&mut self, dir: isize) -> bool {
        let n = self.history.entries.len();
        if n == 0 {
            return false;
        }
        let cur = self.history.pos.map_or(-1, |p| p as isize);
        // entries are oldest first here; Pi counts from the newest
        let from_new = if cur < 0 { -1 } else { n as isize - 1 - cur };
        let target = from_new - dir;
        if target < -1 || target >= n as isize {
            return false;
        }
        if cur < 0 {
            self.undo.push(Snapshot {
                text: self.text.clone(),
                cursor: self.cursor,
            });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
            self.history.draft = self.text.clone();
            self.history.draft_cursor = self.cursor;
        }
        self.goal_col = None;
        self.last_kind = None;
        self.pi.kill_chain = false;
        self.pi.yanked = None;
        if target == -1 {
            self.text = std::mem::take(&mut self.history.draft);
            self.cursor = self.history.draft_cursor.min(self.text.len());
            self.history.pos = None;
        } else {
            let i = n - 1 - target as usize;
            self.text = self.history.entries[i].clone();
            self.cursor = if dir < 0 { 0 } else { self.text.len() };
            self.history.pos = Some(i);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }
    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn alt(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::ALT)
    }
    fn plain(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn typed(s: &str) -> Editor {
        let mut e = Editor::new();
        for c in s.chars() {
            if c == '\n' {
                e.insert_newline();
            } else {
                e.apply_key(plain(KeyCode::Char(c)));
            }
        }
        e
    }

    #[test]
    fn typing_inserts_and_backspace_is_grapheme_aware() {
        let mut e = typed("héllo");
        assert_eq!(e.text(), "héllo");
        e.insert_char('e');
        e.insert_char('\u{301}'); // combining acute: one grapheme with the e
        e.backspace();
        assert_eq!(e.text(), "héllo");
        let mut e = Editor::with_text("a👨‍👩‍👧b");
        e.move_left();
        e.backspace();
        assert_eq!(e.text(), "ab");
        let mut e = Editor::with_text("日本");
        e.backspace();
        assert_eq!(e.text(), "日");
    }

    #[test]
    fn readline_cursor_keys() {
        let mut e = typed("foo bar baz");
        e.apply_key(ctrl('a'));
        assert_eq!(e.cursor(), 0);
        e.apply_key(ctrl('e'));
        assert_eq!(e.cursor(), 11);
        e.apply_key(ctrl('b'));
        e.apply_key(ctrl('b'));
        assert_eq!(e.cursor(), 9);
        e.apply_key(ctrl('f'));
        assert_eq!(e.cursor(), 10);
        e.apply_key(alt('b'));
        assert_eq!(e.cursor(), 8);
        e.apply_key(alt('b'));
        assert_eq!(e.cursor(), 4);
        e.apply_key(alt('f'));
        assert_eq!(e.cursor(), 7);
        e.apply_key(key(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(e.cursor(), 4);
        e.apply_key(key(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(e.cursor(), 7);
        e.apply_key(plain(KeyCode::Home));
        assert_eq!(e.cursor(), 0);
        e.apply_key(plain(KeyCode::End));
        assert_eq!(e.cursor(), 11);
    }

    #[test]
    fn kill_and_delete_bindings() {
        let mut e = typed("foo bar baz");
        e.apply_key(ctrl('w'));
        assert_eq!(e.text(), "foo bar ");
        e.apply_key(ctrl('y'));
        assert_eq!(e.text(), "foo bar baz");
        e.apply_key(ctrl('u'));
        assert_eq!(e.text(), "");
        e.apply_key(ctrl('y'));
        assert_eq!(e.text(), "foo bar baz");
        e.move_doc_start();
        e.apply_key(ctrl('k'));
        assert_eq!(e.text(), "");
        let mut e = typed("abc def");
        e.move_doc_start();
        e.apply_key(alt('d'));
        assert_eq!(e.text(), " def");
        e.move_doc_end();
        e.apply_key(key(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(e.text(), " ");
        let mut e = typed("abc");
        e.move_left();
        e.apply_key(ctrl('d'));
        assert_eq!(e.text(), "ab");
        e.apply_key(ctrl('h'));
        assert_eq!(e.text(), "a");
    }

    #[test]
    fn ctrl_d_on_empty_is_left_to_the_host() {
        let mut e = Editor::new();
        assert_eq!(e.apply_key(ctrl('d')), KeyOutcome::Ignored);
        e.insert_char('x');
        assert_eq!(e.apply_key(ctrl('d')), KeyOutcome::Handled);
    }

    #[test]
    fn kill_line_joins_across_newlines() {
        let mut e = typed("one\ntwo");
        e.move_doc_start();
        e.move_line_end();
        e.apply_key(ctrl('k')); // at end of "one": eats the newline
        assert_eq!(e.text(), "onetwo");
        let mut e = typed("one\ntwo");
        e.apply_key(ctrl('u')); // cursor at end of "two"
        assert_eq!(e.text(), "one\n");
        e.apply_key(ctrl('u')); // at start of empty last line: join
        assert_eq!(e.text(), "one");
    }

    #[test]
    fn enter_tab_escape_are_not_consumed() {
        let mut e = typed("x");
        for k in [
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::BackTab,
            KeyCode::F(1),
        ] {
            assert_eq!(e.apply_key(plain(k)), KeyOutcome::Ignored, "{k:?}");
        }
        assert_eq!(e.apply_key(ctrl('c')), KeyOutcome::Ignored);
        assert_eq!(e.text(), "x");
        let mut released = plain(KeyCode::Char('y'));
        released.kind = KeyEventKind::Release;
        assert_eq!(e.apply_key(released), KeyOutcome::Ignored);
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn newline_and_line_col() {
        let mut e = typed("ab");
        e.insert_newline();
        e.insert_char('é');
        assert_eq!(e.text(), "ab\né");
        assert_eq!(e.line_col(), (1, 1));
        assert_eq!(e.line_count(), 2);
    }

    #[test]
    fn paste_normalises_and_is_one_undo_step() {
        let mut e = typed("a");
        e.paste("x\r\ny\rz\x07\x1b[31m");
        assert_eq!(e.text(), "ax\ny\nz[31m");
        e.undo();
        assert_eq!(e.text(), "a");
        e.redo();
        assert_eq!(e.text(), "ax\ny\nz[31m");
        e.paste("\x07\x08");
        assert_eq!(e.text(), "ax\ny\nz[31m");
    }

    #[test]
    fn undo_groups_typing_by_word_and_redo_restores() {
        let mut e = typed("hello world");
        e.undo();
        assert_eq!(e.text(), "hello ");
        e.undo();
        assert_eq!(e.text(), "");
        assert!(!e.undo());
        e.redo();
        assert_eq!(e.text(), "hello ");
        e.insert_char('z');
        assert!(!e.can_redo(), "a new edit clears redo");
        e.apply_key(ctrl('z'));
        assert_eq!(e.text(), "hello ");
        e.apply_key(key(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(e.text(), "hello z");
    }

    #[test]
    fn undo_restores_cursor_and_deletions() {
        let mut e = typed("abc");
        e.backspace();
        e.backspace();
        e.undo();
        assert_eq!(e.text(), "abc");
        assert_eq!(e.cursor(), 3);
        e.move_doc_start();
        e.delete_word_forward();
        assert_eq!(e.text(), "");
        e.undo();
        assert_eq!(e.text(), "abc");
    }

    #[test]
    fn up_down_walk_logical_lines_keeping_the_column() {
        let mut e = typed("hello\nhi\nworld!");
        e.set_wrap_width(40);
        assert_eq!(e.apply_key(plain(KeyCode::Up)), KeyOutcome::Handled);
        // column 5 does not exist on "hi": clamp to its end
        assert_eq!(&e.text()[..e.cursor()], "hello\nhi");
        e.apply_key(plain(KeyCode::Up));
        assert_eq!(&e.text()[..e.cursor()], "hello");
        assert_eq!(e.apply_key(plain(KeyCode::Up)), KeyOutcome::AtTop);
        e.apply_key(plain(KeyCode::Down));
        e.apply_key(plain(KeyCode::Down));
        // the goal column (6, from the end of "world!") survived the short and the clamped lines
        assert_eq!(&e.text()[..e.cursor()], "hello\nhi\nworld!");
        assert_eq!(e.apply_key(plain(KeyCode::Down)), KeyOutcome::AtBottom);
    }

    #[test]
    fn up_down_follow_soft_wrapped_rows() {
        let mut e = Editor::with_text("aaaa bbbb cccc");
        e.set_wrap_width(5); // rows: "aaaa ", "bbbb ", "cccc"
        assert_eq!(e.layout(5).len(), 3);
        e.set_cursor(12); // after "cc" in the last row
        e.move_up();
        assert_eq!(&e.text()[..e.cursor()], "aaaa bb");
        e.move_up();
        assert_eq!(&e.text()[..e.cursor()], "aa");
        assert_eq!(e.move_up(), KeyOutcome::AtTop);
        e.move_down();
        e.move_down();
        assert_eq!(&e.text()[..e.cursor()], "aaaa bbbb cc");
    }

    #[test]
    fn vertical_movement_never_lands_inside_a_wide_char() {
        let mut e = Editor::with_text("日本語日本語\nabcdefghij");
        e.set_wrap_width(40);
        e.set_cursor(e.text().len());
        e.set_cursor(e.text().len() - 5); // column 5 of the second line
        e.move_up();
        let before = &e.text()[..e.cursor()];
        assert!(e.text().is_char_boundary(e.cursor()));
        assert_eq!(display_width(before) % 2, 0);
    }

    #[test]
    fn history_walks_back_forward_and_restores_the_draft() {
        let mut e = Editor::new();
        for h in ["one", "two", "three"] {
            e.history_push(h);
        }
        e.history_push("three"); // consecutive duplicate skipped
        e.history_push("  ");
        assert_eq!(e.history_entries().count(), 3);
        e.insert_char('d');
        e.move_left(); // cursor before the draft, so the prefix is empty
        assert!(e.history_prev());
        assert_eq!(e.text(), "three");
        assert!(e.history_prev());
        assert!(e.history_prev());
        assert_eq!(e.text(), "one");
        assert!(!e.history_prev());
        assert!(e.history_next());
        assert_eq!(e.text(), "two");
        assert!(e.history_next());
        assert!(e.history_next());
        assert_eq!(e.text(), "d", "past the newest entry brings the draft back");
        assert!(!e.history_next());
    }

    #[test]
    fn history_prefix_filters_entries() {
        let mut e = Editor::new();
        for h in ["git status", "ls", "git diff", "cargo test"] {
            e.history_push(h);
        }
        e.paste("git");
        assert!(e.history_prev());
        assert_eq!(e.text(), "git diff");
        assert!(e.history_prev());
        assert_eq!(e.text(), "git status");
        assert!(!e.history_prev());
        assert!(e.history_next());
        assert_eq!(e.text(), "git diff");
        assert!(e.history_next());
        assert_eq!(e.text(), "git", "draft restored");
    }

    #[test]
    fn editing_during_history_navigation_ends_it() {
        let mut e = Editor::new();
        e.history_push("old");
        assert!(e.history_prev());
        e.insert_char('!');
        assert!(!e.history_next(), "no longer navigating after an edit");
        assert_eq!(e.text(), "old!");
    }

    #[test]
    fn submit_returns_text_and_feeds_history() {
        let mut e = typed("run it");
        let s = e.submit();
        assert_eq!(s, "run it");
        assert!(e.is_empty());
        assert!(!e.can_undo());
        assert!(e.history_prev());
        assert_eq!(e.text(), "run it");
        let mut e2 = Editor::new();
        assert_eq!(e2.submit(), "");
        assert_eq!(e2.history_entries().count(), 0);
    }

    #[test]
    fn layout_wraps_at_words_and_hard_breaks() {
        let e = Editor::with_text("hello world\nxy");
        let rows = e.layout(6);
        let t = |r: &VisualRow| e.text()[r.start..r.end].to_string();
        assert_eq!(
            rows.iter().map(t).collect::<Vec<_>>(),
            vec!["hello", "world", "xy"]
        );
        assert!(!rows[0].last_in_line && rows[1].last_in_line);
        let e = Editor::with_text("abcdefgh");
        assert_eq!(e.layout(3).len(), 3);
        assert_eq!(e.layout(0).len(), 1);
        assert_eq!(Editor::new().layout(5).len(), 1);
    }

    #[test]
    fn cursor_after_a_full_row_gets_its_own_row() {
        let e = Editor::with_text("abcd");
        assert_eq!(e.needed_height(4), 2);
        assert_eq!(e.needed_height(5), 1);
        let mut e = Editor::with_text("abcd");
        e.move_left();
        assert_eq!(e.needed_height(4), 1);
    }

    fn draw(e: &mut Editor, w: u16, h: u16) -> (Vec<String>, RenderInfo) {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let info = e.render(area, &mut buf, &EditorStyle::default());
        let rows = (0..h).map(|y| crate::testing::row_text(&buf, y)).collect();
        (rows, info)
    }

    #[test]
    fn render_draws_wrapped_text_and_reports_cursor_and_height() {
        let mut e = Editor::with_text("hello world foo");
        let (rows, info) = draw(&mut e, 8, 4);
        assert_eq!(rows, vec!["hello", "world", "foo", ""]);
        assert_eq!(info.needed_height, 3);
        assert_eq!(info.cursor, Some((3, 2)));
        e.move_doc_start();
        let (_, info) = draw(&mut e, 8, 4);
        assert_eq!(info.cursor, Some((0, 0)));
    }

    #[test]
    fn render_scrolls_to_keep_the_cursor_visible() {
        let mut e = Editor::with_text("1\n2\n3\n4\n5\n6");
        let (rows, info) = draw(&mut e, 4, 3);
        assert_eq!(rows, vec!["4", "5", "6"]);
        assert_eq!(info.cursor, Some((1, 2)));
        assert_eq!(info.needed_height, 6);
        e.move_doc_start();
        let (rows, info) = draw(&mut e, 4, 3);
        assert_eq!(rows, vec!["1", "2", "3"]);
        assert_eq!(info.cursor, Some((0, 0)));
    }

    #[test]
    fn render_placeholder_when_empty_and_cursor_at_origin() {
        let mut e = Editor::new();
        let area = Rect::new(2, 1, 20, 2);
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 4));
        let style = EditorStyle {
            placeholder: Some(("Ask anything".into(), Style::default())),
            ..Default::default()
        };
        let info = e.render(area, &mut buf, &style);
        assert_eq!(info.cursor, Some((2, 1)));
        assert_eq!(buf[(2, 1)].symbol(), "A");
    }

    #[test]
    fn render_handles_wide_chars_tabs_and_zero_area() {
        let mut e = Editor::with_text("日本\tx");
        let (rows, info) = draw(&mut e, 12, 2);
        assert_eq!(rows[0], "日本    x");
        assert_eq!(info.cursor, Some((9, 0)));
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(area);
        let info = e.render(area, &mut buf, &EditorStyle::default());
        assert_eq!(info.cursor, None);
        let (rows, _) = draw(&mut e, 1, 3);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn click_moves_the_cursor_to_the_cell() {
        let mut e = Editor::with_text("hello\nworld");
        let area = Rect::new(5, 2, 20, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 8));
        e.render(area, &mut buf, &EditorStyle::default());
        e.click(area, 7, 3);
        assert_eq!(&e.text()[..e.cursor()], "hello\nwo");
        e.click(area, 25, 2);
        assert_eq!(&e.text()[..e.cursor()], "hello");
        e.click(area, 6, 5); // below the text
        assert_eq!(e.cursor(), e.text().len());
    }

    #[test]
    fn grown_height_clamps_between_min_and_max() {
        let e = Editor::with_text("1\n2\n3\n4\n5");
        assert_eq!(e.grown_height(10, 1, 3), 3);
        assert_eq!(e.grown_height(10, 7, 9), 7);
        assert_eq!(Editor::new().grown_height(10, 1, 6), 1);
    }

    #[test]
    fn set_cursor_snaps_to_grapheme_boundaries() {
        let mut e = Editor::with_text("e\u{301}x日");
        e.set_cursor(1); // inside e + combining mark
        assert_eq!(e.cursor(), 0);
        e.set_cursor(5); // middle of the 3-byte 日
        assert!(e.text().is_char_boundary(e.cursor()));
        e.set_cursor(999);
        assert_eq!(e.cursor(), e.text().len());
    }

    #[test]
    fn set_text_is_undoable_and_sanitised() {
        let mut e = typed("draft");
        e.set_text("new\r\nvalue\x07");
        assert_eq!(e.text(), "new\nvalue");
        e.undo();
        assert_eq!(e.text(), "draft");
    }

    #[test]
    fn random_key_soup_never_panics_or_breaks_boundaries() {
        let mut e = Editor::with_text("日本語 text\nline é\u{301} 👨‍👩‍👧 end");
        e.set_wrap_width(7);
        let codes = [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Char('x'),
            KeyCode::Char('語'),
            KeyCode::Char('b'),
        ];
        let mods = [KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT];
        let mut seed = 12345u64;
        for _ in 0..3000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let c = codes[(seed >> 33) as usize % codes.len()];
            let m = mods[(seed >> 20) as usize % mods.len()];
            e.apply_key(key(c, m));
            assert!(e.text().is_char_boundary(e.cursor()));
            if seed.is_multiple_of(97) {
                let _ = draw(&mut e, (seed % 12) as u16, 3);
            }
        }
    }

    // ---- Pi's keys -----------------------------------------------------------------------

    fn pi(text: &str) -> Editor {
        let mut e = Editor::new();
        e.set_pi_keys(true);
        e.set_wrap_width(40);
        e.set_text(text);
        e
    }

    fn chars_to(e: &Editor) -> usize {
        e.text()[..e.cursor()].chars().count()
    }

    #[test]
    fn word_stops_and_word_kills_are_the_ones_pis_own_functions_give() {
        let golden: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../themes-pi/words-golden.json")).unwrap();
        assert!(golden.len() >= 15);
        for g in &golden {
            let text = g["text"].as_str().unwrap();
            let stops = |k: &str| -> Vec<usize> {
                g[k].as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap() as usize)
                    .collect()
            };
            // alt+f from the start, one stop at a time
            let mut e = pi(text);
            e.move_doc_start();
            let mut got = Vec::new();
            while e.cursor() < e.text().len() {
                e.apply_key(alt('f'));
                got.push(chars_to(&e));
            }
            assert_eq!(got, stops("fwd"), "alt+f over {text:?}");
            // alt+b from the end
            let mut e = pi(text);
            let mut got = Vec::new();
            while e.cursor() > 0 {
                e.apply_key(alt('b'));
                got.push(chars_to(&e));
            }
            assert_eq!(got, stops("back"), "alt+b over {text:?}");
            // ctrl+w from the end: what is left, and what ctrl+y would bring back
            let mut e = pi(text);
            for (i, step) in g["ctrlw"].as_array().unwrap().iter().enumerate() {
                e.apply_key(ctrl('w'));
                assert_eq!(
                    e.text(),
                    step["text"].as_str().unwrap(),
                    "ctrl+w #{i} over {text:?}"
                );
                let mut y = Editor::new();
                y.set_pi_keys(true);
                y.pi.ring = e.pi.ring.clone();
                y.apply_key(ctrl('y'));
                assert_eq!(
                    y.text(),
                    step["ring"].as_str().unwrap(),
                    "ring after ctrl+w #{i} over {text:?}"
                );
            }
            // alt+d from the start
            let mut e = pi(text);
            e.move_doc_start();
            for (i, step) in g["altd"].as_array().unwrap().iter().enumerate() {
                e.apply_key(alt('d'));
                assert_eq!(
                    e.text(),
                    step["text"].as_str().unwrap(),
                    "alt+d #{i} over {text:?}"
                );
                let mut y = Editor::new();
                y.set_pi_keys(true);
                y.pi.ring = e.pi.ring.clone();
                y.apply_key(ctrl('y'));
                assert_eq!(
                    y.text(),
                    step["ring"].as_str().unwrap(),
                    "ring after alt+d #{i} over {text:?}"
                );
            }
        }
    }

    #[test]
    fn the_opencode_word_rules_are_unchanged_without_pi_keys() {
        let mut e = Editor::with_text("foo.bar-baz");
        e.move_doc_start();
        e.apply_key(alt('f'));
        assert_eq!(e.cursor(), 3);
        e.apply_key(alt('f'));
        assert_eq!(e.cursor(), 7, "punctuation is skipped, not a stop");
    }

    #[test]
    fn word_motion_stops_at_line_ends_and_a_paste_marker_is_one_unit() {
        let mut e = pi("one\ntwo");
        e.move_doc_start();
        e.apply_key(alt('f'));
        assert_eq!(
            e.line_col(),
            (0, 3),
            "stops at the end of the line, does not cross it"
        );
        e.apply_key(alt('f'));
        assert_eq!(
            e.line_col(),
            (1, 0),
            "from the end of a line the next stop is the next line's start"
        );
        e.apply_key(alt('b'));
        assert_eq!(e.line_col(), (0, 3));

        let mut e = pi("see [paste #1 +30 lines] now");
        e.set_atomic_markers(vec!["[paste #1 +30 lines]".into()]);
        e.move_doc_start();
        e.apply_key(alt('f'));
        e.apply_key(alt('f'));
        assert_eq!(
            e.cursor(),
            "see [paste #1 +30 lines]".len(),
            "the marker is one stop"
        );
        e.apply_key(alt('b'));
        assert_eq!(e.cursor(), 4);
    }

    #[test]
    fn consecutive_kills_make_one_ring_entry_and_other_keys_end_the_run() {
        let mut e = pi("one two three");
        e.apply_key(ctrl('w'));
        e.apply_key(ctrl('w'));
        e.apply_key(ctrl('y'));
        assert_eq!(
            e.text(),
            "one two three",
            "two kills backwards are one entry, in order"
        );

        let mut e = pi("abc def");
        e.move_doc_start();
        e.apply_key(alt('d'));
        e.apply_key(alt('d'));
        e.apply_key(ctrl('y'));
        assert_eq!(e.text(), "abc def");

        // a move between two kills starts a new entry
        let mut e = pi("one two three");
        e.apply_key(ctrl('w'));
        e.apply_key(plain(KeyCode::Left));
        e.apply_key(ctrl('w'));
        assert_eq!(e.pi.ring, vec!["three".to_string(), "two".to_string()]);
        // ctrl+k at the end of a line kills the newline, and joins the run
        let mut e = pi("ab\ncd");
        e.move_doc_start();
        e.apply_key(ctrl('k'));
        e.apply_key(ctrl('k'));
        assert_eq!(e.text(), "cd");
        assert_eq!(e.pi.ring, vec!["ab\n".to_string()]);
    }

    #[test]
    fn alt_y_cycles_the_ring_right_after_a_yank_only() {
        let mut e = pi("one two three");
        e.apply_key(ctrl('w'));
        e.apply_key(plain(KeyCode::Left));
        e.apply_key(ctrl('w'));
        e.set_text("");
        e.apply_key(ctrl('y'));
        assert_eq!(e.text(), "two");
        e.apply_key(alt('y'));
        assert_eq!(e.text(), "three");
        e.apply_key(alt('y'));
        assert_eq!(e.text(), "two");
        e.apply_key(plain(KeyCode::Char('x')));
        e.apply_key(alt('y'));
        assert_eq!(
            e.text(),
            "twox",
            "alt+y does nothing once something else was typed"
        );
    }

    #[test]
    fn up_recalls_with_the_cursor_at_the_start_and_down_at_the_end_and_edits_leave_the_walk() {
        let mut e = pi("");
        e.history_push("one [[hello]]");
        e.history_push("two [[hello]]");
        e.apply_key(plain(KeyCode::Up));
        assert_eq!(
            (e.text(), e.cursor()),
            ("two [[hello]]", 0),
            "cursor at the start going up"
        );
        e.apply_key(plain(KeyCode::Up));
        assert_eq!((e.text(), e.cursor()), ("one [[hello]]", 0));
        e.apply_key(plain(KeyCode::Up));
        assert_eq!(e.text(), "one [[hello]]", "nothing older, nothing moves");
        e.apply_key(plain(KeyCode::Down));
        assert_eq!(
            (e.text(), e.cursor()),
            ("two [[hello]]", 13),
            "cursor at the end going down"
        );
        e.apply_key(plain(KeyCode::Down));
        assert_eq!(
            (e.text(), e.cursor()),
            ("", 0),
            "past the newest: the draft is back"
        );

        // browsing continues with the cursor anywhere in the recalled text
        e.apply_key(plain(KeyCode::Up));
        e.apply_key(plain(KeyCode::Right));
        e.apply_key(plain(KeyCode::Up));
        assert_eq!(e.text(), "one [[hello]]");
        // typing ends the walk: the next Up on a one-line text only moves the cursor
        e.apply_key(plain(KeyCode::Char('!')));
        e.apply_key(plain(KeyCode::Up));
        assert_eq!(e.text(), "!one [[hello]]");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn up_only_starts_the_walk_from_an_empty_editor_or_the_very_start_of_the_text() {
        let mut e = pi("");
        e.history_push("old");
        e.set_text("draft");
        e.apply_key(plain(KeyCode::Up));
        assert_eq!(
            (e.text(), e.cursor()),
            ("draft", 0),
            "a typed draft: Up goes to the line start"
        );
        e.apply_key(plain(KeyCode::Up));
        assert_eq!(e.text(), "old", "already at the start: now it browses");
        e.apply_key(plain(KeyCode::Down));
        assert_eq!(
            (e.text(), e.cursor()),
            ("draft", 0),
            "the draft comes back with its cursor"
        );
        // Down with no walk goes to the end of the line
        e.apply_key(plain(KeyCode::Down));
        assert_eq!(e.cursor(), 5);
    }

    #[test]
    fn ctrl_h_is_ignored_in_pi_mode_and_backspace_is_not() {
        let mut e = pi("abc");
        assert_eq!(e.apply_key(ctrl('h')), KeyOutcome::Ignored);
        assert_eq!(e.text(), "abc");
        e.apply_key(plain(KeyCode::Backspace));
        assert_eq!(e.text(), "ab");
    }
}
